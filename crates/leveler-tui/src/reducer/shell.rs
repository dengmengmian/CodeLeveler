//! Shell Mode input state and deterministic completion, kept out of the
//! global keymap so its event ownership is explicit:
//!
//! - an open candidate menu owns Tab/Enter/arrows/Esc;
//! - the composer owns ordinary text (and `!` only while empty + Normal);
//! - a running shell owns `x` under the Command focus, Ctrl+C cancellation and
//!   the clickable stop controls;
//! - permission cycling (Shift+Tab) is untouched when no menu is open.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::action::Effect;
use crate::shell_mode::{
    self, CompletionItem, CompletionResult, CompletionSource, InputMode, TabAction,
};
use crate::state::AppState;

/// Enter Shell Mode without inserting the marker into the command text.
pub(super) fn enter(state: &mut AppState) {
    state.input_mode = InputMode::Shell;
    state.completion.reset_on_edit();
}

/// Leave Shell Mode. The draft is preserved; only the mode marker changes.
pub(super) fn exit(state: &mut AppState) {
    state.input_mode = InputMode::Normal;
    state.completion.reset_on_edit();
}

/// The command was submitted: the composer returns to ordinary chat at once,
/// not when the execution finishes.
pub(super) fn reset_after_submit(state: &mut AppState) {
    state.input_mode = InputMode::Normal;
    state.history_mode = InputMode::Normal;
    state.completion.reset_on_edit();
}

/// Insert text that arrived from the composer's text/paste paths. `!` on an
/// empty Normal composer enters Shell Mode and is never part of the command.
pub(super) fn insert_text(state: &mut AppState, text: &str, paste: bool) -> Vec<Effect> {
    // The side thread is a plain conversation: `!` there is prose.
    if state.surface != crate::btw::SurfaceFocus::Btw
        && state.input_mode == InputMode::Normal
        && state.composer.is_empty()
        && let Some(rest) = text.strip_prefix('!')
    {
        enter(state);
        let rest = rest.strip_prefix(' ').unwrap_or(rest);
        if !rest.is_empty() {
            insert_raw(state, rest, paste);
        }
        return Vec::new();
    }
    insert_raw(state, text, paste);
    Vec::new()
}

fn insert_raw(state: &mut AppState, text: &str, paste: bool) {
    let shell = state.input_mode == InputMode::Shell;
    if !shell && super::insert_file_references(state, text) {
        super::submit::touch_slash_filter(state);
        return;
    }
    if paste || text.contains('\n') {
        state.composer.insert_paste(text);
    } else {
        state.composer.insert_str(text);
    }
    super::submit::touch_slash_filter(state);
}

/// Handle one key while the candidate menu is open. `Some` means the key was
/// consumed; `None` falls through to the normal keymap after closing the menu.
pub(super) fn handle_menu_key(state: &mut AppState, key: KeyEvent) -> Option<Vec<Effect>> {
    if !state.completion.open {
        return None;
    }
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let page = crate::render::SHELL_COMPLETION_MAX_ROWS;
    match key.code {
        KeyCode::Esc => {
            // Close AND invalidate: an in-flight Tab fetch must not reopen the
            // menu the user just dismissed.
            state.completion.reset_on_edit();
            Some(Vec::new())
        }
        KeyCode::Tab | KeyCode::BackTab if shift => {
            state.completion.move_selection(-1);
            Some(Vec::new())
        }
        KeyCode::Tab | KeyCode::Enter => Some(accept_selected(state)),
        KeyCode::Up => {
            state.completion.move_selection(-1);
            Some(Vec::new())
        }
        KeyCode::Down => {
            state.completion.move_selection(1);
            Some(Vec::new())
        }
        KeyCode::PageUp => {
            state.completion.page(-1, page);
            Some(Vec::new())
        }
        KeyCode::PageDown => {
            state.completion.page(1, page);
            Some(Vec::new())
        }
        // Any other key is ordinary editing/navigation: the candidate set is
        // stale from here on, so close and let the normal keymap run.
        _ => {
            state.completion.reset_on_edit();
            None
        }
    }
}

/// Request or reuse candidates for one Tab. Repeated Tab on the same draft
/// never starts a second scan.
pub(super) fn request_tab(state: &mut AppState, tab_armed: bool) -> Vec<Effect> {
    if state.input_mode != InputMode::Shell || state.composer.is_empty() {
        return Vec::new();
    }
    let text = state.composer.text().to_string();
    let cursor = state.composer.cursor_byte();
    if state
        .completion
        .cached
        .as_ref()
        .is_some_and(|r| r.request.text == text && r.request.cursor == cursor)
    {
        let result = state.completion.cached.take().expect("just checked");
        return apply_decision(state, result, tab_armed);
    }
    if state
        .completion
        .pending
        .as_ref()
        .is_some_and(|r| r.text == text && r.cursor == cursor)
    {
        return Vec::new();
    }
    let Some(cwd) = state.repository.clone() else {
        return Vec::new();
    };
    let request = state.completion.next_request(text, cursor, cwd);
    state.completion.tab_armed = tab_armed;
    vec![Effect::ShellComplete { request }]
}

