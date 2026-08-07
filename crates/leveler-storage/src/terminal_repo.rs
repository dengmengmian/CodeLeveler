//! Atomic terminal transitions for the execution aggregate.

use leveler_core::{SessionId, Timestamp, TurnId};
use leveler_lifecycle::{AgentState, SessionStatus, TaskOutcome, TurnOutcome};

use crate::event_repo::EVENT_SCHEMA_VERSION;
use crate::{Database, EventRecord, StorageError};

/// Writes that must land together: appending the terminal event and marking
/// the aggregate finished. Each method runs both in one transaction, so a crash
/// can never leave a session marked complete without its event, or vice versa.
pub struct TerminalRepository<'a> {
    db: &'a Database,
}

impl<'a> TerminalRepository<'a> {
    /// Borrow `db` for the lifetime of this repository handle.
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    /// Append the session's terminal event and record the WHOLE terminal
    /// lifecycle (`outcome`, `status`, `state`) on the session row, atomically.
    /// Returns the appended event with its assigned sequence. One writer, one
    /// transaction: a crash can never leave the outcome and the user-facing
    /// status disagreeing.
    ///
    /// # Errors
    ///
    /// Rolls back and returns [`StorageError::InvalidData`] if `session_id`
    /// matches no row — a terminal transition for an unknown session is a bug,
    /// not a no-op.
    pub async fn finish_task(
        &self,
        session_id: &SessionId,
        event_type: &str,
        payload: &str,
        outcome: TaskOutcome,
        status: SessionStatus,
        state: AgentState,
        now: Timestamp,
    ) -> Result<EventRecord, StorageError> {
        let mut tx = self.db.pool().begin().await?;
        let event = append_event(&mut tx, session_id, None, event_type, payload, &now).await?;
        let updated = sqlx::query(
            "UPDATE sessions SET outcome = ?2, status = ?3, state = ?4, updated_at = ?5 \
             WHERE id = ?1",
        )
        .bind(session_id.as_str())
        .bind(outcome.as_str())
        .bind(status.as_str())
        .bind(state.as_str())
        .bind(now.to_rfc3339())
        .execute(&mut *tx)
        .await;
        let updated = match updated {
            Ok(updated) => updated,
            Err(error) => {
                tx.rollback().await?;
                return Err(error.into());
            }
        };
        if updated.rows_affected() != 1 {
            tx.rollback().await?;
            return Err(StorageError::InvalidData(format!(
                "session {} not found for terminal transition",
                session_id.as_str()
            )));
        }
        tx.commit().await?;
        Ok(event)
    }

    /// Append the turn's terminal event and set the turn's status and
    /// `finished_at`, atomically. Returns the appended event.
    ///
    /// # Errors
    ///
    /// Rolls back and returns [`StorageError::InvalidData`] if `turn_id`
    /// matches no row.
    pub async fn finish_turn(
        &self,
        session_id: &SessionId,
        turn_id: &TurnId,
        event_type: &str,
        payload: &str,
        outcome: TurnOutcome,
        now: Timestamp,
    ) -> Result<EventRecord, StorageError> {
        let mut tx = self.db.pool().begin().await?;
        let event = append_event(
            &mut tx,
            session_id,
            Some(turn_id),
            event_type,
            payload,
            &now,
        )
        .await?;
        let updated = sqlx::query("UPDATE turns SET status = ?2, finished_at = ?3 WHERE id = ?1")
            .bind(turn_id.as_str())
            .bind(outcome.as_str())
            .bind(now.to_rfc3339())
            .execute(&mut *tx)
            .await;
        let updated = match updated {
            Ok(updated) => updated,
            Err(error) => {
                tx.rollback().await?;
                return Err(error.into());
            }
        };
        if updated.rows_affected() != 1 {
            tx.rollback().await?;
            return Err(StorageError::InvalidData(format!(
                "turn {} not found for terminal transition",
                turn_id.as_str()
            )));
        }
        tx.commit().await?;
        Ok(event)
    }
}

