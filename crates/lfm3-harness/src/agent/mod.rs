//! The model-independent agent loop.
//!
//! The runner owns durability and verification order; a [`StepDriver`] performs
//! whatever model/tool work a step needs. Nothing here knows about llama.cpp,
//! Tauri, or any concrete transport, so fake drivers and real drivers share the
//! same state machine.

pub mod executor;
pub mod grounding;
pub mod loop_agent;
pub mod packets;
pub mod planner;
pub mod profile;
pub mod recovery;
pub mod runner;

use std::{fmt, future::Future, pin::Pin, sync::Arc};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::{
    mission::{AcceptanceSpec, CheckSpec, ExecutionStrategy, PlannedStep},
    tools::{CommandRun, ToolResult},
    verify::{
        evaluate_semantic, is_semantic_kind, CheckContext, CheckError, CheckResult, FileManifest,
        SemanticJudge, SemanticPolicy,
    },
    AttemptId, CancelReason, CancellationChild, RunId, RunStatus,
};

pub use executor::{Executor, StepAction};
pub use loop_agent::{
    AgentLoop, DeliverableProof, LoopCall, LoopError, LoopOptions, LoopPhase, LoopReport,
    LoopStatus, LoopTurn, TodoCheck, TodoCheckKind, TodoItem,
};
pub use packets::{FailurePacket, RemainingBudget};
pub use planner::{decode_single_json, plan_request, PlanRequest, Planner, PlannerError};
pub use profile::{ModelProfile, D1, LFM3_AUDIO, LFM3_TEXT, LFM3_VL};
pub use recovery::{
    Budget, FailureClass, FailureRecord, RecoveryController, RecoveryDecision, RecoveryError,
    StopReason,
};
pub use runner::Runner;

/// A future used by the object-safe driver and verifier traits.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// One completed step, carried into the prompts of later steps.
///
/// A job is a sequence: the model that writes the report must be able to use
/// what the model that read the notes actually saw. Observations are bounded so
/// the running context stays small on a 4k-token local window.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct StepObservation {
    pub step_index: u32,
    pub title: String,
    pub tools: Vec<String>,
    pub output: String,
}

impl StepObservation {
    #[must_use]
    pub fn from_execution(step: &PlannedStep, execution: &StepExecution) -> Self {
        let mut tools = Vec::new();
        let mut output = String::new();
        for result in &execution.tool_results {
            tools.push(result.tool.as_str().to_owned());
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(&result.output);
        }
        let output: String = output.chars().take(4_000).collect();
        Self {
            step_index: step.index,
            title: step.title.clone(),
            tools,
            output,
        }
    }
}

/// Everything one attempt needs before it may touch the workspace.
pub struct StepRequest<'a> {
    pub run_id: &'a RunId,
    pub acceptance: &'a AcceptanceSpec,
    pub step: &'a PlannedStep,
    pub attempt: &'a AttemptId,
    pub attempt_number: u32,
    pub strategy: u32,
    /// Digests captured before the step started, for change checks.
    pub before: &'a FileManifest,
    /// Checks that failed on earlier attempts of this step.
    pub prior_failures: &'a [CheckResult],
    /// Compact packet for a retry or replan; `None` on the first attempt.
    pub packet: Option<&'a FailurePacket>,
    /// Observables from steps that already passed in this run.
    pub history: &'a [StepObservation],
}

/// Typed observations returned by one attempt.
#[derive(Clone, Debug)]
pub struct StepExecution {
    /// Identity of the model that acted, recorded for audit.
    pub model: String,
    /// The action that was executed, recorded with the attempt.
    pub action: Value,
    pub tool_results: Vec<ToolResult>,
    pub command_runs: Vec<CommandRun>,
}

#[derive(Debug, Error)]
pub enum DriverError {
    #[error("step driver failed: {0}")]
    Failed(String),
    #[error("step driver produced an invalid action: {0}")]
    InvalidAction(String),
    #[error("step driver cancelled: {0}")]
    Cancelled(CancelReason),
}

impl DriverError {
    #[must_use]
    pub const fn reason(&self) -> Option<CancelReason> {
        match self {
            Self::Cancelled(reason) => Some(*reason),
            Self::Failed(_) | Self::InvalidAction(_) => None,
        }
    }
}

/// Executes one attempt of one step. Implementations must respect `cancel`.
pub trait StepDriver: Send + Sync {
    /// The model identity used for this step, recorded when the attempt starts.
    fn model(&self) -> String;

