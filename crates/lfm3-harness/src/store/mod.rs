//! Durable run journal, atomic artifacts, and ordered events.
//!
//! Exactly one actor owns the SQLite connection. Every state change and its
//! journal event commit in a single transaction; live events are broadcast
//! only after that commit succeeds.

pub mod artifacts;
pub mod events;
mod migrations;

use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::{
    mission::{FrozenAcceptance, PlannedStep},
    tools::{CommandRun, ToolResult},
    verify::{CheckResult, CheckStatus},
    ArtifactId, AttemptId, HarnessError, RunId,
};

pub use artifacts::{sha256_hex, ArtifactRef, ArtifactStore};
pub use events::{RunEvent, EVENT_CHANNEL_CAPACITY};

/// Media type for plain-text artifacts such as transcripts and stdout.
pub const TEXT_PLAIN: &str = "text/plain";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Created,
    Planning,
    AwaitingPlanApproval,
    Running,
    FinalVerifying,
    Passed,
    AwaitingUser,
    Cancelling,
    Cancelled,
    Incomplete,
    Interrupted,
}

impl RunStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Planning => "planning",
            Self::AwaitingPlanApproval => "awaiting_plan_approval",
            Self::Running => "running",
            Self::FinalVerifying => "final_verifying",
            Self::Passed => "passed",
            Self::AwaitingUser => "awaiting_user",
            Self::Cancelling => "cancelling",
            Self::Cancelled => "cancelled",
            Self::Incomplete => "incomplete",
            Self::Interrupted => "interrupted",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, HarnessError> {
        Self::ALL
            .iter()
            .copied()
            .find(|status| status.as_str() == raw)
            .ok_or_else(|| HarnessError::Storage(format!("unknown run status {raw:?}")))
    }

    const ALL: [Self; 11] = [
        Self::Created,
        Self::Planning,
        Self::AwaitingPlanApproval,
        Self::Running,
        Self::FinalVerifying,
        Self::Passed,
        Self::AwaitingUser,
        Self::Cancelling,
        Self::Cancelled,
        Self::Incomplete,
        Self::Interrupted,
    ];

    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Passed | Self::Cancelled | Self::Incomplete | Self::Interrupted
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pending,
    Running,
    Verifying,
    Retrying,
    Replanning,
    Passed,
    Blocked,
    Cancelled,
}

impl StepStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Verifying => "verifying",
            Self::Retrying => "retrying",
            Self::Replanning => "replanning",
            Self::Passed => "passed",
            Self::Blocked => "blocked",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, HarnessError> {
        Self::ALL
            .iter()
            .copied()
            .find(|status| status.as_str() == raw)
            .ok_or_else(|| HarnessError::Storage(format!("unknown step status {raw:?}")))
    }

    const ALL: [Self; 8] = [
        Self::Pending,
        Self::Running,
        Self::Verifying,
        Self::Retrying,
        Self::Replanning,
        Self::Passed,
        Self::Blocked,
        Self::Cancelled,
    ];

    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Passed | Self::Blocked | Self::Cancelled)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptStatus {
    Proposed,
    Guarded,
    Executing,
    Verifying,
    Passed,
    Failed,
    Inconclusive,
    Aborted,
    Interrupted,
}

