//! Durable, ordered event journal.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::RunId;

/// Capacity of the live event broadcast; slow subscribers miss events and
/// must resynchronize from the durable journal.
pub const EVENT_CHANNEL_CAPACITY: usize = 256;

/// Event names written by the engine. Stored as text so new kinds do not
/// require a schema migration.
pub mod kind {
    pub const RUN_CREATED: &str = "run.created";
    pub const RUN_PLANNED: &str = "run.planned";
    pub const RUN_FINAL_VERIFYING: &str = "run.final_verifying";
    pub const RUN_CANCELLING: &str = "run.cancelling";
    pub const RUN_STATUS: &str = "run.status";
    pub const RUN_FINISHED: &str = "run.finished";
    pub const ATTEMPT_STARTED: &str = "attempt.started";
    pub const TOOL_RESULT: &str = "tool.result";
    pub const CHECK_RESULT: &str = "check.result";
    pub const STEP_PASSED: &str = "step.passed";
    pub const STEP_FAILED: &str = "step.failed";
    pub const STEP_INCONCLUSIVE: &str = "step.inconclusive";
    pub const STEP_BLOCKED: &str = "step.blocked";
    pub const RECOVERY_DECISION: &str = "recovery.decision";
    pub const RUN_INTERRUPTED: &str = "run.interrupted";
}

/// A committed journal entry. `seq` is monotonic per run and only assigned
/// inside the transaction that also commits the state change.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunEvent {
    pub run_id: RunId,
    pub seq: i64,
    pub kind: String,
    pub payload: Value,
    pub at: i64,
}
