//! Durable multi-step runner with evidence-driven recovery.
//!
//! Every step is persisted before any side effect. Attempts are committed
//! before a driver may act, checks are evaluated against typed evidence, and a
//! run reaches `passed` only after the frozen plan, required artifacts, and
//! final checks all pass. Failures are classified, journaled as recovery
//! decisions, and retried, backed off, or replanned strictly inside the frozen
//! budget. Replanning replaces a strategy, never the acceptance contract.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
};

use serde_json::json;

use crate::{
    mission::{AcceptanceSpec, CheckSpec, FrozenAcceptance, PlannedStep, RequiredArtifact},
    store::{ArtifactStore, AttemptStatus, FinishAttempt, RunStatus, StartAttempt, Store},
    verify::{CheckContext, CheckResult, CheckStatus, FileManifest},
    AttemptId, CancellationContext, Cancelled, HarnessError, RunId,
};

use super::{
    packets::{self, FailurePacket},
    recovery::{
        Budget, FailureClass, FailureRecord, RecoveryController, RecoveryDecision, StopReason,
    },
    verifier, DriverError, IncompleteReason, RunOutcome, StepDriver, StepExecution,
    StepObservation, StepRequest, Verifier,
};

/// How many completed steps stay in the running context.
const HISTORY_LIMIT: usize = 6;

/// Executes one frozen plan against one workspace.
#[derive(Clone)]
pub struct Runner {
    store: Store,
    acceptance: FrozenAcceptance,
    workspace: PathBuf,
    driver: Arc<dyn StepDriver>,
    verifier: Arc<dyn Verifier>,
    cancel: CancellationContext,
    final_checks: Vec<CheckSpec>,
    recovery: Arc<Mutex<RecoveryController>>,
    history: Arc<Mutex<Vec<StepObservation>>>,
}

impl Runner {
    pub fn new(
        store: Store,
        acceptance: FrozenAcceptance,
        workspace: impl Into<PathBuf>,
        driver: Arc<dyn StepDriver>,
        cancel: CancellationContext,
    ) -> Result<Self, HarnessError> {
        acceptance
            .verify_hash()
            .map_err(|error| HarnessError::Contract(error.to_string()))?;
        let requested = workspace.into();
        let workspace = std::fs::canonicalize(&requested).map_err(|error| {
            HarnessError::Storage(format!("workspace {}: {error}", requested.display()))
        })?;
        if !workspace.is_dir() {
            return Err(HarnessError::Storage(format!(
                "workspace {} is not a directory",
                workspace.display()
            )));
        }
        let budget = Budget::from_max_attempts(
            u32::try_from(acceptance.spec().max_attempts).unwrap_or(u32::MAX),
        );
        Ok(Self {
            store,
            acceptance,
            workspace,
            driver,
            verifier: verifier(),
            cancel,
            final_checks: Vec::new(),
            recovery: Arc::new(Mutex::new(RecoveryController::new(budget))),
            history: Arc::new(Mutex::new(Vec::new())),
        })
    }

    #[must_use]
    pub fn with_verifier(mut self, verifier: Arc<dyn Verifier>) -> Self {
        self.verifier = verifier;
        self
    }

    /// Replaces the recovery limits. The default budget comes from the frozen
    /// `max_attempts` with no strategy replanning.
    #[must_use]
    pub fn with_budget(mut self, budget: Budget) -> Self {
        self.recovery = Arc::new(Mutex::new(RecoveryController::new(budget)));
        self
    }

    /// Extra checks that must pass after the last step and before a pass.
    #[must_use]
    pub fn with_final_checks(mut self, checks: impl IntoIterator<Item = CheckSpec>) -> Self {
        self.final_checks.extend(checks);
        self
    }

    #[must_use]
    pub fn with_final_check(mut self, check: CheckSpec) -> Self {
        self.final_checks.push(check);
        self
    }

    #[must_use]
    pub fn acceptance(&self) -> &FrozenAcceptance {
        &self.acceptance
    }