impl AttemptStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::Guarded => "guarded",
            Self::Executing => "executing",
            Self::Verifying => "verifying",
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Inconclusive => "inconclusive",
            Self::Aborted => "aborted",
            Self::Interrupted => "interrupted",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, HarnessError> {
        Self::ALL
            .iter()
            .copied()
            .find(|status| status.as_str() == raw)
            .ok_or_else(|| HarnessError::Storage(format!("unknown attempt status {raw:?}")))
    }

    const ALL: [Self; 9] = [
        Self::Proposed,
        Self::Guarded,
        Self::Executing,
        Self::Verifying,
        Self::Passed,
        Self::Failed,
        Self::Inconclusive,
        Self::Aborted,
        Self::Interrupted,
    ];

    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Passed | Self::Failed | Self::Inconclusive | Self::Aborted | Self::Interrupted
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RunRecord {
    pub id: RunId,
    pub mission: String,
    pub mission_version: u32,
    pub acceptance_hash: String,
    pub goal: String,
    pub status: RunStatus,
    pub created_at: i64,
    pub updated_at: i64,
    pub parent_run_id: Option<RunId>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AttemptSnapshot {
    pub id: AttemptId,
    pub strategy: u32,
    pub status: AttemptStatus,
    pub action: Value,
    pub model: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StepSnapshot {
    pub index: u32,
    pub title: String,
    pub criterion: String,
    pub status: StepStatus,
    pub attempts: Vec<AttemptSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub run: RunRecord,
    pub steps: Vec<StepSnapshot>,
}

#[derive(Clone, Debug)]
pub struct StartAttempt {
    pub run_id: RunId,
    pub step_index: u32,
    pub strategy: u32,
    pub action: Value,
    pub model: String,
    pub deadline_at: Option<i64>,
}

#[derive(Clone, Debug)]
pub struct FinishAttempt {
    pub run_id: RunId,
    pub attempt_id: AttemptId,
    pub status: AttemptStatus,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FinishedAttempt {
    pub attempt_id: AttemptId,
    pub step_index: u32,
    pub step_status: StepStatus,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Reconciliation {
    pub runs: usize,
    pub steps: usize,
    pub attempts: usize,
}

/// Handle to the single store actor and the live event stream.
#[derive(Clone)]
pub struct Store {
    commands: mpsc::UnboundedSender<Command>,
    events: broadcast::Sender<RunEvent>,
    database: PathBuf,
    artifacts: PathBuf,
    engine_id: String,
    schema_version: u32,
}

impl Store {
    pub async fn open(database: impl AsRef<Path>) -> Result<Self, HarnessError> {
        let database = database.as_ref().to_path_buf();
        let parent = database
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        std::fs::create_dir_all(&parent).map_err(|error| {
            HarnessError::Storage(format!("create {}: {error}", parent.display()))
        })?;
        let artifacts = parent.join("artifacts");
        std::fs::create_dir_all(&artifacts).map_err(|error| {
            HarnessError::Storage(format!("create {}: {error}", artifacts.display()))
        })?;

        let mut connection = Connection::open(&database).map_err(storage_error)?;
        migrations::apply(&mut connection)?;
        let schema_version =
            u32::try_from(migrations::current_version(&connection)).unwrap_or_default();
        let engine_id = uuid::Uuid::new_v4().simple().to_string();
        connection
            .execute(
                "INSERT INTO engine_instances(id, started_at) VALUES (?1, ?2)",
                params![engine_id.as_str(), now_millis()],
            )
            .map_err(storage_error)?;

        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let actor = Actor {
            connection,
            artifacts: ArtifactStore::new(&artifacts),
            events: events.clone(),
            engine_id: engine_id.clone(),
        };
        let commands = spawn_actor(actor)?;

        Ok(Self {
            commands,
            events,
            database,
            artifacts,
            engine_id,
            schema_version,
        })
    }

    #[must_use]
    pub fn engine_id(&self) -> &str {
        &self.engine_id
    }

    #[must_use]
    pub const fn schema_version(&self) -> u32 {
        self.schema_version
    }

    #[must_use]
    pub fn database_path(&self) -> &Path {
        &self.database
    }

    #[must_use]
    pub fn artifacts_path(&self) -> &Path {
        &self.artifacts
    }

    /// Live events after commit. Slow consumers resynchronize with
    /// [`Store::events_after`].
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<RunEvent> {
        self.events.subscribe()
    }

    pub async fn create_run(
        &self,
        acceptance: &FrozenAcceptance,
        parent_run_id: Option<RunId>,
    ) -> Result<RunRecord, HarnessError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::CreateRun {
                acceptance: Box::new(acceptance.clone()),
                parent: parent_run_id,
                reply,
            })
            .map_err(|_| stopped())?;
        response.await.map_err(|_| stopped())?
    }

    pub async fn persist_pending_steps(
        &self,
        run_id: &RunId,
        steps: &[PlannedStep],
    ) -> Result<(), HarnessError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::PersistPendingSteps {
                run_id: run_id.clone(),
                steps: steps.to_vec(),
                reply,
            })
            .map_err(|_| stopped())?;
        response.await.map_err(|_| stopped())?
    }

    pub async fn start_attempt(&self, request: StartAttempt) -> Result<AttemptId, HarnessError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::StartAttempt { request, reply })
            .map_err(|_| stopped())?;
        response.await.map_err(|_| stopped())?
    }

    pub async fn finish_attempt(
        &self,
        request: FinishAttempt,
    ) -> Result<FinishedAttempt, HarnessError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::FinishAttempt { request, reply })
            .map_err(|_| stopped())?;
        response.await.map_err(|_| stopped())?
    }

    pub async fn snapshot(&self, run_id: &RunId) -> Result<Snapshot, HarnessError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::Snapshot {
                run_id: run_id.clone(),
                reply,
            })
            .map_err(|_| stopped())?;
        response.await.map_err(|_| stopped())?
    }

    pub async fn events_after(
        &self,
        run_id: &RunId,
        after_seq: i64,
    ) -> Result<Vec<RunEvent>, HarnessError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::EventsAfter {
                run_id: run_id.clone(),
                after_seq,
                reply,
            })
            .map_err(|_| stopped())?;
        response.await.map_err(|_| stopped())?
    }

    pub async fn reconcile_interrupted(&self) -> Result<Reconciliation, HarnessError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::Reconcile { reply })
            .map_err(|_| stopped())?;
        response.await.map_err(|_| stopped())?
    }

    pub async fn write_artifact(
        &self,
        bytes: &[u8],
        media_type: &str,
    ) -> Result<ArtifactRef, HarnessError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::WriteArtifact {
                bytes: bytes.to_vec(),
                media_type: media_type.to_owned(),
                reply,
            })
            .map_err(|_| stopped())?;
        response.await.map_err(|_| stopped())?
    }

    pub async fn read_artifact(&self, id: &ArtifactId) -> Result<Vec<u8>, HarnessError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::ReadArtifact {
                id: id.clone(),
                reply,
            })
            .map_err(|_| stopped())?;
        response.await.map_err(|_| stopped())?
    }

    /// Commits the one legal status transition from `from`, or fails closed.
    pub async fn transition_run(
        &self,
        run_id: RunId,
        from: RunStatus,
        to: RunStatus,
        reason: Option<String>,
    ) -> Result<RunRecord, HarnessError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::TransitionRun {
                run_id,
                from,
                to,
                reason,
                reply,
            })
            .map_err(|_| stopped())?;
        response.await.map_err(|_| stopped())?
    }

    /// Commits one attempt's typed observations and check results atomically.
    pub async fn record_evidence(
        &self,
        run_id: RunId,
        attempt_id: AttemptId,
        action: Value,
        tool_results: Vec<ToolResult>,
        command_runs: Vec<CommandRun>,
        checks: Vec<CheckResult>,
    ) -> Result<(), HarnessError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::RecordEvidence {
                run_id,
                attempt_id,
                action,
                tool_results,
                command_runs,
                checks,
                reply,
            })
            .map_err(|_| stopped())?;
        response.await.map_err(|_| stopped())?
    }

    /// Records final verification checks against an existing attempt.
    pub async fn record_checks(
        &self,
        run_id: RunId,
        attempt_id: AttemptId,
        checks: Vec<CheckResult>,
    ) -> Result<(), HarnessError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::RecordChecks {
                run_id,
                attempt_id,
                checks,
                reply,
            })
            .map_err(|_| stopped())?;
        response.await.map_err(|_| stopped())?
    }

    /// Appends one recovery decision, with its strategy and step, to the
    /// durable journal.
    pub async fn record_decision(
        &self,
        run_id: RunId,
        step_index: u32,
        strategy: u32,
        decision: Value,
    ) -> Result<(), HarnessError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::RecordDecision {
                run_id,
                step_index,
                strategy,
                decision,
                reply,
            })
            .map_err(|_| stopped())?;
        response.await.map_err(|_| stopped())?
    }

    /// The run this engine instance is currently mutating, if any.
    pub async fn active_run(&self) -> Result<Option<RunId>, HarnessError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::ActiveRun { reply })
            .map_err(|_| stopped())?;
        response.await.map_err(|_| stopped())?
    }

    /// Reloads the frozen acceptance contract a run was created from. Retry
    /// needs it to prove that a rerun still targets the same bytes.
    pub async fn run_acceptance(&self, run_id: RunId) -> Result<FrozenAcceptance, HarnessError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::RunAcceptance { run_id, reply })
            .map_err(|_| stopped())?;
        response.await.map_err(|_| stopped())?
    }

    /// The most recently created run, used by `status` without an ID.
    pub async fn latest_run(&self) -> Result<Option<RunRecord>, HarnessError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::LatestRun { reply })
            .map_err(|_| stopped())?;
        response.await.map_err(|_| stopped())?
    }

    /// Newest runs first, bounded by `limit`.
    pub async fn list_runs(&self, limit: u32) -> Result<Vec<RunRecord>, HarnessError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::ListRuns { limit, reply })
            .map_err(|_| stopped())?;
        response.await.map_err(|_| stopped())?
    }

    /// Trusted on-disk location of a stored artifact. The path is derived from
    /// the content hash in the database, never from caller-supplied names.
    pub async fn artifact_path(&self, id: &ArtifactId) -> Result<PathBuf, HarnessError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::ArtifactPath {
                id: id.clone(),
                reply,
            })
            .map_err(|_| stopped())?;
        let path = response.await.map_err(|_| stopped())??;
        let root = std::fs::canonicalize(&self.artifacts).map_err(|error| {
            HarnessError::Storage(format!(
                "artifact root {}: {error}",
                self.artifacts.display()
            ))
        })?;
        let canonical = std::fs::canonicalize(&path).map_err(|error| {
            HarnessError::Storage(format!("artifact {}: {error}", path.display()))
        })?;
        if !canonical.starts_with(&root) {
            return Err(HarnessError::Storage(format!(
                "artifact {} resolves outside trusted storage",
                id.as_str()
            )));
        }
        Ok(canonical)
    }
}

