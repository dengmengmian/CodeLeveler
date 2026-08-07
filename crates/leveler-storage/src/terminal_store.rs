//! The `TerminalStore` port: atomic terminal-lifecycle commits.
//!
//! The transaction boundary IS the contract. `finish_task` commits the
//! canonical `TaskFinished` event together with every session lifecycle
//! column (outcome + status + state); `finish_turn` commits the canonical
//! `TurnFinished` event together with the turn projection. Either everything
//! lands or nothing does — an implementation must never expose a state where
//! the event exists without its projection or vice versa, and an unknown
//! session/turn is a hard error with nothing written.
//!
//! The engine decides the lifecycle facts; this port only commits them. It is
//! deliberately NOT decomposed into `event_store.append` + `update_status`
//! calls — that split is exactly the half-commit window the port exists to
//! forbid.

use async_trait::async_trait;

use leveler_core::{SessionId, Timestamp, TurnId};
use leveler_lifecycle::{AgentState, SessionStatus, TaskOutcome, TurnOutcome};

use crate::{Database, EventRecord, StorageError, TerminalRepository};

/// The engine-facing atomic terminal-commit contract.
#[async_trait]
pub trait TerminalStore: Send + Sync {
    /// Commit the session's terminal event and the whole terminal lifecycle
    /// (`outcome`, `status`, `state`) atomically. Returns the appended event.
    ///
    /// # Errors
    ///
    /// Fails — leaving NO partial state — when `session_id` matches no
    /// session or the commit cannot complete.
    #[allow(clippy::too_many_arguments)]
    async fn finish_task(
        &self,
        session_id: &SessionId,
        event_type: &str,
        payload: &str,
        outcome: TaskOutcome,
        status: SessionStatus,
        state: AgentState,
        now: Timestamp,
    ) -> Result<EventRecord, StorageError>;

    /// Commit the turn's terminal event and the turn projection (status +
    /// `finished_at`) atomically. Returns the appended event.
    ///
    /// # Errors
    ///
    /// Fails — leaving NO partial state — when `turn_id` matches no turn or
    /// the commit cannot complete.
    #[allow(clippy::too_many_arguments)]
    async fn finish_turn(
        &self,
        session_id: &SessionId,
        turn_id: &TurnId,
        event_type: &str,
        payload: &str,
        outcome: TurnOutcome,
        now: Timestamp,
    ) -> Result<EventRecord, StorageError>;
}

/// The production SQLite adapter: delegates to [`TerminalRepository`], whose
/// single-transaction semantics (and rollback/busy/full tests) are unchanged.
#[async_trait]
impl TerminalStore for Database {
    async fn finish_task(
        &self,
        session_id: &SessionId,
        event_type: &str,
        payload: &str,
        outcome: TaskOutcome,
        status: SessionStatus,
        state: AgentState,
        now: Timestamp,
    ) -> Result<EventRecord, StorageError> {
        TerminalRepository::new(self)
            .finish_task(session_id, event_type, payload, outcome, status, state, now)
            .await
    }

    async fn finish_turn(
        &self,
        session_id: &SessionId,
        turn_id: &TurnId,
        event_type: &str,
        payload: &str,
        outcome: TurnOutcome,
        now: Timestamp,
    ) -> Result<EventRecord, StorageError> {
        TerminalRepository::new(self)
            .finish_turn(session_id, turn_id, event_type, payload, outcome, now)
            .await
    }
}

/// An in-memory [`TerminalStore`] that simulates the atomic semantics, not
/// just the happy path: it validates existence against the sibling memory
/// stores BEFORE writing anything, so a failed commit leaves no partial
/// state — the same observable contract as the SQLite transaction. A `fail`
/// switch injects commit failures for engine-level tests.
pub struct MemoryTerminalStore {
    sessions: std::sync::Arc<crate::MemorySessionStore>,
    turns: std::sync::Arc<crate::MemoryTurnStore>,
    events: std::sync::Arc<crate::MemoryEventStore>,
    /// When set, every commit fails without writing — failure injection for
    /// "terminal write fails ⇒ nothing visible" tests.
    fail: std::sync::atomic::AtomicBool,
}

