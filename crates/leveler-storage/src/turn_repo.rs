//! Turn repository: one row per engine turn (user goal, chat message,
//! orchestrated node, repair attempt). Turns own their transcript messages
//! (`session_messages.turn_id`) and give resume its boundaries.

use serde::{Deserialize, Serialize};

use leveler_core::{SessionId, Timestamp, TurnId};

use crate::database::{Database, StorageError};

/// A persisted turn row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct TurnRecord {
    pub id: String,
    pub session_id: String,
    pub ordinal: i64,
    /// user | chat | node | repair
    pub kind: String,
    /// Kind-specific JSON (e.g. `{"node_id":…,"attempt":…}`).
    pub payload: Option<String>,
    /// running | completed | failed | interrupted
    pub status: String,
    pub created_at: String,
    pub finished_at: Option<String>,
}

/// Read/write access to the `turns` table.
pub struct TurnRepository<'a> {
    db: &'a Database,
}

impl<'a> TurnRepository<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    /// Start a new turn: assigns the next ordinal for the session, inserts the
    /// row with status `running`, and returns it.
    pub async fn start(
        &self,
        session_id: &SessionId,
        kind: &str,
        payload: Option<&str>,
        now: Timestamp,
    ) -> Result<TurnRecord, StorageError> {
        let id = TurnId::generate().into_inner();
        let payload = payload.map(leveler_core::redact_secrets);
        // The ordinal is assigned inside the INSERT so concurrent starts on
        // one connection pool cannot race a read-then-write.
        sqlx::query(
            "INSERT INTO turns (id, session_id, ordinal, kind, payload, status, created_at) \
             SELECT ?1, ?2, COALESCE(MAX(ordinal), 0) + 1, ?3, ?4, 'running', ?5 \
             FROM turns WHERE session_id = ?2",
        )
        .bind(&id)
        .bind(session_id.as_str())
        .bind(kind)
        .bind(payload.as_deref())
        .bind(now.to_rfc3339())
        .execute(self.db.pool())
        .await?;
        let row = sqlx::query_as::<_, TurnRecord>(
            "SELECT id, session_id, ordinal, kind, payload, status, created_at, finished_at \
             FROM turns WHERE id = ?1",
        )
        .bind(&id)
        .fetch_one(self.db.pool())
        .await?;
        Ok(row)
    }

    /// Mark a turn terminal (completed | failed | interrupted).
    pub async fn finish(
        &self,
        id: &TurnId,
        status: &str,
        now: Timestamp,
    ) -> Result<(), StorageError> {
        sqlx::query("UPDATE turns SET status = ?2, finished_at = ?3 WHERE id = ?1")
            .bind(id.as_str())
            .bind(status)
            .bind(now.to_rfc3339())
            .execute(self.db.pool())
            .await?;
        Ok(())
    }

    /// Mark every still-`running` turn for a session as `interrupted`.
    ///
    /// Used as a reaper after process kill / unclean TUI exit left zombie
    /// turns that never got a `finished_at` (cancel path already finishes
    /// turns when the executor is allowed to wind down).
    pub async fn interrupt_running_for_session(
        &self,
        session_id: &SessionId,
        now: Timestamp,
    ) -> Result<u64, StorageError> {
        let result = sqlx::query(
            "UPDATE turns SET status = 'interrupted', finished_at = ?2 \
             WHERE session_id = ?1 AND status = 'running' AND finished_at IS NULL",
        )
        .bind(session_id.as_str())
        .bind(now.to_rfc3339())
        .execute(self.db.pool())
        .await?;
        Ok(result.rows_affected())
    }

    /// Mark every still-`running` turn in the database as `interrupted`.
    ///
    /// Single-user local CLI: safe reaper when the process is quitting or a
    /// new interactive session is about to start after a previous kill.
    pub async fn interrupt_all_running(&self, now: Timestamp) -> Result<u64, StorageError> {
        let result = sqlx::query(
            "UPDATE turns SET status = 'interrupted', finished_at = ?1 \
             WHERE status = 'running' AND finished_at IS NULL",
        )
        .bind(now.to_rfc3339())
        .execute(self.db.pool())
        .await?;
        Ok(result.rows_affected())
    }

    /// All turns of a session, in ordinal order.
    pub async fn list(&self, session_id: &SessionId) -> Result<Vec<TurnRecord>, StorageError> {
        let rows = sqlx::query_as::<_, TurnRecord>(
            "SELECT id, session_id, ordinal, kind, payload, status, created_at, finished_at \
             FROM turns WHERE session_id = ?1 ORDER BY ordinal",
        )
        .bind(session_id.as_str())
        .fetch_all(self.db.pool())
        .await?;
        Ok(rows)
    }

    /// Running turns, optionally restricted to one session. Used by the engine
    /// reaper so each interrupted row can receive a canonical terminal event.
    pub async fn list_running(
        &self,
        session_id: Option<&SessionId>,
    ) -> Result<Vec<TurnRecord>, StorageError> {
        let rows = match session_id {
            Some(session_id) => sqlx::query_as::<_, TurnRecord>(
                "SELECT id, session_id, ordinal, kind, payload, status, created_at, finished_at \
                 FROM turns WHERE session_id = ?1 AND status = 'running' AND finished_at IS NULL \
                 ORDER BY ordinal",
            )
            .bind(session_id.as_str())
            .fetch_all(self.db.pool())
            .await?,
            None => sqlx::query_as::<_, TurnRecord>(
                "SELECT id, session_id, ordinal, kind, payload, status, created_at, finished_at \
                 FROM turns WHERE status = 'running' AND finished_at IS NULL \
                 ORDER BY session_id, ordinal",
            )
            .fetch_all(self.db.pool())
            .await?,
        };
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_repo::{SessionRecord, SessionRepository};

    async fn db_with_session() -> (Database, SessionId) {
        let db = Database::connect_in_memory().await.unwrap();
        let record = SessionRecord::new("/repo", "goal", "mock/m", leveler_core::now());
        SessionRepository::new(&db).create(&record).await.unwrap();
        let id = SessionId::new(record.id);
        (db, id)
    }

    #[tokio::test]
    async fn start_assigns_sequential_ordinals_per_session() {
        let (db, session) = db_with_session().await;
        let repo = TurnRepository::new(&db);

        let t1 = repo
            .start(&session, "user", None, leveler_core::now())
            .await
            .unwrap();
        let t2 = repo
            .start(
                &session,
                "node",
                Some(r#"{"node_id":"n1"}"#),
                leveler_core::now(),
            )
            .await
            .unwrap();

        assert_eq!(t1.ordinal, 1);
        assert_eq!(t2.ordinal, 2);
        assert_eq!(t1.status, "running");
        assert_eq!(t2.payload.as_deref(), Some(r#"{"node_id":"n1"}"#));

        let listed = repo.list(&session).await.unwrap();
        assert_eq!(
            listed.iter().map(|t| t.ordinal).collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    #[tokio::test]
    async fn finish_marks_the_turn_terminal() {
        let (db, session) = db_with_session().await;
        let repo = TurnRepository::new(&db);
        let t = repo
            .start(&session, "user", None, leveler_core::now())
            .await
            .unwrap();

        let finished_at = leveler_core::now();
        repo.finish(&TurnId::new(t.id.clone()), "completed", finished_at)
            .await
            .unwrap();

        let listed = repo.list(&session).await.unwrap();
        assert_eq!(listed[0].status, "completed");
        assert_eq!(
            listed[0].finished_at.as_deref(),
            Some(finished_at.to_rfc3339().as_str())
        );
    }

    #[tokio::test]
    async fn ordinals_are_scoped_per_session() {
        let (db, a) = db_with_session().await;
        let record = SessionRecord::new("/repo", "other", "mock/m", leveler_core::now());
        SessionRepository::new(&db).create(&record).await.unwrap();
        let b = SessionId::new(record.id);

        let repo = TurnRepository::new(&db);
        repo.start(&a, "user", None, leveler_core::now())
            .await
            .unwrap();
        let tb = repo
            .start(&b, "user", None, leveler_core::now())
            .await
            .unwrap();
        assert_eq!(tb.ordinal, 1, "ordinals must not leak across sessions");
    }

    #[tokio::test]
    async fn interrupt_running_for_session_only_touches_that_session() {
        let (db, a) = db_with_session().await;
        let record = SessionRecord::new("/repo", "other", "mock/m", leveler_core::now());
        SessionRepository::new(&db).create(&record).await.unwrap();
        let b = SessionId::new(record.id);
        let repo = TurnRepository::new(&db);

        let ta = repo
            .start(&a, "chat", None, leveler_core::now())
            .await
            .unwrap();
        let tb = repo
            .start(&b, "chat", None, leveler_core::now())
            .await
            .unwrap();
        // Finish B already — reaper must leave it alone.
        repo.finish(
            &TurnId::new(tb.id.clone()),
            "completed",
            leveler_core::now(),
        )
        .await
        .unwrap();

        let n = repo
            .interrupt_running_for_session(&a, leveler_core::now())
            .await
            .unwrap();
        assert_eq!(n, 1);

        let a_turns = repo.list(&a).await.unwrap();
        assert_eq!(a_turns[0].id, ta.id);
        assert_eq!(a_turns[0].status, "interrupted");
        assert!(a_turns[0].finished_at.is_some());

        let b_turns = repo.list(&b).await.unwrap();
        assert_eq!(b_turns[0].status, "completed");
    }

    #[tokio::test]
    async fn interrupt_all_running_reaps_zombie_turns() {
        let (db, a) = db_with_session().await;
        let record = SessionRecord::new("/repo", "other", "mock/m", leveler_core::now());
        SessionRepository::new(&db).create(&record).await.unwrap();
        let b = SessionId::new(record.id);
        let repo = TurnRepository::new(&db);

        repo.start(&a, "chat", None, leveler_core::now())
            .await
            .unwrap();
        repo.start(&b, "chat", None, leveler_core::now())
            .await
            .unwrap();

        let n = repo
            .interrupt_all_running(leveler_core::now())
            .await
            .unwrap();
        assert_eq!(n, 2);
        for session in [&a, &b] {
            let turns = repo.list(session).await.unwrap();
            assert_eq!(turns[0].status, "interrupted");
            assert!(turns[0].finished_at.is_some());
        }
        // Idempotent: nothing left running.
        assert_eq!(
            repo.interrupt_all_running(leveler_core::now())
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn start_redacts_json_secrets() {
        let (db, session) = db_with_session().await;
        let turn = TurnRepository::new(&db)
            .start(
                &session,
                "user",
                Some(r#"{"client_secret":"turn-secret-value"}"#),
                leveler_core::now(),
            )
            .await
            .unwrap();
        let payload = turn.payload.unwrap();
        assert!(!payload.contains("turn-secret-value"), "{payload}");
        assert!(payload.contains("[REDACTED]"), "{payload}");
    }
}
