//! The event loop: bridges terminal input and runtime events into the reducer,
//! performs the reducer's effects, and redraws.
//!
//! Rendering uses the **alternate screen** for the full workbench (header,
//! conversation viewport, composer, overlays). This is intentional: viewport
//! scroll, drag-select, and fixed chrome are not compatible with native
//! scrollback.

use std::collections::VecDeque;
use std::io::{self, Stdout};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::cursor;
use crossterm::event::{
    Event as CtEvent, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, size};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::{broadcast::error::RecvError, mpsc};

use leveler_client_protocol::{
    ClientCommand, CommandEnvelope, CommandId, InteractiveRuntimeClient, NotificationLevel,
    ProtocolEnvelope, RuntimeEvent, SessionId,
};

use crate::action::{Action, Effect, EffectCompletion, WebLauncher};
use crate::reducer::reduce;
use crate::render::render;
use crate::screen::Screen;
use crate::state::{AppState, Boot, Notification, PendingInteraction};
use crate::terminal::TerminalGuard;
use crate::theme::Theme;

enum DeliveryJob {
    Command {
        session_id: SessionId,
        command: ClientCommand,
    },
    Interaction {
        session_id: SessionId,
        command: ClientCommand,
        restore: PendingInteraction,
        command_id: CommandId,
        key: String,
    },
}

const DELIVERY_TIMEOUT: Duration = Duration::from_secs(10);

/// Default status-line notification TTL (warnings). Info is shorter; errors stick.
const NOTIFICATION_TTL_WARNING: Duration = Duration::from_secs(8);
const NOTIFICATION_TTL_INFO: Duration = Duration::from_secs(4);
/// Animation / clock cadence for the busy spinner and header wall clock.
///
/// Drives the busy spinner AND the animated header progress line, so it must run
/// subsecond to look smooth. It only forces a repaint while `is_busy()` (line
/// ~390), so idle sessions stay quiet; the extra frames during a task are the
/// cost of the moving top line (a deliberate visual, unlike the old silent
/// waits). The elapsed clock is `Instant`-based, independent of this cadence.
const BUSY_TICK: Duration = Duration::from_millis(150);
/// Cadence for Conversation edge auto-scroll while drag-selecting text.
const SELECTION_TICK: Duration = Duration::from_millis(50);
/// Coalescing window for PTYs that deliver pasted text as plain key events.
const INPUT_BURST_WINDOW: Duration = Duration::from_millis(2);
/// How many history entries persist across restarts.
const HISTORY_CAP: usize = 100;

