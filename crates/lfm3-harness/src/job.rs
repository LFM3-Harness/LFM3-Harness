//! Durable journal for model-driven jobs.
//!
//! A job is not a verified run: it has no frozen acceptance contract, so it can
//! never be cited as mission evidence. Its truth is the ordered turn stream
//! plus the measured deliverable proof. A job the engine stopped in the middle
//! of is reported as interrupted and never silently resumed.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};

use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{store::now_millis, HarnessError};

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;
PRAGMA busy_timeout = 5000;

CREATE TABLE IF NOT EXISTS jobs (
    id TEXT PRIMARY KEY,
    goal TEXT NOT NULL,
    deliverable TEXT,
    status TEXT NOT NULL,
    stopped TEXT NOT NULL,
    model TEXT NOT NULL,
    max_turns INTEGER NOT NULL,
    minutes INTEGER NOT NULL,
    proof_path TEXT,
    proof_bytes INTEGER,
    proof_digest TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    finished_at INTEGER
);

CREATE INDEX IF NOT EXISTS jobs_by_created ON jobs(created_at DESC);

CREATE TABLE IF NOT EXISTS job_events (
    job_id TEXT NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
    seq INTEGER NOT NULL,
    kind TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (job_id, seq)
);
"#;

/// How a job ended. Only `running` is non-terminal.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Running,
    Passed,
    Failed,
    Cancelled,
    Interrupted,
}

impl JobStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, HarnessError> {
        match raw {
            "running" => Ok(Self::Running),
            "passed" => Ok(Self::Passed),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            "interrupted" => Ok(Self::Interrupted),
            other => Err(HarnessError::Storage(format!(
                "unknown job status {other:?}"
            ))),
        }
    }

    #[must_use]
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }
}

/// Measured proof that the required output exists, not a model claim.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct JobDeliverable {
    pub path: String,
    pub bytes: u64,
    pub digest: String,
}

/// One submitted job.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JobRecord {
    pub id: String,
    pub goal: String,
    pub deliverable: Option<String>,
    pub status: JobStatus,
    pub stopped: String,
    pub model: String,
    pub max_turns: u32,
    pub minutes: u32,
    pub deliverable_proof: Option<JobDeliverable>,
    pub created_at: i64,
    pub updated_at: i64,
    pub finished_at: Option<i64>,
}

/// One item of the acting model's own plan.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct JobTodo {
    pub text: String,
    #[serde(default)]
    pub done: bool,
}

/// One tool call inside one turn.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JobCall {
    pub tool: String,
    #[serde(default)]
    pub arguments: Value,
    pub ok: bool,
    #[serde(default)]
    pub output: String,
}

/// Everything that happened in one model turn.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JobTurn {
    pub index: u32,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub calls: Vec<JobCall>,
    #[serde(default)]
    pub todos: Vec<JobTodo>,
}

/// One ordered journal entry for a job.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JobEvent {
    pub job_id: String,
    pub seq: i64,
    pub kind: String,
    pub payload: Value,
    pub at: i64,
}

/// Everything needed to render one job.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JobView {
    pub job: JobRecord,
    pub todos: Vec<JobTodo>,
    pub turns: Vec<JobTurn>,
}

/// The request that opens a job.
#[derive(Clone, Debug)]
pub struct JobStart {
    pub goal: String,
    pub deliverable: Option<String>,
    pub minutes: u32,
    pub max_turns: u32,
}

/// How a job finished, recorded in one transaction with its final event.
#[derive(Clone, Debug)]
pub struct JobFinish {
    pub status: JobStatus,
    pub stopped: String,
    pub model: String,
    pub proof: Option<JobDeliverable>,
}

/// SQLite-backed job history. Cheap to clone.
#[derive(Clone)]
pub struct JobStore {
    inner: Arc<Inner>,
}

struct Inner {
    connection: Mutex<Connection>,
    database: PathBuf,
}

