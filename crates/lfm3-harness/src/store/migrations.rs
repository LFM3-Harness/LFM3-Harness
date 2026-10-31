//! Append-only schema migrations keyed by SQLite `user_version`.

use rusqlite::{params, Connection};

use crate::{store::now_millis, HarnessError};

const MIGRATIONS: &[&str] = &[SCHEMA_V1];

const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS schema_migrations (
    version INTEGER PRIMARY KEY,
    applied_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS engine_instances (
    id TEXT PRIMARY KEY,
    started_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS acceptance_specs (
    hash TEXT PRIMARY KEY,
    spec_json TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS runs (
    id TEXT PRIMARY KEY,
    mission TEXT NOT NULL,
    mission_version INTEGER NOT NULL,
    acceptance_hash TEXT NOT NULL REFERENCES acceptance_specs(hash),
    goal TEXT NOT NULL,
    status TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    engine_instance_id TEXT NOT NULL REFERENCES engine_instances(id),
    parent_run_id TEXT REFERENCES runs(id)
);

CREATE INDEX IF NOT EXISTS runs_by_engine ON runs(engine_instance_id, status);

CREATE TABLE IF NOT EXISTS steps (
    run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    step_index INTEGER NOT NULL,
    title TEXT NOT NULL,
    criterion TEXT NOT NULL,
    status TEXT NOT NULL,
    PRIMARY KEY (run_id, step_index)
);

CREATE TABLE IF NOT EXISTS attempts (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    step_index INTEGER NOT NULL,
    strategy INTEGER NOT NULL,
    status TEXT NOT NULL,
    action_json TEXT NOT NULL,
    model TEXT NOT NULL,
    deadline_at INTEGER,
    started_at INTEGER NOT NULL,
    finished_at INTEGER,
    error TEXT,
    FOREIGN KEY (run_id, step_index) REFERENCES steps(run_id, step_index)
);

CREATE INDEX IF NOT EXISTS attempts_by_run ON attempts(run_id, step_index, started_at);

CREATE TABLE IF NOT EXISTS tool_results (
    attempt_id TEXT PRIMARY KEY REFERENCES attempts(id) ON DELETE CASCADE,
    outcome TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS check_results (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    attempt_id TEXT NOT NULL REFERENCES attempts(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    passed INTEGER NOT NULL,
    detail TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS strategies (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    number INTEGER NOT NULL,
    acceptance_hash TEXT NOT NULL,
    plan_json TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE (run_id, number)
);

CREATE TABLE IF NOT EXISTS events (
    run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    seq INTEGER NOT NULL,
    kind TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (run_id, seq)
);

CREATE TABLE IF NOT EXISTS artifacts (
    id TEXT PRIMARY KEY,
    sha256 TEXT NOT NULL UNIQUE,
    size INTEGER NOT NULL,
    media_type TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS approvals (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    decision TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS model_leases (
    id TEXT PRIMARY KEY,
    model TEXT NOT NULL,
    endpoint TEXT NOT NULL,
    acquired_at INTEGER NOT NULL,
    released_at INTEGER
);

CREATE TABLE IF NOT EXISTS run_links (
    parent_run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    child_run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    reason TEXT NOT NULL,
    PRIMARY KEY (parent_run_id, child_run_id)
);
"#;

/// Applies pending migrations. Re-running with the same database is a no-op.
pub fn apply(connection: &mut Connection) -> Result<(), HarnessError> {
    connection
        .execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;",
        )
        .map_err(storage)?;

    let current: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(storage)?;
    for (index, sql) in MIGRATIONS.iter().enumerate() {
        let version = i64::try_from(index + 1)
            .map_err(|_| HarnessError::Storage("migration count overflow".to_owned()))?;
        if version <= current {
            continue;
        }
        let transaction = connection.transaction().map_err(storage)?;
        transaction.execute_batch(sql).map_err(storage)?;
        transaction
            .execute(
                "INSERT INTO schema_migrations(version, applied_at) VALUES (?1, ?2)",
                params![version, now_millis()],
            )
            .map_err(storage)?;
        transaction
            .execute_batch(&format!("PRAGMA user_version = {version};"))
            .map_err(storage)?;
        transaction.commit().map_err(storage)?;
    }
    Ok(())
}

#[must_use]
pub fn current_version(connection: &Connection) -> i64 {
    connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap_or(0)
}

fn storage(error: rusqlite::Error) -> HarnessError {
    HarnessError::Storage(error.to_string())
}
