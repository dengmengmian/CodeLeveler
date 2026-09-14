//! The `TerminalStore` port: atomic terminal-lifecycle commits.
//!
//! The transaction boundary IS the contract. `finish_task` commits the
//! canonical `TaskFinished` event together with every session lifecycle
//! column (outcome + status + state) and an optional goal projection;
//! `finish_turn` commits the canonical `TurnFinished` event together with the
//! turn projection. Either everything lands or nothing does — an
//! implementation must never expose a state where the event exists without
//! its projection or vice versa, and an unknown session/turn is a hard error
//! with nothing written.
//!
//! The engine decides the lifecycle facts; this port only commits them. It is
//! deliberately NOT decomposed into `event_store.append` + `update_status`
//! calls — that split is exactly the half-commit window the port exists to
//! forbid.

use async_trait::async_trait;

use leveler_core::{GoalId, SessionId, Timestamp, TurnId};
use leveler_lifecycle::{AgentState, SessionStatus, TaskOutcome, TurnOutcome, VerificationStatus};

use crate::{Database, EventRecord, StorageError, TerminalRepository};

pub(crate) fn task_terminal_payload_for_epoch(
    payload: &str,
    session_id: &SessionId,
    token: &leveler_core::OwnershipToken,
    outcome: TaskOutcome,
    verification: VerificationStatus,
    status: SessionStatus,
    state: AgentState,
    goal: Option<&GoalTerminalUpdate>,
) -> Result<String, StorageError> {
    let redacted = crate::redact_json_payload_for_session(
        "task terminal event",
        payload,
        Some(session_id.as_str()),
    )?;
    let mut value: serde_json::Value = serde_json::from_str(&redacted).map_err(|error| {
        StorageError::InvalidData(format!("invalid task terminal event payload: {error}"))
    })?;
    let object = value.as_object_mut().ok_or_else(|| {
        StorageError::InvalidData("task terminal event payload is not an object".to_string())
    })?;
    object.insert(
        "_terminal_task_id".to_string(),
        serde_json::Value::String(token.task_id.as_str().to_string()),
    );
    object.insert(
        "_terminal_owner_epoch".to_string(),
        serde_json::Value::Number(token.owner_epoch.get().into()),
    );
    object.insert(
        "_terminal_projection".to_string(),
        serde_json::json!({
            "outcome": outcome.as_str(),
            "verification": verification.as_str(),
            "status": status.as_str(),
            "state": state.as_str(),
            "goal": goal.map(|update| serde_json::json!({
                "goal_id": update.goal_id.as_str(),
                "windows_delta": update.windows_delta,
                "settle": update.settle,
            })),
        }),
    );
    Ok(value.to_string())
}

pub(crate) fn task_terminal_payload_matches_epoch(
    payload: &str,
    token: &leveler_core::OwnershipToken,
) -> bool {
    serde_json::from_str::<serde_json::Value>(payload)
        .ok()
        .is_some_and(|value| {
            value
                .get("_terminal_task_id")
                .and_then(serde_json::Value::as_str)
                == Some(token.task_id.as_str())
                && value
                    .get("_terminal_owner_epoch")
                    .and_then(serde_json::Value::as_u64)
                    == Some(token.owner_epoch.get())
        })
}

/// Result of an idempotent task-terminal commit.
///
/// When `inserted` is false, `event` is the terminal fact already committed
/// for the current epoch and must not be projected to observers again.
#[derive(Debug, Clone)]
pub struct TaskTerminalCommit {
    /// Canonical durable task-terminal event.
    pub event: EventRecord,
    /// Whether this call inserted the event and its projections.
    pub inserted: bool,
}

/// Goal projection supplied by a Harness for the owned task-terminal commit.
///
/// Storage applies these already-decided values mechanically. It does not
/// interpret task outcomes or decide whether the goal still owes work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalTerminalUpdate {
    /// Goal whose terminal work window is being recorded.
    pub goal_id: GoalId,
    /// Number of work windows completed by this invocation.
    pub windows_delta: u32,
    /// Whether the Harness decided that this goal owes no further work.
    pub settle: bool,
}