    /// Runs the full state machine and reports how it ended.
    pub async fn execute(&self, run_id: &RunId) -> Result<RunOutcome, HarnessError> {
        let spec = self.acceptance.spec().clone();
        match self.store.snapshot(run_id).await?.run.status {
            status if status.is_terminal() => {
                return Ok(RunOutcome::new(run_id.clone(), status, None));
            }
            RunStatus::Created => {
                self.store
                    .transition_run(run_id.clone(), RunStatus::Created, RunStatus::Running, None)
                    .await?;
            }
            RunStatus::Running => {}
            other => {
                return Err(HarnessError::Conflict(format!(
                    "run {run_id} cannot execute from {}",
                    other.as_str()
                )));
            }
        }
        // All steps commit before the first driver call can touch anything.
        self.store
            .persist_pending_steps(run_id, &spec.steps)
            .await?;
        let run_before = self.capture_manifest()?;

        let mut last_executed: Option<StepExecution> = None;

        for step in &spec.steps {
            if self.cancelled() {
                return self
                    .finish(
                        run_id,
                        RunStatus::Cancelled,
                        Some(IncompleteReason::Cancelled),
                    )
                    .await;
            }
            self.lock_recovery().begin_step(step.index);
            match self.run_step(run_id, &spec, step).await? {
                StepOutcome::Passed(execution) => {
                    self.record_history(step, &execution);
                    last_executed = Some(execution);
                }
                StepOutcome::Stop(outcome) => return Ok(outcome),
            }
        }

        self.final_verify(run_id, &spec, &run_before, last_executed)
            .await
    }