impl JobStore {
    pub fn open(database: impl AsRef<Path>) -> Result<Self, HarnessError> {
        let database = database.as_ref().to_path_buf();
        if let Some(parent) = database.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                HarnessError::Storage(format!("create {}: {error}", parent.display()))
            })?;
        }
        let connection = Connection::open(&database).map_err(storage)?;
        connection.execute_batch(SCHEMA).map_err(storage)?;
        Ok(Self {
            inner: Arc::new(Inner {
                connection: Mutex::new(connection),
                database,
            }),
        })
    }

    #[must_use]
    pub fn database(&self) -> &Path {
        &self.inner.database
    }

    /// Opens one job in the `running` state. Nothing else is written yet; the
    /// caller appends `job.started` in the same breath.
    pub fn create(&self, start: JobStart) -> Result<JobRecord, HarnessError> {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let now = now_millis();
        let connection = self.lock()?;
        connection
            .execute(
                "INSERT INTO jobs (id, goal, deliverable, status, stopped, model, max_turns,
                                   minutes, created_at, updated_at)
                 VALUES (?1, ?2, ?3, 'running', '', '', ?4, ?5, ?6, ?6)",
                params![
                    id,
                    start.goal,
                    start.deliverable,
                    start.max_turns,
                    start.minutes,
                    now
                ],
            )
            .map_err(storage)?;
        drop(connection);
        self.record(&id)
    }

    /// Appends one journal entry and bumps the job's `updated_at` together.
    pub fn append(
        &self,
        job_id: &str,
        kind: &str,
        payload: &Value,
    ) -> Result<JobEvent, HarnessError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction().map_err(storage)?;
        let seq: i64 = transaction
            .query_row(
                "SELECT COALESCE(MAX(seq), 0) + 1 FROM job_events WHERE job_id = ?1",
                params![job_id],
                |row| row.get(0),
            )
            .map_err(storage)?;
        let at = now_millis();
        transaction
            .execute(
                "INSERT INTO job_events (job_id, seq, kind, payload_json, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![job_id, seq, kind, payload.to_string(), at],
            )
            .map_err(storage)?;
        transaction
            .execute(
                "UPDATE jobs SET updated_at = ?2 WHERE id = ?1",
                params![job_id, at],
            )
            .map_err(storage)?;
        transaction.commit().map_err(storage)?;
        Ok(JobEvent {
            job_id: job_id.to_owned(),
            seq,
            kind: kind.to_owned(),
            payload: payload.clone(),
            at,
        })
    }

    /// Records the terminal state and its `job.finished` event atomically.
    pub fn finish(&self, job_id: &str, finish: JobFinish) -> Result<JobEvent, HarnessError> {
        let at = now_millis();
        let mut connection = self.lock()?;
        let transaction = connection.transaction().map_err(storage)?;
        let seq: i64 = transaction
            .query_row(
                "SELECT COALESCE(MAX(seq), 0) + 1 FROM job_events WHERE job_id = ?1",
                params![job_id],
                |row| row.get(0),
            )
            .map_err(storage)?;
        let proof = finish.proof;
        transaction
            .execute(
                "UPDATE jobs SET status = ?2, stopped = ?3, model = ?4, proof_path = ?5,
                                 proof_bytes = ?6, proof_digest = ?7, finished_at = ?8,
                                 updated_at = ?8
                 WHERE id = ?1",
                params![
                    job_id,
                    finish.status.as_str(),
                    finish.stopped,
                    finish.model,
                    proof.as_ref().map(|proof| proof.path.as_str()),
                    proof.as_ref().map(|proof| proof.bytes),
                    proof.as_ref().map(|proof| proof.digest.as_str()),
                    at
                ],
            )
            .map_err(storage)?;
        let payload = json!({
            "status": finish.status.as_str(),
            "stopped": finish.stopped,
            "model": finish.model,
            "deliverable": proof,
        });
        transaction
            .execute(
                "INSERT INTO job_events (job_id, seq, kind, payload_json, created_at)
                 VALUES (?1, ?2, 'job.finished', ?3, ?4)",
                params![job_id, seq, payload.to_string(), at],
            )
            .map_err(storage)?;
        transaction.commit().map_err(storage)?;
        Ok(JobEvent {
            job_id: job_id.to_owned(),
            seq,
            kind: "job.finished".to_owned(),
            payload,
            at,
        })
    }

    pub fn record(&self, job_id: &str) -> Result<JobRecord, HarnessError> {
        let connection = self.lock()?;
        connection
            .query_row(
                "SELECT id, goal, deliverable, status, stopped, model, max_turns, minutes,
                        proof_path, proof_bytes, proof_digest, created_at, updated_at, finished_at
                 FROM jobs WHERE id = ?1",
                params![job_id],
                job_row,
            )
            .optional()
            .map_err(storage)?
            .ok_or_else(|| HarnessError::Storage(format!("unknown job {job_id:?}")))
    }

    /// The job plus the todo list and turns folded from its own journal.
    pub fn view(&self, job_id: &str) -> Result<JobView, HarnessError> {
        let job = self.record(job_id)?;
        let (todos, turns) = fold(&self.events_after(job_id, 0)?);
        Ok(JobView { job, todos, turns })
    }

    pub fn list(&self, limit: u32) -> Result<Vec<JobRecord>, HarnessError> {
        let connection = self.lock()?;
        let mut statement = connection
            .prepare(
                "SELECT id, goal, deliverable, status, stopped, model, max_turns, minutes,
                        proof_path, proof_bytes, proof_digest, created_at, updated_at, finished_at
                 FROM jobs ORDER BY created_at DESC LIMIT ?1",
            )
            .map_err(storage)?;
        let rows = statement
            .query_map(params![limit], job_row)
            .map_err(storage)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(storage)
    }

    pub fn events_after(&self, job_id: &str, after: i64) -> Result<Vec<JobEvent>, HarnessError> {
        let connection = self.lock()?;
        let mut statement = connection
            .prepare(
                "SELECT seq, kind, payload_json, created_at
                 FROM job_events WHERE job_id = ?1 AND seq > ?2 ORDER BY seq",
            )
            .map_err(storage)?;
        let rows = statement
            .query_map(params![job_id, after], |row| {
                let payload: String = row.get(2)?;
                Ok(JobEvent {
                    job_id: job_id.to_owned(),
                    seq: row.get(0)?,
                    kind: row.get(1)?,
                    payload: serde_json::from_str(&payload).unwrap_or(Value::Null),
                    at: row.get(3)?,
                })
            })
            .map_err(storage)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(storage)
    }

    /// The running job, if any. Only one job may own the text model at a time.
    pub fn active(&self) -> Result<Option<JobRecord>, HarnessError> {
        let connection = self.lock()?;
        connection
            .query_row(
                "SELECT id, goal, deliverable, status, stopped, model, max_turns, minutes,
                        proof_path, proof_bytes, proof_digest, created_at, updated_at, finished_at
                 FROM jobs WHERE status = 'running' ORDER BY created_at LIMIT 1",
                [],
                job_row,
            )
            .optional()
            .map_err(storage)
    }

    /// Startup reconciliation: a job still marked `running` belonged to an
    /// engine that is gone. It becomes `interrupted`; nothing is resumed.
    pub fn reconcile_interrupted(&self) -> Result<Vec<String>, HarnessError> {
        let pending: Vec<String> = {
            let connection = self.lock()?;
            let mut statement = connection
                .prepare("SELECT id FROM jobs WHERE status = 'running'")
                .map_err(storage)?;
            let rows = statement.query_map([], |row| row.get(0)).map_err(storage)?;
            rows.collect::<Result<Vec<String>, _>>().map_err(storage)?
        };
        for id in &pending {
            let at = now_millis();
            let event = {
                let mut connection = self.lock()?;
                let transaction = connection.transaction().map_err(storage)?;
                let seq: i64 = transaction
                    .query_row(
                        "SELECT COALESCE(MAX(seq), 0) + 1 FROM job_events WHERE job_id = ?1",
                        params![id],
                        |row| row.get(0),
                    )
                    .map_err(storage)?;
                transaction
                    .execute(
                        "UPDATE jobs SET status = 'interrupted',
                                         stopped = 'the engine stopped before this job finished',
                                         finished_at = ?2, updated_at = ?2
                         WHERE id = ?1",
                        params![id, at],
                    )
                    .map_err(storage)?;
                let payload = json!({
                    "status": "interrupted",
                    "reason": "the engine stopped before this job finished",
                });
                transaction
                    .execute(
                        "INSERT INTO job_events (job_id, seq, kind, payload_json, created_at)
                         VALUES (?1, ?2, 'job.interrupted', ?3, ?4)",
                        params![id, seq, payload.to_string(), at],
                    )
                    .map_err(storage)?;
                transaction.commit().map_err(storage)?;
                payload
            };
            tracing::warn!(job = %id, reason = %event, "job interrupted by restart");
        }
        Ok(pending)
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>, HarnessError> {
        self.inner
            .connection
            .lock()
            .map_err(|_| HarnessError::Storage("job store lock poisoned".to_owned()))
    }
}

