//! Capability-bound missions and immutable acceptance contracts.

pub mod acceptance;
pub mod manifest;

use thiserror::Error;

pub use acceptance::{
    AcceptanceSpec, CheckSpec, ExecutionStrategy, FrozenAcceptance, InputArtifact, OutputContract,
    PlanDraft, PlannedStep, RequiredArtifact, StrategyAction,
};
pub use manifest::{
    load_missions, CommandTemplateDef, InputKind, InputSpec, MissionLimits, MissionManifest,
};

/// A plan or contract that violates the mission's verifiable capability set.
#[derive(Debug, Error)]
pub enum PlanError {
    #[error("the plan has no steps")]
    NoSteps,
    #[error("the plan has too many steps: {actual} (maximum {maximum})")]
    TooManySteps { actual: usize, maximum: usize },
    #[error("step {index} has a criterion too vague to verify")]
    VagueCriterion { index: u32 },
    #[error("step {index} has no checks, so it could never pass honestly")]
    NoChecks { index: u32 },
    #[error("step index {index} is invalid or duplicated")]
    InvalidStepIndex { index: u32 },
    #[error("step {index} uses tool {tool:?}, which this mission does not allow")]
    UnknownTool { index: u32, tool: String },
    #[error("step {index} uses check {kind:?}, which this mission does not allow")]
    UnknownCheck { index: u32, kind: String },
    #[error("step {index} has an invalid check: {message}")]
    InvalidCheck { index: u32, message: String },
    #[error("invalid output path: {0}")]
    InvalidPath(String),
    #[error("the goal is empty")]
    EmptyGoal,
    #[error("invalid artifact hash: {0}")]
    InvalidArtifactHash(String),
    #[error("execution strategy acceptance hash does not match the frozen contract")]
    AcceptanceHashMismatch,
    #[error("execution strategy references unknown step {0}")]
    UnknownStrategyStep(u32),
    #[error("invalid mission manifest: {0}")]
    InvalidManifest(String),
    #[error("mission manifest I/O failed: {0}")]
    Io(String),
    #[error("mission manifest JSON is invalid: {0}")]
    Json(String),
}
