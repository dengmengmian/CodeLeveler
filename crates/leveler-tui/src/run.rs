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
    ClientCommand, ClientError, CommandEnvelope, CommandId, InteractiveRuntimeClient,
    NotificationLevel, ProtocolEnvelope, RuntimeEvent, SessionId, UiSessionSnapshot,
};

use crate::action::{Action, Effect, EffectCompletion, UrlOpener, WebLauncher};
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
    Submission {
        envelope: CommandEnvelope,
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
/// Pause between re-deliveries of a submission that got no answer. Grows to
/// the cap so a runtime that stays away costs a connection attempt every few
/// seconds, not a busy loop.
const REDELIVERY_BACKOFF_MIN: Duration = Duration::from_millis(500);
const REDELIVERY_BACKOFF_MAX: Duration = Duration::from_secs(5);

type DeliveryAttempt = tokio::task::JoinHandle<Result<(), ClientError>>;

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

/// Fallback when the host did not inject a Web launcher. Local Unix-socket
/// daemons are not this case — they carry a launcher. Do not call them remote.
const WEB_LAUNCHER_UNAVAILABLE: &str = "当前 TUI 没有可用的 Web UI 启动器";

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
    url_opener: Option<UrlOpener>,
    remote_launcher: Option<crate::action::RemoteLauncher>,
    boot: Boot,
) -> Result<(), TuiError> {
    let (mut guard, mut stdout) = TerminalGuard::enter()?;
    execute!(stdout, Clear(ClearType::Purge), cursor::MoveTo(0, 0))?;

    let theme_id = crate::theme_config::load_theme_id().unwrap_or(crate::theme::ThemeId::DEFAULT);
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
                    let client = Arc::clone(&delivery_client);
                    let issuer = session_id.clone();
                    let mut attempt =
                        tokio::spawn(async move { client.issue(issuer, command).await });
                    let answer = match tokio::time::timeout(DELIVERY_TIMEOUT, &mut attempt).await {
                        Ok(joined) => delivery_answer(joined),
                        Err(_) => DeliveryAnswer::None,
                    };
                    match answer {
                        DeliveryAnswer::Delivered => EffectCompletion::CommandDelivered,
                        DeliveryAnswer::Rejected(message) => EffectCompletion::CommandRejected {
                            message,
                            snapshot: snapshot_within(&delivery_client, &session_id).await,
                        },
                        // A fresh id per issue never meets an unsettled receipt;
                        // were it to, it is still no delivery this client saw.
                        DeliveryAnswer::Unresolvable | DeliveryAnswer::None => {
                            EffectCompletion::CommandUncertain {
                                snapshot: snapshot_within(&delivery_client, &session_id).await,
                            }
                        }
                    }
                }
                DeliveryJob::Submission { envelope } => {
                    match first_submission_attempt(&delivery_client, &envelope).await {
                        Ok(settled) => settled,
                        Err(in_flight) => {
                            let command_id = envelope.command_id.clone();
                            // Settle off the worker: a Cancel must not queue
                            // behind a runtime that is away.
                            let client = Arc::clone(&delivery_client);
                            let completions = delivery_completions.clone();
                            tokio::spawn(async move {
                                let settled = settle_submission(client, envelope, in_flight).await;
                                let _ = completions.send(Action::EffectCompleted(settled));
                            });
                            EffectCompletion::SubmissionUnconfirmed { command_id }
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
    let mut tab_title = crate::terminal_title::TerminalTitleProjection::default();
    let mut pending_runtime_paint = false;
    let mut pending_terminal_actions: VecDeque<Action> = VecDeque::new();
    // The notification currently on screen and when it appeared, for expiry.
    let mut note_shown: Option<(Notification, Instant)> = None;

    paint(&mut alt, &mut stdout, &mut state, &mut tab_title)?;

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
                _ = selection_tick.tick(), if state.conv.selection.dragging => {
                    effects = reduce(&mut state, Action::SelectionTick);
                    if state.conv.selection_edge_dir != 0 {
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
            &url_opener,
            &remote_launcher,
            &mut alt,
            &mut stdout,
        );

        // Busy spinner + elapsed clock.
        state.tick = state.tick.wrapping_add(1);
        if state.is_busy() {
            let start = *state.turn_started_at.get_or_insert_with(Instant::now);
            state.elapsed_secs = start.elapsed().as_secs();
        } else {
            state.turn_started_at = None;
            state.elapsed_secs = 0;
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
        if state.active_screen == Screen::Conversation
            && crate::conversation::sync_scroll(&mut state)
        {
            paint_now = true;
        }

        // Repaint on input, on runtime updates, and — while busy — on the tick
        // (spinner/elapsed). Idle ticks only repaint when the wall clock minute
        // changes (handled above via paint_now).
        if paint_now || (ticked && state.is_busy()) || (!state.is_busy() && pending_runtime_paint) {
            paint(&mut alt, &mut stdout, &mut state, &mut tab_title)?;
            pending_runtime_paint = false;
        }
    }

    // Leave the alternate screen before restoring terminal state.
    if alt.is_some() {
        let _ = execute!(stdout, LeaveAlternateScreen);
    }

    // Persist (or clear) the composer draft for next launch (spec §24).
    if let Some(path) = state.draft_path() {
        let draft = state.composer.canonical_text();
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

/// A delivery attempt runs as its own task: a timeout here abandons only the
/// wait. Dropping the future instead would cancel an in-process runtime midway
/// through admitting the command and strand its receipt.
fn spawn_delivery(
    client: &Arc<dyn InteractiveRuntimeClient>,
    envelope: CommandEnvelope,
) -> DeliveryAttempt {
    let client = Arc::clone(client);
    tokio::spawn(async move {
        client
            .deliver_protocol(ProtocolEnvelope::wrap(envelope))
            .await
    })
}

/// What the runtime answered for one delivery attempt.
enum DeliveryAnswer {
    Delivered,
    /// Rejected, in the runtime's own words.
    Rejected(String),
    /// Admitted, with an outcome the runtime proves it cannot recover.
    Unresolvable,
    /// No answer — nothing may be concluded.
    None,
}

fn delivery_answer(
    joined: Result<Result<(), ClientError>, tokio::task::JoinError>,
) -> DeliveryAnswer {
    match joined {
        Ok(Ok(())) => DeliveryAnswer::Delivered,
        Ok(Err(ClientError::OutcomeUnknown(_))) | Err(_) => DeliveryAnswer::None,
        Ok(Err(ClientError::Unresolvable(_))) => DeliveryAnswer::Unresolvable,
        Ok(Err(ClientError::Runtime(message))) => DeliveryAnswer::Rejected(message),
        Ok(Err(error)) => DeliveryAnswer::Rejected(error.to_string()),
    }
}

async fn snapshot_within(
    client: &Arc<dyn InteractiveRuntimeClient>,
    session_id: &SessionId,
) -> Option<Box<UiSessionSnapshot>> {
    tokio::time::timeout(DELIVERY_TIMEOUT, client.snapshot(session_id))
        .await
        .ok()
        .and_then(Result::ok)
        .map(Box::new)
}

/// The first delivery of a submission, bounded by [`DELIVERY_TIMEOUT`]. `Ok` is
/// the runtime's answer; `Err` means none yet, carrying the attempt that is
/// still running (if it is) so its answer is not thrown away.
async fn first_submission_attempt(
    client: &Arc<dyn InteractiveRuntimeClient>,
    envelope: &CommandEnvelope,
) -> Result<EffectCompletion, Option<DeliveryAttempt>> {
    let mut attempt = spawn_delivery(client, envelope.clone());
    let answer = match tokio::time::timeout(DELIVERY_TIMEOUT, &mut attempt).await {
        Ok(joined) => delivery_answer(joined),
        Err(_) => return Err(Some(attempt)),
    };
    match answer {
        DeliveryAnswer::Delivered => Ok(EffectCompletion::SubmissionDelivered {
            command_id: envelope.command_id.clone(),
            snapshot: None,
        }),
        DeliveryAnswer::Rejected(message) => Ok(EffectCompletion::SubmissionRejected {
            command_id: envelope.command_id.clone(),
            message,
            snapshot: snapshot_within(client, &envelope.session_id).await,
        }),
        DeliveryAnswer::Unresolvable => Ok(EffectCompletion::SubmissionUnresolvable {
            command_id: envelope.command_id.clone(),
            snapshot: snapshot_within(client, &envelope.session_id).await,
        }),
        DeliveryAnswer::None => Err(None),
    }
}

/// Re-deliver an unanswered submission until the runtime answers it. Every
/// attempt carries the same envelope — the same command id — so the runtime's
/// durable receipt turns them into at most one dispatch, and an attempt that
/// finds the command already admitted is answered "delivered". Only an answer
/// ends this; no answer is never read as "not delivered".
async fn settle_submission(
    client: Arc<dyn InteractiveRuntimeClient>,
    envelope: CommandEnvelope,
    mut in_flight: Option<DeliveryAttempt>,
) -> EffectCompletion {
    let mut backoff = REDELIVERY_BACKOFF_MIN;
    loop {
        let attempt = match in_flight.take() {
            Some(attempt) => attempt,
            None => {
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(REDELIVERY_BACKOFF_MAX);
                spawn_delivery(&client, envelope.clone())
            }
        };
        let answer = delivery_answer(attempt.await);
        if matches!(answer, DeliveryAnswer::None) {
            continue;
        }
        // The answer may come after a reconnect: resync with what the runtime
        // did meanwhile.
        let snapshot = snapshot_within(&client, &envelope.session_id).await;
        let command_id = envelope.command_id;
        return match answer {
            DeliveryAnswer::Delivered => EffectCompletion::SubmissionDelivered {
                command_id,
                snapshot,
            },
            DeliveryAnswer::Rejected(message) => EffectCompletion::SubmissionRejected {
                command_id,
                message,
                snapshot,
            },
            // Final: the loop ends here and the old id is never sent again.
            DeliveryAnswer::Unresolvable | DeliveryAnswer::None => {
                EffectCompletion::SubmissionUnresolvable {
                    command_id,
                    snapshot,
                }
            }
        };
    }
}

/// Carry out the reducer's effects. A failed send means the runtime side is
/// gone — surface that instead of pretending the action happened.
#[allow(clippy::too_many_arguments)]
fn dispatch_effects(
    state: &mut AppState,
    effects: Vec<Effect>,
    completion_tx: &mpsc::UnboundedSender<Action>,
    delivery_tx: &mpsc::UnboundedSender<DeliveryJob>,
    web_launcher: &Option<WebLauncher>,
    url_opener: &Option<UrlOpener>,
    remote_launcher: &Option<crate::action::RemoteLauncher>,
    // `$EDITOR` takes the terminal over, so it needs the alternate-screen
    // handle (dropped for the duration) and stdout to restore modes on.
    alt: &mut Option<Terminal<CrosstermBackend<Stdout>>>,
    stdout: &mut Stdout,
) {
    for effect in effects {
        match effect {
            Effect::Send(command) => {
                if !state.runtime_connected {
                    state.notification = Some(Notification {
                        level: NotificationLevel::Error,
                        message: state.t().commands_disabled_disconnected.to_string(),
                    });
                    continue;
                }
                let session_id = state.session_id.clone();
                let _ = delivery_tx.send(DeliveryJob::Command {
                    session_id,
                    command,
                });
            }
            // The reducer only emits this while the runtime is connected, and
            // already holds the id in `pending_submissions`.
            Effect::Submit {
                command,
                command_id,
            } => {
                let _ = delivery_tx.send(DeliveryJob::Submission {
                    envelope: CommandEnvelope {
                        command_id,
                        session_id: state.session_id.clone(),
                        expected_version: None,
                        issued_at: leveler_core::now().to_rfc3339(),
                        command,
                    },
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
                        WEB_LAUNCHER_UNAVAILABLE.to_string()
                    )));
                }
            },
            Effect::StartRemote { local } => {
                run_remote(
                    remote_launcher.as_ref(),
                    crate::action::RemoteRequest::Invite { local },
                    completion_tx,
                );
                // A phone claims the invite seconds later and then waits for a
                // person. Nothing pushes that fact here, so ask — a user who
                // scanned a code should not have to guess what to press next.
                if let Some(launcher) = remote_launcher.as_ref() {
                    let launcher = Arc::clone(launcher);
                    let tx = completion_tx.clone();
                    tokio::spawn(async move {
                        for _ in 0..600 {
                            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                            let outcome = launcher(crate::action::RemoteRequest::Pending).await;
                            let waiting =
                                matches!(&outcome, crate::action::RemoteOutcome::Waiting(Some(_)));
                            if tx.send(Action::Remote(outcome)).is_err() || waiting {
                                return;
                            }
                        }
                    });
                }
            }
            Effect::AnswerPairing { accept } => {
                let request = if accept {
                    crate::action::RemoteRequest::Accept
                } else {
                    crate::action::RemoteRequest::Reject
                };
                run_remote(remote_launcher.as_ref(), request, completion_tx);
            }
            Effect::OpenWebUrl(url) => {
                let tx = completion_tx.clone();
                match url_opener {
                    Some(opener) => {
                        let opener = Arc::clone(opener);
                        tokio::spawn(async move {
                            let result = opener(url.clone()).await;
                            let _ = tx.send(Action::UrlOpened { url, result });
                        });
                    }
                    None => {
                        let _ = tx.send(Action::UrlOpened {
                            url,
                            result: Err("当前主机没有可用的 URL 打开器".to_string()),
                        });
                    }
                }
            }
            Effect::OpenExternalEditor { text } => {
                // Block the event loop while $EDITOR runs: the user is away from
                // the TUI, and painting over an active editor would corrupt both.
                let result = run_external_editor(alt, stdout, &text);
                let _ = completion_tx.send(Action::EditorFinished(result));
            }
            Effect::Quit => state.running = false,
        }
    }
}

/// Hand the terminal to `$EDITOR` and take it back.
///
/// Every mode the TUI turned on is undone first and restored after — leaving
/// mouse capture or bracketed paste on would feed escape sequences straight
/// into the editor. The alternate screen is dropped entirely so the next paint
/// rebuilds it, rather than restoring a frame drawn before the edit.
fn run_external_editor(
    alt: &mut Option<Terminal<CrosstermBackend<Stdout>>>,
    stdout: &mut Stdout,
    text: &str,
) -> Result<String, String> {
    *alt = None;
    crate::external_editor::suspend_terminal(stdout).map_err(|e| format!("无法让出终端：{e}"))?;
    let result = crate::external_editor::edit_text(text);
    // Restore even when the edit failed: the alternative is a terminal left in
    // cooked mode with the UI still running.
    if let Err(e) = crate::external_editor::resume_terminal(stdout) {
        return Err(format!("终端未能恢复：{e}"));
    }
    result
}

fn collect_project_files(repository: &str) -> Vec<String> {
    let root = std::path::Path::new(repository);
    if !root.is_dir() {
        return Vec::new();
    }
    if let Some(stdout) = leveler_core::git_stdout(
        root,
        &["ls-files", "--cached", "--others", "--exclude-standard"],
    ) {
        let mut files: Vec<String> = stdout
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
    title: &mut crate::terminal_title::TerminalTitleProjection,
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
    // Terminal tab title: a best-effort projection of the same structured
    // state this frame just rendered. Deduplicated inside; a terminal that
    // ignores OSC titles degrades silently.
    title.maybe_apply(state, stdout);
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

/// Ask the host side to do one remote thing and fold the answer back.
///
/// Absent launcher means this TUI is attached to someone else's daemon, where
/// "make *this* machine reachable" is not a question this process can answer.
fn run_remote(
    launcher: Option<&crate::action::RemoteLauncher>,
    request: crate::action::RemoteRequest,
    completion_tx: &tokio::sync::mpsc::UnboundedSender<Action>,
) {
    match launcher {
        Some(launcher) => {
            let launcher = Arc::clone(launcher);
            let tx = completion_tx.clone();
            tokio::spawn(async move {
                let _ = tx.send(Action::Remote(launcher(request).await));
            });
        }
        None => {
            let _ = completion_tx.send(Action::Remote(crate::action::RemoteOutcome::Failed(
                "当前 TUI 连接的是远程 daemon，不能把这台机器开放给手机".to_string(),
            )));
        }
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
                untrusted_config: Vec::new(),
                reasoning_effort: None,
            },
        )
    }

    /// A runtime whose delivery answers are scripted, recording the id every
    /// attempt carried.
    struct ScriptedRuntime {
        answers: std::sync::Mutex<VecDeque<Result<(), ClientError>>>,
        delivered: std::sync::Mutex<Vec<CommandId>>,
        events: tokio::sync::broadcast::Sender<RuntimeEvent>,
    }

    impl ScriptedRuntime {
        fn new(answers: Vec<Result<(), ClientError>>) -> Arc<Self> {
            Arc::new(Self {
                answers: std::sync::Mutex::new(answers.into()),
                delivered: std::sync::Mutex::new(Vec::new()),
                events: tokio::sync::broadcast::channel(1).0,
            })
        }
    }

    #[async_trait::async_trait]
    impl InteractiveRuntimeClient for ScriptedRuntime {
        async fn send(&self, _command: ClientCommand) -> Result<(), ClientError> {
            unreachable!("turn inputs are delivered in envelopes")
        }

        async fn deliver(&self, envelope: CommandEnvelope) -> Result<(), ClientError> {
            self.delivered.lock().unwrap().push(envelope.command_id);
            self.answers.lock().unwrap().pop_front().expect("scripted")
        }

        fn subscribe(&self) -> tokio::sync::broadcast::Receiver<RuntimeEvent> {
            self.events.subscribe()
        }

        async fn snapshot(
            &self,
            _session_id: &SessionId,
        ) -> Result<leveler_client_protocol::UiSessionSnapshot, ClientError> {
            Err(ClientError::OutcomeUnknown("away".into()))
        }
    }

    fn submission_envelope() -> CommandEnvelope {
        CommandEnvelope {
            command_id: CommandId::new("cmd-logical"),
            session_id: SessionId::new("s1"),
            expected_version: None,
            issued_at: "2026-09-15T00:00:00Z".into(),
            command: ClientCommand::SubmitMessage {
                session_id: SessionId::new("s1"),
                content: "实现登录".into(),
                attachments: Vec::new(),
            },
        }
    }

    /// Stable identity: however many attempts it takes, each carries the id
    /// the logical command was born with, and only an answer ends the retries.
    #[tokio::test]
    async fn an_unanswered_submission_is_redelivered_under_its_original_command_id() {
        let runtime = ScriptedRuntime::new(vec![
            Err(ClientError::OutcomeUnknown("connection reset".into())),
            Err(ClientError::OutcomeUnknown("unsettled dispatch".into())),
            Ok(()),
        ]);
        let client: Arc<dyn InteractiveRuntimeClient> = runtime.clone();

        let settled = settle_submission(client, submission_envelope(), None).await;

        assert!(
            matches!(
                &settled,
                EffectCompletion::SubmissionDelivered { command_id, .. }
                    if command_id.as_str() == "cmd-logical"
            ),
            "{settled:?}"
        );
        assert_eq!(
            runtime.delivered.lock().unwrap().as_slice(),
            &[
                CommandId::new("cmd-logical"),
                CommandId::new("cmd-logical"),
                CommandId::new("cmd-logical"),
            ]
        );
    }

    /// An attempt that outlived the first wait still answers. Its answer is
    /// the runtime's, so it must be used — not overwritten by a second attempt
    /// that could only find the first one's receipt mid-dispatch.
    #[tokio::test]
    async fn the_answer_of_an_attempt_still_running_is_not_thrown_away() {
        let runtime = ScriptedRuntime::new(Vec::new());
        let client: Arc<dyn InteractiveRuntimeClient> = runtime.clone();
        let in_flight = tokio::spawn(async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            Err(ClientError::Runtime(
                "session already has an active turn".into(),
            ))
        });

        let settled = tokio::time::timeout(
            Duration::from_secs(5),
            settle_submission(client, submission_envelope(), Some(in_flight)),
        )
        .await
        .expect("the running attempt's answer settles it");

        assert!(
            matches!(
                &settled,
                EffectCompletion::SubmissionRejected { message, .. }
                    if message == "session already has an active turn"
            ),
            "{settled:?}"
        );
        assert!(runtime.delivered.lock().unwrap().is_empty());
    }

    /// The runtime proved the outcome unrecoverable: that is an answer, so
    /// redelivery stops for good — no further attempt carries the old id.
    #[tokio::test]
    async fn an_unresolvable_answer_ends_redelivery_for_good() {
        let runtime = ScriptedRuntime::new(vec![
            Err(ClientError::OutcomeUnknown("daemon restarting".into())),
            Err(ClientError::Unresolvable("boot ended".into())),
        ]);
        let client: Arc<dyn InteractiveRuntimeClient> = runtime.clone();

        let settled = settle_submission(client, submission_envelope(), None).await;

        assert!(
            matches!(
                &settled,
                EffectCompletion::SubmissionUnresolvable { command_id, .. }
                    if command_id.as_str() == "cmd-logical"
            ),
            "{settled:?}"
        );
        assert_eq!(runtime.delivered.lock().unwrap().len(), 2);
    }

    fn note() -> Notification {
        Notification {
            level: NotificationLevel::Info,
            message: "hi".into(),
        }
    }

    #[test]
    fn missing_web_launcher_does_not_call_a_local_daemon_remote() {
        assert!(
            !WEB_LAUNCHER_UNAVAILABLE.contains("远程"),
            "{WEB_LAUNCHER_UNAVAILABLE}"
        );
        assert!(
            !WEB_LAUNCHER_UNAVAILABLE.contains("remote"),
            "{WEB_LAUNCHER_UNAVAILABLE}"
        );
        assert!(
            !WEB_LAUNCHER_UNAVAILABLE.contains("leveler web --connect"),
            "{WEB_LAUNCHER_UNAVAILABLE}"
        );
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