enum Command {
    CreateRun {
        acceptance: Box<FrozenAcceptance>,
        parent: Option<RunId>,
        reply: oneshot::Sender<Result<RunRecord, HarnessError>>,
    },
    PersistPendingSteps {
        run_id: RunId,
        steps: Vec<PlannedStep>,
        reply: oneshot::Sender<Result<(), HarnessError>>,
    },
    StartAttempt {
        request: StartAttempt,
        reply: oneshot::Sender<Result<AttemptId, HarnessError>>,
    },
    FinishAttempt {
        request: FinishAttempt,
        reply: oneshot::Sender<Result<FinishedAttempt, HarnessError>>,
    },
    Snapshot {
        run_id: RunId,
        reply: oneshot::Sender<Result<Snapshot, HarnessError>>,
    },
    EventsAfter {
        run_id: RunId,
        after_seq: i64,
        reply: oneshot::Sender<Result<Vec<RunEvent>, HarnessError>>,
    },
    Reconcile {
        reply: oneshot::Sender<Result<Reconciliation, HarnessError>>,
    },
    WriteArtifact {
        bytes: Vec<u8>,
        media_type: String,
        reply: oneshot::Sender<Result<ArtifactRef, HarnessError>>,
    },
    ReadArtifact {
        id: ArtifactId,
        reply: oneshot::Sender<Result<Vec<u8>, HarnessError>>,
    },
    TransitionRun {
        run_id: RunId,
        from: RunStatus,
        to: RunStatus,
        reason: Option<String>,
        reply: oneshot::Sender<Result<RunRecord, HarnessError>>,
    },
    RecordEvidence {
        run_id: RunId,
        attempt_id: AttemptId,
        action: Value,
        tool_results: Vec<ToolResult>,
        command_runs: Vec<CommandRun>,
        checks: Vec<CheckResult>,
        reply: oneshot::Sender<Result<(), HarnessError>>,
    },
    RecordChecks {
        run_id: RunId,
        attempt_id: AttemptId,
        checks: Vec<CheckResult>,
        reply: oneshot::Sender<Result<(), HarnessError>>,
    },
    RecordDecision {
        run_id: RunId,
        step_index: u32,
        strategy: u32,
        decision: Value,
        reply: oneshot::Sender<Result<(), HarnessError>>,
    },
    ActiveRun {
        reply: oneshot::Sender<Result<Option<RunId>, HarnessError>>,
    },
    RunAcceptance {
        run_id: RunId,
        reply: oneshot::Sender<Result<FrozenAcceptance, HarnessError>>,
    },
    LatestRun {
        reply: oneshot::Sender<Result<Option<RunRecord>, HarnessError>>,
    },
    ListRuns {
        limit: u32,
        reply: oneshot::Sender<Result<Vec<RunRecord>, HarnessError>>,
    },
    ArtifactPath {
        id: ArtifactId,
        reply: oneshot::Sender<Result<PathBuf, HarnessError>>,
    },
}

struct Actor {
    connection: Connection,
    artifacts: ArtifactStore,
    events: broadcast::Sender<RunEvent>,
    engine_id: String,
}

impl Actor {
    fn handle(&mut self, command: Command) {
        match command {
            Command::CreateRun {
                acceptance,
                parent,
                reply,
            } => {
                let result = self.create_run(&acceptance, parent);
                let _ = reply.send(result);
            }
            Command::PersistPendingSteps {
                run_id,
                steps,
                reply,
            } => {
                let result = self.persist_pending_steps(&run_id, &steps);
                let _ = reply.send(result);
            }
            Command::StartAttempt { request, reply } => {
                let result = self.start_attempt(&request);
                let _ = reply.send(result);
            }
            Command::FinishAttempt { request, reply } => {
                let result = self.finish_attempt(&request);
                let _ = reply.send(result);
            }
            Command::Snapshot { run_id, reply } => {
                let result = snapshot_of(&self.connection, &run_id);
                let _ = reply.send(result);
            }
            Command::EventsAfter {
                run_id,
                after_seq,
                reply,
            } => {
                let result = events_after(&self.connection, &run_id, after_seq);
                let _ = reply.send(result);
            }
            Command::Reconcile { reply } => {
                let result = self.reconcile_interrupted();
                let _ = reply.send(result);
            }
            Command::WriteArtifact {
                bytes,
                media_type,
                reply,
            } => {
                let result = self.write_artifact(&bytes, &media_type);
                let _ = reply.send(result);
            }
            Command::ReadArtifact { id, reply } => {
                let result = self.read_artifact(&id);
                let _ = reply.send(result);
            }
            Command::TransitionRun {
                run_id,
                from,
                to,
                reason,
                reply,
            } => {
                let result = self.transition_run(&run_id, from, to, reason.as_deref());
                let _ = reply.send(result);
            }
            Command::RecordEvidence {
                run_id,
                attempt_id,
                action,
                tool_results,
                command_runs,
                checks,
                reply,
            } => {
                let result = self.record_evidence(
                    &run_id,
                    &attempt_id,
                    &action,
                    &tool_results,
                    &command_runs,
                    &checks,
                );
                let _ = reply.send(result);
            }
            Command::RecordChecks {
                run_id,
                attempt_id,
                checks,
                reply,
            } => {
                let result = self.record_checks(&run_id, &attempt_id, &checks);
                let _ = reply.send(result);
            }
            Command::RecordDecision {
                run_id,
                step_index,
                strategy,
                decision,
                reply,
            } => {
                let result = self.record_decision(&run_id, step_index, strategy, &decision);
                let _ = reply.send(result);
            }
            Command::ActiveRun { reply } => {
                let result = self.active_run();
                let _ = reply.send(result);
            }
            Command::RunAcceptance { run_id, reply } => {
                let result = self.run_acceptance(&run_id);
                let _ = reply.send(result);
            }
            Command::LatestRun { reply } => {
                let result = self.latest_run();
                let _ = reply.send(result);
            }
            Command::ListRuns { limit, reply } => {
                let result = self.list_runs(limit);
                let _ = reply.send(result);
            }
            Command::ArtifactPath { id, reply } => {
                let result = self.artifact_path(&id);
                let _ = reply.send(result);
            }
        }
    }

