//! Recovery of turns left running by an unclean process exit — now
//! ownership-aware: reaping is an authoritative write, so it requires a
//! current token, and a runtime never touches another runtime's task.

use leveler_core::{OwnershipToken, RuntimeId, SessionId, TurnId};
use leveler_storage::{EngineStores, TerminalStore, TurnStore};

use crate::{EngineError, EngineEvent, TurnOutcome};

/// Reap the scope's orphan running turns with an ALREADY-HELD current token
/// (the engine's in-run path). Each row transition commits atomically and
/// fenced; a commit failure propagates — a turn is never *assumed*
/// interrupted.
pub async fn reap_running_turns_owned(
    turns: &dyn TurnStore,
    terminal: &dyn TerminalStore,
    token: &OwnershipToken,
    session_id: Option<&SessionId>,
) -> Result<Vec<EngineEvent>, EngineError> {
    let running = turns.list_running(session_id).await?;
    let mut events = Vec::with_capacity(running.len());
    for turn in &running {
        let session_id = SessionId::new(turn.session_id.clone());
        let turn_id = TurnId::new(turn.id.clone());
        let event = EngineEvent::TurnFinished {
            stop: None,
            turn_id: turn_id.clone(),
            outcome: TurnOutcome::Interrupted,
            stop_reason: "unclean process exit".to_string(),
            rounds: 0,
            modified_files: Vec::new(),
        };
        let (event_type, payload) = event.to_row()?;
        terminal
            .finish_turn_owned(
                token,
                &session_id,
                &turn_id,
                &event_type,
                &payload,
                TurnOutcome::Interrupted,
                leveler_core::now(),
            )
            .await?;
        events.push(event);
    }
    Ok(events)
}

/// A task the restart reaper refused to touch because another runtime owns it.
#[derive(Debug)]
pub struct ReapConflict {
    pub session_id: SessionId,
    pub owner: Option<RuntimeId>,
}

/// What a restart reap did: the reaped events, plus every foreign-owned task
/// it explicitly left alone (callers report these; silence would hide a
/// split-ownership situation).
#[derive(Debug, Default)]
pub struct ReapOutcome {
    pub events: Vec<EngineEvent>,
    pub conflicts: Vec<ReapConflict>,
}

/// Daemon-restart recovery: for every session with orphan running turns,
/// explicitly REACQUIRE ownership (same runtime or unowned → CAS to a fresh
/// epoch, fencing any token from the previous incarnation) and reap under the
/// new token. A task owned by a DIFFERENT runtime is never reaped or mutated —
/// it is reported as a conflict.
pub async fn reap_after_restart(
    stores: &EngineStores,
    runtime_id: &RuntimeId,
    session_id: Option<&SessionId>,
) -> Result<ReapOutcome, EngineError> {
    let running = stores.turns.list_running(session_id).await?;
    let mut sessions: Vec<String> = running.iter().map(|t| t.session_id.clone()).collect();
    sessions.dedup();
    let mut outcome = ReapOutcome::default();
    for session in sessions {
        let session = SessionId::new(session);
        let task_id = stores
            .tasks
            .ensure_for_session(&session, leveler_core::now())
            .await?;
        let current = stores
            .ownership
            .current(&task_id)
            .await?
            .ok_or_else(|| EngineError::Config(format!("no task row for session {session}")))?;
        if let Some(owner) = &current.runtime
            && owner != runtime_id
        {
            outcome.conflicts.push(ReapConflict {
                session_id: session,
                owner: Some(owner.clone()),
            });
            continue;
        }
        let token = stores
            .ownership
            .acquire(&task_id, runtime_id, current.epoch)
            .await?;
        let events = reap_running_turns_owned(
            stores.turns.as_ref(),
            stores.terminal.as_ref(),
            &token,
            Some(&session),
        )
        .await?;
        outcome.events.extend(events);
    }
    Ok(outcome)
}