fn job_row(row: &Row<'_>) -> rusqlite::Result<JobRecord> {
    let raw: String = row.get(3)?;
    let proof_path: Option<String> = row.get(8)?;
    let proof_bytes: Option<i64> = row.get(9)?;
    let proof_digest: Option<String> = row.get(10)?;
    let deliverable_proof = match (proof_path, proof_bytes, proof_digest) {
        (Some(path), Some(bytes), Some(digest)) => Some(JobDeliverable {
            path,
            bytes: u64::try_from(bytes).unwrap_or(0),
            digest,
        }),
        _ => None,
    };
    Ok(JobRecord {
        id: row.get(0)?,
        goal: row.get(1)?,
        deliverable: row.get(2)?,
        status: JobStatus::parse(&raw).unwrap_or(JobStatus::Failed),
        stopped: row.get(4)?,
        model: row.get(5)?,
        max_turns: u32::try_from(row.get::<_, i64>(6)?).unwrap_or(0),
        minutes: u32::try_from(row.get::<_, i64>(7)?).unwrap_or(0),
        deliverable_proof,
        created_at: row.get(11)?,
        updated_at: row.get(12)?,
        finished_at: row.get(13)?,
    })
}

/// Folds the journal into the live todo list and turn list the UI renders.
fn fold(events: &[JobEvent]) -> (Vec<JobTodo>, Vec<JobTurn>) {
    let mut todos: Vec<JobTodo> = Vec::new();
    let mut turns: Vec<JobTurn> = Vec::new();
    for event in events {
        match event.kind.as_str() {
            "loop.todos" => {
                if let Ok(items) = serde_json::from_value::<Vec<JobTodo>>(event.payload.clone()) {
                    todos = items;
                }
            }
            "loop.turn" => {
                if let Ok(turn) = serde_json::from_value::<JobTurn>(event.payload.clone()) {
                    if !turn.todos.is_empty() {
                        todos = turn.todos.clone();
                    }
                    turns.push(turn);
                }
            }
            _ => {}
        }
    }
    (todos, turns)
}

fn storage(error: rusqlite::Error) -> HarnessError {
    HarnessError::Storage(error.to_string())
}