    /// Attempts one step until it verifiably passes or recovery stops.
    async fn run_step(
        &self,
        run_id: &RunId,
        spec: &AcceptanceSpec,
        step: &PlannedStep,
    ) -> Result<StepOutcome, HarnessError> {
        let before = self.capture_manifest()?;
        let history = self.history_snapshot();
        let mut state = RetryState::default();
        let budget = self.lock_recovery().budget();
        // Each iteration spends one attempt or one strategy slot; this guard is
        // a backstop against a controller or driver that misreports progress.
        let limit = budget
            .total_attempts
            .saturating_add(budget.strategies)
            .saturating_add(2);
        let mut iterations = 0u32;

        loop {
            iterations = iterations.saturating_add(1);
            if iterations > limit {
                return Ok(StepOutcome::Stop(
                    self.finish(
                        run_id,
                        RunStatus::Incomplete,
                        Some(IncompleteReason::NoProgress),
                    )
                    .await?,
                ));
            }
            if self.cancelled() {
                return Ok(StepOutcome::Stop(
                    self.finish(
                        run_id,
                        RunStatus::Cancelled,
                        Some(IncompleteReason::Cancelled),
                    )
                    .await?,
                ));
            }

            let (strategy, attempt_number) = {
                let recovery = self.lock_recovery();
                (recovery.strategy(), recovery.next_attempt_number())
            };
            let attempt_id = self
                .store
                .start_attempt(StartAttempt {
                    run_id: run_id.clone(),
                    step_index: step.index,
                    strategy,
                    action: json!({
                        "phase": "proposed",
                        "step": step.index,
                        "attempt": attempt_number,
                    }),
                    model: self.driver.model(),
                    deadline_at: None,
                })
                .await?;

            if self.cancelled() {
                self.abort(run_id, &attempt_id, "cancelled before execution")
                    .await?;
                return Ok(StepOutcome::Stop(
                    self.finish(
                        run_id,
                        RunStatus::Cancelled,
                        Some(IncompleteReason::Cancelled),
                    )
                    .await?,
                ));
            }

            let executed = {
                let request = StepRequest {
                    run_id,
                    acceptance: spec,
                    step,
                    attempt: &attempt_id,
                    attempt_number,
                    strategy,
                    before: &before,
                    prior_failures: &state.prior_failures,
                    packet: state.packet.as_ref(),
                    history: &history,
                };
                let child = self.cancel.child();
                self.cancel.run(self.driver.execute(request, &child)).await
            };

            let execution = match executed {
                Err(_) | Ok(Err(DriverError::Cancelled(_))) => {
                    self.abort(run_id, &attempt_id, "cancelled during execution")
                        .await?;
                    return Ok(StepOutcome::Stop(
                        self.finish(
                            run_id,
                            RunStatus::Cancelled,
                            Some(IncompleteReason::Cancelled),
                        )
                        .await?,
                    ));
                }
                Ok(Err(DriverError::Failed(message))) => {
                    let control = self
                        .driver_failure(
                            run_id,
                            spec,
                            step,
                            &before,
                            &attempt_id,
                            strategy,
                            FailureClass::Transient,
                            "driver:failed",
                            message,
                            &mut state,
                        )
                        .await?;
                    match control {
                        Control::Continue => continue,
                        Control::Stop(outcome) => return Ok(StepOutcome::Stop(outcome)),
                    }
                }
                Ok(Err(DriverError::InvalidAction(message))) => {
                    let control = self
                        .driver_failure(
                            run_id,
                            spec,
                            step,
                            &before,
                            &attempt_id,
                            strategy,
                            FailureClass::InvalidAction,
                            "driver:invalid",
                            message,
                            &mut state,
                        )
                        .await?;
                    match control {
                        Control::Continue => continue,
                        Control::Stop(outcome) => return Ok(StepOutcome::Stop(outcome)),
                    }
                }
                Ok(Ok(execution)) => execution,
            };

            if self.cancelled() {
                self.abort(run_id, &attempt_id, "cancelled after execution")
                    .await?;
                return Ok(StepOutcome::Stop(
                    self.finish(
                        run_id,
                        RunStatus::Cancelled,
                        Some(IncompleteReason::Cancelled),
                    )
                    .await?,
                ));
            }

            let mut checks = Vec::with_capacity(step.checks.len());
            for check_spec in &step.checks {
                let mut context = CheckContext::new(
                    self.acceptance.hash().to_owned(),
                    step.index,
                    attempt_id.clone(),
                    self.workspace.clone(),
                );
                context.before = before.clone();
                context.tool_results = execution.tool_results.clone();
                context.command_runs = execution.command_runs.clone();
                context.artifacts = Some(ArtifactStore::new(self.store.artifacts_path()));
                context.inputs = self.acceptance.spec().inputs.clone();
                match self.verify(check_spec.clone(), context).await {
                    Ok(result) => checks.push(result),
                    Err(_) => {
                        self.abort(run_id, &attempt_id, "cancelled during verification")
                            .await?;
                        return Ok(StepOutcome::Stop(
                            self.finish(
                                run_id,
                                RunStatus::Cancelled,
                                Some(IncompleteReason::Cancelled),
                            )
                            .await?,
                        ));
                    }
                }
            }

            self.store
                .record_evidence(
                    run_id.clone(),
                    attempt_id.clone(),
                    execution.action.clone(),
                    execution.tool_results.clone(),
                    execution.command_runs.clone(),
                    checks.clone(),
                )
                .await?;

            if checks.iter().all(|check| check.status == CheckStatus::Pass) {
                if self.cancelled() {
                    self.abort(run_id, &attempt_id, "cancelled before step pass")
                        .await?;
                    return Ok(StepOutcome::Stop(
                        self.finish(
                            run_id,
                            RunStatus::Cancelled,
                            Some(IncompleteReason::Cancelled),
                        )
                        .await?,
                    ));
                }
                self.store
                    .finish_attempt(FinishAttempt {
                        run_id: run_id.clone(),
                        attempt_id: attempt_id.clone(),
                        status: AttemptStatus::Passed,
                        error: None,
                    })
                    .await?;
                return Ok(StepOutcome::Passed(execution));
            }

            let failures: Vec<CheckResult> = checks
                .iter()
                .filter(|check| check.status != CheckStatus::Pass)
                .cloned()
                .collect();
            let status = if checks.iter().any(|check| check.status == CheckStatus::Fail) {
                AttemptStatus::Failed
            } else {
                AttemptStatus::Inconclusive
            };
            let message = failures.first().map_or_else(
                || "step checks did not pass".to_owned(),
                |check| format!("{}: {}", check.kind, check.message),
            );
            self.store
                .finish_attempt(FinishAttempt {
                    run_id: run_id.clone(),
                    attempt_id: attempt_id.clone(),
                    status,
                    error: Some(message.clone()),
                })
                .await?;

            let failure = self.failure_record(
                step,
                FailureClass::CheckFailed,
                packets::canonical_action(&execution.action),
                &checks,
                &message,
            );
            match self
                .handle_failure(
                    run_id,
                    spec,
                    step,
                    &before,
                    &attempt_id,
                    strategy,
                    failure,
                    &checks,
                    &mut state,
                )
                .await?
            {
                Control::Continue => continue,
                Control::Stop(outcome) => return Ok(StepOutcome::Stop(outcome)),
            }
        }
    }