/// Fold one completion response back. A response whose request identity no
/// longer matches the draft is dropped; stale results never rewrite new input.
pub(super) fn apply_result(state: &mut AppState, result: CompletionResult) -> Vec<Effect> {
    if state.input_mode != InputMode::Shell {
        return Vec::new();
    }
    let matches_pending = state
        .completion
        .pending
        .as_ref()
        .is_some_and(|request| request.id == result.request.id);
    if !matches_pending
        || state.composer.text() != result.request.text
        || state.composer.cursor_byte() != result.request.cursor
        || state.repository.as_deref() != Some(result.request.cwd.as_str())
    {
        return Vec::new();
    }
    state.completion.pending = None;
    let armed = state.completion.tab_armed;
    state.completion.tab_armed = false;
    apply_decision(state, result, armed)
}

fn apply_decision(state: &mut AppState, result: CompletionResult, armed: bool) -> Vec<Effect> {
    if !armed {
        // A refetch after an accept/fill: keep the set warm, show nothing. The
        // next Tab consumes it.
        state.completion.cached = Some(result);
        return Vec::new();
    }
    let text = state.composer.text().to_string();
    let cursor = state.composer.cursor_byte();
    match shell_mode::tab_decision(&result, &text, cursor) {
        TabAction::Nothing => {
            state.completion.close();
            Vec::new()
        }
        TabAction::Accept(index) => accept_item(state, &result, index),
        TabAction::Fill(range, fill) => {
            state
                .composer
                .replace_byte_range(range.start, range.end, &fill);
            state.completion.close();
            super::submit::touch_slash_filter(state);
            request_tab(state, false)
        }
        TabAction::Open => {
            state.completion.items = result.items;
            state.completion.truncated = result.truncated;
            state.completion.request = Some(result.request);
            state.completion.open = true;
            state.completion.selected = 0;
            state.completion.hovered = None;
            Vec::new()
        }
    }
}

pub(super) fn accept_selected(state: &mut AppState) -> Vec<Effect> {
    let Some(result) = state
        .completion
        .request
        .clone()
        .map(|request| CompletionResult {
            request,
            items: state.completion.items.clone(),
            truncated: state.completion.truncated,
        })
    else {
        state.completion.close();
        return Vec::new();
    };
    accept_item(state, &result, state.completion.selected)
}

/// Accept one candidate: replace exactly its token range, never execute and
/// never rewrite the rest of the line. A directory accept refetches silently so
/// the next Tab drills into it.
fn accept_item(state: &mut AppState, result: &CompletionResult, index: usize) -> Vec<Effect> {
    let Some(item) = result.items.get(index) else {
        state.completion.close();
        return Vec::new();
    };
    let valid = valid_item(state.composer.text(), state.composer.cursor_byte(), item);
    if !valid {
        state.completion.close();
        return Vec::new();
    }
    state
        .composer
        .replace_byte_range(item.replace_start, item.replace_end, &item.insert);
    let directory = item.source == CompletionSource::Directory;
    state.completion.close();
    state.completion.cached = None;
    state.completion.pending = None;
    super::submit::touch_slash_filter(state);
    if directory {
        request_tab(state, false)
    } else {
        Vec::new()
    }
}

fn valid_item(text: &str, cursor: usize, item: &CompletionItem) -> bool {
    item.replace_start <= item.replace_end
        && item.replace_end == cursor
        && item.replace_end <= text.len()
        && text.is_char_boundary(item.replace_start)
        && text.is_char_boundary(item.replace_end)
}

/// Keep the composer mode in step with history browsing: a recalled `! ` entry
/// restores Shell Mode, a recalled chat entry restores Normal, and returning to
/// the live draft restores the mode that draft was written in.
pub(super) fn after_history_move(state: &mut AppState, was_browsing: bool) {
    if state.composer.is_browsing_history() {
        if state.composer.text().starts_with('!') {
            let _ = state.composer.strip_browsed_prefix("! ");
            let _ = state.composer.strip_browsed_prefix("!");
            state.input_mode = InputMode::Shell;
        } else {
            state.input_mode = InputMode::Normal;
        }
    } else if was_browsing {
        state.input_mode = state.history_mode;
    }
    state.completion.reset_on_edit();
}
