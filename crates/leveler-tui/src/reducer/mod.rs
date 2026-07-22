//! The reducer: `(AppState, Action) -> [Effect]`, pure over state.

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use leveler_client_protocol::{ClientCommand, NotificationLevel, RuntimeEvent};

use crate::action::{Action, Effect, EffectCompletion};
use crate::screen::Screen;
use crate::state::{AppState, Notification, WorkbenchFocus};

pub mod overlay_keys;
mod runtime_apply;
mod screen_nav;
mod submit;

pub use submit::drain_queued;

use overlay_keys::{handle_overlay_key, open_model_picker};
use runtime_apply::apply_runtime;
use screen_nav::{handle_screen_key, open_diff_screen, open_sessions_screen, toggle_screen};
use submit::{
    complete_file_mention, complete_slash, request_file_candidates, submit, touch_slash_filter,
};

const QUIT_CONFIRM_MESSAGE: &str = "再按一次 Ctrl+C 退出";

/// Fold an action into state, returning side effects for the event loop.
pub fn reduce(state: &mut AppState, action: Action) -> Vec<Effect> {
    match action {
        Action::Runtime(event) => {
            apply_runtime(state, event);
            Vec::new()
        }
        Action::Resize(cols, rows) => {
            state.size = (cols, rows);
            Vec::new()
        }
        Action::FileCandidatesLoaded(files) => {
            state.file_candidates = files;
            Vec::new()
        }
        Action::EffectCompleted(completion) => {
            apply_effect_completion(state, completion);
            Vec::new()
        }
        Action::WebLaunched(result) => {
            state.web_starting = false;
            match result {
                Ok(url) => {
                    state.notification = Some(Notification {
                        level: NotificationLevel::Info,
                        message: format!("Web UI 已启动：{url}"),
                    });
                    state.web_url = Some(url);
                }
                Err(message) => {
                    state.notification = Some(Notification {
                        level: NotificationLevel::Warning,
                        message: format!("Web UI 启动失败：{message}"),
                    });
                }
            }
            Vec::new()
        }
        Action::Paste(text) => {
            state.disarm_ctrlc();
            // Only a truly empty payload is treated as a clipboard image
            // (bracketed paste cannot carry image bytes). Whitespace-only
            // pastes are real text and must not be discarded as images.
            if text.is_empty() {
                vec![Effect::Send(ClientCommand::AddClipboardImage {
                    session_id: state.session_id.clone(),
                })]
            } else {
                state.composer.insert_paste(&text);
                touch_slash_filter(state);
                request_file_candidates(state)
            }
        }
        Action::TextInput(text) => {
            state.disarm_ctrlc();
            clear_quit_confirm_notification(state);
            if !text.is_empty()
                && state.overlay.is_none()
                && state.active_screen == Screen::Conversation
            {
                // Typing always claims Input focus (parity with the single-key
                // Char path). Without this, coalesced typing bursts insert but
                // leave focus on the Conversation after a mouse click/scroll, so
                // the composer stays muted and ↑/↓ keep scrolling — typing feels
                // dead even though the text landed.
                state.workbench_focus = WorkbenchFocus::Input;
                // PTY paste without bracketed-paste arrives as a key burst;
                // still fold large multi-line blobs like Action::Paste.
                if text.contains('\n') {
                    state.composer.insert_paste(&text);
                } else {
                    state.composer.insert_str(&text);
                }
                touch_slash_filter(state);
            }
            request_file_candidates(state)
        }
        Action::Key(key) => handle_key(state, key),
        Action::Mouse(mouse) => handle_mouse(state, mouse),
        Action::SelectionTick => handle_selection_tick(state),
    }
}

fn apply_effect_completion(state: &mut AppState, completion: EffectCompletion) {
    match completion {
        EffectCompletion::CommandDelivered => {}
        EffectCompletion::CommandFailed { snapshot } => {
            if let Some(snapshot) = snapshot {
                apply_runtime(state, RuntimeEvent::SessionOpened { session: *snapshot });
            } else {
                // Delivery is uncertain: the runtime may already be executing.
                // Fail closed until reconnect/snapshot establishes authority;
                // never expose Idle and permit a duplicate turn.
                state.runtime_connected = false;
            }
            state.notification = Some(Notification {
                level: NotificationLevel::Error,
                message: "无法连接运行时，命令交付状态未知".to_string(),
            });
        }
        EffectCompletion::InteractionDelivered { key } => {
            state.interaction_command_ids.remove(&key);
            overlay_keys::advance_overlay(state);
        }
        EffectCompletion::InteractionUncertain {
            key,
            restore,
            snapshot,
        } => {
            let still_pending = snapshot
                .as_ref()
                .is_none_or(|snap| snapshot_awaits_interaction(snap, &key));
            if still_pending {
                overlay_keys::restore_interaction_overlay(state, restore);
                state.notification = Some(Notification {
                    level: NotificationLevel::Error,
                    message: "交付状态未知，审批/澄清仍在等待，请重试（将复用同一命令）"
                        .to_string(),
                });
            } else {
                state.interaction_command_ids.remove(&key);
                overlay_keys::advance_overlay(state);
                state.notification = Some(Notification {
                    level: NotificationLevel::Info,
                    message: "连接异常，但运行时已处理该审批/澄清，已同步状态".to_string(),
                });
            }
        }
    }
}