    /// Records a driver-side failure, then hands it to recovery.
    #[allow(clippy::too_many_arguments)]
    async fn driver_failure(
        &self,
        run_id: &RunId,
        spec: &AcceptanceSpec,
        step: &PlannedStep,
        before: &FileManifest,
        attempt: &AttemptId,
        strategy: u32,
        class: FailureClass,
        action: &str,
        message: String,
        state: &mut RetryState,
    ) -> Result<Control, HarnessError> {
        self.store
            .finish_attempt(FinishAttempt {
                run_id: run_id.clone(),
                attempt_id: attempt.clone(),
                status: AttemptStatus::Failed,
                error: Some(message.clone()),
            })
            .await?;
        let failure = self.failure_record(step, class, action, &[], &message);
        self.handle_failure(
            run_id,
            spec,
            step,
            before,
            attempt,
            strategy,
            failure,
            &[],
            state,
        )
        .await
    }

    /// Classifies, journals, and applies one failure. Returns whether the step
    /// may attempt again or the run has stopped.
    #[allow(clippy::too_many_arguments)]
    async fn handle_failure(
        &self,
        run_id: &RunId,
        spec: &AcceptanceSpec,
        step: &PlannedStep,
        before: &FileManifest,
        attempt: &AttemptId,
        strategy: u32,
        failure: FailureRecord,
        checks: &[CheckResult],
        state: &mut RetryState,
    ) -> Result<Control, HarnessError> {
        let decision = self.lock_recovery().next(failure.clone());
        self.journal_decision(run_id, step.index, strategy, decision, &failure)
            .await?;
        self.lock_recovery().record(failure.clone());

        match decision {
            RecoveryDecision::Cancelled => Ok(Control::Stop(
                self.finish(
                    run_id,
                    RunStatus::Cancelled,
                    Some(IncompleteReason::Cancelled),
                )
                .await?,
            )),
            RecoveryDecision::NeedsReview => Ok(Control::Stop(
                self.finish(
                    run_id,
                    RunStatus::Incomplete,
                    Some(IncompleteReason::NeedsReview),
                )
                .await?,
            )),
            RecoveryDecision::Incomplete { reason } => {
                let reason = match reason {
                    StopReason::BudgetExhausted => IncompleteReason::StepBudgetExhausted {
                        step_index: step.index,
                    },
                    StopReason::NoProgress => IncompleteReason::NoProgress,
                    StopReason::Unrecoverable => IncompleteReason::Unrecoverable,
                };
                Ok(Control::Stop(
                    self.finish(run_id, RunStatus::Incomplete, Some(reason))
                        .await?,
                ))
            }
            RecoveryDecision::Backoff => {
                let delay = self.lock_recovery().backoff_delay();
                if self.cancel.run(tokio::time::sleep(delay)).await.is_err() {
                    return Ok(Control::Stop(
                        self.finish(
                            run_id,
                            RunStatus::Cancelled,
                            Some(IncompleteReason::Cancelled),
                        )
                        .await?,
                    ));
                }
                self.prepare_retry(step, &failure, checks, state);
                Ok(Control::Continue)
            }
            RecoveryDecision::Retry => {
                self.prepare_retry(step, &failure, checks, state);
                Ok(Control::Continue)
            }
            RecoveryDecision::Replan => {
                self.replan(
                    run_id, spec, step, before, attempt, strategy, &failure, checks, state,
                )
                .await
            }
        }
    }

