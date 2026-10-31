//! Durable chat transcript state.
//!
//! Chat history is deliberately separate from the run journal: a conversation
//! is not a verified run, and no chat message can ever satisfy an acceptance
//! check. The store keeps sessions, messages, and user-gated file actions so a
//! restart never fabricates an approval.

use std::{
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{model::ChatRole, HarnessError};

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS chat_sessions (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS chat_messages (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES chat_sessions(id) ON DELETE CASCADE,
    seq INTEGER NOT NULL,
    role TEXT NOT NULL,
    content TEXT NOT NULL,
    status TEXT NOT NULL,
    meta_json TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS chat_messages_by_seq
    ON chat_messages(session_id, seq);

CREATE TABLE IF NOT EXISTS chat_actions (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES chat_sessions(id) ON DELETE CASCADE,
    message_id TEXT NOT NULL,
    tool TEXT NOT NULL,
    arguments_json TEXT NOT NULL,
    summary TEXT NOT NULL,
    status TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    decided_at INTEGER
);

CREATE INDEX IF NOT EXISTS chat_actions_by_session
    ON chat_actions(session_id, created_at);
"#;

/// One conversation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChatSession {
    pub id: String,
    pub title: String,
    pub created_at: i64,
    pub updated_at: i64,
}

/// One durable message. `status` is `complete`, `streaming`, `cancelled`, or
/// `error`; the UI never turns an unfinished message into an answer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChatMessageRecord {
    pub id: String,
    pub session_id: String,
    pub seq: i64,
    pub role: ChatRole,
    pub content: String,
    pub status: String,
    #[serde(default)]
    pub meta: Value,
    pub created_at: i64,
}

/// A workspace change that waits for explicit user consent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChatActionRecord {
    pub id: String,
    pub session_id: String,
    pub message_id: String,
    pub tool: String,
    pub arguments: Value,
    pub summary: String,
    pub status: String,
    pub created_at: i64,
    pub decided_at: Option<i64>,
}

/// Everything needed to render one conversation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChatSessionView {
    pub session: ChatSession,
    pub messages: Vec<ChatMessageRecord>,
    pub actions: Vec<ChatActionRecord>,
}

/// SQLite-backed chat history. Cheap to clone.
#[derive(Clone)]
pub struct ChatStore {
    inner: std::sync::Arc<Inner>,
}

struct Inner {
    connection: Mutex<Connection>,
    database: PathBuf,
}

