//! Atomic terminal transitions for the execution aggregate.

use leveler_core::{SessionId, Timestamp, TurnId};
use leveler_lifecycle::{AgentState, SessionStatus, TaskOutcome, TurnOutcome, VerificationStatus};

use crate::event_repo::EVENT_SCHEMA_VERSION;
use crate::{Database, EventRecord, GoalTerminalUpdate, StorageError, TaskTerminalCommit};

/// Writes that must land together: appending the terminal event, marking the
/// aggregate finished, and applying any supplied goal projection. Each method
/// uses one transaction, so a crash cannot expose a partial terminal fact.
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
        verification: VerificationStatus,
        status: SessionStatus,
        state: AgentState,
        now: Timestamp,
    ) -> Result<EventRecord, StorageError> {
        let mut tx = self.db.pool().begin().await?;
        let event = append_event(&mut tx, session_id, None, event_type, payload, &now).await?;
        let updated = sqlx::query(
            "UPDATE sessions SET outcome = ?2, status = ?3, state = ?4, updated_at = ?5, \
             verification = ?6 WHERE id = ?1",
        )
        .bind(session_id.as_str())
        .bind(outcome.as_str())
        .bind(status.as_str())
        .bind(state.as_str())
        .bind(now.to_rfc3339())
        .bind(verification.as_str())
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
        verification: VerificationStatus,
        status: SessionStatus,
        state: AgentState,
        goal: Option<&GoalTerminalUpdate>,
        now: Timestamp,
    ) -> Result<TaskTerminalCommit, crate::OwnershipError> {
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
        let payload = crate::terminal_store::task_terminal_payload_for_epoch(
            payload,
            session_id,
            token,
            outcome,
            verification,
            status,
            state,
            goal,
        )
        .map_err(crate::OwnershipError::Storage)?;
        // The idempotency key is the durable task ownership epoch, not event
        // adjacency. Audit rows may be appended after a terminal without
        // authorizing a second terminal or charging the goal twice.
        let prior_terminals = sqlx::query_as::<_, EventRecord>(
            "SELECT id, session_id, turn_id, sequence, type AS event_type, payload, \
             created_at, schema_version FROM events WHERE session_id = ?1 AND type = ?2 \
             ORDER BY sequence DESC",
        )
        .bind(session_id.as_str())
        .bind(event_type)
        .fetch_all(&mut *tx)
        .await
        .map_err(StorageError::from)
        .map_err(crate::OwnershipError::Storage)?;
        if let Some(event) = prior_terminals.into_iter().find(|event| {
            crate::terminal_store::task_terminal_payload_matches_epoch(&event.payload, token)
        }) {
            if event.payload != payload {
                let _ = tx.rollback().await;
                return Err(crate::OwnershipError::Storage(StorageError::InvalidData(
                    format!(
                        "conflicting task terminal for ownership epoch {}",
                        token.owner_epoch.get()
                    ),
                )));
            }
            tx.commit()
                .await
                .map_err(StorageError::from)
                .map_err(crate::OwnershipError::Storage)?;
            return Ok(TaskTerminalCommit {
                event,
                inserted: false,
            });
        }
        let event = append_event(&mut tx, session_id, None, event_type, &payload, &now)
            .await
            .map_err(crate::OwnershipError::Storage)?;
        let updated = sqlx::query(
            "UPDATE sessions SET outcome = ?2, status = ?3, state = ?4, updated_at = ?5, \
             verification = ?6 WHERE id = ?1",
        )
        .bind(session_id.as_str())
        .bind(outcome.as_str())
        .bind(status.as_str())
        .bind(state.as_str())
        .bind(now.to_rfc3339())
        .bind(verification.as_str())
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
        if let Some(goal) = goal {
            let updated = if goal.settle {
                sqlx::query(
                    "UPDATE goals SET windows_run = windows_run + ?3, state = 'settled', \
                     settled_at = ?4 WHERE id = ?1 AND task_id = ?2 AND state = 'running'",
                )
                .bind(goal.goal_id.as_str())
                .bind(token.task_id.as_str())
                .bind(i64::from(goal.windows_delta))
                .bind(now.to_rfc3339())
                .execute(&mut *tx)
                .await
            } else {
                sqlx::query(
                    "UPDATE goals SET windows_run = windows_run + ?3 \
                     WHERE id = ?1 AND task_id = ?2 AND state = 'running'",
                )
                .bind(goal.goal_id.as_str())
                .bind(token.task_id.as_str())
                .bind(i64::from(goal.windows_delta))
                .execute(&mut *tx)
                .await
            };
            match updated {
                Ok(updated) if updated.rows_affected() == 1 => {}
                Ok(_) => {
                    let _ = tx.rollback().await;
                    return Err(crate::OwnershipError::Storage(StorageError::InvalidData(
                        format!(
                            "running goal {} not found for task {} terminal transition",
                            goal.goal_id, token.task_id
                        ),
                    )));
                }
                Err(error) => {
                    let _ = tx.rollback().await;
                    return Err(crate::OwnershipError::Storage(error.into()));
                }
            }
        }
        tx.commit()
            .await
            .map_err(StorageError::from)
            .map_err(crate::OwnershipError::Storage)?;
        Ok(TaskTerminalCommit {
            event,
            inserted: true,
        })
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
    let payload = crate::redact_json_payload_for_session(
        &format!("event (type '{event_type}')"),
        payload,
        Some(session_id.as_str()),
    )?;
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
    use crate::{
        EventRepository, GoalState, GoalStore, OwnershipStore, SessionRecord, SessionRepository,
        SessionStore, TaskStore, TurnRepository,
    };
    use leveler_core::{OwnerEpoch, OwnershipToken, RuntimeId};

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

    async fn db_with_owned_goal() -> (Database, SessionId, OwnershipToken, leveler_core::GoalId) {
        let db = Database::connect_in_memory().await.unwrap();
        let record = SessionRecord::new("/repo", "goal", "mock/m", leveler_core::now());
        SessionRepository::new(&db).create(&record).await.unwrap();
        let session = SessionId::new(record.id);
        let task = db
            .ensure_for_session(&session, leveler_core::now())
            .await
            .unwrap();
        let token = db
            .acquire(&task, &RuntimeId::new("test-runtime"), OwnerEpoch::UNOWNED)
            .await
            .unwrap();
        let goal = db
            .open(&token, "the goal", leveler_core::now())
            .await
            .unwrap();
        (db, session, token, goal)
    }

    #[tokio::test]
    async fn owned_task_terminal_commits_goal_projection_atomically() {
        let (db, session, token, goal) = db_with_owned_goal().await;
        let update = GoalTerminalUpdate {
            goal_id: goal.clone(),
            windows_delta: 3,
            settle: true,
        };

        TerminalRepository::new(&db)
            .finish_task_owned(
                &token,
                &session,
                "task_finished",
                r#"{"type":"task_finished","payload":{"outcome":"completed"}}"#,
                TaskOutcome::Completed,
                leveler_lifecycle::VerificationStatus::NotRun,
                SessionStatus::Completed,
                AgentState::Complete,
                Some(&update),
                leveler_core::now(),
            )
            .await
            .unwrap();

        EventRepository::new(&db)
            .append(
                &session,
                None,
                "audit_note",
                r#"{"type":"audit_note","detail":"after terminal"}"#,
                leveler_core::now(),
            )
            .await
            .unwrap();

        TerminalRepository::new(&db)
            .finish_task_owned(
                &token,
                &session,
                "task_finished",
                r#"{"type":"task_finished","payload":{"outcome":"completed"}}"#,
                TaskOutcome::Completed,
                leveler_lifecycle::VerificationStatus::NotRun,
                SessionStatus::Completed,
                AgentState::Complete,
                Some(&update),
                leveler_core::now(),
            )
            .await
            .unwrap();

        let projection_conflict = TerminalRepository::new(&db)
            .finish_task_owned(
                &token,
                &session,
                "task_finished",
                r#"{"type":"task_finished","payload":{"outcome":"completed"}}"#,
                TaskOutcome::Completed,
                leveler_lifecycle::VerificationStatus::NotRun,
                SessionStatus::Failed,
                AgentState::Failed,
                Some(&update),
                leveler_core::now(),
            )
            .await;
        assert!(
            projection_conflict.is_err(),
            "the same event body with a different lifecycle projection must conflict"
        );

        let different_goal_update = GoalTerminalUpdate {
            goal_id: goal.clone(),
            windows_delta: 4,
            settle: true,
        };
        let goal_conflict = TerminalRepository::new(&db)
            .finish_task_owned(
                &token,
                &session,
                "task_finished",
                r#"{"type":"task_finished","payload":{"outcome":"completed"}}"#,
                TaskOutcome::Completed,
                leveler_lifecycle::VerificationStatus::NotRun,
                SessionStatus::Completed,
                AgentState::Complete,
                Some(&different_goal_update),
                leveler_core::now(),
            )
            .await;
        assert!(
            goal_conflict.is_err(),
            "the same event body with a different goal projection must conflict"
        );

        let conflict = TerminalRepository::new(&db)
            .finish_task_owned(
                &token,
                &session,
                "task_finished",
                r#"{"type":"task_finished","payload":{"outcome":"failed"}}"#,
                TaskOutcome::Failed,
                leveler_lifecycle::VerificationStatus::Failed,
                SessionStatus::Failed,
                AgentState::Failed,
                Some(&update),
                leveler_core::now(),
            )
            .await;
        assert!(
            conflict.is_err(),
            "one ownership epoch has one terminal truth"
        );

        let stored = db.get(&goal).await.unwrap().unwrap();
        assert_eq!(stored.windows_run, 3);
        assert_eq!(stored.state, GoalState::Settled);
        assert!(stored.settled_at.is_some());
        let events = EventRepository::new(&db).load(&session).await.unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "task_finished")
                .count(),
            1
        );
        assert_eq!(events.len(), 2, "the intervening audit row remains intact");
        assert_eq!(
            SessionStore::execution(&db, &session)
                .await
                .unwrap()
                .unwrap()
                .3,
            Some(TaskOutcome::Completed)
        );
    }

    #[tokio::test]
    async fn owned_task_terminal_can_record_a_window_without_settling_the_goal() {
        let (db, session, token, goal) = db_with_owned_goal().await;
        let update = GoalTerminalUpdate {
            goal_id: goal.clone(),
            windows_delta: 2,
            settle: false,
        };

        TerminalRepository::new(&db)
            .finish_task_owned(
                &token,
                &session,
                "task_finished",
                r#"{"type":"task_finished","payload":{"outcome":"budget_limited"}}"#,
                TaskOutcome::BudgetLimited,
                leveler_lifecycle::VerificationStatus::NotRun,
                SessionStatus::Incomplete,
                AgentState::Execute,
                Some(&update),
                leveler_core::now(),
            )
            .await
            .unwrap();

        let stored = db.get(&goal).await.unwrap().unwrap();
        assert_eq!(stored.windows_run, 2);
        assert_eq!(stored.state, GoalState::Running);
        assert_eq!(stored.settled_at, None);
    }

    #[tokio::test]
    async fn goal_from_another_task_rolls_back_task_terminal() {
        let (db, session, token, _) = db_with_owned_goal().await;
        let other_record = SessionRecord::new("/repo", "other", "mock/m", leveler_core::now());
        SessionRepository::new(&db)
            .create(&other_record)
            .await
            .unwrap();
        let other_session = SessionId::new(other_record.id);
        let other_task = db
            .ensure_for_session(&other_session, leveler_core::now())
            .await
            .unwrap();
        let other_token = db
            .acquire(
                &other_task,
                &RuntimeId::new("other-runtime"),
                OwnerEpoch::UNOWNED,
            )
            .await
            .unwrap();
        let other_goal = db
            .open(&other_token, "other goal", leveler_core::now())
            .await
            .unwrap();
        let update = GoalTerminalUpdate {
            goal_id: other_goal.clone(),
            windows_delta: 1,
            settle: true,
        };

        let result = TerminalRepository::new(&db)
            .finish_task_owned(
                &token,
                &session,
                "task_finished",
                r#"{"type":"task_finished","payload":{"outcome":"completed"}}"#,
                TaskOutcome::Completed,
                leveler_lifecycle::VerificationStatus::NotRun,
                SessionStatus::Completed,
                AgentState::Complete,
                Some(&update),
                leveler_core::now(),
            )
            .await;

        assert!(result.is_err());
        assert!(
            EventRepository::new(&db)
                .load(&session)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            SessionStore::execution(&db, &session)
                .await
                .unwrap()
                .unwrap()
                .3,
            None
        );
        let stored = db.get(&other_goal).await.unwrap().unwrap();
        assert_eq!(stored.windows_run, 0);
        assert_eq!(stored.state, GoalState::Running);
    }

    #[tokio::test]
    async fn goal_projection_failure_rolls_back_task_terminal() {
        let (db, session, token, goal) = db_with_owned_goal().await;
        sqlx::query(
            "CREATE TRIGGER reject_goal_terminal BEFORE UPDATE OF windows_run ON goals \
             BEGIN SELECT RAISE(ABORT, 'goal projection failed'); END",
        )
        .execute(db.pool())
        .await
        .unwrap();
        let update = GoalTerminalUpdate {
            goal_id: goal.clone(),
            windows_delta: 1,
            settle: true,
        };

        let result = TerminalRepository::new(&db)
            .finish_task_owned(
                &token,
                &session,
                "task_finished",
                r#"{"type":"task_finished","payload":{"outcome":"completed"}}"#,
                TaskOutcome::Completed,
                leveler_lifecycle::VerificationStatus::NotRun,
                SessionStatus::Completed,
                AgentState::Complete,
                Some(&update),
                leveler_core::now(),
            )
            .await;

        assert!(result.is_err());
        assert!(
            EventRepository::new(&db)
                .load(&session)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            SessionStore::execution(&db, &session)
                .await
                .unwrap()
                .unwrap()
                .3,
            None
        );
        let stored = db.get(&goal).await.unwrap().unwrap();
        assert_eq!(stored.windows_run, 0);
        assert_eq!(stored.state, GoalState::Running);
        assert_eq!(stored.settled_at, None);
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
                leveler_lifecycle::VerificationStatus::NotRun,
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
                leveler_lifecycle::VerificationStatus::NotRun,
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
                leveler_lifecycle::VerificationStatus::NotRun,
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
                TaskOutcome::Completed,
                leveler_lifecycle::VerificationStatus::NotRun,
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
