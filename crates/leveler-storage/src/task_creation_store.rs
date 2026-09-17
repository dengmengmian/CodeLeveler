//! Atomic creation of the session/task aggregate.
//!
//! A runtime task does not exist as two independently useful rows: the
//! session carries its durable configuration and the task carries ownership.
//! This port keeps their initial write in one database transaction so a crash
//! cannot expose a session without its task association.

use async_trait::async_trait;

use leveler_core::TaskId;

use crate::{Database, SessionRecord, StorageError};

/// Atomic persistence boundary for a newly created runtime task.
#[async_trait]
pub trait TaskCreationStore: Send + Sync {
    /// Insert the configured session and its task association as one fact.
    async fn create_task(
        &self,
        record: &SessionRecord,
        mode: &str,
        sandbox: bool,
        kind: &str,
    ) -> Result<TaskId, StorageError>;
}

#[async_trait]
impl TaskCreationStore for Database {
    async fn create_task(
        &self,
        record: &SessionRecord,
        mode: &str,
        sandbox: bool,
        kind: &str,
    ) -> Result<TaskId, StorageError> {
        let mut tx = self.pool().begin().await?;
        sqlx::query(
            "INSERT INTO sessions \
             (id, repository, goal, status, model, state, created_at, updated_at, \
              collaboration, work_profile, mode, sandbox, kind) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        )
        .bind(&record.id)
        .bind(&record.repository)
        .bind(&record.goal)
        .bind(record.status.as_str())
        .bind(&record.model)
        .bind(record.state.as_str())
        .bind(&record.created_at)
        .bind(&record.updated_at)
        .bind(&record.collaboration)
        .bind(&record.work_profile)
        .bind(mode)
        .bind(sandbox)
        .bind(kind)
        .execute(&mut *tx)
        .await?;

        sqlx::query("INSERT INTO tasks (id, session_id, created_at) VALUES (?1, ?1, ?2)")
            .bind(&record.id)
            .bind(&record.created_at)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(TaskId::new(record.id.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SessionRepository, SessionStore, TaskStore};
    use leveler_core::SessionId;

    #[tokio::test]
    async fn configured_session_and_task_commit_together() {
        let db = Database::connect_in_memory().await.unwrap();
        let record = SessionRecord::new("/repo", "goal", "mock/m", leveler_core::now())
            .with_axes("plan", "delivery");
        let task = db
            .create_task(&record, "read_only", true, "direct")
            .await
            .unwrap();
        let session = SessionId::new(record.id);

        assert_eq!(db.task_for_session(&session).await.unwrap(), Some(task));
        assert_eq!(
            db.execution(&session).await.unwrap(),
            Some(("read_only".into(), true, "direct".into(), None))
        );
        let stored = SessionRepository::new(&db)
            .get(&session)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.collaboration, "plan");
        assert_eq!(stored.work_profile, "delivery");
    }

    #[tokio::test]
    async fn task_insert_failure_rolls_back_the_session() {
        let db = Database::connect_in_memory().await.unwrap();
        sqlx::query(
            "CREATE TRIGGER reject_task_creation BEFORE INSERT ON tasks \
             BEGIN SELECT RAISE(ABORT, 'injected task creation failure'); END",
        )
        .execute(db.pool())
        .await
        .unwrap();
        let record = SessionRecord::new("/repo", "goal", "mock/m", leveler_core::now());
        let session = SessionId::new(record.id.clone());

        assert!(
            db.create_task(&record, "assisted", false, "direct")
                .await
                .is_err()
        );
        assert_eq!(
            SessionRepository::new(&db).get(&session).await.unwrap(),
            None,
            "a failed task association must expose no session row"
        );
        assert_eq!(db.task_for_session(&session).await.unwrap(), None);
    }
}