impl ChatStore {
    pub fn open(database: impl AsRef<Path>) -> Result<Self, HarnessError> {
        let database = database.as_ref().to_path_buf();
        if let Some(parent) = database.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                HarnessError::Storage(format!("create {}: {error}", parent.display()))
            })?;
        }
        let connection = Connection::open(&database).map_err(storage_error)?;
        connection.execute_batch(SCHEMA).map_err(storage_error)?;
        // A turn that was streaming when the process died is not an answer.
        connection
            .execute(
                "UPDATE chat_messages SET status = 'cancelled' WHERE status = 'streaming'",
                [],
            )
            .map_err(storage_error)?;
        connection
            .execute(
                "UPDATE chat_actions SET status = 'expired', decided_at = ?1 \
                 WHERE status = 'pending'",
                params![now_millis()],
            )
            .map_err(storage_error)?;
        Ok(Self {
            inner: std::sync::Arc::new(Inner {
                connection: Mutex::new(connection),
                database,
            }),
        })
    }

    #[must_use]
    pub fn database_path(&self) -> &Path {
        &self.inner.database
    }

    pub fn list_sessions(&self, limit: u32) -> Result<Vec<ChatSession>, HarnessError> {
        let connection = self.lock()?;
        let limit = i64::from(limit.max(1));
        let mut statement = connection
            .prepare(
                "SELECT id, title, created_at, updated_at FROM chat_sessions \
                 ORDER BY updated_at DESC LIMIT ?1",
            )
            .map_err(storage_error)?;
        let rows = statement
            .query_map(params![limit], row_session)
            .map_err(storage_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(storage_error)
    }

    pub fn create_session(&self, title: Option<&str>) -> Result<ChatSession, HarnessError> {
        let now = now_millis();
        let session = ChatSession {
            id: uuid::Uuid::new_v4().simple().to_string(),
            title: title
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map_or_else(
                    || "New chat".to_owned(),
                    |value| value.chars().take(80).collect(),
                ),
            created_at: now,
            updated_at: now,
        };
        let connection = self.lock()?;
        connection
            .execute(
                "INSERT INTO chat_sessions(id, title, created_at, updated_at) VALUES (?1, ?2, ?3, ?4)",
                params![session.id, session.title, session.created_at, session.updated_at],
            )
            .map_err(storage_error)?;
        Ok(session)
    }

    /// Returns the session, or `NotFound` without creating a replacement.
    pub fn session(&self, session_id: &str) -> Result<ChatSession, HarnessError> {
        let connection = self.lock()?;
        connection
            .query_row(
                "SELECT id, title, created_at, updated_at FROM chat_sessions WHERE id = ?1",
                params![session_id],
                row_session,
            )
            .optional()
            .map_err(storage_error)?
            .ok_or_else(|| HarnessError::NotFound(format!("chat session {session_id:?}")))
    }

    pub fn rename_session(&self, session_id: &str, title: &str) -> Result<(), HarnessError> {
        let title: String = title.trim().chars().take(80).collect();
        if title.is_empty() {
            return Err(HarnessError::InvalidConfiguration(
                "a chat title cannot be empty".to_owned(),
            ));
        }
        let connection = self.lock()?;
        let changed = connection
            .execute(
                "UPDATE chat_sessions SET title = ?1, updated_at = ?2 WHERE id = ?3",
                params![title, now_millis(), session_id],
            )
            .map_err(storage_error)?;
        if changed == 0 {
            return Err(HarnessError::NotFound(format!(
                "chat session {session_id:?}"
            )));
        }
        Ok(())
    }

    pub fn delete_session(&self, session_id: &str) -> Result<(), HarnessError> {
        let connection = self.lock()?;
        let changed = connection
            .execute(
                "DELETE FROM chat_sessions WHERE id = ?1",
                params![session_id],
            )
            .map_err(storage_error)?;
        if changed == 0 {
            return Err(HarnessError::NotFound(format!(
                "chat session {session_id:?}"
            )));
        }
        Ok(())
    }

    pub fn view(&self, session_id: &str) -> Result<ChatSessionView, HarnessError> {
        let session = self.session(session_id)?;
        let messages = self.messages(session_id, 400)?;
        let actions = self.actions(session_id)?;
        Ok(ChatSessionView {
            session,
            messages,
            actions,
        })
    }

    pub fn messages(
        &self,
        session_id: &str,
        limit: u32,
    ) -> Result<Vec<ChatMessageRecord>, HarnessError> {
        let connection = self.lock()?;
        let limit = i64::from(limit.max(1));
        let mut statement = connection
            .prepare(
                "SELECT id, session_id, seq, role, content, status, meta_json, created_at \
                 FROM chat_messages WHERE session_id = ?1 ORDER BY seq DESC LIMIT ?2",
            )
            .map_err(storage_error)?;
        let rows = statement
            .query_map(params![session_id, limit], row_message)
            .map_err(storage_error)?;
        let mut messages = rows.collect::<Result<Vec<_>, _>>().map_err(storage_error)?;
        messages.reverse();
        Ok(messages)
    }

    pub fn append_message(
        &self,
        session_id: &str,
        role: ChatRole,
        content: &str,
        status: &str,
        meta: Value,
    ) -> Result<ChatMessageRecord, HarnessError> {
        let connection = self.lock()?;
        let seq: i64 = connection
            .query_row(
                "SELECT COALESCE(MAX(seq), 0) + 1 FROM chat_messages WHERE session_id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .map_err(storage_error)?;
        let message = ChatMessageRecord {
            id: uuid::Uuid::new_v4().simple().to_string(),
            session_id: session_id.to_owned(),
            seq,
            role,
            content: content.to_owned(),
            status: status.to_owned(),
            meta,
            created_at: now_millis(),
        };
        connection
            .execute(
                "INSERT INTO chat_messages(id, session_id, seq, role, content, status, meta_json, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    message.id,
                    message.session_id,
                    message.seq,
                    message.role.as_str(),
                    message.content,
                    message.status,
                    message.meta.to_string(),
                    message.created_at
                ],
            )
            .map_err(storage_error)?;
        connection
            .execute(
                "UPDATE chat_sessions SET updated_at = ?1 WHERE id = ?2",
                params![message.created_at, session_id],
            )
            .map_err(storage_error)?;
        Ok(message)
    }

    pub fn finish_message(
        &self,
        message_id: &str,
        content: &str,
        status: &str,
        meta: Value,
    ) -> Result<ChatMessageRecord, HarnessError> {
        let connection = self.lock()?;
        let changed = connection
            .execute(
                "UPDATE chat_messages SET content = ?1, status = ?2, meta_json = ?3 WHERE id = ?4",
                params![content, status, meta.to_string(), message_id],
            )
            .map_err(storage_error)?;
        if changed == 0 {
            return Err(HarnessError::NotFound(format!(
                "chat message {message_id:?}"
            )));
        }
        connection
            .query_row(
                "SELECT id, session_id, seq, role, content, status, meta_json, created_at \
                 FROM chat_messages WHERE id = ?1",
                params![message_id],
                row_message,
            )
            .map_err(storage_error)
    }

    /// The first user message becomes the conversation title. Explicit renames
    /// are never overwritten.
    pub fn maybe_retitle(&self, session_id: &str, content: &str) -> Result<(), HarnessError> {
        let title: String = content
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(48)
            .collect();
        if title.is_empty() {
            return Ok(());
        }
        let connection = self.lock()?;
        connection
            .execute(
                "UPDATE chat_sessions SET title = ?1, updated_at = ?2 \
                 WHERE id = ?3 AND title = 'New chat'",
                params![title, now_millis(), session_id],
            )
            .map_err(storage_error)?;
        Ok(())
    }

    pub fn create_action(
        &self,
        session_id: &str,
        message_id: &str,
        tool: &str,
        arguments: Value,
        summary: &str,
    ) -> Result<ChatActionRecord, HarnessError> {
        let action = ChatActionRecord {
            id: uuid::Uuid::new_v4().simple().to_string(),
            session_id: session_id.to_owned(),
            message_id: message_id.to_owned(),
            tool: tool.to_owned(),
            arguments,
            summary: summary.to_owned(),
            status: "pending".to_owned(),
            created_at: now_millis(),
            decided_at: None,
        };
        let connection = self.lock()?;
        connection
            .execute(
                "INSERT INTO chat_actions(id, session_id, message_id, tool, arguments_json, summary, status, created_at, decided_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL)",
                params![
                    action.id,
                    action.session_id,
                    action.message_id,
                    action.tool,
                    action.arguments.to_string(),
                    action.summary,
                    action.status,
                    action.created_at
                ],
            )
            .map_err(storage_error)?;
        Ok(action)
    }

    pub fn resolve_action(
        &self,
        action_id: &str,
        status: &str,
    ) -> Result<ChatActionRecord, HarnessError> {
        if !matches!(
            status,
            "approved" | "denied" | "applied" | "failed" | "expired"
        ) {
            return Err(HarnessError::InvalidConfiguration(format!(
                "invalid chat action status {status:?}"
            )));
        }
        let connection = self.lock()?;
        // An approval decision moves a pending action to approved/denied.
        // The execution outcome then moves an approved action to
        // applied/failed. The second transition must not erase the moment the
        // person decided, so it leaves `decided_at` untouched.
        let changed = if matches!(status, "applied" | "failed") {
            connection
                .execute(
                    "UPDATE chat_actions SET status = ?1 \
                     WHERE id = ?2 AND status = 'approved'",
                    params![status, action_id],
                )
                .map_err(storage_error)?
        } else {
            connection
                .execute(
                    "UPDATE chat_actions SET status = ?1, decided_at = ?2 \
                     WHERE id = ?3 AND status = 'pending'",
                    params![status, now_millis(), action_id],
                )
                .map_err(storage_error)?
        };
        if changed == 0 {
            return Err(HarnessError::NotFound(format!(
                "action {action_id:?} cannot move to {status:?}"
            )));
        }
        connection
            .query_row(
                "SELECT id, session_id, message_id, tool, arguments_json, summary, status, created_at, decided_at \
                 FROM chat_actions WHERE id = ?1",
                params![action_id],
                row_action,
            )
            .map_err(storage_error)
    }

    pub fn actions(&self, session_id: &str) -> Result<Vec<ChatActionRecord>, HarnessError> {
        let connection = self.lock()?;
        let mut statement = connection
            .prepare(
                "SELECT id, session_id, message_id, tool, arguments_json, summary, status, created_at, decided_at \
                 FROM chat_actions WHERE session_id = ?1 ORDER BY created_at ASC",
            )
            .map_err(storage_error)?;
        let rows = statement
            .query_map(params![session_id], row_action)
            .map_err(storage_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(storage_error)
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, HarnessError> {
        self.inner
            .connection
            .lock()
            .map_err(|_| HarnessError::Storage("chat store lock is poisoned".to_owned()))
    }
}

fn row_session(row: &Row<'_>) -> rusqlite::Result<ChatSession> {
    Ok(ChatSession {
        id: row.get(0)?,
        title: row.get(1)?,
        created_at: row.get(2)?,
        updated_at: row.get(3)?,
    })
}

fn row_message(row: &Row<'_>) -> rusqlite::Result<ChatMessageRecord> {
    let raw_role: String = row.get(3)?;
    let role = match raw_role.as_str() {
        "system" => ChatRole::System,
        "user" => ChatRole::User,
        "assistant" => ChatRole::Assistant,
        "tool" => ChatRole::Tool,
        _ => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                3,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("unknown chat role {raw_role:?}"),
                )),
            ))
        }
    };
    let raw_meta: String = row.get(6)?;
    Ok(ChatMessageRecord {
        id: row.get(0)?,
        session_id: row.get(1)?,
        seq: row.get(2)?,
        role,
        content: row.get(4)?,
        status: row.get(5)?,
        meta: serde_json::from_str(&raw_meta).unwrap_or_else(|_| json!({})),
        created_at: row.get(7)?,
    })
}

