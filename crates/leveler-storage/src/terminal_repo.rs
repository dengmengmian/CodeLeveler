//! Atomic terminal transitions for the execution aggregate.

use leveler_core::{SessionId, Timestamp, TurnId};
use leveler_lifecycle::{TaskOutcome, TurnOutcome};

use crate::event_repo::EVENT_SCHEMA_VERSION;
use crate::{Database, EventRecord, StorageError};

pub struct TerminalRepository<'a> {
    db: &'a Database,
}

impl<'a> TerminalRepository<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    pub async fn finish_task(
        &self,
        session_id: &SessionId,
        event_type: &str,
        payload: &str,
        outcome: TaskOutcome,
        now: Timestamp,
    ) -> Result<EventRecord, StorageError> {
        let mut tx = self.db.pool().begin().await?;
        let event = append_event(&mut tx, session_id, None, event_type, payload, &now).await?;
        let updated =
            sqlx::query("UPDATE sessions SET outcome = ?2, updated_at = ?3 WHERE id = ?1")
                .bind(session_id.as_str())
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
                "session {} not found for terminal transition",
                session_id.as_str()
            )));
        }
        tx.commit().await?;
        Ok(event)
    }

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