/// Errors running the terminal UI.
#[derive(Debug, thiserror::Error)]
pub enum TuiError {
    #[error("terminal io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Run the interactive terminal UI against a runtime client until the user
/// quits. Restores the terminal on any exit path.
pub async fn run(
    client: Arc<dyn InteractiveRuntimeClient>,
    web_launcher: Option<WebLauncher>,
    boot: Boot,
) -> Result<(), TuiError> {
    let (mut guard, mut stdout) = TerminalGuard::enter()?;
    execute!(stdout, Clear(ClearType::Purge), cursor::MoveTo(0, 0))?;

    let theme_id = crate::theme_config::load_theme_id().unwrap_or(crate::theme::ThemeId::Ion);
    let mut state = AppState::new(Theme::resolve(theme_id, Theme::env_no_color()), boot);
    let (cols, rows) = size().unwrap_or((80, 24));
    state.size = (cols, rows);
    state.clock_label = chrono::Local::now().format("%H:%M").to_string();

    // Restore a persisted composer draft (spec §24) and the input history.
    if let Some(path) = state.draft_path()
        && let Ok(text) = std::fs::read_to_string(path)
        && !text.trim().is_empty()
    {
        state.composer.replace(text);
    }
    if let Some(path) = state.history_path()
        && let Ok(text) = std::fs::read_to_string(path)
        && let Ok(history) = serde_json::from_str::<Vec<String>>(&text)
    {
        state.composer.set_history(history);
    }

    let mut events = client.subscribe_session(&state.session_id);
    // When the broadcast channel closes, stop selecting on it so we don't
    // busy-spin on RecvError::Closed.
    let mut events_open = true;
    let (completion_tx, mut completion_rx) = mpsc::unbounded_channel::<Action>();
    let (delivery_tx, mut delivery_rx) = mpsc::unbounded_channel::<DeliveryJob>();
    let delivery_client = Arc::clone(&client);
    let delivery_completions = completion_tx.clone();
    tokio::spawn(async move {
        while let Some(job) = delivery_rx.recv().await {
            let completion = match job {
                DeliveryJob::Command {
                    session_id,
                    command,
                } => {
                    if tokio::time::timeout(
                        DELIVERY_TIMEOUT,
                        delivery_client.issue(session_id.clone(), command),
                    )
                    .await
                    .is_ok_and(|result| result.is_ok())
                    {
                        EffectCompletion::CommandDelivered
                    } else {
                        EffectCompletion::CommandFailed {
                            snapshot: tokio::time::timeout(
                                DELIVERY_TIMEOUT,
                                delivery_client.snapshot(&session_id),
                            )
                            .await
                            .ok()
                            .and_then(Result::ok)
                            .map(Box::new),
                        }
                    }
                }
                DeliveryJob::Interaction {
                    session_id,
                    command,
                    restore,
                    command_id,
                    key,
                } => {
                    let envelope = ProtocolEnvelope::wrap(CommandEnvelope {
                        command_id,
                        session_id: session_id.clone(),
                        expected_version: None,
                        issued_at: leveler_core::now().to_rfc3339(),
                        command,
                    });
                    match tokio::time::timeout(
                        DELIVERY_TIMEOUT,
                        delivery_client.deliver_protocol(envelope),
                    )
                    .await
                    {
                        Ok(Ok(())) => EffectCompletion::InteractionDelivered { key },
                        Ok(Err(_)) | Err(_) => EffectCompletion::InteractionUncertain {
                            key,
                            restore,
                            snapshot: tokio::time::timeout(
                                DELIVERY_TIMEOUT,
                                delivery_client.snapshot(&session_id),
                            )
                            .await
                            .ok()
                            .and_then(Result::ok)
                            .map(Box::new),
                        },
                    }
                }
            };
            let _ = delivery_completions.send(Action::EffectCompleted(completion));
        }
    });
    if let Ok(snapshot) = client.snapshot(&state.session_id).await {
        reduce(
            &mut state,
            Action::Runtime(RuntimeEvent::SessionOpened { session: snapshot }),
        );
    }

    let mut term_events = EventStream::new();
    let mut tick = tokio::time::interval(BUSY_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut selection_tick = tokio::time::interval(SELECTION_TICK);
    selection_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut alt: Option<Terminal<CrosstermBackend<Stdout>>> = None;
    let mut pending_runtime_paint = false;
    let mut pending_terminal_actions: VecDeque<Action> = VecDeque::new();
    // The notification currently on screen and when it appeared, for expiry.
    let mut note_shown: Option<(Notification, Instant)> = None;

    paint(&mut alt, &mut stdout, &mut state)?;

    while state.running {
        let mut effects: Vec<Effect> = Vec::new();
        let mut paint_now = false;
        let mut ticked = false;

        if let Some(action) = pending_terminal_actions.pop_front() {
            effects = reduce(&mut state, action);
            paint_now = true;
        } else {
            tokio::select! {
                maybe = term_events.next() => {
                if let Some(Ok(ev)) = maybe {
                    if let CtEvent::Resize(c, r) = ev {
                        state.size = (c, r);
                    }
                    if let Some(action) = map_terminal_event(ev) {
                        let action = maybe_coalesce_text_input(
                            action,
                            &state,
                            &mut term_events,
                            &mut pending_terminal_actions,
                        )
                        .await;
                        effects = reduce(&mut state, action);
                    }
                    paint_now = true;
                }
                },
                received = events.recv(), if events_open => match received {
                    Ok(event) => {
                        if let RuntimeEvent::SessionOpened { session } = &event
                            && session.id != state.session_id
                        {
                            events = client.subscribe_session(&session.id);
                            events_open = true;
                        }
                        effects = reduce(&mut state, Action::Runtime(event));
                        pending_runtime_paint = true;
                    }
                    Err(RecvError::Lagged(_)) => {
                        match client.snapshot(&state.session_id).await {
                            Ok(snapshot) => {
                                reduce(
                                    &mut state,
                                    Action::Runtime(RuntimeEvent::SessionOpened {
                                        session: snapshot,
                                    }),
                                );
                            }
                            Err(error) => {
                                // Resync failed: surface it instead of silently
                                // dropping to a stale view. A later event or a
                                // manual refresh retries.
                                state.notification = Some(Notification {
                                    level: NotificationLevel::Error,
                                    message: format!("事件流滞后后重同步失败：{error}"),
                                });
                            }
                        }
                        pending_runtime_paint = true;
                    }
                    Err(RecvError::Closed) => {
                        events_open = false;
                        state.runtime_connected = false;
                        state.notification = Some(Notification {
                            level: NotificationLevel::Error,
                            message: "与运行时的事件流已断开".to_string(),
                        });
                        pending_runtime_paint = true;
                    }
                },
                Some(action) = completion_rx.recv() => {
                    effects = reduce(&mut state, action);
                    paint_now = true;
                }
                _ = tick.tick() => { ticked = true; }
                _ = selection_tick.tick(), if state.selection.dragging => {
                    effects = reduce(&mut state, Action::SelectionTick);
                    if state.selection_edge_dir != 0 {
                        paint_now = true;
                    }
                }
            }
        }

        dispatch_effects(
            &mut state,
            effects,
            &completion_tx,
            &delivery_tx,
            &web_launcher,
        );

        // Busy spinner + elapsed clock, and draining queued input when idle.
        state.tick = state.tick.wrapping_add(1);
        if state.is_busy() {
            let start = *state.turn_started_at.get_or_insert_with(Instant::now);
            state.elapsed_secs = start.elapsed().as_secs();
        } else {
            state.turn_started_at = None;
            state.elapsed_secs = 0;
            let queued = crate::reducer::drain_queued(&mut state);
            dispatch_effects(
                &mut state,
                queued,
                &completion_tx,
                &delivery_tx,
                &web_launcher,
            );
        }

        // Wall clock in the header — repaint when the minute rolls over.
        let clock = chrono::Local::now().format("%H:%M").to_string();
        if clock != state.clock_label {
            state.clock_label = clock;
            paint_now = true;
        }

        // Fade a stale notification so old notices don't linger forever.
        if expire_notification(&mut state, &mut note_shown, Instant::now()) {
            paint_now = true;
        }

        // End / Ctrl+End returns to the conversation's live edge.
        if state.jump_to_bottom {
            state.jump_to_bottom = false;
            state.active_screen = Screen::Conversation;
            paint_now = true;
        }

        // Keep conversation viewport scroll in range as content/layout changes.
        if state.active_screen == Screen::Conversation {
            let width = state.size.0.max(1) as usize;
            let height = state.size.1.saturating_sub(10).max(3) as usize;
            if crate::workbench::sync_conversation_scroll(&mut state, width, height) {
                paint_now = true;
            }
        }

        // Repaint on input, on runtime updates, and — while busy — on the tick
        // (spinner/elapsed). Idle ticks only repaint when the wall clock minute
        // changes (handled above via paint_now).
        if paint_now || (ticked && state.is_busy()) || (!state.is_busy() && pending_runtime_paint) {
            paint(&mut alt, &mut stdout, &mut state)?;
            pending_runtime_paint = false;
        }
    }

    // Leave the alternate screen before restoring terminal state.
    if alt.is_some() {
        let _ = execute!(stdout, LeaveAlternateScreen);
    }

    // Persist (or clear) the composer draft for next launch (spec §24).
    if let Some(path) = state.draft_path() {
        let draft = state.composer.text();
        if draft.trim().is_empty() {
            let _ = std::fs::remove_file(path);
        } else if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
            let _ = std::fs::write(path, draft);
        }
    }
    // Persist the input history (last HISTORY_CAP entries) for ↑ recall.
    if let Some(path) = state.history_path() {
        let history = state.composer.history();
        let tail = &history[history.len().saturating_sub(HISTORY_CAP)..];
        if let (Ok(json), Some(parent)) = (serde_json::to_string(tail), path.parent()) {
            let _ = std::fs::create_dir_all(parent);
            let _ = std::fs::write(path, json);
        }
    }

    let session_id = state.session_id.clone();
    guard.restore();
    // After raw mode is off: print full resume command so the user can reconnect.
    println!("{}", session_exit_hint(session_id.as_str()));
    Ok(())
}

/// Text printed when the TUI exits (full copy-paste command to reopen chat).
///
/// Leaving the alternate screen restores the primary screen, which may still
/// hold pre-launch content on these rows. Each line ends with `\x1b[K` (clear to
/// end of line) — and the last with `\x1b[J` (clear to end of screen) — so that
/// stale content cannot bleed into the printed lines (which previously appended
/// a stray character onto the copy-paste `resume` command).
fn session_exit_hint(session_id: &str) -> String {
    format!(
        "Session: {session_id}\x1b[K\n\
         Reopen:  leveler resume {session_id}\x1b[K\n\
         (or `leveler resume` to pick from recent sessions)\x1b[J"
    )
}

/// Carry out the reducer's effects. A failed send means the runtime side is
/// gone — surface that instead of pretending the action happened.
fn dispatch_effects(
    state: &mut AppState,
    effects: Vec<Effect>,
    completion_tx: &mpsc::UnboundedSender<Action>,
    delivery_tx: &mpsc::UnboundedSender<DeliveryJob>,
    web_launcher: &Option<WebLauncher>,
) {
    for effect in effects {
        match effect {
            Effect::Send(command) => {
                if !state.runtime_connected {
                    state.notification = Some(Notification {
                        level: NotificationLevel::Error,
                        message: "事件流已断开；命令已禁用，请退出后重新连接".to_string(),
                    });
                    continue;
                }
                let session_id = state.session_id.clone();
                let _ = delivery_tx.send(DeliveryJob::Command {
                    session_id,
                    command,
                });
            }
            Effect::SendInteraction {
                command,
                restore,
                command_id,
            } => {
                let key = restore.request_key();
                // Keep the sticky id so a user retry reuses the same envelope.
                state
                    .interaction_command_ids
                    .insert(key.clone(), command_id.clone());
                if !state.runtime_connected {
                    crate::reducer::overlay_keys::restore_interaction_overlay(state, restore);
                    state.notification = Some(Notification {
                        level: NotificationLevel::Error,
                        message: "事件流已断开；审批/澄清未发送，请退出后重新连接".to_string(),
                    });
                    continue;
                }
                let session_id = state.session_id.clone();
                let _ = delivery_tx.send(DeliveryJob::Interaction {
                    session_id,
                    command,
                    restore,
                    command_id,
                    key,
                });
            }
            Effect::LoadFileCandidates { repository } => {
                let tx = completion_tx.clone();
                tokio::spawn(async move {
                    let files =
                        tokio::task::spawn_blocking(move || collect_project_files(&repository))
                            .await
                            .unwrap_or_default();
                    let _ = tx.send(Action::FileCandidatesLoaded(files));
                });
            }
            Effect::StartWeb => match web_launcher {
                Some(launcher) => {
                    let launcher = Arc::clone(launcher);
                    let tx = completion_tx.clone();
                    tokio::spawn(async move {
                        let result = launcher().await;
                        let _ = tx.send(Action::WebLaunched(result));
                    });
                }
                None => {
                    let _ = completion_tx.send(Action::WebLaunched(Err(
                        "当前 TUI 连接的是远程 daemon，不能就地起 Web UI；\
                         请改用 `leveler web --connect`"
                            .to_string(),
                    )));
                }
            },
            Effect::OpenWebUrl(url) => open_in_browser(&url),
            Effect::Quit => state.running = false,
        }
    }
}

/// Open `url` in the platform default browser (best-effort, non-blocking).
/// Shared by the `/web` first launch (CLI launcher closure) and re-invocation
/// (the [`Effect::OpenWebUrl`] path) so both behave identically.
pub fn open_in_browser(url: &str) {
    let (program, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else if cfg!(target_os = "windows") {
        // `start` needs an empty title argument before the URL.
        ("cmd", vec!["/C", "start", "", url])
    } else {
        ("xdg-open", vec![url])
    };
    let _ = std::process::Command::new(program).args(args).spawn();
}

fn collect_project_files(repository: &str) -> Vec<String> {
    let root = std::path::Path::new(repository);
    if !root.is_dir() {
        return Vec::new();
    }
    if let Ok(output) = std::process::Command::new("git")
        .args([
            "-C",
            repository,
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .output()
        && output.status.success()
    {
        let mut files: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| !line.is_empty())
            .take(20_000)
            .map(str::to_string)
            .collect();
        files.sort();
        files.dedup();
        return files;
    }

    fn visit(root: &std::path::Path, dir: &std::path::Path, out: &mut Vec<String>) {
        if out.len() >= 20_000 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            if path.is_dir() {
                if matches!(
                    name.to_str(),
                    Some(".git" | "target" | "node_modules" | ".venv")
                ) {
                    continue;
                }
                visit(root, &path, out);
            } else if path.is_file()
                && let Ok(relative) = path.strip_prefix(root)
            {
                out.push(relative.to_string_lossy().replace('\\', "/"));
            }
            if out.len() >= 20_000 {
                break;
            }
        }
    }

    let mut files = Vec::new();
    visit(root, root, &mut files);
    files.sort();
    files
}

/// Clear `state.notification` once it has been on screen longer than its TTL.
/// Errors stick until Esc / next action; info is short-lived, warnings longer.
fn expire_notification(
    state: &mut AppState,
    shown: &mut Option<(Notification, Instant)>,
    now: Instant,
) -> bool {
    match (&state.notification, shown.as_ref()) {
        (Some(current), _) if current.message == "再按一次 Ctrl+C 退出" => {
            *shown = Some((current.clone(), now));
            false
        }
        // Errors are sticky (also written into the transcript).
        (Some(current), _) if current.level == NotificationLevel::Error => {
            *shown = Some((current.clone(), now));
            false
        }
        (Some(current), Some((seen, at))) if current == seen => {
            let ttl = match current.level {
                NotificationLevel::Info => NOTIFICATION_TTL_INFO,
                NotificationLevel::Warning => NOTIFICATION_TTL_WARNING,
                NotificationLevel::Error => return false,
            };
            // saturating: never panic if a caller passes a `now` before `at`.
            if now.saturating_duration_since(*at) >= ttl {
                state.notification = None;
                // The Ctrl+C escalation window closes with its prompt.
                state.disarm_ctrlc();
                *shown = None;
                return true;
            }
            false
        }
        (Some(current), _) => {
            *shown = Some((current.clone(), now));
            false
        }
        (None, _) => {
            *shown = None;
            false
        }
    }
}

/// Draw one frame on the alternate-screen workbench.
/// Conversation uses fixed Header / Conversation / Plan / Input / Footer with
/// viewport scroll — not native terminal scrollback.
fn paint(
    alt: &mut Option<Terminal<CrosstermBackend<Stdout>>>,
    stdout: &mut Stdout,
    state: &mut AppState,
) -> Result<(), TuiError> {
    if alt.is_none() {
        execute!(stdout, EnterAlternateScreen)?;
        let mut t = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        t.clear()?;
        *alt = Some(t);
    }
    if let Some(t) = alt {
        t.draw(|f| render(f, state))?;
    }
    Ok(())
}

fn map_terminal_event(event: CtEvent) -> Option<Action> {
    match event {
        CtEvent::Key(key) => Some(Action::Key(key)),
        CtEvent::Mouse(mouse) => Some(Action::Mouse(mouse)),
        CtEvent::Paste(text) => Some(Action::Paste(text)),
        CtEvent::Resize(cols, rows) => Some(Action::Resize(cols, rows)),
        _ => None,
    }
}

async fn maybe_coalesce_text_input(
    first: Action,
    state: &AppState,
    term_events: &mut EventStream,
    pending: &mut VecDeque<Action>,
) -> Action {
    let Action::Key(key) = first else {
        return first;
    };
    let Some(first_char) = plain_text_char(&key) else {
        return Action::Key(key);
    };
    if state.overlay.is_some() || state.active_screen != Screen::Conversation {
        return Action::Key(key);
    }

    let mut text = String::new();
    text.push(first_char);
    loop {
        let next = tokio::time::timeout(INPUT_BURST_WINDOW, term_events.next()).await;
        let Ok(Some(Ok(event))) = next else {
            break;
        };
        let Some(action) = map_terminal_event(event) else {
            continue;
        };
        match action {
            Action::Key(key) => {
                if let Some(ch) = plain_text_char(&key) {
                    text.push(ch);
                } else {
                    pending.push_back(Action::Key(key));
                    break;
                }
            }
            other => {
                pending.push_back(other);
                break;
            }
        }
    }
    Action::TextInput(text)
}

fn plain_text_char(key: &KeyEvent) -> Option<char> {
    if key.kind == KeyEventKind::Release
        || key.modifiers.contains(KeyModifiers::CONTROL)
        || key.modifiers.contains(KeyModifiers::ALT)
    {
        return None;
    }
    match key.code {
        KeyCode::Char(c) if !c.is_control() => Some(c),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> AppState {
        AppState::new(
            Theme::no_color(),
            Boot {
                session_id: leveler_client_protocol::SessionId::new("s1"),
                user: "u".into(),
                version: "0".into(),
                show_welcome: false,
                draft_path: None,
                history_path: None,
                context_window: 0,
                locale: crate::i18n::Locale::Zh,
            },
        )
    }

    fn note() -> Notification {
        Notification {
            level: NotificationLevel::Info,
            message: "hi".into(),
        }
    }

    #[test]
    fn notification_expires_after_ttl() {
        let mut s = state();
        s.notification = Some(note());
        // Drive time by ADDING to a base, never subtracting a TTL from
        // Instant::now() (which underflows Instant's monotonic epoch and panics
        // on low-uptime Windows runners).
        let base = Instant::now();
        let mut shown = None;
        // First sighting: stamped, not cleared.
        assert!(!expire_notification(&mut s, &mut shown, base));
        assert!(s.notification.is_some());
        // Advance the clock past the TTL: cleared and reported.
        shown = Some((note(), base));
        assert!(expire_notification(
            &mut s,
            &mut shown,
            base + NOTIFICATION_TTL_INFO
        ));
        assert!(s.notification.is_none());
    }

    #[test]
    fn replaced_notification_restarts_the_clock() {
        let mut s = state();
        s.notification = Some(note());
        let mut shown = Some((
            Notification {
                level: NotificationLevel::Warning,
                message: "old".into(),
            },
            Instant::now(),
        ));
        // Different message on screen: re-stamp, do not clear.
        assert!(!expire_notification(&mut s, &mut shown, Instant::now()));
        assert!(s.notification.is_some());
    }

    #[test]
    fn quit_confirmation_notification_does_not_expire() {
        let mut s = state();
        s.quit_armed = true;
        s.notification = Some(Notification {
            level: NotificationLevel::Info,
            message: "再按一次 Ctrl+C 退出".into(),
        });
        let mut shown = Some((s.notification.clone().unwrap(), Instant::now()));

        assert!(!expire_notification(&mut s, &mut shown, Instant::now()));
        assert!(s.quit_armed);
        assert_eq!(
            s.notification.as_ref().map(|n| n.message.as_str()),
            Some("再按一次 Ctrl+C 退出")
        );
    }

    #[test]
    fn session_exit_hint_includes_full_reopen_command() {
        let id = "c1be5e5e-c3f8-4caa-abf4-18f66eb0aa57";
        let hint = session_exit_hint(id);
        assert!(
            hint.contains(&format!("leveler resume {id}")),
            "must be a full copy-paste reopen command: {hint}"
        );
        // The reopen line must clear to end of line right after the id so stale
        // primary-screen content can't append a stray char to the command.
        assert!(
            hint.contains(&format!("leveler resume {id}\x1b[K")),
            "reopen line must clear residue after the id: {hint:?}"
        );
    }

    #[test]
    fn error_notification_is_sticky() {
        let mut s = state();
        s.notification = Some(Notification {
            level: NotificationLevel::Error,
            message: "boom".into(),
        });
        let mut shown = Some((s.notification.clone().unwrap(), Instant::now()));
        assert!(!expire_notification(&mut s, &mut shown, Instant::now()));
        assert_eq!(
            s.notification.as_ref().map(|n| n.message.as_str()),
            Some("boom")
        );
    }
}
