//! Capability-granted tools that execute inside one workspace.
//!
//! The planner selects a granted tool name and typed arguments. It can never
//! see or supply a command line, an executable path, or a path outside the
//! workspace. Every tool result echoes the attempt it belongs to and any
//! content digests it produced.

pub mod command;
pub mod fs;
pub mod search;
pub mod senses;

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::{Path, PathBuf},
    str::FromStr,
    time::{Duration, Instant},
};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::paths;
use crate::verify::ContentDigest;
use crate::{
    model::{ModelError, ModelManager},
    CancelReason, CancellationChild, ProcessSupervisor,
};

pub use command::{CommandArg, CommandRun, CommandTemplate};

/// Largest file a text tool reads or writes.
pub(crate) const MAX_READ_BYTES: u64 = 1024 * 1024;
pub(crate) const MAX_WRITE_BYTES: usize = 1024 * 1024;
/// Largest observation handed back to a model in one tool message.
pub(crate) const MAX_OUTPUT_CHARS: usize = 32_000;
pub(crate) const MAX_LIST_ENTRIES: usize = 500;

/// The tools the engine implements. Missions may grant a subset.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolName {
    ListFiles,
    ReadFile,
    WriteFile,
    EditFile,
    SearchFiles,
    RelatedFiles,
    Look,
    Transcribe,
    Speak,
}

impl ToolName {
    pub const ALL: [Self; 9] = [
        Self::ListFiles,
        Self::ReadFile,
        Self::WriteFile,
        Self::EditFile,
        Self::SearchFiles,
        Self::RelatedFiles,
        Self::Look,
        Self::Transcribe,
        Self::Speak,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ListFiles => "list_files",
            Self::ReadFile => "read_file",
            Self::WriteFile => "write_file",
            Self::EditFile => "edit_file",
            Self::SearchFiles => "search_files",
            Self::RelatedFiles => "related_files",
            Self::Look => "look",
            Self::Transcribe => "transcribe",
            Self::Speak => "speak",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|tool| tool.as_str() == value)
    }
}

impl fmt::Display for ToolName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ToolName {
    type Err = ToolError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value).ok_or_else(|| ToolError::UnknownTool(value.to_owned()))
    }
}