    fn execute<'a>(
        &'a self,
        request: StepRequest<'a>,
        cancel: &'a CancellationChild,
    ) -> BoxFuture<'a, Result<StepExecution, DriverError>>;

    /// Produces a new strategy after recovery decides the current one made no
    /// progress. The strategy must keep the frozen acceptance hash; drivers
    /// without replanning support use the default failure.
    fn replan_strategy<'a>(
        &'a self,
        _request: StepRequest<'a>,
        _cancel: &'a CancellationChild,
    ) -> BoxFuture<'a, Result<ExecutionStrategy, DriverError>> {
        Box::pin(async {
            Err(DriverError::Failed(
                "this step driver cannot replan a strategy".to_owned(),
            ))
        })
    }
}

/// Decides a frozen check. Production uses the deterministic verifier; tests
/// may wrap it to model slow or lying verifiers.
pub trait Verifier: Send + Sync {
    fn evaluate<'a>(
        &'a self,
        spec: CheckSpec,
        context: CheckContext,
    ) -> BoxFuture<'a, Result<CheckResult, CheckError>>;
}

/// The production verifier: deterministic checks only.
#[derive(Clone, Copy, Debug, Default)]
pub struct DeterministicVerifier;

impl Verifier for DeterministicVerifier {
    fn evaluate<'a>(
        &'a self,
        spec: CheckSpec,
        context: CheckContext,
    ) -> BoxFuture<'a, Result<CheckResult, CheckError>> {
        Box::pin(crate::verify::evaluate(spec, context))
    }
}

/// The production verifier for a mission that declares semantic criteria.
///
/// Deterministic kinds keep the deterministic path unchanged; only the four
/// semantic kinds consult the judge, and only under the mission's frozen
/// policy. A human-review-only policy therefore turns every semantic criterion
/// into an explicit error instead of a silent pass.
#[derive(Clone)]
pub struct SemanticVerifier {
    policy: SemanticPolicy,
    judge: Arc<dyn SemanticJudge>,
}

impl SemanticVerifier {
    #[must_use]
    pub fn new(policy: SemanticPolicy, judge: Arc<dyn SemanticJudge>) -> Self {
        Self { policy, judge }
    }
}

impl Verifier for SemanticVerifier {
    fn evaluate<'a>(
        &'a self,
        spec: CheckSpec,
        context: CheckContext,
    ) -> BoxFuture<'a, Result<CheckResult, CheckError>> {
        if !is_semantic_kind(&spec.kind) {
            return Box::pin(crate::verify::evaluate(spec, context));
        }
        let judge = Arc::clone(&self.judge);
        let policy = self.policy.clone();
        Box::pin(async move { Ok(evaluate_semantic(spec, context, judge, policy).await) })
    }
}

/// Why a run stopped without passing.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncompleteReason {
    /// The last step passed, but final output verification did not.
    FinalVerificationFailed,
    /// Every allowed attempt for a step was used without a verified pass.
    StepBudgetExhausted { step_index: u32 },
    /// The same action or failure signature repeated; retrying cannot help.
    NoProgress,
    /// A check could not be decided and a person must review the evidence.
    NeedsReview,
    /// The goal cannot be verified, approval was denied, or the model is gone.
    Unrecoverable,
    /// A person or deadline cancelled the run.
    Cancelled,
}

impl fmt::Display for IncompleteReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FinalVerificationFailed => formatter.write_str("final verification failed"),
            Self::StepBudgetExhausted { step_index } => {
                write!(formatter, "step {step_index} exhausted its attempt budget")
            }
            Self::NoProgress => formatter.write_str("recovery made no progress"),
            Self::NeedsReview => formatter.write_str("evidence needs human review"),
            Self::Unrecoverable => formatter.write_str("the step cannot be recovered"),
            Self::Cancelled => formatter.write_str("run cancelled"),
        }
    }
}

/// The terminal result of one runner invocation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RunOutcome {
    pub run_id: RunId,
    pub status: RunStatus,
    pub reason: Option<IncompleteReason>,
}

impl RunOutcome {
    #[must_use]
    pub fn new(run_id: RunId, status: RunStatus, reason: Option<IncompleteReason>) -> Self {
        Self {
            run_id,
            status,
            reason,
        }
    }
}

/// Shared verifier handle used by the runner.
pub fn verifier() -> Arc<dyn Verifier> {
    Arc::new(DeterministicVerifier)
}