impl MemoryTerminalStore {
    /// Couple the terminal store to the memory stores whose state it commits
    /// against.
    pub fn new(
        sessions: std::sync::Arc<crate::MemorySessionStore>,
        turns: std::sync::Arc<crate::MemoryTurnStore>,
        events: std::sync::Arc<crate::MemoryEventStore>,
    ) -> Self {
        Self {
            sessions,
            turns,
            events,
            fail: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Make every subsequent commit fail without writing.
    pub fn fail_commits(&self, fail: bool) {
        self.fail.store(fail, std::sync::atomic::Ordering::SeqCst);
    }

    fn check_injected_failure(&self) -> Result<(), StorageError> {
        if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(StorageError::InvalidData(
                "injected terminal commit failure".to_string(),
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl TerminalStore for MemoryTerminalStore {
    async fn finish_task(
        &self,
        session_id: &SessionId,
        event_type: &str,
        payload: &str,
        outcome: TaskOutcome,
        status: SessionStatus,
        state: AgentState,
        now: Timestamp,
    ) -> Result<EventRecord, StorageError> {
        self.check_injected_failure()?;
        // Validate BEFORE any write — the memory equivalent of rollback.
        {
            let mut rows = self.sessions.rows.lock().unwrap();
            let Some(session) = rows.get_mut(session_id.as_str()) else {
                return Err(StorageError::InvalidData(format!(
                    "session {} not found for terminal transition",
                    session_id.as_str()
                )));
            };
            session.outcome = Some(outcome);
            session.status = status;
            session.state = state;
        }
        crate::EventStore::append(
            self.events.as_ref(),
            session_id,
            None,
            event_type,
            &leveler_core::redact_secrets(payload),
            now,
        )
        .await
    }

    async fn finish_turn(
        &self,
        session_id: &SessionId,
        turn_id: &TurnId,
        event_type: &str,
        payload: &str,
        outcome: TurnOutcome,
        now: Timestamp,
    ) -> Result<EventRecord, StorageError> {
        self.check_injected_failure()?;
        {
            let mut rows = self.turns.rows.lock().unwrap();
            let Some(turn) = rows.iter_mut().find(|t| t.id == turn_id.as_str()) else {
                return Err(StorageError::InvalidData(format!(
                    "turn {} not found for terminal transition",
                    turn_id.as_str()
                )));
            };
            turn.status = outcome.as_str().to_string();
            turn.finished_at = Some(now.to_rfc3339());
        }
        crate::EventStore::append(
            self.events.as_ref(),
            session_id,
            Some(turn_id),
            event_type,
            &leveler_core::redact_secrets(payload),
            now,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        EventStore, MemoryEventStore, MemorySessionStore, MemoryTurnStore, SessionRecord,
        SessionStore, TurnStore,
    };
    use std::sync::Arc;

    // Shared contract, verified purely through ports: terminal commits are
    // observable on BOTH sides (event log + projection) on success, and on
    // NEITHER side on failure.
    async fn assert_terminal_store_contract(
        terminal: &dyn TerminalStore,
        sessions: &dyn SessionStore,
        turns: &dyn TurnStore,
        events: &dyn EventStore,
    ) {
        // Unknown session: hard error, nothing written.
        let ghost = SessionId::new("ghost");
        assert!(
            terminal
                .finish_task(
                    &ghost,
                    "task_finished",
                    "{}",
                    TaskOutcome::Failed,
                    SessionStatus::Failed,
                    AgentState::Failed,
                    leveler_core::now(),
                )
                .await
                .is_err()
        );
        assert!(
            events.load(&ghost).await.unwrap().is_empty(),
            "a failed terminal commit must leave no event"
        );

        // Success: event + projection land together.
        let record = SessionRecord::new("/repo", "goal", "mock/m", leveler_core::now());
        let session = SessionId::new(record.id.clone());
        sessions.create(&record).await.unwrap();
        let turn = turns
            .start(&session, "user", None, leveler_core::now())
            .await
            .unwrap();
        let turn_id = TurnId::new(turn.id.clone());

        let event = terminal
            .finish_turn(
                &session,
                &turn_id,
                "turn_finished",
                r#"{"ok":true}"#,
                TurnOutcome::Interrupted,
                leveler_core::now(),
            )
            .await
            .unwrap();
        assert_eq!(event.event_type, "turn_finished");
        assert!(
            turns.list_running(Some(&session)).await.unwrap().is_empty(),
            "finish_turn must terminate the running turn"
        );

        terminal
            .finish_task(
                &session,
                "task_finished",
                r#"{"outcome":"interrupted"}"#,
                TaskOutcome::Interrupted,
                SessionStatus::Interrupted,
                AgentState::Execute,
                leveler_core::now(),
            )
            .await
            .unwrap();
        let (_, _, _, outcome) = sessions.execution(&session).await.unwrap().unwrap();
        assert_eq!(outcome, Some(TaskOutcome::Interrupted));
        let log = events.load(&session).await.unwrap();
        assert_eq!(
            log.iter()
                .map(|e| e.event_type.as_str())
                .collect::<Vec<_>>(),
            vec!["turn_finished", "task_finished"],
            "both terminal events must be on the canonical log, in order"
        );

        // Unknown turn: hard error, event count unchanged.
        assert!(
            terminal
                .finish_turn(
                    &session,
                    &TurnId::new("ghost-turn"),
                    "turn_finished",
                    "{}",
                    TurnOutcome::Failed,
                    leveler_core::now(),
                )
                .await
                .is_err()
        );
        assert_eq!(events.load(&session).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn sqlite_store_honors_the_contract() {
        let db = Database::connect_in_memory().await.unwrap();
        assert_terminal_store_contract(&db, &db, &db, &db).await;
    }

    #[tokio::test]
    async fn memory_store_honors_the_contract() {
        let sessions = Arc::new(MemorySessionStore::new());
        let turns = Arc::new(MemoryTurnStore::new());
        let events = Arc::new(MemoryEventStore::new());
        let terminal = MemoryTerminalStore::new(sessions.clone(), turns.clone(), events.clone());
        assert_terminal_store_contract(
            &terminal,
            sessions.as_ref(),
            turns.as_ref(),
            events.as_ref(),
        )
        .await;
    }

    #[tokio::test]
    async fn injected_commit_failure_leaves_no_partial_state() {
        let sessions = Arc::new(MemorySessionStore::new());
        let turns = Arc::new(MemoryTurnStore::new());
        let events = Arc::new(MemoryEventStore::new());
        let terminal = MemoryTerminalStore::new(sessions.clone(), turns.clone(), events.clone());

        let record = SessionRecord::new("/repo", "goal", "mock/m", leveler_core::now());
        let session = SessionId::new(record.id.clone());
        SessionStore::create(sessions.as_ref(), &record)
            .await
            .unwrap();

        terminal.fail_commits(true);
        assert!(
            terminal
                .finish_task(
                    &session,
                    "task_finished",
                    "{}",
                    TaskOutcome::Verified,
                    SessionStatus::Completed,
                    AgentState::Complete,
                    leveler_core::now(),
                )
                .await
                .is_err()
        );
        let (_, _, _, outcome) = SessionStore::execution(sessions.as_ref(), &session)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(outcome, None, "no outcome without its event");
        assert!(
            EventStore::load(events.as_ref(), &session)
                .await
                .unwrap()
                .is_empty(),
            "no event without its projection"
        );
    }
}
