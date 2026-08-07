//! The `TaskStore` port: durable task identity and the task↔session
//! association.
//!
//! Same pattern as [`crate::EventStore`]: the trait is the narrow seam the
//! engine depends on, storage owns the port and its SQLite adapter, and a
//! [`MemoryTaskStore`] exercises the identical contract without SQLite. The
//! port is deliberately small — identity and association only. Task lifecycle
//! stays on the session row (`sessions.status/state/outcome`), which remains
//! the single lifecycle projection while task and session are 1:1.

use std::sync::Mutex;

use async_trait::async_trait;

use leveler_core::{SessionId, TaskId, Timestamp};

use crate::{Database, StorageError};

/// Identity/association access to durable tasks. One task has exactly one
/// primary session today; the trait names the association direction so a
/// future multi-session task extends rather than rewrites it.
#[async_trait]
pub trait TaskStore: Send + Sync {
    /// Idempotently ensure a task exists for `session_id` and return its id.
    /// Sessions created before the task table existed (or by a path that does
    /// not go through the engine) get their task row here, deterministically:
    /// the first ensure wins and every later call returns the same id.
    async fn ensure_for_session(
        &self,
        session_id: &SessionId,
        now: Timestamp,
    ) -> Result<TaskId, StorageError>;

    /// The task owning `session_id`, if one has been recorded.
    async fn task_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<TaskId>, StorageError>;

    /// The primary session of `task_id`, if the task exists.
    async fn session_for_task(&self, task_id: &TaskId) -> Result<Option<SessionId>, StorageError>;
}

/// The production SQLite adapter over the `tasks` table (migration 0016).
#[async_trait]
impl TaskStore for Database {
    async fn ensure_for_session(
        &self,
        session_id: &SessionId,
        now: Timestamp,
    ) -> Result<TaskId, StorageError> {
        // Mirror the migration's deterministic backfill: a task minted for an
        // existing session reuses the session's id string. Idempotent under
        // concurrency via the UNIQUE(session_id) constraint — the insert is a
        // no-op when a row already exists, and the read below returns whoever
        // won.
        sqlx::query(
            "INSERT INTO tasks (id, session_id, created_at) VALUES (?1, ?1, ?2) \
             ON CONFLICT(session_id) DO NOTHING",
        )
        .bind(session_id.as_str())
        .bind(now.to_rfc3339())
        .execute(self.pool())
        .await?;
        let id: Option<String> = sqlx::query_scalar("SELECT id FROM tasks WHERE session_id = ?1")
            .bind(session_id.as_str())
            .fetch_optional(self.pool())
            .await?;
        id.map(TaskId::new).ok_or_else(|| {
            StorageError::InvalidData(format!(
                "task row for session {} missing after ensure",
                session_id.as_str()
            ))
        })
    }

    async fn task_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<TaskId>, StorageError> {
        let id: Option<String> = sqlx::query_scalar("SELECT id FROM tasks WHERE session_id = ?1")
            .bind(session_id.as_str())
            .fetch_optional(self.pool())
            .await?;
        Ok(id.map(TaskId::new))
    }

    async fn session_for_task(&self, task_id: &TaskId) -> Result<Option<SessionId>, StorageError> {
        let id: Option<String> = sqlx::query_scalar("SELECT session_id FROM tasks WHERE id = ?1")
            .bind(task_id.as_str())
            .fetch_optional(self.pool())
            .await?;
        Ok(id.map(SessionId::new))
    }
}

/// An in-memory [`TaskStore`] for tests and ephemeral runs, honoring the same
/// contract as the SQLite adapter (deterministic ids, idempotent ensure).
#[derive(Default)]
pub struct MemoryTaskStore {
    /// `(task_id, session_id)` pairs; session ids are unique.
    rows: Mutex<Vec<(String, String)>>,
}

impl MemoryTaskStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl TaskStore for MemoryTaskStore {
    async fn ensure_for_session(
        &self,
        session_id: &SessionId,
        _now: Timestamp,
    ) -> Result<TaskId, StorageError> {
        let mut rows = self.rows.lock().unwrap();
        if let Some((task, _)) = rows.iter().find(|(_, s)| s == session_id.as_str()) {
            return Ok(TaskId::new(task.clone()));
        }
        rows.push((
            session_id.as_str().to_string(),
            session_id.as_str().to_string(),
        ));
        Ok(TaskId::new(session_id.as_str()))
    }

    async fn task_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<TaskId>, StorageError> {
        let rows = self.rows.lock().unwrap();
        Ok(rows
            .iter()
            .find(|(_, s)| s == session_id.as_str())
            .map(|(t, _)| TaskId::new(t.clone())))
    }

    async fn session_for_task(&self, task_id: &TaskId) -> Result<Option<SessionId>, StorageError> {
        let rows = self.rows.lock().unwrap();
        Ok(rows
            .iter()
            .find(|(t, _)| t == task_id.as_str())
            .map(|(_, s)| SessionId::new(s.clone())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SessionRecord, SessionRepository};

    /// One contract exercised against both implementations: ensure is
    /// idempotent and deterministic, and the association reads back in both
    /// directions.
    async fn assert_task_store_contract(store: &dyn TaskStore, session: &SessionId) {
        assert_eq!(store.task_for_session(session).await.unwrap(), None);

        let task = store
            .ensure_for_session(session, leveler_core::now())
            .await
            .unwrap();
        let again = store
            .ensure_for_session(session, leveler_core::now())
            .await
            .unwrap();
        assert_eq!(task, again, "ensure must be idempotent");

        assert_eq!(
            store.task_for_session(session).await.unwrap(),
            Some(task.clone())
        );
        assert_eq!(
            store.session_for_task(&task).await.unwrap(),
            Some(session.clone())
        );
        assert_eq!(
            store
                .session_for_task(&TaskId::new("missing"))
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn memory_store_honors_the_contract() {
        let store = MemoryTaskStore::new();
        assert_task_store_contract(&store, &SessionId::generate()).await;
    }

    #[tokio::test]
    async fn sqlite_store_honors_the_contract() {
        let db = Database::connect_in_memory().await.unwrap();
        let record = SessionRecord::new("/repo", "goal", "mock/m", leveler_core::now());
        SessionRepository::new(&db).create(&record).await.unwrap();
        assert_task_store_contract(&db, &SessionId::new(record.id)).await;
    }

    #[tokio::test]
    async fn sqlite_task_rows_cascade_with_their_session() {
        let db = Database::connect_in_memory().await.unwrap();
        let record = SessionRecord::new("/repo", "goal", "mock/m", leveler_core::now());
        SessionRepository::new(&db).create(&record).await.unwrap();
        let session = SessionId::new(record.id);
        let task = db
            .ensure_for_session(&session, leveler_core::now())
            .await
            .unwrap();

        assert!(SessionRepository::new(&db).delete(&session).await.unwrap());
        assert_eq!(
            db.session_for_task(&task).await.unwrap(),
            None,
            "deleting a session must not leave an orphan task row"
        );
    }
}