    fn create_run(
        &mut self,
        acceptance: &FrozenAcceptance,
        parent: Option<RunId>,
    ) -> Result<RunRecord, HarnessError> {
        acceptance
            .verify_hash()
            .map_err(|error| HarnessError::Contract(error.to_string()))?;
        let spec = acceptance.spec();
        let now = now_millis();
        let run_id = RunId::new();
        let spec_json = serde_json::to_string(acceptance)
            .map_err(|error| HarnessError::Storage(format!("serialize acceptance: {error}")))?;

        let transaction = self.connection.transaction().map_err(storage_error)?;
        transaction
            .execute(
                "INSERT OR IGNORE INTO acceptance_specs(hash, spec_json, created_at)
                 VALUES (?1, ?2, ?3)",
                params![acceptance.hash(), spec_json, now],
            )
            .map_err(storage_error)?;
        let stored: String = transaction
            .query_row(
                "SELECT spec_json FROM acceptance_specs WHERE hash = ?1",
                params![acceptance.hash()],
                |row| row.get(0),
            )
            .map_err(storage_error)?;
        if stored != spec_json {
            return Err(HarnessError::Conflict(
                "acceptance hash collides with a different contract".to_owned(),
            ));
        }
        transaction
            .execute(
                "INSERT INTO runs(id, mission, mission_version, acceptance_hash, goal, status,
                                  created_at, updated_at, engine_instance_id, parent_run_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    run_id.as_str(),
                    spec.mission,
                    spec.mission_version,
                    acceptance.hash(),
                    spec.goal,
                    RunStatus::Created.as_str(),
                    now,
                    now,
                    self.engine_id,
                    parent.as_ref().map(RunId::as_str),
                ],
            )
            .map_err(storage_error)?;
        let event = append_event(
            &transaction,
            &run_id,
            events::kind::RUN_CREATED,
            json!({
                "mission": spec.mission,
                "mission_version": spec.mission_version,
                "goal": spec.goal,
                "acceptance_hash": acceptance.hash(),
            }),
        )?;
        transaction.commit().map_err(storage_error)?;
        self.publish(&[event]);

