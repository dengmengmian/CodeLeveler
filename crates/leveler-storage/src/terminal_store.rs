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

    /// Fenced [`Self::finish_task`]: ownership assertion + terminal event +
    /// projection in ONE transaction. A stale token rolls back everything.
    #[allow(clippy::too_many_arguments)]
    async fn finish_task_owned(
        &self,
        token: &leveler_core::OwnershipToken,
        session_id: &SessionId,
        event_type: &str,
        payload: &str,
        outcome: TaskOutcome,
        status: SessionStatus,
        state: AgentState,
        now: Timestamp,
    ) -> Result<EventRecord, crate::OwnershipError>;

    /// Fenced [`Self::finish_turn`], same single-transaction contract.
    #[allow(clippy::too_many_arguments)]
    async fn finish_turn_owned(
        &self,
        token: &leveler_core::OwnershipToken,
        session_id: &SessionId,
        turn_id: &TurnId,
        event_type: &str,
        payload: &str,
        outcome: TurnOutcome,
        now: Timestamp,
    ) -> Result<EventRecord, crate::OwnershipError>;
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

    async fn finish_task_owned(
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
        TerminalRepository::new(self)
            .finish_task_owned(
                token, session_id, event_type, payload, outcome, status, state, now,
            )
            .await
    }

    async fn finish_turn_owned(
        &self,
        token: &leveler_core::OwnershipToken,
        session_id: &SessionId,
        turn_id: &TurnId,
        event_type: &str,
        payload: &str,
        outcome: TurnOutcome,
        now: Timestamp,
    ) -> Result<EventRecord, crate::OwnershipError> {
        TerminalRepository::new(self)
            .finish_turn_owned(
                token, session_id, turn_id, event_type, payload, outcome, now,
            )
            .await
    }
}

/// An in-memory [`TerminalStore`] that honors the atomic contract by
/// construction: every fallible step (existence validation, the injected
/// failure hook, the canonical event append) runs BEFORE the infallible
/// projection apply. A failure at any stage — including the event
/// append/commit stage itself — therefore leaves projections untouched and
/// no event behind, with no rollback machinery: the same observable contract
/// as the SQLite transaction.
pub struct MemoryTerminalStore {
    sessions: std::sync::Arc<crate::MemorySessionStore>,
    turns: std::sync::Arc<crate::MemoryTurnStore>,
    events: std::sync::Arc<crate::MemoryEventStore>,
    /// Shared ownership authority for the fenced (`*_owned`) commits.
    ownership: std::sync::OnceLock<std::sync::Arc<crate::MemoryOwnershipState>>,
    /// When set, the commit fails AT THE EVENT-APPEND STAGE — after
    /// validation, before anything lands — modeling "projection logically
    /// prepared, commit fails". Deterministic failure injection for
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
            ownership: std::sync::OnceLock::new(),
            fail: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Couple to the shared ownership authority for fenced commits.
    pub fn with_ownership(self, state: std::sync::Arc<crate::MemoryOwnershipState>) -> Self {
        let _ = self.ownership.set(state);
        self
    }

    /// Make every subsequent commit fail without writing.
    pub fn fail_commits(&self, fail: bool) {
        self.fail.store(fail, std::sync::atomic::Ordering::SeqCst);
    }

    /// The synchronous commit body shared by the fenced and unfenced task
    /// terminals: validate, "commit" (injected failure point), then apply
    /// projection + event. All fallible steps precede all writes.
    fn finish_task_sync(
        &self,
        session_id: &SessionId,
        event_type: &str,
        payload: &str,
        outcome: TaskOutcome,
        status: SessionStatus,
        state: AgentState,
        now: Timestamp,
    ) -> Result<EventRecord, StorageError> {
        if !self
            .sessions
            .rows
            .lock()
            .unwrap()
            .contains_key(session_id.as_str())
        {
            return Err(StorageError::InvalidData(format!(
                "session {} not found for terminal transition",
                session_id.as_str()
            )));
        }
        self.check_injected_failure()?;
        let record = self
            .events
            .append_record_for_terminal(session_id, None, event_type, payload, now);
        {
            let mut rows = self.sessions.rows.lock().unwrap();
            if let Some(session) = rows.get_mut(session_id.as_str()) {
                session.outcome = Some(outcome);
                session.status = status;
                session.state = state;
            }
        }
        Ok(record)
    }