impl TerminalRepository<'_> {
    /// Fenced [`Self::finish_task`]: the ownership assertion runs INSIDE the
    /// same transaction as the terminal event and the projection update —
    /// assert, append, project, COMMIT, with any failure rolling back all of
    /// it. No assert-then-begin TOCTOU.
    #[allow(clippy::too_many_arguments)]
    pub async fn finish_task_owned(
        &self,
        token: &leveler_core::OwnershipToken,
        session_id: &SessionId,
        event_type: &str,
        payload: &str,
        outcome: TaskOutcome,
        status: SessionStatus,
        state: AgentState,
        now: Timestamp,
    ) -> Result<EventRecord, crate::OwnershipError> {
        // BEGIN IMMEDIATE: the ownership SELECT below precedes the writes, and
        // a deferred read-then-write upgrade deadlocks against a concurrent
        // writer with an immediate "database is locked" no busy_timeout can
        // wait out (same rule as MessageRepository::append_in_turn).
        let mut tx = self
            .db
            .pool()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(StorageError::from)
            .map_err(crate::OwnershipError::Storage)?;
        if !owner_current_in_tx(&mut tx, token, session_id).await? {
            let _ = tx.rollback().await;
            return Err(crate::ownership_store::sqlite_stale_error(self.db, token).await);
        }
        let event = append_event(&mut tx, session_id, None, event_type, payload, &now)
            .await
            .map_err(crate::OwnershipError::Storage)?;
        let updated = sqlx::query(
            "UPDATE sessions SET outcome = ?2, status = ?3, state = ?4, updated_at = ?5 \
             WHERE id = ?1",
        )
        .bind(session_id.as_str())
        .bind(outcome.as_str())
        .bind(status.as_str())
        .bind(state.as_str())
        .bind(now.to_rfc3339())
        .execute(&mut *tx)
        .await;
        match updated {
            Ok(updated) if updated.rows_affected() == 1 => {}
            Ok(_) => {
                let _ = tx.rollback().await;
                return Err(crate::OwnershipError::Storage(StorageError::InvalidData(
                    format!("session {} not found for terminal transition", session_id),
                )));
            }
            Err(error) => {
                let _ = tx.rollback().await;
                return Err(crate::OwnershipError::Storage(error.into()));
            }
        }
        tx.commit()
            .await
            .map_err(StorageError::from)
            .map_err(crate::OwnershipError::Storage)?;
        Ok(event)
    }

    /// Fenced [`Self::finish_turn`], same single-transaction contract.
    pub async fn finish_turn_owned(
        &self,
        token: &leveler_core::OwnershipToken,
        session_id: &SessionId,
        turn_id: &TurnId,
        event_type: &str,
        payload: &str,
        outcome: TurnOutcome,
        now: Timestamp,
    ) -> Result<EventRecord, crate::OwnershipError> {
        // BEGIN IMMEDIATE: the ownership SELECT below precedes the writes, and
        // a deferred read-then-write upgrade deadlocks against a concurrent
        // writer with an immediate "database is locked" no busy_timeout can
        // wait out (same rule as MessageRepository::append_in_turn).
        let mut tx = self
            .db
            .pool()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(StorageError::from)
            .map_err(crate::OwnershipError::Storage)?;
        if !owner_current_in_tx(&mut tx, token, session_id).await? {
            let _ = tx.rollback().await;
            return Err(crate::ownership_store::sqlite_stale_error(self.db, token).await);
        }
        let event = append_event(
            &mut tx,
            session_id,
            Some(turn_id),
            event_type,
            payload,
            &now,
        )
        .await
        .map_err(crate::OwnershipError::Storage)?;
        let updated = sqlx::query("UPDATE turns SET status = ?2, finished_at = ?3 WHERE id = ?1")
            .bind(turn_id.as_str())
            .bind(outcome.as_str())
            .bind(now.to_rfc3339())
            .execute(&mut *tx)
            .await;
        match updated {
            Ok(updated) if updated.rows_affected() == 1 => {}
            Ok(_) => {
                let _ = tx.rollback().await;
                return Err(crate::OwnershipError::Storage(StorageError::InvalidData(
                    format!("turn {} not found for terminal transition", turn_id),
                )));
            }
            Err(error) => {
                let _ = tx.rollback().await;
                return Err(crate::OwnershipError::Storage(error.into()));
            }
        }
        tx.commit()
            .await
            .map_err(StorageError::from)
            .map_err(crate::OwnershipError::Storage)?;
        Ok(event)
    }
}

