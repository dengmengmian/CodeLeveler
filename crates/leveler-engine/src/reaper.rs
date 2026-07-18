//! Recovery of turns left running by an unclean process exit.

use leveler_core::{SessionId, TurnId};
use leveler_storage::{Database, TerminalRepository, TurnRepository};

use crate::{EngineError, EngineEvent, TurnOutcome};

/// Interrupt every orphan running turn in scope. Each row transition and its
/// canonical event commit atomically before the observer is notified.
pub async fn reap_running_turns(
    db: &Database,
    session_id: Option<&SessionId>,
) -> Result<Vec<EngineEvent>, EngineError> {
    let turns = TurnRepository::new(db).list_running(session_id).await?;
    let mut events = Vec::with_capacity(turns.len());
    for turn in &turns {
        let session_id = SessionId::new(turn.session_id.clone());
        let turn_id = TurnId::new(turn.id.clone());
        let event = EngineEvent::TurnFinished {
            turn_id: turn_id.clone(),
            outcome: TurnOutcome::Interrupted,
            stop_reason: "unclean process exit".to_string(),
            rounds: 0,
            modified_files: Vec::new(),
        };
        let (event_type, payload) = event.to_row()?;
        TerminalRepository::new(db)
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