        Ok(RunRecord {
            id: run_id,
            mission: spec.mission.clone(),
            mission_version: spec.mission_version,
            acceptance_hash: acceptance.hash().to_owned(),
            goal: spec.goal.clone(),
            status: RunStatus::Created,
            created_at: now,
            updated_at: now,
            parent_run_id: parent,
        })
    }

    fn persist_pending_steps(
        &mut self,
        run_id: &RunId,
        steps: &[PlannedStep],
    ) -> Result<(), HarnessError> {
        if steps.is_empty() {
            return Err(HarnessError::Conflict(
                "cannot persist an empty plan".to_owned(),
            ));
        }
        let now = now_millis();
        let transaction = self.connection.transaction().map_err(storage_error)?;
        let current = load_run(&transaction, run_id)?;
        if current.status.is_terminal() {
            return Err(HarnessError::Conflict(format!(
                "run {run_id} is already {}",
                current.status.as_str()
            )));
        }
        for step in steps {
            transaction
                .execute(
                    "INSERT INTO steps(run_id, step_index, title, criterion, status)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(run_id, step_index) DO NOTHING",
                    params![
                        run_id.as_str(),
                        step.index,
                        step.title,
                        step.criterion,
                        StepStatus::Pending.as_str(),
                    ],
                )
                .map_err(storage_error)?;
        }
        let mut pending = Vec::new();
        if current.status != RunStatus::Running {
            pending.push(append_event(
                &transaction,
                run_id,
                events::kind::RUN_STATUS,
                json!({
                    "from": current.status.as_str(),
                    "status": RunStatus::Running.as_str(),
                    "reason": null,
                }),
            )?);
        }
        transaction
            .execute(
                "UPDATE runs SET status = ?2, updated_at = ?3 WHERE id = ?1",
                params![run_id.as_str(), RunStatus::Running.as_str(), now],
            )
            .map_err(storage_error)?;
        pending.push(append_event(
            &transaction,
            run_id,
            events::kind::RUN_PLANNED,
            json!({
                "steps": steps.iter().map(|step| step.index).collect::<Vec<_>>(),
            }),
        )?);
        transaction.commit().map_err(storage_error)?;
        self.publish(&pending);
        Ok(())
    }

    fn start_attempt(&mut self, request: &StartAttempt) -> Result<AttemptId, HarnessError> {
        let now = now_millis();
        let transaction = self.connection.transaction().map_err(storage_error)?;
        let run = load_run(&transaction, &request.run_id)?;
        if run.status.is_terminal() {
            return Err(HarnessError::Conflict(format!(
                "run {} is already {}",
                request.run_id,
                run.status.as_str()
            )));
        }
        let step_status: String = transaction
            .query_row(
                "SELECT status FROM steps WHERE run_id = ?1 AND step_index = ?2",
                params![request.run_id.as_str(), request.step_index],
                |row| row.get(0),
            )
            .optional()
            .map_err(storage_error)?
            .ok_or_else(|| {
                HarnessError::NotFound(format!(
                    "step {} of run {}",
                    request.step_index, request.run_id
                ))
            })?;
        if StepStatus::parse(&step_status)?.is_terminal() {
            return Err(HarnessError::Conflict(format!(
                "step {} of run {} is already {step_status}",
                request.step_index, request.run_id
            )));
        }
        transaction
            .execute(
                "UPDATE steps SET status = ?3 WHERE run_id = ?1 AND step_index = ?2",
                params![
                    request.run_id.as_str(),
                    request.step_index,
                    StepStatus::Running.as_str(),
                ],
            )
            .map_err(storage_error)?;

        let attempt_id = AttemptId::new();
        let action_json = serde_json::to_string(&request.action)
            .map_err(|error| HarnessError::Storage(format!("serialize action: {error}")))?;
        transaction
            .execute(
                "INSERT INTO attempts(id, run_id, step_index, strategy, status, action_json,
                                      model, deadline_at, started_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    attempt_id.as_str(),
                    request.run_id.as_str(),
                    request.step_index,
                    request.strategy,
                    AttemptStatus::Executing.as_str(),
                    action_json,
                    request.model,
                    request.deadline_at,
                    now,
                ],
            )
            .map_err(storage_error)?;
        transaction
            .execute(
                "UPDATE runs SET updated_at = ?2 WHERE id = ?1",
                params![request.run_id.as_str(), now],
            )
            .map_err(storage_error)?;
        let event = append_event(
            &transaction,
            &request.run_id,
            events::kind::ATTEMPT_STARTED,
            json!({
                "attempt": attempt_id,
                "step_index": request.step_index,
                "strategy": request.strategy,
                "action": request.action,
            }),
        )?;
        transaction.commit().map_err(storage_error)?;
        self.publish(&[event]);
        Ok(attempt_id)
    }

    fn finish_attempt(&mut self, request: &FinishAttempt) -> Result<FinishedAttempt, HarnessError> {
        if !request.status.is_terminal() {
            return Err(HarnessError::Conflict(format!(
                "attempt cannot finish as {}",
                request.status.as_str()
            )));
        }
        let now = now_millis();
        let transaction = self.connection.transaction().map_err(storage_error)?;
        let (owner, step_index, current): (String, i64, String) = transaction
            .query_row(
                "SELECT run_id, step_index, status FROM attempts WHERE id = ?1",
                params![request.attempt_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(storage_error)?
            .ok_or_else(|| HarnessError::NotFound(format!("attempt {}", request.attempt_id)))?;
        if owner != request.run_id.as_str() {
            return Err(HarnessError::Conflict(format!(
                "attempt {} belongs to run {owner}",
                request.attempt_id
            )));
        }
        if AttemptStatus::parse(&current)?.is_terminal() {
            return Err(HarnessError::Conflict(format!(
                "attempt {} already finished as {current}",
                request.attempt_id
            )));
        }
        transaction
            .execute(
                "UPDATE attempts SET status = ?2, finished_at = ?3, error = ?4 WHERE id = ?1",
                params![
                    request.attempt_id.as_str(),
                    request.status.as_str(),
                    now,
                    request.error,
                ],
            )
            .map_err(storage_error)?;

        let (step_status, event_kind) = match request.status {
            AttemptStatus::Passed => (StepStatus::Passed, events::kind::STEP_PASSED),
            AttemptStatus::Failed => (StepStatus::Retrying, events::kind::STEP_FAILED),
            AttemptStatus::Inconclusive => {
                (StepStatus::Replanning, events::kind::STEP_INCONCLUSIVE)
            }
            AttemptStatus::Aborted => (StepStatus::Blocked, events::kind::STEP_BLOCKED),
            AttemptStatus::Interrupted => (StepStatus::Blocked, events::kind::STEP_BLOCKED),
            _ => {
                return Err(HarnessError::Conflict(
                    "attempt status must be terminal".to_owned(),
                ))
            }
        };
        transaction
            .execute(
                "UPDATE steps SET status = ?3 WHERE run_id = ?1 AND step_index = ?2",
                params![request.run_id.as_str(), step_index, step_status.as_str()],
            )
            .map_err(storage_error)?;
        transaction
            .execute(
                "UPDATE runs SET updated_at = ?2 WHERE id = ?1",
                params![request.run_id.as_str(), now],
            )
            .map_err(storage_error)?;

        let step_index = u32::try_from(step_index)
            .map_err(|_| HarnessError::Storage("step index out of range".to_owned()))?;
        let event = append_event(
            &transaction,
            &request.run_id,
            event_kind,
            json!({
                "attempt": request.attempt_id,
                "step_index": step_index,
                "status": request.status.as_str(),
                "error": request.error,
            }),
        )?;
        transaction.commit().map_err(storage_error)?;
        self.publish(&[event]);

        Ok(FinishedAttempt {
            attempt_id: request.attempt_id.clone(),
            step_index,
            step_status,
        })
    }

    fn reconcile_interrupted(&mut self) -> Result<Reconciliation, HarnessError> {
        let now = now_millis();
        let transaction = self.connection.transaction().map_err(storage_error)?;
        let stale: Vec<String> = {
            let mut statement = transaction
                .prepare(
                    "SELECT id FROM runs
                     WHERE engine_instance_id <> ?1
                       AND status NOT IN ('passed', 'cancelled', 'incomplete', 'interrupted')
                     ORDER BY created_at",
                )
                .map_err(storage_error)?;
            let rows = statement
                .query_map(params![self.engine_id.as_str()], |row| {
                    row.get::<_, String>(0)
                })
                .map_err(storage_error)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(storage_error)?;
            rows
        };

        let mut reconciliation = Reconciliation::default();
        let mut pending_events = Vec::with_capacity(stale.len());
        for raw_run_id in stale {
            let run_id: RunId = raw_run_id.parse()?;
            let attempts = transaction
                .execute(
                    "UPDATE attempts SET status = 'interrupted', finished_at = ?2
                     WHERE run_id = ?1 AND status IN ('proposed', 'guarded', 'executing', 'verifying')",
                    params![run_id.as_str(), now],
                )
                .map_err(storage_error)?;
            let steps = transaction
                .execute(
                    "UPDATE steps SET status = 'blocked'
                     WHERE run_id = ?1
                       AND status IN ('pending', 'running', 'verifying', 'retrying', 'replanning')",
                    params![run_id.as_str()],
                )
                .map_err(storage_error)?;
            transaction
                .execute(
                    "UPDATE runs SET status = 'interrupted', updated_at = ?2 WHERE id = ?1",
                    params![run_id.as_str(), now],
                )
                .map_err(storage_error)?;
            pending_events.push(append_event(
                &transaction,
                &run_id,
                events::kind::RUN_INTERRUPTED,
                json!({
                    "reason": "engine_restarted",
                    "attempts": attempts,
                    "steps": steps,
                }),
            )?);
            reconciliation.runs += 1;
            reconciliation.steps += steps;
            reconciliation.attempts += attempts;
        }
        transaction.commit().map_err(storage_error)?;
        self.publish(&pending_events);
        Ok(reconciliation)
    }

    fn write_artifact(
        &mut self,
        bytes: &[u8],
        media_type: &str,
    ) -> Result<ArtifactRef, HarnessError> {
        let sha256 = sha256_hex(bytes);
        let existing: Option<(String, i64, String)> = self
            .connection
            .query_row(
                "SELECT id, size, media_type FROM artifacts WHERE sha256 = ?1",
                params![sha256.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(storage_error)?;
        if let Some((id, size, stored_media)) = existing {
            return Ok(ArtifactRef {
                id: id.parse()?,
                sha256,
                size: u64::try_from(size).unwrap_or_default(),
                media_type: stored_media,
            });
        }
        self.artifacts.write(&sha256, bytes)?;
        let id = ArtifactId::new();
        let size = i64::try_from(bytes.len()).unwrap_or(i64::MAX);
        self.connection
            .execute(
                "INSERT INTO artifacts(id, sha256, size, media_type, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id.as_str(), sha256.as_str(), size, media_type, now_millis()],
            )
            .map_err(storage_error)?;
        Ok(ArtifactRef {
            id,
            sha256,
            size: bytes.len() as u64,
            media_type: media_type.to_owned(),
        })
    }

    fn read_artifact(&self, id: &ArtifactId) -> Result<Vec<u8>, HarnessError> {
        let sha256: String = self
            .connection
            .query_row(
                "SELECT sha256 FROM artifacts WHERE id = ?1",
                params![id.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(storage_error)?
            .ok_or_else(|| HarnessError::NotFound(format!("artifact {id}")))?;
        self.artifacts.read(&sha256)
    }

    fn transition_run(
        &mut self,
        run_id: &RunId,
        from: RunStatus,
        to: RunStatus,
        reason: Option<&str>,
    ) -> Result<RunRecord, HarnessError> {
        if !is_legal_transition(from, to) {
            return Err(HarnessError::Conflict(format!(
                "run {} cannot move from {} to {}",
                run_id,
                from.as_str(),
                to.as_str()
            )));
        }
        let now = now_millis();
        let transaction = self.connection.transaction().map_err(storage_error)?;
        let current = load_run(&transaction, run_id)?;
        if current.status != from {
            return Err(HarnessError::Conflict(format!(
                "run {} is {}, not {}",
                run_id,
                current.status.as_str(),
                from.as_str()
            )));
        }
        transaction
            .execute(
                "UPDATE runs SET status = ?2, updated_at = ?3 WHERE id = ?1",
                params![run_id.as_str(), to.as_str(), now],
            )
            .map_err(storage_error)?;
        let payload = json!({
            "from": from.as_str(),
            "status": to.as_str(),
            "reason": reason,
        });
        let kind = match to {
            RunStatus::FinalVerifying => events::kind::RUN_FINAL_VERIFYING,
            RunStatus::Cancelling => events::kind::RUN_CANCELLING,
            status if status.is_terminal() => events::kind::RUN_FINISHED,
            _ => events::kind::RUN_STATUS,
        };
        let event = append_event(&transaction, run_id, kind, payload)?;
        let updated = load_run(&transaction, run_id)?;
        transaction.commit().map_err(storage_error)?;
        self.publish(&[event]);
        Ok(updated)
    }

    fn record_evidence(
        &mut self,
        run_id: &RunId,
        attempt_id: &AttemptId,
        action: &Value,
        tool_results: &[ToolResult],
        command_runs: &[CommandRun],
        checks: &[CheckResult],
    ) -> Result<(), HarnessError> {
        let now = now_millis();
        let transaction = self.connection.transaction().map_err(storage_error)?;
        let (owner, step_index) = attempt_owner(&transaction, attempt_id)?;
        if owner != *run_id {
            return Err(HarnessError::Conflict(format!(
                "attempt {attempt_id} belongs to run {owner}"
            )));
        }
        let action_json = serde_json::to_string(action)
            .map_err(|error| HarnessError::Storage(format!("serialize action: {error}")))?;
        transaction
            .execute(
                "UPDATE attempts SET action_json = ?2 WHERE id = ?1",
                params![attempt_id.as_str(), action_json],
            )
            .map_err(storage_error)?;

        let outcome = if tool_results.iter().all(ToolResult::succeeded)
            && command_runs.iter().all(|run| {
                run.exit_code == Some(0) && run.status == crate::tools::ToolStatus::Succeeded
            }) {
            "succeeded"
        } else {
            "mixed"
        };
        let payload = serde_json::to_string(&json!({
            "tool_results": tool_results,
            "command_runs": command_runs,
        }))
        .map_err(|error| HarnessError::Storage(format!("serialize tool evidence: {error}")))?;
        transaction
            .execute(
                "INSERT INTO tool_results(attempt_id, outcome, payload_json, created_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(attempt_id) DO UPDATE SET
                     outcome = excluded.outcome,
                     payload_json = excluded.payload_json,
                     created_at = excluded.created_at",
                params![attempt_id.as_str(), outcome, payload, now],
            )
            .map_err(storage_error)?;

        let mut pending = Vec::new();
        for result in tool_results {
            pending.push(append_event(
                &transaction,
                run_id,
                events::kind::TOOL_RESULT,
                json!({
                    "attempt": attempt_id,
                    "tool": result.tool.as_str(),
                    "call_id": result.call_id,
                    "status": result.status.as_str(),
                    "exit_code": result.exit_code,
                    "artifacts": result.artifacts.iter().map(|digest| digest.sha256.clone()).collect::<Vec<_>>(),
                }),
            )?);
        }
        for run in command_runs {
            pending.push(append_event(
                &transaction,
                run_id,
                events::kind::TOOL_RESULT,
                json!({
                    "attempt": attempt_id,
                    "tool": "command",
                    "template": run.template,
                    "status": run.status.as_str(),
                    "exit_code": run.exit_code,
                }),
            )?);
        }
        insert_checks(
            &transaction,
            run_id,
            attempt_id,
            step_index,
            checks,
            &mut pending,
        )?;

        transaction
            .execute(
                "UPDATE runs SET updated_at = ?2 WHERE id = ?1",
                params![run_id.as_str(), now],
            )
            .map_err(storage_error)?;
        transaction.commit().map_err(storage_error)?;
        self.publish(&pending);
        Ok(())
    }

    fn record_checks(
        &mut self,
        run_id: &RunId,
        attempt_id: &AttemptId,
        checks: &[CheckResult],
    ) -> Result<(), HarnessError> {
        let now = now_millis();
        let transaction = self.connection.transaction().map_err(storage_error)?;
        let (owner, step_index) = attempt_owner(&transaction, attempt_id)?;
        if owner != *run_id {
            return Err(HarnessError::Conflict(format!(
                "attempt {attempt_id} belongs to run {owner}"
            )));
        }
        let mut pending = Vec::new();
        insert_checks(
            &transaction,
            run_id,
            attempt_id,
            step_index,
            checks,
            &mut pending,
        )?;
        transaction
            .execute(
                "UPDATE runs SET updated_at = ?2 WHERE id = ?1",
                params![run_id.as_str(), now],
            )
            .map_err(storage_error)?;
        transaction.commit().map_err(storage_error)?;
        self.publish(&pending);
        Ok(())
    }

    fn record_decision(
        &mut self,
        run_id: &RunId,
        step_index: u32,
        strategy: u32,
        decision: &Value,
    ) -> Result<(), HarnessError> {
        let now = now_millis();
        let transaction = self.connection.transaction().map_err(storage_error)?;
        let current = load_run(&transaction, run_id)?;
        if current.status.is_terminal() {
            return Err(HarnessError::Conflict(format!(
                "run {run_id} is already {}",
                current.status.as_str()
            )));
        }
        transaction
            .execute(
                "UPDATE runs SET updated_at = ?2 WHERE id = ?1",
                params![run_id.as_str(), now],
            )
            .map_err(storage_error)?;
        let event = append_event(
            &transaction,
            run_id,
            events::kind::RECOVERY_DECISION,
            json!({
                "step_index": step_index,
                "strategy": strategy,
                "decision": decision,
            }),
        )?;
        transaction.commit().map_err(storage_error)?;
        self.publish(&[event]);
        Ok(())
    }

    fn active_run(&self) -> Result<Option<RunId>, HarnessError> {
        let raw: Option<String> = self
            .connection
            .query_row(
                "SELECT id FROM runs
                 WHERE engine_instance_id = ?1
                   AND status IN ('planning', 'awaiting_plan_approval', 'running',
                                  'final_verifying', 'cancelling')
                 ORDER BY updated_at DESC LIMIT 1",
                params![self.engine_id.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(storage_error)?;
        raw.map(|value| value.parse()).transpose()
    }

    fn run_acceptance(&self, run_id: &RunId) -> Result<FrozenAcceptance, HarnessError> {
        load_run(&self.connection, run_id)?;
        let spec_json: String = self
            .connection
            .query_row(
                "SELECT a.spec_json FROM runs r
                 JOIN acceptance_specs a ON a.hash = r.acceptance_hash
                 WHERE r.id = ?1",
                params![run_id.as_str()],
                |row| row.get(0),
            )
            .map_err(storage_error)?;
        let acceptance: FrozenAcceptance = serde_json::from_str(&spec_json)
            .map_err(|error| HarnessError::Storage(format!("corrupt acceptance: {error}")))?;
        acceptance
            .verify_hash()
            .map_err(|error| HarnessError::Contract(error.to_string()))?;
        Ok(acceptance)
    }

    fn latest_run(&self) -> Result<Option<RunRecord>, HarnessError> {
        let raw: Option<String> = self
            .connection
            .query_row(
                "SELECT id FROM runs ORDER BY created_at DESC, updated_at DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(storage_error)?;
        let Some(raw) = raw else {
            return Ok(None);
        };
        let run_id: RunId = raw.parse()?;
        load_run(&self.connection, &run_id).map(Some)
    }

    fn list_runs(&self, limit: u32) -> Result<Vec<RunRecord>, HarnessError> {
        let limit = limit.clamp(1, 200);
        let mut statement = self
            .connection
            .prepare("SELECT id FROM runs ORDER BY created_at DESC, updated_at DESC LIMIT ?1")
            .map_err(storage_error)?;
        let ids = statement
            .query_map(params![limit], |row| row.get::<_, String>(0))
            .map_err(storage_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(storage_error)?;
        ids.into_iter()
            .map(|raw| {
                let run_id: RunId = raw.parse()?;
                load_run(&self.connection, &run_id)
            })
            .collect()
    }

    fn artifact_path(&self, id: &ArtifactId) -> Result<PathBuf, HarnessError> {
        let sha256: String = self
            .connection
            .query_row(
                "SELECT sha256 FROM artifacts WHERE id = ?1",
                params![id.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(storage_error)?
            .ok_or_else(|| HarnessError::NotFound(format!("artifact {id}")))?;
        Ok(self.artifacts.path_for(&sha256))
    }

    fn publish(&self, events: &[RunEvent]) {
        for event in events {
            let _ = self.events.send(event.clone());
        }
    }
}

fn spawn_actor(mut actor: Actor) -> Result<mpsc::UnboundedSender<Command>, HarnessError> {
    let (sender, mut receiver) = mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name("lfm3-store".to_owned())
        .spawn(move || {
            while let Some(command) = receiver.blocking_recv() {
                actor.handle(command);
            }
        })
        .map_err(|error| HarnessError::Storage(format!("start store actor: {error}")))?;
    Ok(sender)
}

fn snapshot_of(connection: &Connection, run_id: &RunId) -> Result<Snapshot, HarnessError> {
    let run = load_run(connection, run_id)?;
    Ok(Snapshot {
        steps: load_steps(connection, run_id)?,
        run,
    })
}

fn events_after(
    connection: &Connection,
    run_id: &RunId,
    after_seq: i64,
) -> Result<Vec<RunEvent>, HarnessError> {
    load_run(connection, run_id)?;
    let mut statement = connection
        .prepare(
            "SELECT seq, kind, payload_json, created_at FROM events
             WHERE run_id = ?1 AND seq > ?2 ORDER BY seq",
        )
        .map_err(storage_error)?;
    let rows = statement
        .query_map(params![run_id.as_str(), after_seq], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })
        .map_err(storage_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(storage_error)?;
    rows.into_iter()
        .map(|(seq, kind, payload, at)| {
            Ok(RunEvent {
                run_id: run_id.clone(),
                seq,
                kind,
                payload: serde_json::from_str(&payload).map_err(|error| {
                    HarnessError::Storage(format!("corrupt event payload: {error}"))
                })?,
                at,
            })
        })
        .collect()
}

fn append_event(
    transaction: &Transaction<'_>,
    run_id: &RunId,
    kind: &str,
    payload: Value,
) -> Result<RunEvent, HarnessError> {
    let seq: i64 = transaction
        .query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM events WHERE run_id = ?1",
            params![run_id.as_str()],
            |row| row.get(0),
        )
        .map_err(storage_error)?;
    let at = now_millis();
    let payload_json = serde_json::to_string(&payload)
        .map_err(|error| HarnessError::Storage(format!("serialize event: {error}")))?;
    transaction
        .execute(
            "INSERT INTO events(run_id, seq, kind, payload_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![run_id.as_str(), seq, kind, payload_json, at],
        )
        .map_err(storage_error)?;
    Ok(RunEvent {
        run_id: run_id.clone(),
        seq,
        kind: kind.to_owned(),
        payload,
        at,
    })
}

struct RunRow {
    mission: String,
    mission_version: i64,
    acceptance_hash: String,
    goal: String,
    status: String,
    created_at: i64,
    updated_at: i64,
    parent_run_id: Option<String>,
}

fn load_run(connection: &Connection, run_id: &RunId) -> Result<RunRecord, HarnessError> {
    let row = connection
        .query_row(
            "SELECT mission, mission_version, acceptance_hash, goal, status,
                    created_at, updated_at, parent_run_id
             FROM runs WHERE id = ?1",
            params![run_id.as_str()],
            |row| {
                Ok(RunRow {
                    mission: row.get(0)?,
                    mission_version: row.get(1)?,
                    acceptance_hash: row.get(2)?,
                    goal: row.get(3)?,
                    status: row.get(4)?,
                    created_at: row.get(5)?,
                    updated_at: row.get(6)?,
                    parent_run_id: row.get(7)?,
                })
            },
        )
        .optional()
        .map_err(storage_error)?
        .ok_or_else(|| HarnessError::NotFound(format!("run {run_id}")))?;
    Ok(RunRecord {
        id: run_id.clone(),
        mission: row.mission,
        mission_version: u32::try_from(row.mission_version).unwrap_or_default(),
        acceptance_hash: row.acceptance_hash,
        goal: row.goal,
        status: RunStatus::parse(&row.status)?,
        created_at: row.created_at,
        updated_at: row.updated_at,
        parent_run_id: row.parent_run_id.map(|value| value.parse()).transpose()?,
    })
}

struct StepRow {
    index: i64,
    title: String,
    criterion: String,
    status: String,
}

fn load_steps(connection: &Connection, run_id: &RunId) -> Result<Vec<StepSnapshot>, HarnessError> {
    let mut statement = connection
        .prepare(
            "SELECT step_index, title, criterion, status FROM steps
             WHERE run_id = ?1 ORDER BY step_index",
        )
        .map_err(storage_error)?;
    let rows = statement
        .query_map(params![run_id.as_str()], |row| {
            Ok(StepRow {
                index: row.get(0)?,
                title: row.get(1)?,
                criterion: row.get(2)?,
                status: row.get(3)?,
            })
        })
        .map_err(storage_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(storage_error)?;
    rows.into_iter()
        .map(|row| {
            let index = u32::try_from(row.index)
                .map_err(|_| HarnessError::Storage("step index out of range".to_owned()))?;
            Ok(StepSnapshot {
                index,
                attempts: load_attempts(connection, run_id, index)?,
                title: row.title,
                criterion: row.criterion,
                status: StepStatus::parse(&row.status)?,
            })
        })
        .collect()
}

struct AttemptRow {
    id: String,
    strategy: i64,
    status: String,
    action: String,
    model: String,
    started_at: i64,
    finished_at: Option<i64>,
    error: Option<String>,
}

fn load_attempts(
    connection: &Connection,
    run_id: &RunId,
    step_index: u32,
) -> Result<Vec<AttemptSnapshot>, HarnessError> {
    let mut statement = connection
        .prepare(
            "SELECT id, strategy, status, action_json, model, started_at, finished_at, error
             FROM attempts WHERE run_id = ?1 AND step_index = ?2
             ORDER BY started_at, rowid",
        )
        .map_err(storage_error)?;
    let rows = statement
        .query_map(params![run_id.as_str(), step_index], |row| {
            Ok(AttemptRow {
                id: row.get(0)?,
                strategy: row.get(1)?,
                status: row.get(2)?,
                action: row.get(3)?,
                model: row.get(4)?,
                started_at: row.get(5)?,
                finished_at: row.get(6)?,
                error: row.get(7)?,
            })
        })
        .map_err(storage_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(storage_error)?;
    rows.into_iter()
        .map(|row| {
            Ok(AttemptSnapshot {
                id: row.id.parse()?,
                strategy: u32::try_from(row.strategy).unwrap_or_default(),
                status: AttemptStatus::parse(&row.status)?,
                action: serde_json::from_str(&row.action).map_err(|error| {
                    HarnessError::Storage(format!("corrupt attempt action: {error}"))
                })?,
                model: row.model,
                started_at: row.started_at,
                finished_at: row.finished_at,
                error: row.error,
            })
        })
        .collect()
}

pub(crate) fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

fn stopped() -> HarnessError {
    HarnessError::Storage("store actor is not running".to_owned())
}

fn insert_checks(
    transaction: &Transaction<'_>,
    run_id: &RunId,
    attempt_id: &AttemptId,
    step_index: i64,
    checks: &[CheckResult],
    pending: &mut Vec<RunEvent>,
) -> Result<(), HarnessError> {
    let now = now_millis();
    for check in checks {
        let detail = serde_json::to_string(check)
            .map_err(|error| HarnessError::Storage(format!("serialize check result: {error}")))?;
        transaction
            .execute(
                "INSERT INTO check_results(attempt_id, kind, passed, detail, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    attempt_id.as_str(),
                    check.kind,
                    i64::from(check.status == CheckStatus::Pass),
                    detail,
                    now,
                ],
            )
            .map_err(storage_error)?;
        // The deciding semantic judgement is the last one in the chain: a
        // `vl->d1` pair records the observation first and the decision second.
        let semantic = check
            .evidence
            .iter()
            .rev()
            .find_map(|item| item.semantic.as_ref());
        let mut payload = json!({
            "attempt": attempt_id,
            "step_index": step_index,
            "kind": check.kind,
            "status": check.status.as_str(),
            "message": check.message,
            "evidence_count": check.evidence.len(),
            "sources": check.evidence.iter().map(|item| item.source.clone()).collect::<Vec<_>>(),
        });
        // Semantic metadata is added only when present so deterministic check
        // events keep their existing shape.
        if let Some(semantic) = semantic {
            payload["semantic"] = json!(true);
            payload["semantic_chain"] = json!(semantic.chain);
            payload["semantic_label"] = json!(semantic.label);
            payload["confidence_bps"] = json!(semantic.confidence_bps);
            payload["threshold_bps"] = json!(semantic.threshold_bps);
        }
        pending.push(append_event(
            transaction,
            run_id,
            events::kind::CHECK_RESULT,
            payload,
        )?);
    }
    Ok(())
}

fn attempt_owner(
    connection: &Connection,
    attempt_id: &AttemptId,
) -> Result<(RunId, i64), HarnessError> {
    let (owner, step_index): (String, i64) = connection
        .query_row(
            "SELECT run_id, step_index FROM attempts WHERE id = ?1",
            params![attempt_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(storage_error)?
        .ok_or_else(|| HarnessError::NotFound(format!("attempt {attempt_id}")))?;
    Ok((owner.parse()?, step_index))
}

/// Run states only move forward through this explicit table. Anything else is
/// a conflict, so a bug or a race cannot jump straight to `passed`.
fn is_legal_transition(from: RunStatus, to: RunStatus) -> bool {
    use RunStatus::{
        AwaitingPlanApproval, Cancelled, Cancelling, Created, FinalVerifying, Incomplete, Passed,
        Planning, Running,
    };
    matches!(
        (from, to),
        (
            Created,
            Planning | AwaitingPlanApproval | Running | Cancelled
        ) | (
            Planning,
            AwaitingPlanApproval | Running | Cancelled | Incomplete
        ) | (AwaitingPlanApproval, Running | Cancelled | Incomplete)
            | (
                Running,
                FinalVerifying | Cancelling | Cancelled | Incomplete
            )
            | (FinalVerifying, Passed | Cancelled | Incomplete)
            | (Cancelling, Cancelled | Incomplete)
    )
}

fn storage_error(error: rusqlite::Error) -> HarnessError {
    HarnessError::Storage(error.to_string())
}