fn row_action(row: &Row<'_>) -> rusqlite::Result<ChatActionRecord> {
    let raw_args: String = row.get(4)?;
    Ok(ChatActionRecord {
        id: row.get(0)?,
        session_id: row.get(1)?,
        message_id: row.get(2)?,
        tool: row.get(3)?,
        arguments: serde_json::from_str(&raw_args).unwrap_or_else(|_| json!({})),
        summary: row.get(5)?,
        status: row.get(6)?,
        created_at: row.get(7)?,
        decided_at: row.get(8)?,
    })
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
        .unwrap_or_default()
}

fn storage_error(error: rusqlite::Error) -> HarnessError {
    HarnessError::Storage(format!("chat database: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (ChatStore, tempfile::TempDir) {
        let directory = tempfile::tempdir().expect("temp dir");
        let store = ChatStore::open(directory.path().join("chat.db")).expect("chat store");
        (store, directory)
    }

    #[test]
    fn sessions_titles_and_messages_round_trip() {
        let (store, _directory) = store();
        let session = store.create_session(None).expect("create session");
        assert_eq!(session.title, "New chat");

        let message = store
            .append_message(
                &session.id,
                ChatRole::User,
                "Plan my week",
                "complete",
                json!({}),
            )
            .expect("append message");
        assert_eq!(message.seq, 1);
        store
            .maybe_retitle(&session.id, "Plan my week with three priorities")
            .expect("retitle");

        let view = store.view(&session.id).expect("view");
        assert_eq!(view.session.title, "Plan my week with three priorities");
        assert_eq!(view.messages.len(), 1);
        assert_eq!(view.messages[0].content, "Plan my week");
    }

    #[test]
    fn actions_move_from_pending_through_approval_to_the_outcome() {
        let (store, _directory) = store();
        let session = store.create_session(Some("Files")).expect("session");
        let action = store
            .create_action(
                &session.id,
                "message",
                "write_file",
                json!({"path": "notes.md", "content": "hello"}),
                "Write notes.md",
            )
            .expect("action");
        assert_eq!(action.status, "pending");

        let approved = store
            .resolve_action(&action.id, "approved")
            .expect("approve");
        assert_eq!(approved.status, "approved");
        let decided_at = approved.decided_at;
        assert!(decided_at.is_some());

        let applied = store
            .resolve_action(&action.id, "applied")
            .expect("record the execution outcome");
        assert_eq!(applied.status, "applied");
        assert_eq!(applied.decided_at, decided_at, "approval time is kept");
        assert!(store.resolve_action(&action.id, "denied").is_err());
    }

    #[test]
    fn an_unapproved_action_cannot_be_applied() {
        let (store, _directory) = store();
        let session = store.create_session(None).expect("session");
        let action = store
            .create_action(&session.id, "message", "write_file", json!({}), "Write")
            .expect("action");
        assert!(store.resolve_action(&action.id, "applied").is_err());
        assert_eq!(
            store.actions(&session.id).expect("actions")[0].status,
            "pending"
        );
    }

    #[test]
    fn reopening_marks_unfinished_messages_and_actions() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("chat.db");
        {
            let store = ChatStore::open(&path).expect("first open");
            let session = store.create_session(None).expect("session");
            store
                .append_message(
                    &session.id,
                    ChatRole::Assistant,
                    "partial",
                    "streaming",
                    json!({}),
                )
                .expect("message");
            store
                .create_action(&session.id, "message", "write_file", json!({}), "Write")
                .expect("action");
        }
        let store = ChatStore::open(&path).expect("reopen");
        let view = store
            .view(&store.list_sessions(10).expect("sessions")[0].id)
            .expect("view");
        assert_eq!(view.messages[0].status, "cancelled");
        assert_eq!(view.actions[0].status, "expired");
    }
}