fn snapshot_awaits_interaction(
    snap: &leveler_client_protocol::UiSessionSnapshot,
    key: &str,
) -> bool {
    snap.pending_interactions
        .iter()
        .any(|pending| match pending {
            leveler_client_protocol::UiPendingInteraction::Approval(request) => {
                format!("a:{}", request.id.as_str()) == key
            }
            leveler_client_protocol::UiPendingInteraction::Clarification(request) => {
                format!("c:{}", request.id.as_str()) == key
            }
        })
}

/// Rows at top/bottom of Conversation that trigger continuous scroll while dragging.
const SELECTION_EDGE_ROWS: u16 = 2;

/// Mouse: wheel scrolls Conversation; drag selects text with edge auto-scroll; release copies.
///
/// `Shift`+mouse is ignored so the terminal can offer native selection as a fallback
/// when mouse capture is not fully exclusive.
fn handle_mouse(state: &mut AppState, mouse: MouseEvent) -> Vec<Effect> {
    if state.overlay.is_some() || state.active_screen != Screen::Conversation {
        return Vec::new();
    }
    // Shift+drag: do not capture selection — leave room for terminal-native select.
    if mouse.modifiers.contains(KeyModifiers::SHIFT) {
        return Vec::new();
    }
    state.disarm_ctrlc();
    clear_quit_confirm_notification(state);

    let over_input = point_in_rect(mouse.column, mouse.row, state.input_rect);
    let over_conv = point_in_rect(mouse.column, mouse.row, state.conversation_rect);
    let over_jump = point_in_rect(mouse.column, mouse.row, state.scroll_bottom_rect);

    match mouse.kind {
        // Wheel scrolls Conversation (never over Input — keeps history focus).
        MouseEventKind::ScrollUp if over_conv || over_jump => {
            state.workbench_focus = WorkbenchFocus::Conversation;
            clear_selection_drag(state);
            state.selection.clear();
            scroll_conversation(state, -3);
        }
        MouseEventKind::ScrollDown if over_conv || over_jump => {
            state.workbench_focus = WorkbenchFocus::Conversation;
            clear_selection_drag(state);
            state.selection.clear();
            scroll_conversation(state, 3);
        }
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {}
        MouseEventKind::Down(MouseButton::Left) => {
            // Jump-to-bottom only when not mid-selection; badge is hidden during
            // an active selection so hit-testing is usually empty anyway.
            if over_jump && !state.selection.is_active() {
                request_jump_to_bottom(state);
                clear_selection_drag(state);
                state.selection.clear();
                return Vec::new();
            }
            if over_input {
                state.workbench_focus = WorkbenchFocus::Input;
                clear_selection_drag(state);
                state.selection.clear();
                return Vec::new();
            }
            if over_conv {
                state.workbench_focus = WorkbenchFocus::Conversation;
                // Pin viewport so agent streaming cannot yank us to the bottom.
                state.conversation_auto_scroll = false;
                ensure_conversation_plain(state);
                // Cmd/Ctrl+click on a URL opens it in the browser instead of
                // starting a selection. Both modifiers are accepted so it works
                // whichever the terminal forwards (Cmd is often eaten by macOS
                // terminals, which then open the link themselves anyway).
                if mouse
                    .modifiers
                    .intersects(KeyModifiers::SUPER | KeyModifiers::CONTROL)
                    && let Some(pos) = mouse_to_text_pos_clamped(state, mouse.column, mouse.row)
                    && let Some(url) = url_at_pos(state, pos)
                {
                    open_url(state, &url);
                    return Vec::new();
                }
                state.selection_last_mouse = Some((mouse.column, mouse.row));
                update_selection_edge(state, mouse.column, mouse.row);
                if let Some(pos) = mouse_to_text_pos_clamped(state, mouse.column, mouse.row) {
                    state.selection.begin(pos);
                }
            } else {
                state.workbench_focus = WorkbenchFocus::Input;
                clear_selection_drag(state);
                state.selection.clear();
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if !state.selection.dragging {
                return Vec::new();
            }
            state.selection_last_mouse = Some((mouse.column, mouse.row));
            update_selection_edge(state, mouse.column, mouse.row);
            ensure_conversation_plain(state);
            if let Some(pos) = mouse_to_text_pos_clamped(state, mouse.column, mouse.row) {
                state.selection.extend(pos);
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            if state.selection.dragging {
                // One last edge settle is not needed; stop continuous scroll.
                clear_selection_drag(state);
                state.selection.finish();
                if !state.selection.is_empty() {
                    ensure_conversation_plain(state);
                    let text = crate::selection::extract_selected_text(
                        &state.conversation_plain,
                        &state.selection,
                    );
                    match crate::selection::copy_to_clipboard(&text) {
                        Ok(()) if !text.is_empty() => {
                            let n = text.chars().count();
                            state.notification = Some(Notification {
                                level: NotificationLevel::Info,
                                message: format!("已复制 {n} 字符"),
                            });
                        }
                        Ok(()) => {}
                        Err(e) => {
                            state.notification = Some(Notification {
                                level: NotificationLevel::Warning,
                                message: e,
                            });
                        }
                    }
                }
            }
        }
        _ => {}
    }
    Vec::new()
}

/// Continuous edge scroll while the primary button is held in a hot zone.
fn handle_selection_tick(state: &mut AppState) -> Vec<Effect> {
    if !state.selection.dragging || state.selection_edge_dir == 0 {
        return Vec::new();
    }
    let step = edge_scroll_step(state.selection_edge_streak);
    state.selection_edge_streak = state.selection_edge_streak.saturating_add(1);
    let delta = i32::from(state.selection_edge_dir) * step as i32;
    scroll_conversation_pinned(state, delta);
    ensure_conversation_plain(state);
    if let Some((col, row)) = state.selection_last_mouse
        && let Some(pos) = mouse_to_text_pos_clamped(state, col, row)
    {
        state.selection.extend(pos);
    }
    Vec::new()
}

fn clear_selection_drag(state: &mut AppState) {
    state.selection_edge_dir = 0;
    state.selection_edge_streak = 0;
    state.selection_last_mouse = None;
}

/// Accelerate scroll while the pointer stays in an edge hot zone.
fn edge_scroll_step(streak: u32) -> usize {
    match streak {
        0..=2 => 1,
        3..=6 => 2,
        7..=12 => 3,
        _ => 5,
    }
}

/// Top / bottom Conversation rows (or outside those edges) drive auto-scroll.
fn update_selection_edge(state: &mut AppState, col: u16, row: u16) {
    let Some((rx, ry, rw, rh)) = state.conversation_rect else {
        state.selection_edge_dir = 0;
        state.selection_edge_streak = 0;
        return;
    };
    if rh == 0 {
        state.selection_edge_dir = 0;
        return;
    }
    // Horizontally outside the conversation: stop edge scroll (still may clamp select).
    let in_x = col >= rx && col < rx.saturating_add(rw);
    if !in_x {
        state.selection_edge_dir = 0;
        state.selection_edge_streak = 0;
        return;
    }
    let edge = SELECTION_EDGE_ROWS.min(rh);
    let top_end = ry.saturating_add(edge.saturating_sub(1));
    let bottom_start = ry.saturating_add(rh.saturating_sub(edge));
    let dir = if row <= top_end || row < ry {
        -1
    } else if row >= bottom_start || row >= ry.saturating_add(rh) {
        1
    } else {
        0
    };
    if dir == 0 {
        state.selection_edge_dir = 0;
        state.selection_edge_streak = 0;
    } else if state.selection_edge_dir != dir {
        state.selection_edge_dir = dir;
        state.selection_edge_streak = 0;
    }
}

/// Content width for Conversation layout / selection. Must match the painted
/// viewport (`conversation_rect`), not the full terminal width — a mismatch
/// re-wraps lines under the cursor and the selection highlight "jumps".
fn conversation_content_width(state: &AppState) -> usize {
    state
        .conversation_rect
        .map(|(_, _, w, _)| w as usize)
        .filter(|w| *w > 0)
        .unwrap_or_else(|| state.size.0.max(1) as usize)
}

/// The http(s) URL under char column `col` of plain line `pos.row`, if any.
fn url_at_pos(state: &AppState, pos: crate::selection::TextPos) -> Option<String> {
    url_at(state.conversation_plain.get(pos.row)?, pos.col)
}

fn chars_start_with(chars: &[char], i: usize, pat: &str) -> bool {
    pat.chars()
        .enumerate()
        .all(|(k, pc)| chars.get(i + k) == Some(&pc))
}

/// Extract the http(s) URL spanning char index `col` in `line`, or `None`.
/// Only http/https are recognized so a click can never launch another scheme.
fn url_at(line: &str, col: usize) -> Option<String> {
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    let mut i = 0;
    while i < n {
        let scheme = if chars_start_with(&chars, i, "https://") {
            8
        } else if chars_start_with(&chars, i, "http://") {
            7
        } else {
            i += 1;
            continue;
        };
        let mut j = i;
        while j < n && is_url_char(chars[j]) {
            j += 1;
        }
        // Drop trailing sentence punctuation that is not really part of the URL.
        let mut end = j;
        while end > i + scheme && is_trailing_punct(chars[end - 1]) {
            end -= 1;
        }
        if (i..end).contains(&col) {
            return Some(chars[i..end].iter().collect());
        }
        i = j;
    }
    None
}

fn is_url_char(c: char) -> bool {
    !c.is_whitespace() && !matches!(c, '<' | '>' | '"' | '`' | '\'' | '|')
}

fn is_trailing_punct(c: char) -> bool {
    matches!(
        c,
        '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '"' | '\''
    )
}

/// Open a URL in the OS default browser, notifying success/failure.
fn open_url(state: &mut AppState, url: &str) {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    state.notification = Some(match std::process::Command::new(program).arg(url).spawn() {
        Ok(_) => Notification {
            level: NotificationLevel::Info,
            message: format!("已在浏览器打开 {url}"),
        },
        Err(e) => Notification {
            level: NotificationLevel::Warning,
            message: format!("打开失败: {e}"),
        },
    });
}

fn ensure_conversation_plain(state: &mut AppState) {
    let width = conversation_content_width(state);
    if state.conversation_plain_width == width && !state.conversation_plain.is_empty() {
        return;
    }
    let lines = state.conversation_lines(width);
    state.conversation_plain = lines.iter().map(crate::selection::line_to_plain).collect();
    state.conversation_plain_width = width;
}

/// Map screen cell → absolute content coordinates, clamping to the Conversation
/// viewport. Used while dragging so the selection endpoint tracks the pointer
/// even when it sits in an edge hot zone or briefly leaves the rect.
fn mouse_to_text_pos_clamped(
    state: &AppState,
    col: u16,
    row: u16,
) -> Option<crate::selection::TextPos> {
    let (rx, ry, rw, rh) = state.conversation_rect?;
    if rw == 0 || rh == 0 {
        return None;
    }
    let clamped_col = col.clamp(rx, rx.saturating_add(rw.saturating_sub(1)));
    let clamped_row = row.clamp(ry, ry.saturating_add(rh.saturating_sub(1)));
    let width = conversation_content_width(state);
    let height = rh as usize;
    let total = if state.conversation_plain_width == width && !state.conversation_plain.is_empty() {
        state.conversation_plain.len()
    } else {
        crate::workbench::conversation_line_count(state, width)
    };
    let max_scroll = total.saturating_sub(height.max(1));
    // While selecting we always use the pinned scroll, never auto-follow bottom.
    let scroll = if state.conversation_auto_scroll && !state.selection.dragging {
        max_scroll
    } else {
        state.conversation_scroll.min(max_scroll)
    };
    let viewport_row = (clamped_row - ry) as usize;
    let abs_row = (scroll + viewport_row).min(total.saturating_sub(1));
    let abs_col = (clamped_col - rx) as usize;
    Some(crate::selection::TextPos {
        row: abs_row,
        col: abs_col,
    })
}

fn point_in_rect(x: u16, y: u16, rect: Option<(u16, u16, u16, u16)>) -> bool {
    let Some((rx, ry, rw, rh)) = rect else {
        return false;
    };
    x >= rx && y >= ry && x < rx.saturating_add(rw) && y < ry.saturating_add(rh)
}

fn handle_key(state: &mut AppState, key: KeyEvent) -> Vec<Effect> {
    // Only react to presses (Windows also emits Release/Repeat).
    if key.kind == KeyEventKind::Release {
        return Vec::new();
    }

    // An open overlay captures all key input .
    if state.overlay.is_some() {
        return handle_overlay_key(state, key);
    }

    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    // Ctrl+C is the one key that reads (and advances) the escalation state.
    if is_ctrl_c(&key) {
        return handle_ctrl_c(state);
    }
    // Any intentional user key disarms a pending Ctrl+C. Some terminals also
    // emit non-printing control events around Ctrl+C; those should not consume
    // the confirmation window.
    if should_disarm_ctrlc(&key) {
        state.disarm_ctrlc();
        clear_quit_confirm_notification(state);
    }

    // Ctrl+<key> screen toggles work from anywhere (spec §57).
    if ctrl {
        match key.code {
            KeyCode::Char('t') => return toggle_screen(state, Screen::Tools),
            KeyCode::Char('p') => return toggle_screen(state, Screen::Plan),
            KeyCode::Char('r') => return toggle_screen(state, Screen::Verification),
            KeyCode::Char('d') => return open_diff_screen(state),
            KeyCode::Char('s') => return open_sessions_screen(state),
            KeyCode::Char('g') => return toggle_screen(state, Screen::Agents),
            KeyCode::Char('o') => {
                toggle_current_expand(state);
                return Vec::new();
            }
            // Jump back to the live edge after scrolling native history
            // (Approach A). Ctrl+End and Ctrl+↓ — the latter is easier on macOS.
            KeyCode::End | KeyCode::Down => {
                request_jump_to_bottom(state);
                return Vec::new();
            }
            _ => {}
        }
    }

    // Non-conversation screens handle their own navigation.
    if state.active_screen != Screen::Conversation {
        return handle_screen_key(state, key);
    }

    // Whether the slash-command popup is showing (drives Up/Down/Tab/Enter/Esc).
    let popup_len = crate::screen::visible_slash_popup(state).len();
    let file_popup_len = crate::screen::visible_file_popup(state).len();
    let popup_len = popup_len.max(file_popup_len);
    if popup_len > 0 {
        state.slash_selected = state.slash_selected.min(popup_len - 1);
    } else {
        state.slash_selected = 0;
    }

    match key.code {
        KeyCode::Enter if ctrl || alt => {
            state.composer.newline();
            touch_slash_filter(state);
        }
        // With the popup open, Enter on a partial command only completes the
        // highlighted row. Requiring a second Enter to execute avoids opening a
        // picker accidentally while the user is still narrowing `/m` or `/mod`.
        // A fully typed command still executes immediately; when it is also a
        // prefix of another command, run exactly what was typed (e.g. `/mode`
        // rather than the highlighted `/model`).
        KeyCode::Enter if file_popup_len > 0 => {
            complete_file_mention(state);
        }
        KeyCode::Enter if popup_len > 0 => {
            let text = state.composer.text();
            let token = text.split_whitespace().next().unwrap_or(text);
            let exact = crate::screen::SLASH_NAMES.contains(&token);
            if !exact {
                complete_slash(state);
                return Vec::new();
            }
            return submit(state);
        }
        // Empty composer + expanded queue: Enter starts the selected/next queued
        // item now (interrupting any running turn).
        KeyCode::Enter
            if state.composer.is_empty()
                && popup_len == 0
                && !state.queue_collapsed
                && state.input_queues.waiting_len() > 0 =>
        {
            return start_queued_now(state);
        }
        // Conversation focus while reading history: Enter = jump to live edge.
        KeyCode::Enter
            if state.workbench_focus == WorkbenchFocus::Conversation
                && !state.conversation_auto_scroll =>
        {
            request_jump_to_bottom(state);
        }
        KeyCode::Enter => {
            state.workbench_focus = WorkbenchFocus::Input;
            return submit(state);
        }
        KeyCode::Char('j') if ctrl => {
            state.composer.newline();
            touch_slash_filter(state);
        }
        KeyCode::Char('\n') => {
            state.composer.newline();
            touch_slash_filter(state);
        }
        KeyCode::Char('a') if ctrl => state.composer.move_to_line_start(),
        KeyCode::Char('\u{1}') => state.composer.move_to_line_start(),
        KeyCode::Char('e') if ctrl => state.composer.move_to_line_end(),
        KeyCode::Char('\u{5}') => state.composer.move_to_line_end(),
        KeyCode::Char('u') if ctrl => {
            state.composer.kill_to_line_start();
            touch_slash_filter(state);
        }
        KeyCode::Char('\u{15}') => {
            state.composer.kill_to_line_start();
            touch_slash_filter(state);
        }
        KeyCode::Char('k') if ctrl => {
            state.composer.kill_to_line_end();
            touch_slash_filter(state);
        }
        KeyCode::Char('\u{b}') => {
            state.composer.kill_to_line_end();
            touch_slash_filter(state);
        }
        KeyCode::Char('w') if ctrl => {
            state.composer.delete_word_back();
            touch_slash_filter(state);
        }
        KeyCode::Char('\u{17}') => {
            state.composer.delete_word_back();
            touch_slash_filter(state);
        }
        KeyCode::Char('m') if ctrl => open_model_picker(state),
        // Ctrl+? (and Ctrl+/) open Help — low-frequency bindings live there.
        KeyCode::Char('?') | KeyCode::Char('/') if ctrl => {
            return toggle_screen(state, Screen::Help);
        }
        // Ctrl+Q: expand/collapse the Prompt Queue panel.
        KeyCode::Char('q') if ctrl => {
            if !state.input_queues.is_empty() {
                state.queue_collapsed = !state.queue_collapsed;
                if !state.queue_collapsed && state.queue_selected.is_none() {
                    let pending_n = state.input_queues.pending.len();
                    if state.input_queues.waiting_len() > 0 {
                        state.queue_selected = Some(pending_n);
                    }
                }
            }
        }
        KeyCode::Tab if file_popup_len > 0 => complete_file_mention(state),
        KeyCode::Tab if popup_len > 0 => complete_slash(state),
        // No completion popup: Tab switches Input ↔ Conversation focus.
        KeyCode::Tab => {
            state.workbench_focus = match state.workbench_focus {
                WorkbenchFocus::Input => WorkbenchFocus::Conversation,
                WorkbenchFocus::Conversation => WorkbenchFocus::Input,
            };
        }
        KeyCode::Char(c) if !ctrl && !c.is_control() => {
            // Typing always claims Input focus.
            state.workbench_focus = WorkbenchFocus::Input;
            state.composer.insert_char(c);
            touch_slash_filter(state);
            return request_file_candidates(state);
        }
        // Alt+Backspace is word-delete everywhere else in a terminal; honoring
        // that beats overloading it for attachments.
        KeyCode::Backspace if alt => {
            state.composer.delete_word_back();
            touch_slash_filter(state);
        }
        KeyCode::Backspace | KeyCode::Char('\u{8}') | KeyCode::Char('\u{7f}') => {
            // On an empty composer, Backspace peels off pending state: attachment
            // chips first, then the selected/last queued message.
            if state.composer.is_empty() && !state.pending_attachments.is_empty() {
                state.pending_attachments.pop();
            } else if state.composer.is_empty() && !state.input_queues.is_empty() {
                let pending_n = state.input_queues.pending.len();
                let removed = if let Some(sel) = state.queue_selected {
                    // Display index includes pending; waiting index is sel - pending_n.
                    if sel >= pending_n {
                        state.input_queues.remove_waiting_at(sel - pending_n)
                    } else {
                        state.input_queues.pop_last_waiting()
                    }
                } else {
                    state.input_queues.pop_last_waiting()
                };
                if removed.is_some() {
                    crate::footer_queue::on_queue_changed(state);
                    let remaining = state.input_queues.visible_len();
                    let t = state.t();
                    state.notification = Some(Notification {
                        level: NotificationLevel::Info,
                        message: if remaining == 0 {
                            t.cleared_queue.to_string()
                        } else {
                            t.deleted_queue_n.replacen("{}", &remaining.to_string(), 1)
                        },
                    });
                }
            } else {
                state.composer.backspace();
            }
            touch_slash_filter(state);
        }
        // Empty composer + expanded queue: Delete cancels the selected/next item.
        KeyCode::Delete
            if state.composer.is_empty()
                && !state.queue_collapsed
                && state.input_queues.waiting_len() > 0 =>
        {
            cancel_selected_queued(state);
            touch_slash_filter(state);
        }
        KeyCode::Delete => {
            state.composer.delete();
            touch_slash_filter(state);
        }
        // With the popup open, Up/Down move the highlight instead of the cursor.
        KeyCode::Up if popup_len > 0 => {
            state.slash_selected = state.slash_selected.saturating_sub(1);
        }
        KeyCode::Down if popup_len > 0 => {
            state.slash_selected = (state.slash_selected + 1).min(popup_len - 1);
        }
        // Shift+↑/↓: jump between user turns without touching the composer draft.
        KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => {
            navigate_user_turn(state, -1);
        }
        KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => {
            navigate_user_turn(state, 1);
        }
        KeyCode::Left => state.composer.move_left(),
        KeyCode::Right => state.composer.move_right(),
        KeyCode::Home => state.composer.move_to_line_start(),
        // Empty composer: End jumps to the live bottom (Approach A). With text,
        // End stays "end of line" so multi-line editing is unchanged.
        KeyCode::End if state.composer.is_empty() => {
            request_jump_to_bottom(state);
        }
        KeyCode::End => state.composer.move_to_line_end(),
        // PageUp/PageDown always scroll Conversation (and pin auto-scroll off).
        KeyCode::PageUp => {
            state.workbench_focus = WorkbenchFocus::Conversation;
            scroll_conversation(state, -(state.size.1 as i32 / 2).max(1));
        }
        KeyCode::PageDown => {
            state.workbench_focus = WorkbenchFocus::Conversation;
            scroll_conversation(state, (state.size.1 as i32 / 2).max(1));
        }
        // Alt+↑/↓: reorder selected queue item (or last waiting item).
        KeyCode::Up if alt && !state.input_queues.is_empty() => {
            reorder_queue(state, -1);
        }
        KeyCode::Down if alt && !state.input_queues.is_empty() => {
            reorder_queue(state, 1);
        }
        // Empty composer + expanded queue: ↑/↓ move selection.
        KeyCode::Up
            if state.composer.is_empty()
                && popup_len == 0
                && !state.queue_collapsed
                && state.input_queues.waiting_len() > 0
                && state.queue_selected.is_some() =>
        {
            move_queue_selection(state, -1);
        }
        KeyCode::Down
            if state.composer.is_empty()
                && popup_len == 0
                && !state.queue_collapsed
                && state.input_queues.waiting_len() > 0
                && state.queue_selected.is_some() =>
        {
            move_queue_selection(state, 1);
        }
        // Conversation focus: ↑/↓ scroll the viewport only.
        KeyCode::Up if state.workbench_focus == WorkbenchFocus::Conversation && popup_len == 0 => {
            scroll_conversation(state, -1);
        }
        KeyCode::Down
            if state.workbench_focus == WorkbenchFocus::Conversation && popup_len == 0 =>
        {
            scroll_conversation(state, 1);
        }
        // Input focus: ↑/↓ = history only (never steal for conversation scroll).
        KeyCode::Up if popup_len == 0 => state.composer.up(),
        KeyCode::Down if popup_len == 0 => state.composer.down(),
        KeyCode::Char('p') if !ctrl && state.composer.is_empty() => {
            // Toggle plan panel when not typing.
            state.plan_collapsed = !state.plan_collapsed;
        }
        // Esc priority: slash popup → turn-nav → finished 旁问 card → notice.
        KeyCode::Esc if popup_len > 0 => {
            state.slash_popup_dismissed = true;
            state.slash_selected = 0;
            state.notification = None;
        }
        KeyCode::Esc if state.turn_nav.is_some() => {
            state.turn_nav = None;
            state.notification = Some(Notification {
                level: NotificationLevel::Info,
                message: state.t().turn_nav_live.to_string(),
            });
        }
        KeyCode::Esc if state.transcript.has_finished_btw() => {
            let _ = state.transcript.dismiss_latest_finished_btw();
            state.notification = None;
        }
        KeyCode::Esc => {
            state.notification = None;
        }
        _ => {}
    }
    Vec::new()
}

/// Scroll the conversation viewport by `delta` lines (negative = up).
fn scroll_conversation(state: &mut AppState, delta: i32) {
    // Approximate viewport height: total rows minus chrome (header 3 + strips).
    let height = state.size.1.saturating_sub(12).max(3) as usize;
    let width = state.size.0.max(1) as usize;
    let total = crate::workbench::conversation_line_count(state, width);
    let max_scroll = total.saturating_sub(height);
    if delta < 0 {
        state.conversation_auto_scroll = false;
        state.conversation_scroll = state
            .conversation_scroll
            .saturating_sub((-delta) as usize)
            .min(max_scroll);
    } else {
        let next = (state.conversation_scroll + delta as usize).min(max_scroll);
        state.conversation_scroll = next;
        if next >= max_scroll {
            state.conversation_auto_scroll = true;
            state.conversation_unread = 0;
        }
    }
}

/// Scroll during text selection: never re-enable auto-follow (streaming must not
/// yank the viewport while the user is still choosing text).
fn scroll_conversation_pinned(state: &mut AppState, delta: i32) {
    if delta == 0 {
        return;
    }
    let height = conversation_viewport_height(state);
    let width = state.size.0.max(1) as usize;
    let total = crate::workbench::conversation_line_count(state, width);
    let max_scroll = total.saturating_sub(height.max(1));
    state.conversation_auto_scroll = false;
    if delta < 0 {
        state.conversation_scroll = state
            .conversation_scroll
            .saturating_sub((-delta) as usize)
            .min(max_scroll);
    } else {
        state.conversation_scroll = (state.conversation_scroll + delta as usize).min(max_scroll);
    }
}

fn conversation_viewport_height(state: &AppState) -> usize {
    if let Some((_, _, _, rh)) = state.conversation_rect {
        rh.max(1) as usize
    } else {
        state.size.1.saturating_sub(12).max(3) as usize
    }
}

/// Move focus across user turns. `delta` is -1 (older) or +1 (newer).
/// Does not modify `state.composer` — draft is always preserved (UI-N3/N4).
fn navigate_user_turn(state: &mut AppState, delta: i32) {
    let turns = crate::render::user_turn_summaries(state);
    if turns.is_empty() {
        state.notification = Some(Notification {
            level: NotificationLevel::Info,
            message: state.t().turn_nav_empty.to_string(),
        });
        return;
    }
    let last = turns.len() - 1;
    let next = match state.turn_nav {
        None if delta < 0 => Some(last),
        None => None, // already live; Shift+Down is a no-op
        Some(i) => {
            let n = i as i32 + delta;
            if n < 0 {
                Some(0)
            } else if n as usize > last {
                None // past the newest → live edge
            } else {
                Some(n as usize)
            }
        }
    };
    state.turn_nav = next;
    if let Some(i) = next {
        let (_, preview) = &turns[i.min(last)];
        let msg = state
            .t()
            .turn_nav
            .replacen("{}", &(i + 1).to_string(), 1)
            .replacen("{}", &turns.len().to_string(), 1);
        state.notification = Some(Notification {
            level: NotificationLevel::Info,
            message: format!("{msg} · {}", crate::render::truncate_display(preview, 48)),
        });
    } else {
        state.notification = Some(Notification {
            level: NotificationLevel::Info,
            message: state.t().turn_nav_live.to_string(),
        });
    }
}

/// Ctrl+O: expand/collapse only the *current* focus item — never every group.
///
/// Priority:
/// 1. Live reasoning (when non-empty) — toggle `reasoning_expanded` only.
/// 2. Else the latest tool group — toggle its per-group `expanded` only.
fn toggle_current_expand(state: &mut AppState) {
    if !state.reasoning.trim().is_empty() {
        state.reasoning_expanded = !state.reasoning_expanded;
        // Reasoning lives only in the live footer; no scrollback rebuild.
        return;
    }
    if let Some(expanded) = state.transcript.toggle_last_tool_group() {
        // Mirror into the workbench flag used to render the focused group.
        state.tools_expanded = expanded;
    }
}

/// Jump conversation viewport to bottom (auto-follow resumes).
fn request_jump_to_bottom(state: &mut AppState) {
    state.conversation_auto_scroll = true;
    state.conversation_unread = 0;
    state.jump_to_bottom = true;
    let height = state.size.1.saturating_sub(10).max(3) as usize;
    let width = state.size.0.max(1) as usize;
    let total = crate::workbench::conversation_line_count(state, width);
    state.conversation_scroll = total.saturating_sub(height);
    state.notification = Some(Notification {
        level: NotificationLevel::Info,
        message: state.t().back_to_bottom.to_string(),
    });
}

fn move_queue_selection(state: &mut AppState, delta: i32) {
    let pending_n = state.input_queues.pending.len();
    let waiting_n = state.input_queues.waiting_len();
    if waiting_n == 0 {
        state.queue_selected = None;
        return;
    }
    let min_sel = pending_n;
    let max_sel = pending_n + waiting_n - 1;
    let cur = state.queue_selected.unwrap_or(min_sel) as i32;
    let next = (cur + delta).clamp(min_sel as i32, max_sel as i32) as usize;
    state.queue_selected = Some(next);
    // Keep selection visible inside the body window.
    const BODY: usize = 5;
    if next < state.queue_scroll {
        state.queue_scroll = next;
    } else if next >= state.queue_scroll + BODY {
        state.queue_scroll = next + 1 - BODY;
    }
}

fn reorder_queue(state: &mut AppState, delta: i32) {
    let pending_n = state.input_queues.pending.len();
    let waiting_n = state.input_queues.waiting_len();
    if waiting_n == 0 {
        return;
    }
    let sel = state
        .queue_selected
        .unwrap_or(pending_n + waiting_n - 1)
        .max(pending_n);
    let waiting_idx = sel - pending_n;
    if let Some(new_w) = state.input_queues.move_waiting(waiting_idx, delta) {
        state.queue_selected = Some(pending_n + new_w);
        state.queue_collapsed = false;
        crate::footer_queue::normalize_queue_focus(state);
    }
}

/// Waiting flat index the queue actions target: the selected waiting row, else
/// the first waiting item. `None` when nothing is waiting.
fn queue_action_target(state: &AppState) -> Option<usize> {
    if state.input_queues.waiting_len() == 0 {
        return None;
    }
    let pending_n = state.input_queues.pending.len();
    match state.queue_selected {
        Some(sel) if sel >= pending_n => Some(sel - pending_n),
        _ => Some(0),
    }
}

/// "Start now": promote the targeted queued item to the front; if a turn is
/// running, cancel it so the runtime idles and `drain_queued` submits this item
/// next. When idle, the drain path picks it up on the following loop tick.
fn start_queued_now(state: &mut AppState) -> Vec<Effect> {
    let Some(waiting_idx) = queue_action_target(state) else {
        return Vec::new();
    };
    if state
        .input_queues
        .promote_waiting_to_front(waiting_idx)
        .is_none()
    {
        return Vec::new();
    }
    let pending_n = state.input_queues.pending.len();
    state.queue_selected = Some(pending_n + state.input_queues.rejected.len());
    crate::footer_queue::on_queue_changed(state);
    if state.is_busy() {
        state.notification = Some(Notification {
            level: NotificationLevel::Info,
            message: state.t().queue_starting_now.to_string(),
        });
        return vec![Effect::Send(ClientCommand::CancelCurrentTurn {
            session_id: state.session_id.clone(),
        })];
    }
    Vec::new()
}

/// Cancel (remove) the targeted queued item.
fn cancel_selected_queued(state: &mut AppState) {
    let Some(waiting_idx) = queue_action_target(state) else {
        return;
    };
    if state.input_queues.remove_waiting_at(waiting_idx).is_some() {
        crate::footer_queue::on_queue_changed(state);
        let remaining = state.input_queues.visible_len();
        let t = state.t();
        state.notification = Some(Notification {
            level: NotificationLevel::Info,
            message: if remaining == 0 {
                t.cleared_queue.to_string()
            } else {
                t.deleted_queue_n.replacen("{}", &remaining.to_string(), 1)
            },
        });
    }
}

fn is_ctrl_c(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('\u{3}'))
        || (key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'C')))
}

