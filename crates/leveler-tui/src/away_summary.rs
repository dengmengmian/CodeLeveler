//! One-shot post-turn idle recap scheduling.
//!
//! The TUI owns user activity and therefore the idle clock. The runtime owns
//! generation. This module only turns a completed, sufficiently substantial
//! conversation into one protocol request, cancelled by any input.

use std::time::{Duration, Instant};

use leveler_client_protocol::ClientCommand;

use crate::action::Effect;
use crate::state::AppState;

pub const IDLE_DELAY: Duration = Duration::from_secs(3 * 60);

pub fn arm(state: &mut AppState, now: Instant) {
    state.away_summary_pending = false;
    state.away_summary_due_at = state
        .transcript
        .has_away_summary_context()
        .then_some(now + IDLE_DELAY);
}

pub fn cancel(state: &mut AppState) {
    state.away_summary_due_at = None;
    state.away_summary_pending = false;
}

pub fn poll(state: &mut AppState, now: Instant) -> Option<Effect> {
    let due = state.away_summary_due_at?;
    if now < due {
        return None;
    }
    // One-shot even when the UI is no longer eligible. A new successful turn
    // may arm a fresh window; an old deadline never leaks across activity.
    state.away_summary_due_at = None;
    if state.is_busy() || !state.composer.is_empty() || !state.pending_attachments.is_empty() {
        return None;
    }
    state.away_summary_pending = true;
    Some(Effect::Send(ClientCommand::RequestAwaySummary {
        session_id: state.session_id.clone(),
    }))
}