    /// Asks the driver for a replacement strategy and accepts it only when it
    /// keeps the frozen acceptance hash and declared steps and tools.
    #[allow(clippy::too_many_arguments)]
    async fn replan(
        &self,
        run_id: &RunId,
        spec: &AcceptanceSpec,
        step: &PlannedStep,
        before: &FileManifest,
        attempt: &AttemptId,
        strategy: u32,
        failure: &FailureRecord,
        checks: &[CheckResult],
        state: &mut RetryState,
    ) -> Result<Control, HarnessError> {
        let request_packet = {
            let recovery = self.lock_recovery();
            FailurePacket::build(
                &self.acceptance,
                step,
                strategy,
                failure,
                checks,
                &recovery.prior_signatures(),
                recovery.remaining(),
            )
        };
        let attempt_number = self.lock_recovery().next_attempt_number();
        let history = self.history_snapshot();
        let planned = {
            let request = StepRequest {
                run_id,
                acceptance: spec,
                step,
                attempt,
                attempt_number,
                strategy,
                before,
                prior_failures: &state.prior_failures,
                packet: Some(&request_packet),
                history: &history,
            };
            let child = self.cancel.child();
            self.cancel
                .run(self.driver.replan_strategy(request, &child))
                .await
        };

        let proposed = match planned {
            Err(_) => {
                return Ok(Control::Stop(
                    self.finish(
                        run_id,
                        RunStatus::Cancelled,
                        Some(IncompleteReason::Cancelled),
                    )
                    .await?,
                ));
            }
            Ok(Err(DriverError::Cancelled(_))) => {
                return Ok(Control::Stop(
                    self.finish(
                        run_id,
                        RunStatus::Cancelled,
                        Some(IncompleteReason::Cancelled),
                    )
                    .await?,
                ));
            }
            Ok(Err(DriverError::Failed(message) | DriverError::InvalidAction(message))) => {
                self.journal_replan_failure(run_id, step.index, strategy, &message)
                    .await?;
                return Ok(Control::Stop(
                    self.finish(
                        run_id,
                        RunStatus::Incomplete,
                        Some(IncompleteReason::NoProgress),
                    )
                    .await?,
                ));
            }
            Ok(Ok(strategy)) => strategy,
        };

        let accepted = self
            .lock_recovery()
            .accept_strategy(&self.acceptance, &proposed);
        match accepted {
            Ok(started) => {
                self.journal_replan_accepted(run_id, step.index, started)
                    .await?;
                self.prepare_retry(step, failure, checks, state);
                Ok(Control::Continue)
            }
            Err(error) => {
                self.journal_replan_failure(run_id, step.index, strategy, &error.to_string())
                    .await?;
                Ok(Control::Stop(
                    self.finish(
                        run_id,
                        RunStatus::Incomplete,
                        Some(IncompleteReason::NoProgress),
                    )
                    .await?,
                ))
            }
        }
    }

    /// Builds the compact packet the next attempt sees.
    fn prepare_retry(
        &self,
        step: &PlannedStep,
        failure: &FailureRecord,
        checks: &[CheckResult],
        state: &mut RetryState,
    ) {
        let recovery = self.lock_recovery();
        state.packet = Some(FailurePacket::build(
            &self.acceptance,
            step,
            recovery.strategy(),
            failure,
            checks,
            &recovery.prior_signatures(),
            recovery.remaining(),
        ));
        state.prior_failures = checks
            .iter()
            .filter(|check| check.status != CheckStatus::Pass)
            .cloned()
            .collect();
    }

