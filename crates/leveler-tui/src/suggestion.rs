//! The contextual prompt suggestion: the structured next step the runtime
//! already produced, offered as ghost text the user accepts with Tab.
//!
//! It is **presentation state, never input**. It lives beside the composer, not
//! inside it, so it can never reach `canonical_text()`, the persisted draft,
//! the input history, the `$EDITOR` seed, slash/`@file` parsing, or a
//! `UserMessage`. Only [`accept`] turns it into real text, and only because the
//! user pressed Tab.

use leveler_client_protocol::RuntimeStatus;
use unicode_segmentation::UnicodeSegmentation;

use crate::screen::Screen;
use crate::state::{AppState, WorkbenchFocus};

/// Longest suggestion we will ghost, in graphemes. `latest_turn_handoff`
/// already caps `next_step`; this is the display-side guard so an unusually
/// long structured step cannot own the whole input row.
const MAX_GRAPHEMES: usize = 120;

/// Fold a candidate into a one-line suggestion, or reject it.
///
/// Newlines and whitespace runs collapse to single spaces so the ghost cannot
/// grow the composer, and the cut is by grapheme so a CJK character or an emoji
/// cluster is never halved.
pub fn normalize(text: &str) -> Option<String> {
    let single = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if single.is_empty() {
        return None;
    }
    let mut out: String = single.graphemes(true).take(MAX_GRAPHEMES).collect();
    if single.graphemes(true).count() > MAX_GRAPHEMES {
        out.push('…');
    }
    Some(out)
}

/// Offer `text` as the next-step suggestion.
///
/// A user draft always wins: with text in the composer we neither overwrite it
/// nor park a suggestion that would ambush the user the moment they clear the
/// line.
pub fn offer(state: &mut AppState, text: &str) {
    state.prompt_suggestion = if state.composer.is_empty() {
        normalize(text)
    } else {
        None
    };
}

/// Destroy the suggestion. Called from every path where the user has said
/// something else with their hands (typing, pasting, deleting, submitting,
/// browsing history, opening `$EDITOR`, Esc) and at every turn/session
/// boundary.
pub fn clear(state: &mut AppState) {
    state.prompt_suggestion = None;
}

/// The single definition of "the ghost is on screen".
///
/// Both the reducer (Tab / Esc) and the renderer read this, so there is exactly
/// one answer to whether Tab means *accept* or *switch focus*.
pub fn is_visible(state: &AppState) -> bool {
    state.prompt_suggestion.is_some()
        && state.active_screen == Screen::Conversation
        && state.status == RuntimeStatus::Idle
        && state.runtime_connected
        && state.workbench_focus == WorkbenchFocus::Input
        && state.composer.is_empty()
        && !state.composer.is_browsing_history()
        && state.overlay.is_none()
        && state.turn_nav.is_none()
        && crate::screen::visible_slash_popup(state).is_empty()
        && crate::screen::visible_file_popup(state).is_empty()
}

/// Tab: move the suggestion into the real buffer and stop there.
///
/// No command, no submission, no history entry — the user still has to press
/// Enter, and until they do this is an ordinary draft they can edit or erase.
pub fn accept(state: &mut AppState) {
    if let Some(text) = state.prompt_suggestion.take() {
        state.composer.replace(text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_folds_newlines_and_trims() {
        assert_eq!(
            normalize("  查看 demo-order-01\n的里程碑进度 ").as_deref(),
            Some("查看 demo-order-01 的里程碑进度")
        );
    }

    #[test]
    fn normalize_rejects_blank() {
        assert_eq!(normalize(""), None);
        assert_eq!(normalize("   \n\t "), None);
    }

    #[test]
    fn normalize_cuts_on_grapheme_boundaries() {
        let long = "提".repeat(MAX_GRAPHEMES + 40);
        let out = normalize(&long).expect("non-empty");
        assert_eq!(
            out.graphemes(true).count(),
            MAX_GRAPHEMES + 1,
            "plus ellipsis"
        );
        assert!(out.ends_with('…'));

        // A ZWJ emoji cluster survives as one unit.
        let family = "👨‍👩‍👧";
        let out = normalize(&family.repeat(MAX_GRAPHEMES + 5)).expect("non-empty");
        assert!(out.starts_with(family));
    }
}
