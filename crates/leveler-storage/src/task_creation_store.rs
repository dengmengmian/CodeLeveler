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

    /// Atomically create the aggregate and its existing command receipt, or
    /// return the aggregate already bound to this logical creation identity.
    #[allow(clippy::too_many_arguments)]
    async fn create_task_identified(
        &self,
        record: &SessionRecord,
        mode: &str,
        sandbox: bool,
        kind: &str,
        request_id: &leveler_core::CommandId,
        fingerprint: &str,
    ) -> Result<(TaskId, bool), StorageError>;
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
        Ok(self
            .create_task_once(record, mode, sandbox, kind, None)
            .await?
            .0)
    }

    async fn create_task_identified(
        &self,
        record: &SessionRecord,
        mode: &str,
        sandbox: bool,
        kind: &str,
        request_id: &leveler_core::CommandId,
        fingerprint: &str,
    ) -> Result<(TaskId, bool), StorageError> {
        self.create_task_once(record, mode, sandbox, kind, Some((request_id, fingerprint)))
            .await
    }
}

impl Database {
    async fn create_task_once(
        &self,
        record: &SessionRecord,
        mode: &str,
        sandbox: bool,
        kind: &str,
        identity: Option<(&leveler_core::CommandId, &str)>,
    ) -> Result<(TaskId, bool), StorageError> {
        let mut tx = self.pool().begin_with("BEGIN IMMEDIATE").await?;
        if let Some((request_id, fingerprint)) = identity {
            if request_id.as_str().is_empty() {
                return Err(StorageError::InvalidData("empty creation identity".into()));
            }
            let prior: Option<(String, String, String)> = sqlx::query_as(
                "SELECT session_id, command_fingerprint, status FROM command_receipts WHERE command_id = ?1")
                .bind(request_id.as_str()).fetch_optional(&mut *tx).await?;
            if let Some((session_id, stored, status)) = prior {
                if stored != fingerprint || status != "completed" {
                    return Err(StorageError::InvalidData(
                        "creation identity conflicts with an existing command or request".into(),
                    ));
                }
                tx.commit().await?;
                return Ok((TaskId::new(session_id), false));
            }
        }
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
        .bind("single")
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
        if let Some((request_id, fingerprint)) = identity {
            sqlx::query(
                "INSERT INTO command_receipts \
                (command_id, session_id, command_fingerprint, issued_at, admitted_at, status) \
                VALUES (?1, ?2, ?3, ?4, ?4, 'completed')",
            )
            .bind(request_id.as_str())
            .bind(&record.id)
            .bind(fingerprint)
            .bind(&record.created_at)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok((TaskId::new(record.id.clone()), true))
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
        assert_eq!(stored.work_profile, "single");
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
    #[tokio::test]
    async fn identified_creation_replays_conflicts_and_preserves_profile() {
        let db = Database::connect_in_memory().await.unwrap();
        let record = SessionRecord::new("/repo", "goal", "mock/m", leveler_core::now());
        let id = leveler_core::CommandId::new("logical-create");
        let (task, inserted) = db
            .create_task_identified(&record, "read_only", true, "direct", &id, "create:first")
            .await
            .unwrap();
        assert!(inserted);
        sqlx::query(
            "UPDATE sessions SET mode = 'full_access', collaboration = 'plan' WHERE id = ?1",
        )
        .bind(&record.id)
        .execute(db.pool())
        .await
        .unwrap();
        let retry = SessionRecord::new("/repo", "goal", "mock/newdefault", leveler_core::now());
        assert_eq!(
            db.create_task_identified(&retry, "assisted", false, "direct", &id, "create:first")
                .await
                .unwrap(),
            (task, false)
        );
        assert!(
            db.create_task_identified(&retry, "assisted", false, "direct", &id, "create:changed")
                .await
                .is_err()
        );
        let stored = SessionRepository::new(&db)
            .get(&SessionId::new(&record.id))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.collaboration, "plan");
        assert_eq!(
            db.execution(&SessionId::new(&record.id))
                .await
                .unwrap()
                .unwrap()
                .0,
            "full_access"
        );
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM sessions")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(count.0, 1);
        let other = leveler_core::CommandId::new("new-logical-create");
        assert!(
            db.create_task_identified(&retry, "assisted", false, "direct", &other, "create:first")
                .await
                .unwrap()
                .1
        );
    }

    #[tokio::test]
    async fn identified_receipt_failure_rolls_back_entire_aggregate() {
        let db = Database::connect_in_memory().await.unwrap();
        sqlx::query("CREATE TRIGGER reject_create_receipt BEFORE INSERT ON command_receipts BEGIN SELECT RAISE(ABORT, 'injected receipt failure'); END").execute(db.pool()).await.unwrap();
        let record = SessionRecord::new("/repo", "goal", "mock/m", leveler_core::now());
        assert!(
            db.create_task_identified(
                &record,
                "assisted",
                false,
                "direct",
                &leveler_core::CommandId::new("create"),
                "create:fp"
            )
            .await
            .is_err()
        );
        for table in ["sessions", "tasks", "command_receipts"] {
            let count: (i64,) = sqlx::query_as(&format!("SELECT COUNT(*) FROM {table}"))
                .fetch_one(db.pool())
                .await
                .unwrap();
            assert_eq!(count.0, 0, "{table} must roll back");
        }
    }

    #[tokio::test]
    async fn concurrent_identified_creation_has_one_winner() {
        let temp = tempfile::tempdir().unwrap();
        let db = Database::connect(&temp.path().join("concurrent.db"))
            .await
            .unwrap();
        let one = SessionRecord::new("/repo", "goal", "mock/m", leveler_core::now());
        let two = SessionRecord::new("/repo", "goal", "mock/m", leveler_core::now());
        let id = leveler_core::CommandId::new("create-concurrent");
        let (left, right) = tokio::join!(
            db.create_task_identified(&one, "assisted", false, "direct", &id, "create:fp"),
            db.create_task_identified(&two, "assisted", false, "direct", &id, "create:fp")
        );
        let (left, right) = (left.unwrap(), right.unwrap());
        assert_eq!(left.0, right.0);
        assert_ne!(left.1, right.1);
        for table in ["sessions", "tasks", "command_receipts"] {
            let count: (i64,) = sqlx::query_as(&format!("SELECT COUNT(*) FROM {table}"))
                .fetch_one(db.pool())
                .await
                .unwrap();
            assert_eq!(count.0, 1);
        }
    }

    #[tokio::test]
    async fn creation_identity_cannot_reuse_regular_command_receipt() {
        let db = Database::connect_in_memory().await.unwrap();
        let first = SessionRecord::new("/repo", "first", "mock/m", leveler_core::now());
        db.create_task(&first, "assisted", false, "direct")
            .await
            .unwrap();
        sqlx::query("INSERT INTO command_receipts (command_id, session_id, command_fingerprint, issued_at, admitted_at, status) VALUES ('shared-id', ?1, 'regular-command', ?2, ?2, 'completed')").bind(&first.id).bind(&first.created_at).execute(db.pool()).await.unwrap();
        let second = SessionRecord::new("/repo", "second", "mock/m", leveler_core::now());
        assert!(
            db.create_task_identified(
                &second,
                "assisted",
                false,
                "direct",
                &leveler_core::CommandId::new("shared-id"),
                "create:fp"
            )
            .await
            .is_err()
        );
        assert!(
            SessionRepository::new(&db)
                .get(&SessionId::new(second.id))
                .await
                .unwrap()
                .is_none()
        );
    }
}