    async fn journal_decision(
        &self,
        run_id: &RunId,
        step_index: u32,
        strategy: u32,
        decision: RecoveryDecision,
        failure: &FailureRecord,
    ) -> Result<(), HarnessError> {
        self.store
            .record_decision(
                run_id.clone(),
                step_index,
                strategy,
                json!({
                    "outcome": decision.as_str(),
                    "class": failure.class.as_str(),
                    "action": failure.action,
                    "signature": failure.signature,
                    "message": failure.message,
                    "stop": decision.stop_reason().map(StopReason::as_str),
                }),
            )
            .await
    }

    async fn journal_replan_accepted(
        &self,
        run_id: &RunId,
        step_index: u32,
        strategy: u32,
    ) -> Result<(), HarnessError> {
        self.store
            .record_decision(
                run_id.clone(),
                step_index,
                strategy,
                json!({"outcome": "replan_accepted", "strategy": strategy}),
            )
            .await
    }

    async fn journal_replan_failure(
        &self,
        run_id: &RunId,
        step_index: u32,
        strategy: u32,
        message: &str,
    ) -> Result<(), HarnessError> {
        self.store
            .record_decision(
                run_id.clone(),
                step_index,
                strategy,
                json!({"outcome": "replan_failed", "message": message}),
            )
            .await
    }

    async fn final_verify(
        &self,
        run_id: &RunId,
        spec: &AcceptanceSpec,
        run_before: &FileManifest,
        last_executed: Option<StepExecution>,
    ) -> Result<RunOutcome, HarnessError> {
        let snapshot = self.store.snapshot(run_id).await?;
        if snapshot.run.status != RunStatus::Running {
            return Ok(RunOutcome::new(run_id.clone(), snapshot.run.status, None));
        }
        if self.cancelled() {
            return self
                .finish(
                    run_id,
                    RunStatus::Cancelled,
                    Some(IncompleteReason::Cancelled),
                )
                .await;
        }
        self.store
            .transition_run(
                run_id.clone(),
                RunStatus::Running,
                RunStatus::FinalVerifying,
                None,
            )
            .await?;
        if self.cancelled() {
            return self
                .finish(
                    run_id,
                    RunStatus::Cancelled,
                    Some(IncompleteReason::Cancelled),
                )
                .await;
        }

        let Some(execution) = last_executed else {
            return self
                .finish(
                    run_id,
                    RunStatus::Incomplete,
                    Some(IncompleteReason::FinalVerificationFailed),
                )
                .await;
        };
        let attempt = snapshot
            .steps
            .last()
            .and_then(|step| {
                step.attempts
                    .iter()
                    .rev()
                    .find(|attempt| attempt.status == AttemptStatus::Passed)
            })
            .map(|attempt| attempt.id.clone());
        let Some(attempt) = attempt else {
            return self
                .finish(
                    run_id,
                    RunStatus::Incomplete,
                    Some(IncompleteReason::FinalVerificationFailed),
                )
                .await;
        };

        let final_checks = self.final_checks_for(spec);
        let mut results = Vec::with_capacity(final_checks.len());
        for check in final_checks {
            let mut context = CheckContext::new(
                self.acceptance.hash().to_owned(),
                spec.steps.last().map_or(1, |step| step.index),
                attempt.clone(),
                self.workspace.clone(),
            );
            context.before = run_before.clone();
            context.tool_results = execution.tool_results.clone();
            context.command_runs = execution.command_runs.clone();
            context.artifacts = Some(ArtifactStore::new(self.store.artifacts_path()));
            context.inputs = self.acceptance.spec().inputs.clone();
            match self.verify(check.spec, context).await {
                Ok(result) => results.push(match check.required {
                    Some(required) => enforce_required(result, &required),
                    None => result,
                }),
                Err(_) => {
                    return self
                        .finish(
                            run_id,
                            RunStatus::Cancelled,
                            Some(IncompleteReason::Cancelled),
                        )
                        .await
                }
            }
        }
        self.store
            .record_checks(run_id.clone(), attempt, results.clone())
            .await?;

        if !results
            .iter()
            .all(|check| check.status == CheckStatus::Pass)
        {
            return self
                .finish(
                    run_id,
                    RunStatus::Incomplete,
                    Some(IncompleteReason::FinalVerificationFailed),
                )
                .await;
        }
        // Last line of defence: a cancellation race must never become a pass.
        if self.cancelled() {
            return self
                .finish(
                    run_id,
                    RunStatus::Cancelled,
                    Some(IncompleteReason::Cancelled),
                )
                .await;
        }
        let record = self
            .store
            .transition_run(
                run_id.clone(),
                RunStatus::FinalVerifying,
                RunStatus::Passed,
                None,
            )
            .await?;
        Ok(RunOutcome::new(run_id.clone(), record.status, None))
    }