    /// The synchronous commit body for turn terminals.
    fn finish_turn_sync(
        &self,
        session_id: &SessionId,
        turn_id: &TurnId,
        event_type: &str,
        payload: &str,
        outcome: TurnOutcome,
        now: Timestamp,
    ) -> Result<EventRecord, StorageError> {
        if !self
            .turns
            .rows
            .lock()
            .unwrap()
            .iter()
            .any(|t| t.id == turn_id.as_str())
        {
            return Err(StorageError::InvalidData(format!(
                "turn {} not found for terminal transition",
                turn_id.as_str()
            )));
        }
        self.check_injected_failure()?;
        let record = self.events.append_record_for_terminal(
            session_id,
            Some(turn_id),
            event_type,
            payload,
            now,
        );
        {
            let mut rows = self.turns.rows.lock().unwrap();
            if let Some(turn) = rows.iter_mut().find(|t| t.id == turn_id.as_str()) {
                turn.status = outcome.as_str().to_string();
                turn.finished_at = Some(now.to_rfc3339());
            }
        }
        Ok(record)
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
        self.finish_task_sync(session_id, event_type, payload, outcome, status, state, now)
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
        self.finish_turn_sync(session_id, turn_id, event_type, payload, outcome, now)
    }

    async fn finish_task_owned(
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
        let Some(ownership) = self.ownership.get() else {
            return Err(crate::OwnershipError::Storage(StorageError::InvalidData(
                "memory terminal store has no ownership authority configured".to_string(),
            )));
        };
        // Ownership lock held across the WHOLE commit body — no CAS window.
        ownership
            .with_current(token, || {
                self.finish_task_sync(session_id, event_type, payload, outcome, status, state, now)
            })?
            .map_err(crate::OwnershipError::Storage)
    }

    async fn finish_turn_owned(
        &self,
        token: &leveler_core::OwnershipToken,
        session_id: &SessionId,
        turn_id: &TurnId,
        event_type: &str,
        payload: &str,
        outcome: TurnOutcome,
        now: Timestamp,
    ) -> Result<EventRecord, crate::OwnershipError> {
        let Some(ownership) = self.ownership.get() else {
            return Err(crate::OwnershipError::Storage(StorageError::InvalidData(
                "memory terminal store has no ownership authority configured".to_string(),
            )));
        };
        ownership
            .with_current(token, || {
                self.finish_turn_sync(session_id, turn_id, event_type, payload, outcome, now)
            })?
            .map_err(crate::OwnershipError::Storage)
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
        // The injected failure fires AT THE APPEND/COMMIT STAGE — after the
        // target session/turn was validated to exist — so this covers the
        // real half-write window: "projection logically prepared, commit
        // fails". Under a mutate-then-append implementation this test fails
        // (the projection would already carry the terminal fact).
        let sessions = Arc::new(MemorySessionStore::new());
        let turns = Arc::new(MemoryTurnStore::new());
        let events = Arc::new(MemoryEventStore::new());
        let terminal = MemoryTerminalStore::new(sessions.clone(), turns.clone(), events.clone());

        // A real session AND a real running turn: validation passes, so the
        // failure can only come from the commit stage itself.
        let record = SessionRecord::new("/repo", "goal", "mock/m", leveler_core::now());
        let session = SessionId::new(record.id.clone());
        SessionStore::create(sessions.as_ref(), &record)
            .await
            .unwrap();
        let before = sessions.lifecycle(&session).expect("session row");
        let turn = TurnStore::start(turns.as_ref(), &session, "user", None, leveler_core::now())
            .await
            .unwrap();
        let turn_id = TurnId::new(turn.id.clone());

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
        assert!(
            terminal
                .finish_turn(
                    &session,
                    &turn_id,
                    "turn_finished",
                    "{}",
                    TurnOutcome::Interrupted,
                    leveler_core::now(),
                )
                .await
                .is_err()
        );

        // Session projection: outcome, status, and state all unchanged.
        let (_, _, _, outcome) = SessionStore::execution(sessions.as_ref(), &session)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(outcome, None, "no outcome without its event");
        assert_eq!(
            sessions.lifecycle(&session).unwrap(),
            before,
            "status/state must be untouched by a failed commit"
        );
        // Turn projection: still running, finished_at unset.
        assert_eq!(turns.status(&turn_id).as_deref(), Some("running"));
        assert_eq!(
            TurnStore::list_running(turns.as_ref(), Some(&session))
                .await
                .unwrap()
                .len(),
            1,
            "the turn must still be visibly running"
        );
        // Canonical log: empty.
        assert!(
            EventStore::load(events.as_ref(), &session)
                .await
                .unwrap()
                .is_empty(),
            "no event without its projection"
        );

        // Once the store recovers, the same commits succeed and land BOTH
        // sides together.
        terminal.fail_commits(false);
        terminal
            .finish_turn(
                &session,
                &turn_id,
                "turn_finished",
                "{}",
                TurnOutcome::Interrupted,
                leveler_core::now(),
            )
            .await
            .unwrap();
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
            .unwrap();
        assert_eq!(turns.status(&turn_id).as_deref(), Some("interrupted"));
        let (_, _, _, outcome) = SessionStore::execution(sessions.as_ref(), &session)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(outcome, Some(TaskOutcome::Verified));
        assert_eq!(
            EventStore::load(events.as_ref(), &session)
                .await
                .unwrap()
                .len(),
            2
        );
    }
}
