//! Verification types shared across tools, checks, and the runner.

pub mod deterministic;
pub mod evidence;
pub mod semantic;

pub use deterministic::{
    evaluate, Check, CheckContext, CheckError, CheckEvidence, CheckResult, CheckStatus,
    FileManifest,
};
pub use evidence::{media_type_for, ContentDigest};
pub use semantic::{
    evaluate as evaluate_semantic, is_semantic_kind, JudgeError, JudgeFuture, JudgeRequest,
    JudgeVerdict, ModelJudge, Qualification, SemanticEvidence, SemanticJudge, SemanticMode,
    SemanticPolicy, SEMANTIC_KINDS,
};