    fn final_checks_for(&self, spec: &AcceptanceSpec) -> Vec<FinalCheck> {
        let mut checks = Vec::new();
        for artifact in &spec.output.required_artifacts {
            checks.push(FinalCheck {
                spec: CheckSpec::new("file_exists", [("path", json!(artifact.path))]),
                required: Some(artifact.clone()),
            });
            if spec.output.schema.is_some() && artifact.path.ends_with(".json") {
                checks.push(FinalCheck {
                    spec: CheckSpec::new(
                        "json_schema",
                        [
                            ("path", json!(artifact.path)),
                            ("schema", spec.output.schema.clone().unwrap_or_default()),
                        ],
                    ),
                    required: None,
                });
            }
        }
        if spec.output.schema.is_some()
            && !spec
                .output
                .required_artifacts
                .iter()
                .any(|artifact| artifact.path.ends_with(".json"))
        {
            checks.push(FinalCheck {
                spec: CheckSpec::new("json_schema", [("path", json!(""))]),
                required: None,
            });
        }
        if let Some(last) = spec.steps.last() {
            for check in &last.checks {
                checks.push(FinalCheck {
                    spec: check.clone(),
                    required: None,
                });
            }
        }
        for check in &self.final_checks {
            checks.push(FinalCheck {
                spec: check.clone(),
                required: None,
            });
        }
        checks
    }

    async fn verify(
        &self,
        spec: CheckSpec,
        context: CheckContext,
    ) -> Result<CheckResult, Cancelled> {
        let kind = spec.kind.clone();
        let hash = context.acceptance_hash.clone();
        let step_index = context.step_index;
        let attempt = context.attempt.clone();
        match self.cancel.run(self.verifier.evaluate(spec, context)).await {
            Ok(Ok(result)) => Ok(enforce_provenance(result, &hash, step_index, &attempt)),
            Ok(Err(error)) => Ok(check_error(
                &kind,
                &hash,
                step_index,
                &attempt,
                error.to_string(),
            )),
            Err(cancelled) => Err(cancelled),
        }
    }

    async fn abort(
        &self,
        run_id: &RunId,
        attempt_id: &AttemptId,
        message: &str,
    ) -> Result<(), HarnessError> {
        self.store
            .finish_attempt(FinishAttempt {
                run_id: run_id.clone(),
                attempt_id: attempt_id.clone(),
                status: AttemptStatus::Aborted,
                error: Some(message.to_owned()),
            })
            .await?;
        Ok(())
    }