/// The engine-facing atomic terminal-commit contract.
#[async_trait]
pub trait TerminalStore: Send + Sync {
    /// Commit the session's terminal event and the whole terminal lifecycle
    /// (`outcome`, `verification`, `status`, `state`) atomically. Returns the
    /// appended event.
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
        verification: VerificationStatus,
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
    /// session projection + optional Harness-supplied goal projection in ONE
    /// transaction. A stale token or invalid goal rolls back everything.
    #[allow(clippy::too_many_arguments)]
    async fn finish_task_owned(
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
    ) -> Result<TaskTerminalCommit, crate::OwnershipError>;

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
        verification: VerificationStatus,
        status: SessionStatus,
        state: AgentState,
        now: Timestamp,
    ) -> Result<EventRecord, StorageError> {
        TerminalRepository::new(self)
            .finish_task(
                session_id,
                event_type,
                payload,
                outcome,
                verification,
                status,
                state,
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
        verification: VerificationStatus,
        status: SessionStatus,
        state: AgentState,
        goal: Option<&GoalTerminalUpdate>,
        now: Timestamp,
    ) -> Result<TaskTerminalCommit, crate::OwnershipError> {
        TerminalRepository::new(self)
            .finish_task_owned(
                token,
                session_id,
                event_type,
                payload,
                outcome,
                verification,
                status,
                state,
                goal,
                now,
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
    /// Goal rows participating in an optional task-terminal commit.
    goals: std::sync::OnceLock<std::sync::Arc<crate::MemoryGoalStore>>,
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
            goals: std::sync::OnceLock::new(),
            ownership: std::sync::OnceLock::new(),
            fail: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Couple to the shared ownership authority for fenced commits.
    pub fn with_ownership(self, state: std::sync::Arc<crate::MemoryOwnershipState>) -> Self {
        let _ = self.ownership.set(state);
        self
    }

    /// Couple to the memory goal rows used by optional terminal goal updates.
    pub fn with_goals(self, goals: std::sync::Arc<crate::MemoryGoalStore>) -> Self {
        let _ = self.goals.set(goals);
        self
    }

    /// Make every subsequent commit fail without writing.
    pub fn fail_commits(&self, fail: bool) {
        self.fail.store(fail, std::sync::atomic::Ordering::SeqCst);
    }

    /// The synchronous commit body shared by the fenced and unfenced task
    /// terminals: validate, "commit" (injected failure point), then apply
    /// projection + event. All fallible steps precede all writes.
    #[allow(clippy::too_many_arguments)]
    fn finish_task_sync(
        &self,
        token: Option<&leveler_core::OwnershipToken>,
        session_id: &SessionId,
        event_type: &str,
        payload: &str,
        outcome: TaskOutcome,
        verification: VerificationStatus,
        status: SessionStatus,
        state: AgentState,
        goal: Option<&GoalTerminalUpdate>,
        now: Timestamp,
    ) -> Result<TaskTerminalCommit, StorageError> {
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
        let payload = match token {
            Some(token) => task_terminal_payload_for_epoch(
                payload,
                session_id,
                token,
                outcome,
                verification,
                status,
                state,
                goal,
            )?,
            None => payload.to_string(),
        };
        if let Some(token) = token
            && let Some(event) = self.events.task_terminal_for_epoch(session_id, token)
        {
            if event.payload != payload {
                return Err(StorageError::InvalidData(format!(
                    "conflicting task terminal for ownership epoch {}",
                    token.owner_epoch.get()
                )));
            }
            return Ok(TaskTerminalCommit {
                event,
                inserted: false,
            });
        }
        let mut goal_rows = match goal {
            Some(update) => {
                let Some(token) = token else {
                    return Err(StorageError::InvalidData(
                        "terminal goal update requires owned task identity".to_string(),
                    ));
                };
                let Some(goals) = self.goals.get() else {
                    return Err(StorageError::InvalidData(
                        "memory terminal store has no goal store configured".to_string(),
                    ));
                };
                let rows = goals.rows.lock().unwrap();
                let Some(record) = rows.iter().find(|record| {
                    record.id == update.goal_id
                        && record.task_id == token.task_id
                        && record.state == crate::GoalState::Running
                }) else {
                    return Err(StorageError::InvalidData(format!(
                        "running goal {} not found for task {} terminal transition",
                        update.goal_id, token.task_id
                    )));
                };
                record
                    .windows_run
                    .checked_add(update.windows_delta)
                    .ok_or_else(|| {
                        StorageError::InvalidData(format!(
                            "goal {} work-window count overflow",
                            update.goal_id
                        ))
                    })?;
                Some(rows)
            }
            None => None,
        };
        self.check_injected_failure()?;
        let record = self
            .events
            .append_record_for_terminal_validated(session_id, None, event_type, &payload, now)?;
        {
            let mut rows = self.sessions.rows.lock().unwrap();
            if let Some(session) = rows.get_mut(session_id.as_str()) {
                session.outcome = Some(outcome);
                session.verification = Some(verification);
                session.status = status;
                session.state = state;
            }
        }
        if let (Some(rows), Some(update)) = (goal_rows.as_mut(), goal) {
            let record = rows
                .iter_mut()
                .find(|record| record.id == update.goal_id)
                .expect("terminal goal was validated while its rows lock was held");
            record.windows_run += update.windows_delta;
            if update.settle {
                record.state = crate::GoalState::Settled;
                record.settled_at = Some(now);
            }
        }
        Ok(TaskTerminalCommit {
            event: record,
            inserted: true,
        })
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
        let record = self.events.append_record_for_terminal_validated(
            session_id,
            Some(turn_id),
            event_type,
            payload,
            now,
        )?;
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
        verification: VerificationStatus,
        status: SessionStatus,
        state: AgentState,
        now: Timestamp,
    ) -> Result<EventRecord, StorageError> {
        self.finish_task_sync(
            None,
            session_id,
            event_type,
            payload,
            outcome,
            verification,
            status,
            state,
            None,
            now,
        )
        .map(|commit| commit.event)
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
        verification: VerificationStatus,
        status: SessionStatus,
        state: AgentState,
        goal: Option<&GoalTerminalUpdate>,
        now: Timestamp,
    ) -> Result<TaskTerminalCommit, crate::OwnershipError> {
        let Some(ownership) = self.ownership.get() else {
            return Err(crate::OwnershipError::Storage(StorageError::InvalidData(
                "memory terminal store has no ownership authority configured".to_string(),
            )));
        };
        // Ownership lock held across the WHOLE commit body — no CAS window.
        ownership
            .with_current(token, || {
                self.finish_task_sync(
                    Some(token),
                    session_id,
                    event_type,
                    payload,
                    outcome,
                    verification,
                    status,
                    state,
                    goal,
                    now,
                )
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
        EventStore, GoalState, GoalStore, MemoryEventStore, MemoryGoalStore, MemoryOwnershipState,
        MemoryOwnershipStore, MemorySessionStore, MemoryTurnStore, OwnershipStore, SessionRecord,
        SessionStore, TurnStore,
    };
    use leveler_core::{OwnerEpoch, RuntimeId, TaskId};
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
                    leveler_lifecycle::VerificationStatus::NotRun,
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
                leveler_lifecycle::VerificationStatus::NotRun,
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
    async fn memory_owned_terminal_commits_or_rejects_goal_projection_as_one_fact() {
        let ownership_state = Arc::new(MemoryOwnershipState::new());
        let session = SessionId::new("session-1");
        let task = TaskId::new(session.as_str());
        ownership_state.register_task(&task);
        let ownership = MemoryOwnershipStore::new(ownership_state.clone());
        let token = ownership
            .acquire(&task, &RuntimeId::new("test-runtime"), OwnerEpoch::UNOWNED)
            .await
            .unwrap();
        let sessions = Arc::new(MemorySessionStore::new().with_ownership(ownership_state.clone()));
        let turns = Arc::new(MemoryTurnStore::new().with_ownership(ownership_state.clone()));
        let events = Arc::new(MemoryEventStore::new().with_ownership(ownership_state.clone()));
        let goals = Arc::new(MemoryGoalStore::new().with_ownership(ownership_state.clone()));
        let terminal = MemoryTerminalStore::new(sessions.clone(), turns, events.clone())
            .with_goals(goals.clone())
            .with_ownership(ownership_state);
        let record = SessionRecord {
            id: session.as_str().to_string(),
            ..SessionRecord::new("/repo", "goal", "mock/m", leveler_core::now())
        };
        sessions.create(&record).await.unwrap();
        let goal = goals
            .open(&token, "the goal", leveler_core::now())
            .await
            .unwrap();
        let update = GoalTerminalUpdate {
            goal_id: goal.clone(),
            windows_delta: 2,
            settle: true,
        };

        terminal.fail_commits(true);
        assert!(
            terminal
                .finish_task_owned(
                    &token,
                    &session,
                    "task_finished",
                    "{}",
                    TaskOutcome::Completed,
                    VerificationStatus::NotRun,
                    SessionStatus::Completed,
                    AgentState::Complete,
                    Some(&update),
                    leveler_core::now(),
                )
                .await
                .is_err()
        );
        assert!(events.load(&session).await.unwrap().is_empty());
        assert_eq!(sessions.execution(&session).await.unwrap().unwrap().3, None);
        let stored = goals.get(&goal).await.unwrap().unwrap();
        assert_eq!(stored.windows_run, 0);
        assert_eq!(stored.state, GoalState::Running);

        terminal.fail_commits(false);
        terminal
            .finish_task_owned(
                &token,
                &session,
                "task_finished",
                "{}",
                TaskOutcome::Completed,
                VerificationStatus::NotRun,
                SessionStatus::Completed,
                AgentState::Complete,
                Some(&update),
                leveler_core::now(),
            )
            .await
            .unwrap();
        terminal
            .finish_task_owned(
                &token,
                &session,
                "task_finished",
                "{}",
                TaskOutcome::Completed,
                VerificationStatus::NotRun,
                SessionStatus::Completed,
                AgentState::Complete,
                Some(&update),
                leveler_core::now(),
            )
            .await
            .expect("an identical full terminal commit is idempotent");
        assert!(
            terminal
                .finish_task_owned(
                    &token,
                    &session,
                    "task_finished",
                    "{}",
                    TaskOutcome::Completed,
                    VerificationStatus::NotRun,
                    SessionStatus::Failed,
                    AgentState::Failed,
                    Some(&update),
                    leveler_core::now(),
                )
                .await
                .is_err(),
            "the same event body with a different lifecycle projection must conflict"
        );
        let different_goal_update = GoalTerminalUpdate {
            goal_id: goal.clone(),
            windows_delta: 3,
            settle: true,
        };
        assert!(
            terminal
                .finish_task_owned(
                    &token,
                    &session,
                    "task_finished",
                    "{}",
                    TaskOutcome::Completed,
                    VerificationStatus::NotRun,
                    SessionStatus::Completed,
                    AgentState::Complete,
                    Some(&different_goal_update),
                    leveler_core::now(),
                )
                .await
                .is_err(),
            "the same event body with a different goal projection must conflict"
        );
        let stored = goals.get(&goal).await.unwrap().unwrap();
        assert_eq!(stored.windows_run, 2);
        assert_eq!(stored.state, GoalState::Settled);
        assert!(stored.settled_at.is_some());
        assert_eq!(events.load(&session).await.unwrap().len(), 1);
        assert_eq!(
            sessions.execution(&session).await.unwrap().unwrap().3,
            Some(TaskOutcome::Completed)
        );
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
                    TaskOutcome::Completed,
                    leveler_lifecycle::VerificationStatus::NotRun,
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
                TaskOutcome::Completed,
                leveler_lifecycle::VerificationStatus::NotRun,
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
        assert_eq!(outcome, Some(TaskOutcome::Completed));
        assert_eq!(
            EventStore::load(events.as_ref(), &session)
                .await
                .unwrap()
                .len(),
            2
        );
    }
}