/// Failures that are errors for the call, not attempts with observable output.
#[derive(Debug, Error)]
pub enum ToolError {
    #[error("unknown tool {0:?}")]
    UnknownTool(String),
    #[error("tool {tool} is not granted by this mission")]
    NotGranted { tool: ToolName },
    #[error("invalid arguments for {tool}: {message}")]
    InvalidArguments { tool: ToolName, message: String },
    #[error("path {path:?} leaves the workspace")]
    OutsideWorkspace { path: String },
    #[error("path {path:?} is invalid: {message}")]
    InvalidPath { path: String, message: String },
    #[error("path {path:?} passes through a link or reparse point")]
    ReparsePoint { path: String },
    #[error("{path:?} is not a file")]
    NotAFile { path: String },
    #[error("{path:?} is not a directory")]
    NotADirectory { path: String },
    #[error("{path:?} does not exist")]
    NotFound { path: String },
    #[error("{path:?} exceeds the {limit}-byte limit")]
    TooLarge { path: String, limit: u64 },
    #[error("{path:?}: {message}")]
    Io { path: String, message: String },
    #[error("edit anchor occurs {count} times in {path:?}; exactly one is required")]
    AmbiguousEdit { path: String, count: usize },
    #[error("tool cancelled: {0}")]
    Cancelled(CancelReason),
    #[error("tool {tool} is not configured: {reason}")]
    NotConfigured { tool: &'static str, reason: String },
    #[error("unsupported tool {tool}: {reason}")]
    Unsupported { tool: &'static str, reason: String },
    #[error("unknown command template {0:?}")]
    UnknownTemplate(String),
    #[error("invalid command template {id:?}: {message}")]
    InvalidTemplate { id: String, message: String },
    #[error("invalid arguments for command {template:?}: {message}")]
    InvalidCommandArguments { template: String, message: String },
    #[error("model call failed: {0}")]
    Model(#[from] ModelError),
    #[error("process containment failed: {0}")]
    Process(String),
}

/// One typed request to execute a granted tool.
#[derive(Clone, Debug)]
pub struct ToolRequest {
    pub attempt: crate::AttemptId,
    pub call_id: String,
    pub tool: ToolName,
    pub arguments: Value,
}

impl ToolRequest {
    #[must_use]
    pub fn new(
        attempt: crate::AttemptId,
        call_id: impl Into<String>,
        tool: ToolName,
        arguments: Value,
    ) -> Self {
        Self {
            attempt,
            call_id: call_id.into(),
            tool,
            arguments,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Succeeded,
    Failed,
}

impl ToolStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }
}

/// Observable result of one tool call, bound to its attempt and call id.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub attempt: crate::AttemptId,
    pub call_id: String,
    pub tool: ToolName,
    pub status: ToolStatus,
    /// Only a real process exit sets this; text output never does.
    pub exit_code: Option<i32>,
    pub output: String,
    pub artifacts: Vec<ContentDigest>,
    pub duration: Duration,
}

impl ToolResult {
    #[must_use]
    pub const fn succeeded(&self) -> bool {
        matches!(self.status, ToolStatus::Succeeded)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Access {
    Read,
    List,
    Write,
}

/// Tools granted by the mission, bound to one workspace and one supervisor.
pub struct ToolRegistry {
    root: PathBuf,
    granted: BTreeSet<ToolName>,
    commands: BTreeMap<String, CommandTemplate>,
    models: Option<ModelManager>,
    supervisor: ProcessSupervisor,
}

impl ToolRegistry {
    pub fn new(workspace: impl Into<PathBuf>) -> Result<Self, ToolError> {
        let requested = workspace.into();
        let root = std::fs::canonicalize(&requested).map_err(|error| ToolError::Io {
            path: requested.display().to_string(),
            message: error.to_string(),
        })?;
        if !root.is_dir() {
            return Err(ToolError::NotADirectory {
                path: root.display().to_string(),
            });
        }
        let supervisor =
            ProcessSupervisor::new().map_err(|error| ToolError::Process(error.to_string()))?;
        Ok(Self {
            root,
            granted: BTreeSet::new(),
            commands: BTreeMap::new(),
            models: None,
            supervisor,
        })
    }

    #[must_use]
    pub fn grant(mut self, tool: ToolName) -> Self {
        self.granted.insert(tool);
        self
    }

    #[must_use]
    pub fn with_models(mut self, models: ModelManager) -> Self {
        self.models = Some(models);
        self
    }

    #[must_use]
    pub fn with_supervisor(mut self, supervisor: ProcessSupervisor) -> Self {
        self.supervisor = supervisor;
        self
    }

    pub fn add_command(&mut self, template: CommandTemplate) -> Result<(), ToolError> {
        if self.commands.contains_key(&template.id) {
            return Err(ToolError::InvalidTemplate {
                id: template.id.clone(),
                message: "template ids must be unique".to_owned(),
            });
        }
        self.commands.insert(template.id.clone(), template);
        Ok(())
    }

    #[must_use]
    pub fn workspace(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn granted(&self) -> &BTreeSet<ToolName> {
        &self.granted
    }

    pub(crate) fn models(&self) -> Option<&ModelManager> {
        self.models.as_ref()
    }

    pub(crate) fn supervisor(&self) -> &ProcessSupervisor {
        &self.supervisor
    }

    pub(crate) fn command(&self, id: &str) -> Option<&CommandTemplate> {
        self.commands.get(id)
    }

    /// Executes exactly one granted tool. The grant check happens before any
    /// path resolution or side effect.
    pub async fn execute(
        &self,
        request: ToolRequest,
        cancel: &CancellationChild,
    ) -> Result<ToolResult, ToolError> {
        if !self.granted.contains(&request.tool) {
            return Err(ToolError::NotGranted { tool: request.tool });
        }
        check_cancelled(cancel)?;
        let started = Instant::now();
        match request.tool {
            ToolName::ListFiles => fs::list_files(self, &request, cancel, started).await,
            ToolName::ReadFile => fs::read_file(self, &request, cancel, started).await,
            ToolName::WriteFile => fs::write_file(self, &request, cancel, started).await,
            ToolName::EditFile => fs::edit_file(self, &request, cancel, started).await,
            ToolName::SearchFiles => search::search_files(self, &request, cancel, started).await,
            ToolName::RelatedFiles => search::related_files(self, &request, cancel, started).await,
            ToolName::Look => senses::look(self, &request, cancel, started).await,
            ToolName::Transcribe => senses::transcribe(self, &request, cancel, started).await,
            ToolName::Speak => senses::speak(self, &request, cancel, started).await,
        }
    }

    /// Resolves a model-supplied relative path, refusing every escape.
    ///
    /// The workspace confinement rules live in [`crate::paths`] so tools and
    /// the verifier cannot diverge on what counts as an escape.
    pub(crate) fn resolve(&self, raw: &str, access: Access) -> Result<PathBuf, ToolError> {
        let mode = match access {
            Access::Read => paths::Mode::Read,
            Access::List => paths::Mode::List,
            Access::Write => paths::Mode::Write,
        };
        paths::resolve(&self.root, raw, mode).map_err(|error| match error {
            paths::PathError::Outside => ToolError::OutsideWorkspace {
                path: raw.to_owned(),
            },
            paths::PathError::Invalid(message) => ToolError::InvalidPath {
                path: raw.to_owned(),
                message,
            },
            paths::PathError::Reparse(path) => ToolError::ReparsePoint { path },
            paths::PathError::NotFound => ToolError::NotFound {
                path: raw.to_owned(),
            },
            paths::PathError::NotAFile => ToolError::NotAFile {
                path: raw.to_owned(),
            },
            paths::PathError::NotADirectory => ToolError::NotADirectory {
                path: raw.to_owned(),
            },
            paths::PathError::Io(message) => ToolError::Io {
                path: raw.to_owned(),
                message,
            },
        })
    }
}

pub(crate) fn check_cancelled(cancel: &CancellationChild) -> Result<(), ToolError> {
    if cancel.is_cancelled() {
        return Err(ToolError::Cancelled(CancelReason::Requested));
    }
    if cancel.remaining().is_zero() {
        return Err(ToolError::Cancelled(CancelReason::Deadline));
    }
    Ok(())
}

pub(crate) fn parse_args<T: DeserializeOwned>(request: &ToolRequest) -> Result<T, ToolError> {
    serde_json::from_value(request.arguments.clone()).map_err(|error| ToolError::InvalidArguments {
        tool: request.tool,
        message: error.to_string(),
    })
}

pub(crate) fn finish(
    request: &ToolRequest,
    started: Instant,
    status: ToolStatus,
    exit_code: Option<i32>,
    output: String,
    artifacts: Vec<ContentDigest>,
) -> ToolResult {
    ToolResult {
        attempt: request.attempt.clone(),
        call_id: request.call_id.clone(),
        tool: request.tool,
        status,
        exit_code,
        output,
        artifacts,
        duration: started.elapsed(),
    }
}

pub(crate) fn clip(text: String, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text;
    }
    let mut clipped: String = text.chars().take(limit).collect();
    clipped.push_str("\n[truncated]");
    clipped
}

pub(crate) fn io_error(raw: &str, error: std::io::Error) -> ToolError {
    ToolError::Io {
        path: raw.to_owned(),
        message: error.to_string(),
    }
}