    async fn finish(
        &self,
        run_id: &RunId,
        to: RunStatus,
        reason: Option<IncompleteReason>,
    ) -> Result<RunOutcome, HarnessError> {
        let snapshot = self.store.snapshot(run_id).await?;
        if snapshot.run.status.is_terminal() {
            return Ok(RunOutcome::new(run_id.clone(), snapshot.run.status, None));
        }
        let payload =
            reason.map(|value| serde_json::to_string(&value).unwrap_or_else(|_| value.to_string()));
        let record = self
            .store
            .transition_run(run_id.clone(), snapshot.run.status, to, payload)
            .await?;
        Ok(RunOutcome::new(run_id.clone(), record.status, reason))
    }

    fn cancelled(&self) -> bool {
        self.cancel.is_cancelled() || self.cancel.remaining().is_zero()
    }

    fn capture_manifest(&self) -> Result<FileManifest, HarnessError> {
        FileManifest::capture(&self.workspace)
            .map_err(|error| HarnessError::Storage(format!("capture workspace: {error}")))
    }

    fn failure_record(
        &self,
        step: &PlannedStep,
        class: FailureClass,
        action: impl Into<String>,
        checks: &[CheckResult],
        message: &str,
    ) -> FailureRecord {
        let mut record = FailureRecord::new(
            step.index,
            class,
            action,
            packets::failure_signature(class, checks),
        )
        .with_message(message);
        record.evidence = packets::evidence_refs(checks);
        record
    }

    fn record_history(&self, step: &PlannedStep, execution: &StepExecution) {
        let observation = StepObservation::from_execution(step, execution);
        let mut history = self
            .history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        history.push(observation);
        while history.len() > HISTORY_LIMIT {
            history.remove(0);
        }
    }

    fn history_snapshot(&self) -> Vec<StepObservation> {
        self.history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn lock_recovery(&self) -> MutexGuard<'_, RecoveryController> {
        self.recovery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Compact state carried between attempts of the same step.
#[derive(Default)]
struct RetryState {
    prior_failures: Vec<CheckResult>,
    packet: Option<FailurePacket>,
}

enum Control {
    Continue,
    Stop(RunOutcome),
}

enum StepOutcome {
    Passed(StepExecution),
    Stop(RunOutcome),
}

struct FinalCheck {
    spec: CheckSpec,
    required: Option<RequiredArtifact>,
}

/// Rejects any pass that is not bound to this attempt, hash, and evidence.
fn enforce_provenance(
    result: CheckResult,
    hash: &str,
    step_index: u32,
    attempt: &AttemptId,
) -> CheckResult {
    if result.acceptance_hash != hash
        || result.step_index != step_index
        || result.attempt != *attempt
    {
        return check_error(
            &result.kind,
            hash,
            step_index,
            attempt,
            "check result provenance does not match this attempt".to_owned(),
        );
    }
    if result.status != CheckStatus::Pass {
        return result;
    }
    let bound = !result.evidence.is_empty()
        && result
            .evidence
            .iter()
            .all(|item| item.attempt.as_ref() == Some(attempt));
    if bound {
        result
    } else {
        check_error(
            &result.kind,
            hash,
            step_index,
            attempt,
            "pass rejected: no same-attempt evidence".to_owned(),
        )
    }
}

fn enforce_required(mut result: CheckResult, required: &RequiredArtifact) -> CheckResult {
    if result.status != CheckStatus::Pass || !required.non_empty {
        return result;
    }
    let Some(digest) = result.evidence.iter().find_map(|item| item.digest.as_ref()) else {
        result.status = CheckStatus::Error;
        result.message = format!("{} has no digest evidence", required.path);
        result.evidence.clear();
        return result;
    };
    if digest.size == 0 {
        result.status = CheckStatus::Fail;
        result.message = format!("{} is empty", required.path);
    }
    result
}

fn check_error(
    kind: &str,
    hash: &str,
    step_index: u32,
    attempt: &AttemptId,
    message: String,
) -> CheckResult {
    CheckResult {
        kind: kind.to_owned(),
        acceptance_hash: hash.to_owned(),
        step_index,
        attempt: attempt.clone(),
        status: CheckStatus::Error,
        message,
        evidence: Vec::new(),
    }
}