fn should_disarm_ctrlc(key: &KeyEvent) -> bool {
    match key.code {
        KeyCode::Null => false,
        KeyCode::Char(c) if c.is_control() => false,
        _ => true,
    }
}

fn clear_quit_confirm_notification(state: &mut AppState) {
    if state
        .notification
        .as_ref()
        .is_some_and(|n| n.message == QUIT_CONFIRM_MESSAGE)
    {
        state.notification = None;
    }
}

fn handle_ctrl_c(state: &mut AppState) -> Vec<Effect> {
    if state.is_busy() {
        // Escalation: cancel → force-cancel → quit. Force-cancel alone is not
        // enough when a tool is stuck in process wait: the runtime re-cancels
        // the same token and the UI stays Busy forever. Third press exits.
        if state.force_cancel_armed {
            state.notification = None;
            return vec![Effect::Quit];
        }
        if state.cancel_armed {
            state.force_cancel_armed = true;
            state.notification = Some(Notification {
                level: NotificationLevel::Warning,
                message: "强制取消中…仍卡住再按 Ctrl+C 退出，或输入 /quit".to_string(),
            });
            return vec![Effect::Send(ClientCommand::ForceCancelCurrentTurn {
                session_id: state.session_id.clone(),
            })];
        }
        state.cancel_armed = true;
        state.notification = Some(Notification {
            level: NotificationLevel::Warning,
            message: "正在取消当前任务，再按一次 Ctrl+C 强制取消".to_string(),
        });
        vec![Effect::Send(ClientCommand::CancelCurrentTurn {
            session_id: state.session_id.clone(),
        })]
    } else {
        let quit_prompt_visible = state
            .notification
            .as_ref()
            .is_some_and(|n| n.message == QUIT_CONFIRM_MESSAGE);
        if state.quit_armed || quit_prompt_visible {
            state.notification = None;
            return vec![Effect::Quit];
        }
        state.quit_armed = true;
        state.notification = Some(Notification {
            level: NotificationLevel::Info,
            message: QUIT_CONFIRM_MESSAGE.to_string(),
        });
        Vec::new()
    }
}

#[cfg(test)]
mod url_tests {
    use super::url_at;

    #[test]
    fn detects_http_url_only_under_the_clicked_column() {
        let line = "见 https://example.com/path 了解详情";
        // Column inside the URL → returns it.
        assert_eq!(
            url_at(line, 10).as_deref(),
            Some("https://example.com/path")
        );
        // Column outside the URL → None.
        assert_eq!(url_at(line, 0), None);
        assert_eq!(url_at(line, 30), None);
    }

    #[test]
    fn trims_trailing_punctuation_and_ignores_other_schemes() {
        // A trailing period/paren is not part of the link.
        assert_eq!(url_at("(http://a.io).", 3).as_deref(), Some("http://a.io"));
        // Non-http schemes are never linkified.
        assert_eq!(url_at("file:///etc/passwd", 3), None);
        assert_eq!(url_at("run ssh://x", 6), None);
    }
}
