//! The reducer: `(AppState, Action) -> [Effect]`, pure over state.

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use leveler_client_protocol::{ClientCommand, NotificationLevel, PermissionProfile, RuntimeEvent};

use crate::action::{Action, Effect, EffectCompletion};
use crate::conversation::interaction::{self, Hit};
use crate::screen::Screen;
use crate::state::{AppState, Notification, WorkbenchFocus};

pub mod overlay_keys;
mod runtime_apply;
mod screen_nav;
mod submit;

use overlay_keys::{handle_overlay_key, open_model_picker};
use runtime_apply::apply_runtime;
use runtime_apply::mode_label;
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
        Action::Remote(outcome) => {
            apply_remote(state, outcome);
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
        Action::EditorFinished(result) => {
            match result {
                // An empty buffer is a decision, not a failure: the user wiped
                // the prompt in the editor and meant to start over.
                Ok(text) => {
                    state.composer.replace(text.trim_end_matches('\n'));
                    touch_slash_filter(state);
                }
                Err(message) => {
                    state.notification = Some(Notification {
                        level: NotificationLevel::Warning,
                        message: format!("外部编辑器未能打开：{message}"),
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
    let over_conv = point_in_rect(mouse.column, mouse.row, state.conv.rect);
    let over_jump = point_in_rect(mouse.column, mouse.row, state.conv.scroll_bottom_rect);

    match mouse.kind {
        // Wheel scrolls Conversation (never over Input — keeps history focus).
        MouseEventKind::ScrollUp if over_conv || over_jump => {
            state.workbench_focus = WorkbenchFocus::Conversation;
            interaction::clear_selection_drag(state);
            state.conv.selection.clear();
            interaction::scroll_by(state, -3);
        }
        MouseEventKind::ScrollDown if over_conv || over_jump => {
            state.workbench_focus = WorkbenchFocus::Conversation;
            interaction::clear_selection_drag(state);
            state.conv.selection.clear();
            interaction::scroll_by(state, 3);
        }
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {}
        MouseEventKind::Down(MouseButton::Left) => {
            // Jump-to-bottom only when not mid-selection; badge is hidden during
            // an active selection so hit-testing is usually empty anyway.
            if over_jump && !state.conv.selection.is_active() {
                request_jump_to_bottom(state);
                interaction::clear_selection_drag(state);
                state.conv.selection.clear();
                return Vec::new();
            }
            if over_input {
                state.workbench_focus = WorkbenchFocus::Input;
                interaction::clear_selection_drag(state);
                state.conv.selection.clear();
                return Vec::new();
            }
            if over_conv {
                state.workbench_focus = WorkbenchFocus::Conversation;
                // Pin the viewport exactly where it is painted so agent
                // streaming cannot yank us to the bottom AND the hit test
                // below maps against the same scroll the user is looking at.
                crate::conversation::pin_at_current_viewport(state);
                interaction::ensure_plain(state);
                // Policy over semantics: the interaction layer says WHAT is
                // under the cursor; this reducer decides what to do with it.
                // A disclosure row toggles exactly its group and never begins
                // a selection; Cmd/Ctrl+click opens a URL immediately (both
                // modifiers — macOS terminals often eat Cmd); a plain click
                // on a URL opens on release when the selection stayed empty
                // (see Up); anything else begins a selection.
                match interaction::hit_test(state, mouse.column, mouse.row) {
                    Hit::Disclosure { item } => {
                        // A user shell row opens its Details screen (running
                        // or finished); tool groups / sub-agents toggle
                        // inline. Same hit map, dispatch by item identity.
                        if matches!(
                            state.transcript.items().get(item),
                            Some(crate::transcript::TranscriptItem::UserShell(_))
                        ) {
                            state.shell_screen_item = Some(item);
                            state.active_screen = Screen::Shell;
                            state.screen_scroll = 0;
                        } else {
                            state.transcript.toggle_tool_group_at(item);
                        }
                        state.conv.plain.clear();
                        interaction::clear_selection_drag(state);
                        state.conv.selection.clear();
                        return Vec::new();
                    }
                    Hit::Url { url, .. }
                        if mouse
                            .modifiers
                            .intersects(KeyModifiers::SUPER | KeyModifiers::CONTROL) =>
                    {
                        return open_url(state, &url);
                    }
                    Hit::Url { pos, .. } | Hit::Text(pos) => {
                        state.conv.selection_last_mouse = Some((mouse.column, mouse.row));
                        interaction::update_selection_edge(state, mouse.column, mouse.row);
                        state.conv.selection.begin(pos);
                    }
                    Hit::Outside => {}
                }
            } else {
                state.workbench_focus = WorkbenchFocus::Input;
                interaction::clear_selection_drag(state);
                state.conv.selection.clear();
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if !state.conv.selection.dragging {
                return Vec::new();
            }
            state.conv.selection_last_mouse = Some((mouse.column, mouse.row));
            interaction::update_selection_edge(state, mouse.column, mouse.row);
            interaction::ensure_plain(state);
            if let Some(pos) =
                crate::conversation::geometry::screen_to_content(state, mouse.column, mouse.row)
            {
                state.conv.selection.extend(pos);
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            if state.conv.selection.dragging {
                // One last edge settle is not needed; stop continuous scroll.
                interaction::clear_selection_drag(state);
                state.conv.selection.finish();
                if state.conv.selection.is_empty() {
                    // Click without drag: open the URL under the cursor, if any.
                    interaction::ensure_plain(state);
                    if let Hit::Url { url, .. } =
                        interaction::hit_test(state, mouse.column, mouse.row)
                    {
                        return open_url(state, &url);
                    }
                } else {
                    interaction::ensure_plain(state);
                    let text = crate::selection::extract_selected_text(
                        &state.conv.plain,
                        &state.conv.selection,
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
    if !state.conv.selection.dragging || state.conv.selection_edge_dir == 0 {
        return Vec::new();
    }
    let step = interaction::edge_scroll_step(state.conv.selection_edge_streak);
    state.conv.selection_edge_streak = state.conv.selection_edge_streak.saturating_add(1);
    let delta = i32::from(state.conv.selection_edge_dir) * step as i32;
    interaction::scroll_pinned_by(state, delta);
    interaction::ensure_plain(state);
    if let Some((col, row)) = state.conv.selection_last_mouse
        && let Some(pos) = crate::conversation::geometry::screen_to_content(state, col, row)
    {
        state.conv.selection.extend(pos);
    }
    Vec::new()
}

/// Queue opening `url` in the OS default browser (handled by the event loop).
fn open_url(state: &mut AppState, url: &str) -> Vec<Effect> {
    state.notification = Some(Notification {
        level: NotificationLevel::Info,
        message: format!("已在浏览器打开 {url}"),
    });
    vec![Effect::OpenWebUrl(url.to_string())]
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
    // the confirmation window. Esc while busy is exempt: it drives the same
    // cancel → force-cancel escalation, so disarming would strand it on step one.
    if should_disarm_ctrlc(&key) && !(matches!(key.code, KeyCode::Esc) && state.is_busy()) {
        state.disarm_ctrlc();
        clear_quit_confirm_notification(state);
    }

    // `Ctrl+X Ctrl+E` opens $EDITOR on the draft (readline's binding, which
    // every other agent CLI inherited). The chord is spent by whatever key
    // follows, so a stray Ctrl+X never swallows the next keystroke's meaning.
    let editor_chord = std::mem::take(&mut state.editor_chord_armed);
    if ctrl && matches!(key.code, KeyCode::Char('x')) {
        state.editor_chord_armed = true;
        return Vec::new();
    }
    if editor_chord && ctrl && matches!(key.code, KeyCode::Char('e')) {
        return open_external_editor(state);
    }

    // Ctrl+<key> screen toggles work from anywhere (spec §57).
    if ctrl {
        match key.code {
            KeyCode::Char('t') => return toggle_screen(state, Screen::Tools),
            KeyCode::Char('d') => return open_diff_screen(state),
            KeyCode::Char('s') => return open_sessions_screen(state),
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

    // Shift+Tab cycles the permission profile from anywhere — the one binding
    // every terminal coding agent shares, so muscle memory carries over.
    // Some terminals report it as Tab+SHIFT rather than BackTab.
    if matches!(key.code, KeyCode::BackTab)
        || (matches!(key.code, KeyCode::Tab) && key.modifiers.contains(KeyModifiers::SHIFT))
    {
        return cycle_permission_profile(state);
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
            // Primary or alias counts as exact so `/mode` runs permission,
            // not the highlighted `/model` completion.
            let exact = crate::screen::is_exact_slash_token(token);
            if !exact {
                complete_slash(state);
                return Vec::new();
            }
            return submit(state);
        }
        // Conversation focus while reading history: Enter = jump to live edge.
        KeyCode::Enter
            if state.workbench_focus == WorkbenchFocus::Conversation && !state.conv.auto_scroll =>
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
            // On an empty composer, Backspace peels off the last attachment chip.
            if state.composer.is_empty() && !state.pending_attachments.is_empty() {
                state.pending_attachments.pop();
            } else {
                state.composer.backspace();
            }
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
            interaction::scroll_by(state, -(state.size.1 as i32 / 2).max(1));
        }
        KeyCode::PageDown => {
            state.workbench_focus = WorkbenchFocus::Conversation;
            interaction::scroll_by(state, (state.size.1 as i32 / 2).max(1));
        }
        // Conversation focus: ↑/↓ scroll the viewport only.
        KeyCode::Up if state.workbench_focus == WorkbenchFocus::Conversation && popup_len == 0 => {
            interaction::scroll_by(state, -1);
        }
        KeyCode::Down
            if state.workbench_focus == WorkbenchFocus::Conversation && popup_len == 0 =>
        {
            interaction::scroll_by(state, 1);
        }
        // Input focus: ↑/↓ = history only (never steal for conversation scroll).
        KeyCode::Up if popup_len == 0 => state.composer.up(),
        KeyCode::Down if popup_len == 0 => state.composer.down(),
        KeyCode::Char('p') if !ctrl && state.composer.is_empty() => {
            // Toggle plan panel when not typing.
            state.plan_collapsed = !state.plan_collapsed;
        }
        // Esc priority: slash popup → turn-nav → interrupt → finished 旁问 card
        // → notice. Dismissing a popup or leaving turn review is a *narrower*
        // undo than killing the turn, so those win while they are on screen.
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
        // Esc is the interrupt every other coding-agent CLI uses. It escalates
        // cancel → force-cancel like Ctrl+C does, but stops there: quitting on
        // a keypress the user reached for to *stay* would be a trap.
        KeyCode::Esc if state.is_busy() => return request_cancel(state),
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
    interaction::jump_to_live_edge(state);
    state.jump_to_bottom = true;
    state.notification = Some(Notification {
        level: NotificationLevel::Info,
        message: state.t().back_to_bottom.to_string(),
    });
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

/// Cancel the running turn, escalating cancel → force-cancel on repeat.
///
/// Shared by Esc and Ctrl+C. Force-cancel is needed because a tool stuck in
/// process wait makes the runtime re-cancel the same token while the UI stays
/// Busy forever; only Ctrl+C escalates past this into a quit.
fn request_cancel(state: &mut AppState) -> Vec<Effect> {
    if state.force_cancel_armed {
        // Already forced; re-sending the same cancel adds nothing.
        return Vec::new();
    }
    if state.cancel_armed {
        state.force_cancel_armed = true;
        state.notification = Some(Notification {
            level: NotificationLevel::Warning,
            message: "强制取消中…仍卡住按 Ctrl+C 退出，或输入 /quit".to_string(),
        });
        return vec![Effect::Send(ClientCommand::ForceCancelCurrentTurn {
            session_id: state.session_id.clone(),
        })];
    }
    state.cancel_armed = true;
    state.notification = Some(Notification {
        level: NotificationLevel::Warning,
        message: "正在取消当前任务，再按一次强制取消".to_string(),
    });
    vec![Effect::Send(ClientCommand::CancelCurrentTurn {
        session_id: state.session_id.clone(),
    })]
}

/// Hand the current draft to `$EDITOR` (Ctrl+X Ctrl+E, or `/editor`).
pub(super) fn open_external_editor(state: &mut AppState) -> Vec<Effect> {
    vec![Effect::OpenExternalEditor {
        text: state.composer.text().to_string(),
    }]
}

/// Cycle the permission profile (Shift+Tab), least → most privileged.
fn cycle_permission_profile(state: &mut AppState) -> Vec<Effect> {
    let next = match state.mode {
        PermissionProfile::RequestApproval => PermissionProfile::Assisted,
        PermissionProfile::Assisted => PermissionProfile::FullAccess,
        PermissionProfile::FullAccess => PermissionProfile::RequestApproval,
    };
    state.mode = next;
    state.mode_label = mode_label(next).to_string();
    let t = state.t();
    let human = match next {
        PermissionProfile::RequestApproval => t.perm_readonly,
        PermissionProfile::Assisted => t.perm_workspace,
        PermissionProfile::FullAccess => t.perm_full,
    };
    // A permission change must never be silent — full access especially.
    state.notification = Some(Notification {
        level: if next == PermissionProfile::FullAccess {
            NotificationLevel::Warning
        } else {
            NotificationLevel::Info
        },
        message: format!("{}: {human}", t.overlay_mode),
    });
    vec![Effect::Send(ClientCommand::SetPermissionProfile {
        session_id: state.session_id.clone(),
        mode: next,
    })]
}

fn handle_ctrl_c(state: &mut AppState) -> Vec<Effect> {
    if state.is_busy() {
        // Third press: force-cancel did not free the turn, so exit rather than
        // trap the user in cancel-only key handling.
        if state.force_cancel_armed {
            state.notification = None;
            return vec![Effect::Quit];
        }
        request_cancel(state)
    } else {
        // A draft on screen makes Ctrl+C "clear what I typed", not "leave" —
        // arming a quit here turns a routine erase into a two-key exit.
        if !state.composer.is_empty() {
            state.composer.replace("");
            touch_slash_filter(state);
            state.notification = None;
            return Vec::new();
        }
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

/// Fold what the host side reported into the invite screen.
fn apply_remote(state: &mut AppState, outcome: crate::action::RemoteOutcome) {
    use crate::action::RemoteOutcome;
    match outcome {
        RemoteOutcome::Invited(invite) => {
            state.remote = Some(crate::state::RemoteState {
                invite,
                pending: None,
                outcome: None,
            });
            state.active_screen = crate::screen::Screen::Remote;
        }
        // Polled while the invite is up; a phone has claimed it and is waiting
        // for the person at the keyboard.
        RemoteOutcome::Waiting(pending) => {
            if let Some(remote) = state.remote.as_mut() {
                remote.pending = pending;
            }
        }
        RemoteOutcome::Paired { device_name } => {
            if let Some(remote) = state.remote.as_mut() {
                remote.pending = None;
                remote.outcome = Some(format!("已接受「{device_name}」，手机现在可以连接了。"));
            }
        }
        RemoteOutcome::Rejected => {
            if let Some(remote) = state.remote.as_mut() {
                remote.pending = None;
                remote.outcome = Some("已拒绝这次配对，本机没有记录任何密钥。".to_string());
            }
        }
        RemoteOutcome::Failed(message) => {
            state.notification = Some(Notification {
                level: NotificationLevel::Warning,
                message,
            });
            // Leave the screen up only if it already had something to show.
            if state.remote.is_none() {
                state.active_screen = crate::screen::Screen::Conversation;
            }
        }
    }
}

#[cfg(test)]
mod disclosure_tests {
    use super::*;
    use crate::state::Boot;
    use crate::transcript::TranscriptItem;
    use crossterm::event::{MouseButton, MouseEvent};
    use leveler_client_protocol::{SessionId, ToolCallId};

    fn test_state() -> AppState {
        let mut s = AppState::new(
            crate::theme::Theme::no_color(),
            Boot {
                session_id: SessionId::new("s1"),
                user: "u".into(),
                version: "0.1.0".into(),
                show_welcome: false,
                draft_path: None,
                history_path: None,
                context_window: 200_000,
                locale: crate::i18n::Locale::Zh,
                untrusted_config: Vec::new(),
            },
        );
        s.size = (80, 40);
        s.conv.rect = Some((0, 2, 80, 30));
        s.conv.auto_scroll = false;
        s.conv.scroll = 0;
        s
    }

    fn finished_tool(s: &mut AppState, id: &str, name: &str, args: &str) {
        s.transcript
            .push_tool_started(ToolCallId::new(id), name.into(), args.into(), false, 0);
        s.transcript
            .complete_tool(&ToolCallId::new(id), true, "done".into(), 1200);
    }

    /// Three finished work-tool groups separated by user rows — the layout the
    /// per-group disclosure contract is about.
    fn three_groups(s: &mut AppState) {
        s.transcript.push_user("q1".into());
        finished_tool(
            s,
            "a1",
            "run_command",
            r#"{"program":"cargo","args":["test"]}"#,
        );
        s.transcript.push_user("q2".into());
        finished_tool(s, "b1", "read_file", r#"{"path":"a.rs"}"#);
        s.transcript.push_user("q3".into());
        finished_tool(s, "c1", "grep", r#"{"pattern":"x"}"#);
        s.transcript.push_user("tail".into());
    }

    /// Screen row of absolute content line `abs_line` under the current rect
    /// (short transcripts render bottom-aligned, so pad rows come first).
    fn screen_row_of(s: &AppState, abs_line: usize) -> u16 {
        let (_, ry, rw, rh) = s.conv.rect.unwrap();
        let total = s.conversation_lines_and_hits(rw as usize).0.len();
        let pad = (rh as usize).saturating_sub(total);
        ry + (pad + abs_line) as u16
    }

    fn click(s: &mut AppState, col: u16, row: u16) {
        reduce(
            s,
            Action::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: col,
                row,
                modifiers: KeyModifiers::empty(),
            }),
        );
    }

    fn expanded_flags(s: &AppState) -> Vec<(usize, bool)> {
        s.transcript
            .items()
            .iter()
            .enumerate()
            .filter_map(|(i, item)| match item {
                TranscriptItem::ToolGroup(g) => Some((i, g.expanded)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn user_shell_row_click_opens_its_details_not_a_toggle() {
        let mut s = test_state();
        three_groups(&mut s);
        s.transcript.push_user_shell_started(
            leveler_core::UserShellId::new("ush-9"),
            "cargo test".into(),
            "/repo".into(),
            0,
        );
        s.transcript.complete_user_shell(
            &leveler_core::UserShellId::new("ush-9"),
            Some(0),
            4200,
            crate::transcript::UserShellStatus::Success,
        );
        let shell_idx = s
            .transcript
            .user_shell_index(&leveler_core::UserShellId::new("ush-9"))
            .unwrap();
        let hits = s.conversation_lines_and_hits(80).1.as_ref().clone();
        let (line, item) = *hits.iter().find(|(_, i)| *i == shell_idx).unwrap();
        let row = screen_row_of(&s, line);
        click(&mut s, 3, row);
        assert_eq!(
            s.active_screen,
            crate::screen::Screen::Shell,
            "a user shell row opens Shell Details"
        );
        assert_eq!(s.shell_screen_item, Some(item), "focused on THAT execution");
        assert!(
            expanded_flags(&s).iter().all(|(_, e)| !e),
            "no tool group was toggled by the shell click"
        );
    }

    #[test]
    fn two_groups_stay_expanded_simultaneously() {
        let mut s = test_state();
        three_groups(&mut s);
        let hits = s.conversation_lines_and_hits(80).1.as_ref().clone();
        let (line_a, item_a) = hits[0];
        let row = screen_row_of(&s, line_a);
        click(&mut s, 1, row);
        // A stays open while C is opened — history is not a radio group.
        let hits = s.conversation_lines_and_hits(80).1.as_ref().clone();
        let (line_c, item_c) = *hits.iter().find(|(_, i)| *i > item_a + 1).unwrap();
        let row = screen_row_of(&s, line_c);
        click(&mut s, 1, row);
        let flags = expanded_flags(&s);
        let open: Vec<usize> = flags.iter().filter(|(_, e)| *e).map(|(i, _)| *i).collect();
        assert_eq!(open, vec![item_a, item_c], "both stay open: {flags:?}");
        // Collapse A independently; C must remain open.
        let hits = s.conversation_lines_and_hits(80).1.as_ref().clone();
        let (line_a2, _) = *hits.iter().find(|(_, i)| *i == item_a).unwrap();
        let row = screen_row_of(&s, line_a2);
        click(&mut s, 1, row);
        let flags = expanded_flags(&s);
        let open: Vec<usize> = flags.iter().filter(|(_, e)| *e).map(|(i, _)| *i).collect();
        assert_eq!(open, vec![item_c], "collapsing A leaves C open: {flags:?}");
    }

    #[test]
    fn scrolled_to_middle_click_lands_on_the_painted_row() {
        let mut s = test_state();
        for i in 0..30 {
            s.transcript.push_user(format!("early {i}"));
        }
        three_groups(&mut s);
        for i in 0..30 {
            s.transcript.push_user(format!("late {i}"));
        }
        // Pinned mid-history: the middle disclosure is inside the viewport.
        let (lines, hits) = s.conversation_lines_and_hits(80);
        let (line_b, item_b) = hits[1];
        let (_, ry, _, rh) = s.conv.rect.unwrap();
        let scroll = line_b.saturating_sub(rh as usize / 2);
        let max_scroll = lines.len().saturating_sub(rh as usize);
        let scroll = scroll.min(max_scroll);
        s.conv.auto_scroll = false;
        s.conv.scroll = scroll;
        let row = ry + (line_b - scroll) as u16;
        click(&mut s, 1, row);
        let flags = expanded_flags(&s);
        assert!(
            flags.iter().all(|(i, e)| *e == (*i == item_b)),
            "mid-scroll click toggles the painted group: {flags:?}"
        );
    }

    #[test]
    fn hit_identity_survives_invisible_items_between_groups() {
        let mut s = test_state();
        s.transcript.push_user("q".into());
        // A silent successful probe group renders NOTHING — its transcript
        // item still occupies an index the hit map must step over.
        s.transcript.push_tool_started(
            ToolCallId::new("s1"),
            "run_command".into(),
            r#"{"program":"ls"}"#.into(),
            false,
            0,
        );
        s.transcript
            .complete_tool(&ToolCallId::new("s1"), true, "".into(), 3);
        s.transcript.push_user("between".into());
        finished_tool(&mut s, "w1", "grep", r#"{"pattern":"x"}"#);
        let hits = s.conversation_lines_and_hits(80).1.as_ref().clone();
        assert_eq!(hits.len(), 1, "only the visible work group: {hits:?}");
        let (line, item) = hits[0];
        let row = screen_row_of(&s, line);
        click(&mut s, 1, row);
        let flags = expanded_flags(&s);
        assert!(
            flags.iter().any(|(i, e)| *i == item && *e),
            "the click toggles the grep group, not the silent one: {flags:?}"
        );
    }

    #[test]
    fn hundred_group_history_random_clicks_land() {
        let mut s = test_state();
        s.conv.rect = Some((0, 2, 100, 24));
        for i in 0..100 {
            s.transcript.push_user(format!("turn {i}"));
            finished_tool(
                &mut s,
                &format!("g{i}"),
                if i % 3 == 0 { "grep" } else { "read_file" },
                &format!(r#"{{"path":"f{i}.rs"}}"#),
            );
        }
        let (lines, hits) = s.conversation_lines_and_hits(100);
        assert_eq!(hits.len(), 100);
        let (_, ry, _, rh) = s.conv.rect.unwrap();
        let max_scroll = lines.len().saturating_sub(rh as usize);
        // Deterministic spread of historical picks: near start, middle, end.
        for pick in [3usize, 49, 71, 96] {
            let (line, item) = s.conversation_lines_and_hits(100).1[pick];
            let scroll = line.saturating_sub(rh as usize / 2).min(max_scroll);
            s.conv.auto_scroll = false;
            s.conv.scroll = scroll;
            let row = ry + (line - scroll) as u16;
            click(&mut s, 1, row);
            let flags = expanded_flags(&s);
            assert!(
                flags.iter().all(|(i, e)| *e == (*i == item)),
                "pick {pick}: exactly one group open"
            );
            // Fold back so the next pick starts from a collapsed history.
            let hits = s.conversation_lines_and_hits(100).1.as_ref().clone();
            let (line2, _) = *hits.iter().find(|(_, i)| *i == item).unwrap();
            let scroll2 = line2.saturating_sub(rh as usize / 2);
            let total = s.conversation_lines_and_hits(100).0.len();
            let scroll2 = scroll2.min(total.saturating_sub(rh as usize));
            s.conv.scroll = scroll2;
            let row2 = ry + (line2 - scroll2) as u16;
            click(&mut s, 1, row2);
        }
        assert!(
            expanded_flags(&s).iter().all(|(_, e)| !e),
            "history fully folded again"
        );
    }

    #[test]
    fn live_edge_click_lands_while_auto_following() {
        let mut s = test_state();
        // Dynamic chrome: the painted viewport (height 22) is NOT what a
        // rows-minus-constant guess would give for size (80, 40) — busy
        // status, an active plan, and a tall composer all shrink it.
        s.conv.rect = Some((0, 5, 80, 22));
        for i in 0..40 {
            s.transcript.push_user(format!("filler row {i}"));
        }
        three_groups(&mut s);
        // Live edge: the renderer has been auto-following; the pinned scroll
        // value in state is stale (never synced since the user last scrolled).
        s.conv.auto_scroll = true;
        s.conv.scroll = 0;

        let (lines, hits) = s.conversation_lines_and_hits(80);
        let total = lines.len();
        let (_, ry, _, rh) = s.conv.rect.unwrap();
        let live_scroll = total.saturating_sub(rh as usize);
        let (line_c, item_c) = *hits.last().unwrap();
        assert!(
            line_c >= live_scroll,
            "fixture: the last disclosure must be on the live screen"
        );
        let row = ry + (line_c - live_scroll) as u16;
        click(&mut s, 1, row);

        for (i, expanded) in expanded_flags(&s) {
            assert_eq!(
                expanded,
                i == item_c,
                "the click must land on the group painted under the cursor"
            );
        }
        assert!(s.conv.selection.anchor.is_none(), "no selection began");
        // The viewport was frozen exactly where the live edge was painted —
        // leaving auto-follow must not make the screen jump.
        assert!(!s.conv.auto_scroll);
        assert_eq!(
            s.conv.scroll, live_scroll,
            "pin must capture the painted scroll, not a stale or guessed one"
        );
    }

    #[test]
    fn bottom_aligned_short_transcript_clicks_land_on_each_group() {
        let mut s = test_state();
        three_groups(&mut s);
        // Short transcript renders bottom-aligned: pad rows precede content.
        let hits = s.conversation_lines_and_hits(80).1.as_ref().clone();
        assert_eq!(hits.len(), 3);
        for (line, item) in hits {
            let row = screen_row_of(&s, line);
            click(&mut s, 40, row); // any column on the row is a target
            let flags = expanded_flags(&s);
            assert!(
                flags.iter().all(|(i, e)| *e == (*i == item)),
                "row {line} must toggle item {item} only: {flags:?}"
            );
            let row = screen_row_of(&s, line);
            click(&mut s, 40, row); // fold back before the next round
        }
    }

    #[test]
    fn click_toggles_exactly_the_target_group_and_back() {
        let mut s = test_state();
        three_groups(&mut s);
        let hits = s.conversation_lines_and_hits(80).1;
        assert_eq!(hits.len(), 3, "three disclosure rows: {hits:?}");

        // Expand the MIDDLE (historical) group — not the latest.
        let (line_b, item_b) = hits[1];
        let row = screen_row_of(&s, line_b);
        click(&mut s, 1, row);
        for (i, expanded) in expanded_flags(&s) {
            assert_eq!(expanded, i == item_b, "only the clicked group opens");
        }
        assert!(
            s.conv.selection.anchor.is_none() && !s.conv.selection.dragging,
            "a disclosure click never begins a selection"
        );

        // Click the same header again (rows may have shifted) — collapses it.
        let hits = s.conversation_lines_and_hits(80).1;
        let (line_b2, _) = *hits.iter().find(|(_, i)| *i == item_b).unwrap();
        let row = screen_row_of(&s, line_b2);
        click(&mut s, 1, row);
        assert!(
            expanded_flags(&s).iter().all(|(_, e)| !e),
            "second click collapses the group again"
        );
    }

    #[test]
    fn oldest_group_is_independently_expandable() {
        let mut s = test_state();
        three_groups(&mut s);
        let hits = s.conversation_lines_and_hits(80).1;
        let (line_a, item_a) = hits[0];
        let row = screen_row_of(&s, line_a);
        click(&mut s, 1, row);
        for (i, expanded) in expanded_flags(&s) {
            assert_eq!(expanded, i == item_a, "history is individually addressable");
        }
    }

    #[test]
    fn click_on_plain_text_starts_selection_and_toggles_nothing() {
        let mut s = test_state();
        three_groups(&mut s);
        // Absolute line 0 is the first user row ("q1"), never a disclosure.
        let hits = s.conversation_lines_and_hits(80).1;
        assert!(hits.iter().all(|(line, _)| *line != 0));
        let row = screen_row_of(&s, 0);
        click(&mut s, 1, row);
        assert!(
            expanded_flags(&s).iter().all(|(_, e)| !e),
            "no group toggled by a text-row click"
        );
        assert!(
            s.conv.selection.anchor.is_some(),
            "text click begins a selection"
        );
    }

    #[test]
    fn running_group_has_no_disclosure_until_it_finishes() {
        let mut s = test_state();
        s.transcript.push_user("q".into());
        s.transcript.push_tool_started(
            ToolCallId::new("r1"),
            "run_command".into(),
            r#"{"program":"cargo"}"#.into(),
            false,
            0,
        );
        let running_idx = s.transcript.items().len() - 1;
        let hits = s.conversation_lines_and_hits(80).1;
        assert!(
            hits.iter().all(|(_, i)| *i != running_idx),
            "a running group is not clickable: {hits:?}"
        );
        s.transcript
            .complete_tool(&ToolCallId::new("r1"), true, "ok".into(), 10);
        let hits = s.conversation_lines_and_hits(80).1;
        assert!(
            hits.iter().any(|(_, i)| *i == running_idx),
            "completion turns the group into a disclosure: {hits:?}"
        );
    }

    #[test]
    fn resize_rebuilds_hit_rows_and_click_still_lands() {
        let mut s = test_state();
        three_groups(&mut s);
        let hits = s.conversation_lines_and_hits(80).1;
        let row = screen_row_of(&s, hits[1].0);
        click(&mut s, 1, row);
        let item_b = hits[1].1;

        // Narrow the viewport — wraps change, the hit map must follow.
        s.conv.rect = Some((0, 2, 60, 30));
        s.conv.plain.clear();
        s.conv.plain_width = 0;
        let hits = s.conversation_lines_and_hits(60).1;
        let (line_b, _) = *hits.iter().find(|(_, i)| *i == item_b).unwrap();
        let row = screen_row_of(&s, line_b);
        click(&mut s, 1, row);
        assert!(
            expanded_flags(&s).iter().all(|(_, e)| !e),
            "post-resize click targets the same group (collapses it)"
        );
    }

    #[test]
    fn user_expanded_group_survives_new_transcript_events() {
        let mut s = test_state();
        three_groups(&mut s);
        let hits = s.conversation_lines_and_hits(80).1;
        let (line_b, item_b) = hits[1];
        let row = screen_row_of(&s, line_b);
        click(&mut s, 1, row);

        s.transcript.push_user("next question".into());
        finished_tool(&mut s, "d1", "grep", r#"{"pattern":"y"}"#);
        let flags = expanded_flags(&s);
        assert!(
            flags.iter().any(|(i, e)| *i == item_b && *e),
            "streaming new events must not auto-collapse a user-opened group: {flags:?}"
        );
    }
}
