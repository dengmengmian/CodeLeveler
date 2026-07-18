use crossterm::event::{KeyCode, KeyEvent};

use leveler_client_protocol::ClientCommand;

use crate::action::Effect;
use crate::screen::Screen;
use crate::state::AppState;

fn page_size(state: &AppState) -> usize {
    // Rough page: the screen viewport minus header/status/composer chrome.
    (state.size.1 as usize).saturating_sub(4).max(1)
}

/// Scroll keys shared by the full-screen views' content panes.
fn scroll_screen_key(state: &mut AppState, key: &KeyEvent, line_keys: bool) -> bool {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') if line_keys => {
            state.screen_scroll = state.screen_scroll.saturating_sub(1);
            true
        }
        KeyCode::Down | KeyCode::Char('j') if line_keys => {
            state.screen_scroll = state.screen_scroll.saturating_add(1);
            true
        }
        KeyCode::PageUp => {
            state.screen_scroll = state.screen_scroll.saturating_sub(page_size(state));
            true
        }
        KeyCode::PageDown => {
            state.screen_scroll = state.screen_scroll.saturating_add(page_size(state));
            true
        }
        _ => false,
    }
}

/// Handle keys for a full-screen view (Tools today). Esc returns to the
/// conversation .
pub(super) fn handle_screen_key(state: &mut AppState, key: KeyEvent) -> Vec<Effect> {
    match state.active_screen {
        Screen::Tools => {
            let len = state
                .transcript
                .tool_calls()
                .into_iter()
                .filter(|b| state.tools_screen.filter.matches(b))
                .count();
            match key.code {
                KeyCode::Esc => close_screen(state),
                KeyCode::Up | KeyCode::Char('k') => {
                    state.tools_screen.move_up();
                    state.screen_scroll = 0;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    state.tools_screen.move_down(len);
                    state.screen_scroll = 0;
                }
                KeyCode::Tab | KeyCode::Char('f') => {
                    state.tools_screen.cycle_filter();
                    state.screen_scroll = 0;
                }
                // PageUp/PageDown scroll the detail pane.
                _ => {
                    scroll_screen_key(state, &key, false);
                }
            }
        }
        Screen::Diff => {
            let len = state.diff.as_ref().map(|d| d.files.len()).unwrap_or(0);
            match key.code {
                KeyCode::Esc => close_screen(state),
                KeyCode::Up | KeyCode::Char('k') => {
                    state.diff_selected = state.diff_selected.saturating_sub(1);
                    state.screen_scroll = 0;
                }
                KeyCode::Down | KeyCode::Char('j') if len > 0 && state.diff_selected + 1 < len => {
                    state.diff_selected += 1;
                    state.screen_scroll = 0;
                }
                // PageUp/PageDown scroll the patch pane.
                _ => {
                    scroll_screen_key(state, &key, false);
                }
            }
        }
        Screen::Sessions => {
            let len = state.sessions.len();
            match key.code {
                KeyCode::Esc => close_screen(state),
                KeyCode::Up | KeyCode::Char('k') => {
                    state.sessions_selected = state.sessions_selected.saturating_sub(1)
                }
                KeyCode::Down | KeyCode::Char('j')
                    if len > 0 && state.sessions_selected + 1 < len =>
                {
                    state.sessions_selected += 1;
                }
                KeyCode::Enter => {
                    if let Some(s) = state.sessions.get(state.sessions_selected) {
                        let id = s.id.clone();
                        let requester_session_id = state.session_id.clone();
                        close_screen(state);
                        return vec![Effect::Send(ClientCommand::OpenSessionFor {
                            requester_session_id,
                            session_id: id,
                        })];
                    }
                }
                KeyCode::Char('d') => {
                    if let Some(s) = state.sessions.get(state.sessions_selected) {
                        return vec![Effect::Send(ClientCommand::DeleteSessionFor {
                            requester_session_id: state.session_id.clone(),
                            session_id: s.id.clone(),
                        })];
                    }
                }
                _ => {}
            }
        }
        Screen::Plan | Screen::Verification | Screen::Context | Screen::Agents | Screen::Help => {
            if key.code == KeyCode::Esc {
                close_screen(state);
            } else {
                scroll_screen_key(state, &key, true);
            }
        }
        Screen::Conversation => {}
    }
    Vec::new()
}

/// Return to the conversation, dropping the view's scroll offset.
fn close_screen(state: &mut AppState) {
    state.active_screen = Screen::Conversation;
    state.screen_scroll = 0;
}

/// Toggle a screen on/off (returns to the conversation if already active).
pub(super) fn toggle_screen(state: &mut AppState, screen: Screen) -> Vec<Effect> {
    state.screen_scroll = 0;
    state.active_screen = if state.active_screen == screen {
        Screen::Conversation
    } else {
        screen
    };
    Vec::new()
}

/// Open the Diff screen and request a fresh diff (with patches) from the runtime.
pub(super) fn open_diff_screen(state: &mut AppState) -> Vec<Effect> {
    if state.active_screen == Screen::Diff {
        state.active_screen = Screen::Conversation;
        return Vec::new();
    }
    state.active_screen = Screen::Diff;
    state.diff_selected = 0;
    vec![Effect::Send(ClientCommand::RequestDiff {
        session_id: state.session_id.clone(),
    })]
}

/// Open the Sessions screen and refresh the list (spec §52).
pub(super) fn open_sessions_screen(state: &mut AppState) -> Vec<Effect> {
    if state.active_screen == Screen::Sessions {
        state.active_screen = Screen::Conversation;
        return Vec::new();
    }
    state.active_screen = Screen::Sessions;
    state.sessions_selected = 0;
    vec![Effect::Send(ClientCommand::RequestSessionListFor {
        requester_session_id: state.session_id.clone(),
    })]
}
