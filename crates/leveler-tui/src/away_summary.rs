//! When to ask for the one-line turn recap.
//!
//! The recap is a transcript row. The next-step suggestion is ghost text in
//! the composer. They are separate model calls and neither one cancels the
//! other. A new turn drops a recap that has not arrived yet. Typing does not:
//! the row is about the turn that just finished, not about the draft.

use leveler_client_protocol::ClientCommand;

use crate::action::Effect;
use crate::state::AppState;

/// Ask for a recap of the conversation now on screen.
///
/// Skips when one is already in flight, or when this turn already showed a
/// structured goal recap. Those are the same `※ 回顾` row; a second call would
/// only repeat it.
pub fn request(state: &mut AppState) -> Option<Effect> {
    if state.away_summary_pending || state.is_busy() {
        return None;
    }
    // A turn that already has a recap — the structured goal recap the turn
    // terminal pushed, or a review replayed from history — must not ask a
    // second time. Rows that belong to the same turn (a review card, a shell
    // execution, a note) can follow the recap, so identity is the current
    // turn segment, not the last item.
    if state.transcript.current_turn_has_recap() {
        return None;
    }
    state.away_summary_pending = true;
    Some(Effect::Send(ClientCommand::RequestAwaySummary {
        session_id: state.session_id.clone(),
    }))
}

pub fn cancel(state: &mut AppState) {
    state.away_summary_pending = false;
}
