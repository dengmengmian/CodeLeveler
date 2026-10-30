//! Keyboard and pointer scrolling for one user-shell output window.
//!
//! The window's VIEW lives on the transcript record
//! ([`crate::transcript::ShellOutputView`]); the layout lives in
//! [`crate::shell_render::output_window`]. This module is the only place that
//! joins them: it computes the clamp from the same projection the renderer
//! paints, then asks the transcript to move. No second layout, no second scroll
//! owner — a wheel over a record can never scroll the Conversation and a
//! Conversation scroll never moves a record's window.

use leveler_core::UserShellId;

use crate::state::AppState;
use crate::transcript::TranscriptItem;

/// Net cells between the rail and the widest output row.
fn output_room(state: &AppState) -> usize {
    crate::render::user_shell_output_room(crate::conversation::geometry::content_width(state))
}

/// Whether the record has more rows than its window can show.
fn max_scroll_for(state: &AppState, id: &UserShellId) -> Option<usize> {
    let shell = state.transcript.user_shell(id)?;
    let total =
        crate::shell_render::output_rows(&shell.output, &shell.output_marks, output_room(state))
            .len();
    (total > crate::shell_render::output_window_height(total))
        .then(|| crate::shell_render::output_max_scroll(total))
}

/// Give the keyboard to one record's window. Returns whether focus moved.
pub(super) fn focus(state: &mut AppState, id: UserShellId) -> bool {
    if state.shell_output_focus.as_ref() == Some(&id) {
        return false;
    }
    state.shell_output_focus = Some(id);
    true
}

/// Drop the window focus (Esc, typing, another focus, the record disappearing).
pub(super) fn release(state: &mut AppState) -> bool {
    state.shell_output_focus.take().is_some()
}

/// Scroll the focused window. Returns false when nothing owns the keys, so the
/// caller can let the key fall through to its ordinary meaning.
pub(super) fn scroll_focused(state: &mut AppState, delta: isize) -> bool {
    let Some(id) = state.shell_output_focus.clone() else {
        return false;
    };
    if state.transcript.user_shell(&id).is_none() {
        // The record this focus named is gone (the transcript was replaced).
        // A stale focus must never keep swallowing keys.
        release(state);
        return false;
    }
    let Some(max) = max_scroll_for(state, &id) else {
        // Focused but not scrollable: the key belongs to the record, not to the
        // Conversation underneath it, so it is consumed and does nothing.
        return true;
    };
    state.transcript.scroll_user_shell_output(&id, delta, max);
    true
}

/// Page the focused window by its own height.
pub(super) fn page_focused(state: &mut AppState, direction: isize) -> bool {
    let page = (crate::shell_render::SHELL_OUTPUT_WINDOW_ROWS.saturating_sub(1)).max(1) as isize;
    scroll_focused(state, direction * page)
}

/// Jump the focused window to its first (`top`) or newest row.
pub(super) fn edge_focused(state: &mut AppState, top: bool) -> bool {
    let Some(id) = state.shell_output_focus.clone() else {
        return false;
    };
    if state.transcript.user_shell(&id).is_none() {
        release(state);
        return false;
    }
    let max = max_scroll_for(state, &id).unwrap_or(0);
    state.transcript.send_user_shell_output_to(&id, top, max);
    true
}

/// The id of the record painted at screen row `row`, when it is a user shell
/// with a scrollable window.
pub(super) fn scrollable_at(state: &AppState, row: u16) -> Option<UserShellId> {
    let pos = crate::conversation::geometry::screen_to_content(state, state.size.0 / 2, row)?;
    let width = crate::conversation::geometry::content_width(state);
    let item = state.item_at_line(pos.row, width)?;
    let TranscriptItem::UserShell(shell) = state.transcript.items().get(item)? else {
        return None;
    };
    max_scroll_for(state, &shell.id).map(|_| shell.id.clone())
}

/// A wheel event over a record scrolls that record's window and nothing else.
pub(super) fn scroll_at_pointer(state: &mut AppState, row: u16, delta: isize) -> bool {
    let Some(id) = scrollable_at(state, row) else {
        return false;
    };
    let Some(max) = max_scroll_for(state, &id) else {
        return false;
    };
    focus(state, id.clone());
    state.transcript.scroll_user_shell_output(&id, delta, max);
    true
}

/// A click on a record's header gives its window the keyboard, exactly like a
/// click on a background task's console gives that pane the keys.
pub(super) fn focus_item(state: &mut AppState, item: usize) -> bool {
    let Some(TranscriptItem::UserShell(shell)) = state.transcript.items().get(item) else {
        return false;
    };
    let id = shell.id.clone();
    let focuses = focus(state, id);
    state.workbench_focus = crate::state::WorkbenchFocus::Conversation;
    focuses
}
