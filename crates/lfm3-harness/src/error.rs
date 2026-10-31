use thiserror::Error;

/// Errors that can cross a harness boundary without exposing internal panics.
#[derive(Debug, Error)]
pub enum HarnessError {
    #[error("remote model endpoint rejected: {0}")]
    RemoteEndpointRejected(String),
    #[error("invalid configuration: {0}")]
    InvalidConfiguration(String),
    #[error("invalid {kind} identifier: {value}")]
    InvalidId { kind: &'static str, value: String },
    #[error("stored state is unavailable or inconsistent: {0}")]
    Storage(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("acceptance contract rejected: {0}")]
    Contract(String),
    #[error("process supervision failed: {0}")]
    Process(String),
}
