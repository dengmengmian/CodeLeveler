//! Recovery of turns left running by an unclean process exit.

use leveler_core::{SessionId, TurnId};
use leveler_storage::{TerminalStore, TurnStore};

use crate::{EngineError, EngineEvent, TurnOutcome};

/// Interrupt every orphan running turn in scope. Each row transition and its
/// canonical event commit atomically (the terminal port's contract) before
/// the observer is notified; a commit failure propagates — a turn is never
/// *assumed* interrupted.
pub async fn reap_running_turns(
    turns: &dyn TurnStore,
    terminal: &dyn TerminalStore,
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
            .finish_turn(
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

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_storage::{
        MemoryEventStore, MemorySessionStore, MemoryTerminalStore, MemoryTurnStore,
    };
    use std::sync::Arc;

    /// Failure injection D: when the terminal commit fails, the reaper must
    /// propagate the error and the turn must NOT be assumed interrupted — it
    /// stays visibly `running` for the next recovery attempt.
    #[tokio::test]
    async fn a_failed_terminal_commit_never_guesses_a_turn_interrupted() {
        let sessions = Arc::new(MemorySessionStore::new());
        let turns = Arc::new(MemoryTurnStore::new());
        let events = Arc::new(MemoryEventStore::new());
        let terminal = MemoryTerminalStore::new(sessions.clone(), turns.clone(), events.clone());

        let session = SessionId::new("s1");
        let orphan = turns
            .start(&session, "user", None, leveler_core::now())
            .await
            .unwrap();

        terminal.fail_commits(true);
        let result = reap_running_turns(turns.as_ref(), &terminal, None).await;
        assert!(result.is_err(), "a failed commit must propagate");
        let still_running = turns.list_running(None).await.unwrap();
        assert_eq!(
            still_running
                .iter()
                .map(|t| t.id.as_str())
                .collect::<Vec<_>>(),
            vec![orphan.id.as_str()],
            "the turn must stay running — never guessed interrupted"
        );
        assert!(
            leveler_storage::EventStore::load(events.as_ref(), &session)
                .await
                .unwrap()
                .is_empty(),
            "no canonical event without its projection"
        );

        // Once the store recovers, the same reap succeeds.
        terminal.fail_commits(false);
        let reaped = reap_running_turns(turns.as_ref(), &terminal, None)
            .await
            .unwrap();
        assert_eq!(reaped.len(), 1);
        assert!(turns.list_running(None).await.unwrap().is_empty());
    }
}
