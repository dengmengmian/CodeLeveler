//! When to ask for the one-line turn recap.
//!
//! The recap is a transcript row. The next-step suggestion is ghost text in
//! the composer. They are separate model calls and neither one cancels the
//! other. A new turn drops a recap that has not arrived yet. Typing does not:
//! the row is about the turn that just finished, not about the draft.

use leveler_client_protocol::ClientCommand;

use crate::action::Effect;
use crate::state::AppState;
use crate::transcript::TranscriptItem;

/// Ask for a recap of the conversation now on screen.
///
/// Skips when one is already in flight, or when this turn already showed a
/// structured goal recap. Those are the same `※ 回顾` row; a second call would
/// only repeat it.
pub fn request(state: &mut AppState) -> Option<Effect> {
    if state.away_summary_pending || state.is_busy() {
        return None;
    }
    if matches!(
        state.transcript.items().last(),
        Some(TranscriptItem::Recap(_))
    ) {
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
