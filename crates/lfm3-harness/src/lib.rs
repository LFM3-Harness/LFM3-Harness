//! Core library for the local, purpose-bound LFM3 harness.

pub mod agent;
pub mod cancel;
pub mod chat;
pub mod config;
pub mod coordinator;
pub mod error;
pub mod ids;
pub mod job;
pub mod mission;
pub mod model;
pub mod observability;
pub(crate) mod paths;
pub mod runtime;
pub mod store;
pub mod supervisor;
pub mod task;
pub mod tools;
pub mod verify;

pub use agent::{
    DeterministicVerifier, DriverError, Executor, IncompleteReason, LoopPhase, ModelProfile,
    PlanRequest, Planner, PlannerError, RunOutcome, Runner, StepAction, StepDriver, StepExecution,
    StepRequest, TodoCheck, TodoCheckKind, Verifier, D1, LFM3_AUDIO, LFM3_TEXT, LFM3_VL,
};
pub use cancel::{CancelReason, CancellationChild, CancellationContext, Cancelled};
pub use chat::{
    ChatActionRecord, ChatCapabilities, ChatIntent, ChatMessageRecord, ChatRoute, ChatSession,
    ChatSessionView, ChatStore, DecisionBackend, DecisionOutcome, DecisionPoint, HarnessDecision,
    RouteRole, RouteSource, EVIDENCE_GATE_FLOOR_BPS, WRITE_GATE_FLOOR_BPS,
};
pub use config::{is_loopback, Config, Limits, LlamaPaths, Profile};
pub use coordinator::{CoordinatorError, RunCoordinator};
pub use error::HarnessError;
pub use ids::{ArtifactId, AttemptId, RunId, StepId};
pub use job::{
    JobCall, JobDeliverable, JobEvent, JobFinish, JobRecord, JobStart, JobStatus, JobStore,
    JobTodo, JobTodoCheck, JobTurn, JobView,
};
pub use mission::{
    AcceptanceSpec, CheckSpec, ExecutionStrategy, FrozenAcceptance, InputArtifact, MissionManifest,
    OutputContract, PlanDraft, PlanError, PlannedStep, RequiredArtifact, StrategyAction,
};
pub use model::{
    labels, ChatMessage, ChatRequest, ChatResponse, ChatRole, ContentPart, Decision,
    DecisionRequest, InputAudio, LaunchSpec, LlamaClient, ModelError, ModelGateway, ModelKey,
    ModelLease, ModelManager, ModelManagerConfig, ModelSpec, ResponseFormat, ToolCall,
    ToolDefinition, TranscribeRequest, Transcript,
};
pub use runtime::configure_runtime_environment;
pub use store::{
    sha256_hex, AttemptSnapshot, AttemptStatus, FinishAttempt, FinishedAttempt, Reconciliation,
    RunEvent, RunRecord, RunStatus, Snapshot, StartAttempt, StepSnapshot, StepStatus, Store,
};
pub use supervisor::{is_process_alive, OwnedChild, ProcessSupervisor};
pub use task::{
    draft_task, manifest as task_manifest, verify_step_checks, Role, Route, TaskError, TaskPlan,
    TaskRequest, TaskRouter, GENERAL_TASK,
};
pub use tools::{
    CommandArg, CommandRun, CommandTemplate, ToolError, ToolName, ToolRegistry, ToolRequest,
    ToolResult, ToolStatus,
};
pub use verify::{media_type_for, ContentDigest};