/// The in-transaction ownership assertion shared by both fenced terminals.
async fn owner_current_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    token: &leveler_core::OwnershipToken,
    session_id: &SessionId,
) -> Result<bool, crate::OwnershipError> {
    let row: Option<(Option<String>, i64)> = sqlx::query_as(
        "SELECT owner_runtime_id, owner_epoch FROM tasks WHERE session_id = ?1 AND id = ?2",
    )
    .bind(session_id.as_str())
    .bind(token.task_id.as_str())
    .fetch_optional(&mut **tx)
    .await
    .map_err(|e| crate::OwnershipError::Storage(e.into()))?;
    Ok(row.is_some_and(|(runtime, epoch)| {
        runtime.as_deref() == Some(token.runtime_id.as_str())
            && epoch == token.owner_epoch.get() as i64
    }))
}

async fn append_event(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    session_id: &SessionId,
    turn_id: Option<&TurnId>,
    event_type: &str,
    payload: &str,
    now: &Timestamp,
) -> Result<EventRecord, StorageError> {
    let id = leveler_core::EventId::generate().into_inner();
    let payload = leveler_core::redact_secrets(payload);
    sqlx::query(
        "INSERT INTO events \
         (id, session_id, turn_id, sequence, type, payload, created_at, schema_version) \
         SELECT ?1, ?2, ?3, COALESCE(MAX(sequence), 0) + 1, ?4, ?5, ?6, ?7 \
         FROM events WHERE session_id = ?2",
    )
    .bind(&id)
    .bind(session_id.as_str())
    .bind(turn_id.map(|turn| turn.as_str().to_string()))
    .bind(event_type)
    .bind(&payload)
    .bind(now.to_rfc3339())
    .bind(EVENT_SCHEMA_VERSION)
    .execute(&mut **tx)
    .await?;
    Ok(sqlx::query_as::<_, EventRecord>(
        "SELECT id, session_id, turn_id, sequence, type AS event_type, payload, created_at, \
         schema_version FROM events WHERE id = ?1",
    )
    .bind(&id)
    .fetch_one(&mut **tx)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EventRepository, SessionRecord, SessionRepository, TurnRepository};

    async fn db_with_turn() -> (Database, SessionId, TurnId) {
        let db = Database::connect_in_memory().await.unwrap();
        let record = SessionRecord::new("/repo", "goal", "mock/m", leveler_core::now());
        SessionRepository::new(&db).create(&record).await.unwrap();
        let session = SessionId::new(record.id);
        let turn = TurnRepository::new(&db)
            .start(&session, "user", None, leveler_core::now())
            .await
            .unwrap();
        (db, session, TurnId::new(turn.id))
    }

    #[tokio::test]
    async fn task_projection_failure_rolls_back_terminal_event() {
        let (db, session, _) = db_with_turn().await;
        sqlx::query(
            "CREATE TRIGGER reject_session_terminal BEFORE UPDATE OF outcome ON sessions \
             BEGIN SELECT RAISE(ABORT, 'projection failed'); END",
        )
        .execute(db.pool())
        .await
        .unwrap();

        let result = TerminalRepository::new(&db)
            .finish_task(
                &session,
                "task_finished",
                r#"{"type":"task_finished","payload":{"outcome":"failed","reason":null}}"#,
                TaskOutcome::Failed,
                SessionStatus::Failed,
                AgentState::Failed,
                leveler_core::now(),
            )
            .await;
        assert!(result.is_err());
        assert!(
            EventRepository::new(&db)
                .load(&session)
                .await
                .unwrap()
                .is_empty(),
            "the event insert must roll back with the projection update"
        );
    }

    #[tokio::test]
    async fn turn_projection_failure_rolls_back_terminal_event() {
        let (db, session, turn) = db_with_turn().await;
        sqlx::query(
            "CREATE TRIGGER reject_turn_terminal BEFORE UPDATE OF status ON turns \
             BEGIN SELECT RAISE(ABORT, 'projection failed'); END",
        )
        .execute(db.pool())
        .await
        .unwrap();

        let result = TerminalRepository::new(&db)
            .finish_turn(
                &session,
                &turn,
                "turn_finished",
                r#"{"type":"turn_finished","payload":{"turn_id":"t","outcome":"failed","stop_reason":"x","rounds":0,"modified_files":[]}}"#,
                TurnOutcome::Failed,
                leveler_core::now(),
            )
            .await;
        assert!(result.is_err());
        assert!(
            EventRepository::new(&db)
                .load(&session)
                .await
                .unwrap()
                .is_empty(),
            "the event insert must roll back with the projection update"
        );
    }

    #[tokio::test]
    async fn sqlite_busy_does_not_create_a_partial_terminal_fact() {
        let dir = std::env::temp_dir().join(format!(
            "leveler-sqlite-busy-{}-{}",
            std::process::id(),
            leveler_core::new_uuid_string()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.db");
        let db = Database::connect(&path).await.unwrap();
        let record = SessionRecord::new("/repo", "goal", "mock/m", leveler_core::now());
        SessionRepository::new(&db).create(&record).await.unwrap();
        let session = SessionId::new(record.id);
        // Keep the test fast while still exercising the real SQLite lock path.
        sqlx::query("PRAGMA busy_timeout = 10")
            .execute(db.pool())
            .await
            .unwrap();

        let blocker = Database::connect(&path).await.unwrap();
        let mut lock = blocker.pool().acquire().await.unwrap();
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut *lock)
            .await
            .unwrap();

        let result = TerminalRepository::new(&db)
            .finish_task(
                &session,
                "task_finished",
                r#"{"type":"task_finished","payload":{"outcome":"failed","reason":null}}"#,
                TaskOutcome::Failed,
                SessionStatus::Failed,
                AgentState::Failed,
                leveler_core::now(),
            )
            .await;
        assert!(result.is_err(), "a locked writer must fail explicitly");
        sqlx::query("ROLLBACK").execute(&mut *lock).await.unwrap();

        assert!(
            EventRepository::new(&db)
                .load(&session)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            SessionRepository::new(&db)
                .execution(&session)
                .await
                .unwrap()
                .unwrap()
                .3,
            None
        );
        drop(lock);
        drop(blocker);
        drop(db);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn sqlite_full_does_not_create_a_partial_terminal_fact() {
        let (db, session, _) = db_with_turn().await;
        let pages: i64 = sqlx::query_scalar("PRAGMA page_count")
            .fetch_one(db.pool())
            .await
            .unwrap();
        sqlx::query(&format!("PRAGMA max_page_count = {pages}"))
            .execute(db.pool())
            .await
            .unwrap();
        let oversized_payload = "x".repeat(1024 * 1024);

        let result = TerminalRepository::new(&db)
            .finish_task(
                &session,
                "task_finished",
                &oversized_payload,
                TaskOutcome::Failed,
                SessionStatus::Failed,
                AgentState::Failed,
                leveler_core::now(),
            )
            .await;
        assert!(result.is_err(), "SQLite full must fail explicitly");
        assert!(
            EventRepository::new(&db)
                .load(&session)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            SessionRepository::new(&db)
                .execution(&session)
                .await
                .unwrap()
                .unwrap()
                .3,
            None
        );
    }

    #[tokio::test]
    async fn terminal_event_redacts_json_secrets() {
        let (db, session, _) = db_with_turn().await;
        let event = TerminalRepository::new(&db)
            .finish_task(
                &session,
                "task_finished",
                r#"{"api_key":"terminal-secret-value"}"#,
                TaskOutcome::CompletedUnverified,
                SessionStatus::Completed,
                AgentState::Complete,
                leveler_core::now(),
            )
            .await
            .unwrap();
        assert!(
            !event.payload.contains("terminal-secret-value"),
            "{event:?}"
        );
        assert!(event.payload.contains("[REDACTED]"), "{event:?}");
    }
}
