//! The event loop: bridges terminal input and runtime events into the reducer,
//! performs the reducer's effects, and redraws.
//!
//! Rendering uses the **alternate screen** for the full workbench (header,
//! conversation viewport, composer, overlays). This is intentional: viewport
//! scroll, drag-select, and fixed chrome are not compatible with native
//! scrollback.

use std::collections::VecDeque;
use std::io::{self, Stdout, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::cursor;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, size};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::{broadcast::error::RecvError, mpsc};

use leveler_client_protocol::{
    ClientCommand, ClientError, CommandEnvelope, CommandId, InteractiveRuntimeClient,
    NotificationLevel, ProtocolEnvelope, RuntimeEvent, SessionId, UiSessionSnapshot,
};

use crate::action::{Action, CleanHost, Effect, EffectCompletion, UrlOpener};
use crate::input_guard::TerminalInput;
use crate::reducer::reduce;
use crate::render::render;
use crate::screen::Screen;
use crate::state::{AppState, Boot, Notification, PendingInteraction};
use crate::terminal::TerminalGuard;
use crate::theme::Theme;

enum DeliveryJob {
    Quit {
        session_id: SessionId,
        timeout: Duration,
        terminal_lost: bool,
    },
    Command {
        session_id: SessionId,
        command: ClientCommand,
        expected_version: Option<i64>,
        then: Option<ClientCommand>,
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

fn observed_command_version(
    command: &ClientCommand,
    snapshot_version: Option<i64>,
) -> Result<Option<i64>, String> {
    if command
        .requires_snapshot_version()
        .map_err(|error| error.to_string())?
    {
        snapshot_version.map(Some).ok_or_else(|| {
            "snapshot version required; refresh session before changing settings".into()
        })
    } else {
        Ok(None)
    }
}

const DELIVERY_TIMEOUT: Duration = Duration::from_secs(10);
const QUIT_CLEANUP_TIMEOUT: Duration = Duration::from_secs(30);
/// A terminal-loss exit cannot ask the user and must not hold a headless
/// process open: stop what it can within this bound, then report honestly.
const QUIT_SIGNAL_TIMEOUT: Duration = Duration::from_secs(5);
/// Pause between re-deliveries of a submission that got no answer. Grows to
/// the cap so a runtime that stays away costs a connection attempt every few
/// seconds, not a busy loop.
const REDELIVERY_BACKOFF_MIN: Duration = Duration::from_millis(500);
const REDELIVERY_BACKOFF_MAX: Duration = Duration::from_secs(5);

type DeliveryAttempt = tokio::task::JoinHandle<Result<(), ClientError>>;

/// Default status-line notification TTL (warnings). Info is shorter; errors stick.
const NOTIFICATION_TTL_WARNING: Duration = Duration::from_secs(8);
const NOTIFICATION_TTL_INFO: Duration = Duration::from_secs(4);
/// Animation / clock cadence for the busy spinner and footer wall clock.
///
/// Drives the busy spinner AND the animated header progress line, so it must run
/// subsecond to look smooth. It only forces a repaint while `is_busy()` (line
/// ~390), so idle sessions stay quiet; the extra frames during a task are the
/// cost of the moving top line (a deliberate visual, unlike the old silent
/// waits). The elapsed clock is `Instant`-based, independent of this cadence.
const BUSY_TICK: Duration = Duration::from_millis(150);
/// Minimum interval between two frames driven only by background-task output.
/// A dev server can emit hundreds of lines per second, and repainting a full
/// frame per chunk spends the whole budget on terminal writes an idle reader
/// never sees. The busy tick still guarantees progress, and any other runtime
/// event repaints immediately.
const BACKGROUND_PAINT_INTERVAL: Duration = Duration::from_millis(50);
/// Cadence for Conversation edge auto-scroll while drag-selecting text.
const SELECTION_TICK: Duration = Duration::from_millis(50);
/// Coalescing window for PTYs that deliver pasted text as plain key events.
const INPUT_BURST_WINDOW: Duration = Duration::from_millis(2);
/// How many history entries persist across restarts.
const HISTORY_CAP: usize = 100;

/// `/clean` needs a host-provided scanner; without it the page cannot run.
/// Only the standalone TUI tests hit this — the CLI always injects one.
const CLEAN_UNAVAILABLE: &str = "storage cleanup host unavailable";

/// Errors running the terminal UI.
#[derive(Debug, thiserror::Error)]
pub enum TuiError {
    #[error("terminal io error: {0}")]
    Io(#[from] std::io::Error),
    /// Opening the session failed before any authoritative state was adopted.
    #[error("could not read the session from the runtime")]
    InitialSessionUnavailable,
}

/// Why the TUI event loop ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuiExit {
    /// The user quit. The process should exit normally.
    Quit,
    /// `/update` installed a new binary. The caller replaces the process once
    /// the terminal is restored.
    Restart,
}

/// Run the interactive terminal UI against a runtime client until the user
/// quits. Restores the terminal on any exit path.
pub async fn run(
    client: Arc<dyn InteractiveRuntimeClient>,
    url_opener: Option<UrlOpener>,
    clean_host: Option<CleanHost>,
    boot: Boot,
    exit_scope: crate::exit::ClientExitScope,
) -> Result<TuiExit, TuiError> {
    let (mut guard, mut stdout) = TerminalGuard::enter()?;
    execute!(stdout, Clear(ClearType::Purge), cursor::MoveTo(0, 0))?;

    let theme_id = crate::theme_config::load_theme_id().unwrap_or(crate::theme::ThemeId::DEFAULT);
    let mut state = AppState::new(Theme::resolve(theme_id, Theme::env_no_color()), boot);
    state.runtime_outlives_client = exit_scope == crate::exit::ClientExitScope::DetachesFromRuntime;
    let mut startup_input = crate::input_guard::StartupInput::default();
    if !state.theme.monochrome {
        let (image, startup) = crate::splash::NativeMascot::detect();
        startup_input = startup;
        match image {
            Ok(image) => state.welcome_image = image,
            Err(error) => {
                tracing::warn!(%error, "welcome PNG could not be decoded; hiding the image")
            }
        }
        if let Some((r, g, b)) = crate::theme::sample_rgb(state.theme.surface.canvas)
            && let Some(image) = state.welcome_image.as_mut()
        {
            image.set_background([r, g, b, 255]);
        }
    }
    let (cols, rows) = size().unwrap_or((80, 24));
    state.size = (cols, rows);
    state.clock_label = crate::status_line::fmt_clock(chrono::Local::now().time());

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
    // Subscribe first so events that arrive during the snapshot can still be
    // applied. A failed read cannot establish an empty session or crash window.
    let snapshot = client.snapshot(&state.session_id).await.map_err(|error| {
        tracing::warn!(%error, "could not read the initial session snapshot");
        TuiError::InitialSessionUnavailable
    })?;
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
                DeliveryJob::Quit {
                    session_id,
                    timeout,
                    terminal_lost,
                } => {
                    let result =
                        stop_session_before_quit(&delivery_client, &session_id, timeout).await;
                    if terminal_lost {
                        EffectCompletion::TerminalLostCleanupFinished(result)
                    } else {
                        EffectCompletion::QuitFinished(result)
                    }
                }
                DeliveryJob::Command {
                    session_id,
                    command,
                    expected_version,
                    then,
                } => {
                    let completion = deliver_command(
                        &delivery_client,
                        session_id.clone(),
                        command,
                        expected_version,
                    )
                    .await;
                    if matches!(completion, EffectCompletion::CommandDelivered)
                        && let Some(then) = then
                    {
                        deliver_command(&delivery_client, session_id, then, None).await
                    } else {
                        completion
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
    // Opening the session can ask the runtime for more (its history), so the
    // effects are carried into the first loop pass instead of dropped.
    let mut carried_effects = reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::SessionOpened { session: snapshot }),
    );

    let mut term_events = TerminalInput::with_startup(startup_input);
    let mut tick = tokio::time::interval(BUSY_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Origin of the UI's motion clock. Live animations sweep on elapsed time
    // against it, so a burst of stream deltas (many loop iterations, many
    // repaints per second) cannot make an animation run faster than its period.
    let motion_origin = Instant::now();
    let mut selection_tick = tokio::time::interval(SELECTION_TICK);
    selection_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut alt: Option<Terminal<CrosstermBackend<Stdout>>> = None;
    let mut tab_title = crate::terminal_title::TerminalTitleProjection::default();
    let mut pending_runtime_paint = false;
    // A background task's output arrives as many small chunks. Painting a full
    // frame per chunk is what makes a chatty process stall an idle TUI, so a
    // background-only batch is throttled to this minimum interval (the busy
    // tick still guarantees a repaint). Any other runtime event paints at once.
    let mut pending_immediate_paint = false;
    let mut pending_background_paint = false;
    let mut last_paint = Instant::now();
    let mut pending_terminal_actions: VecDeque<Action> = VecDeque::new();
    // The notification currently on screen and when it appeared, for expiry.
    let mut note_shown: Option<(Notification, Instant)> = None;

    // Profiling bookkeeping. Inert unless LEVELER_TUI_PROFILE is set: the
    // `Option<Instant>` slots stay `None`, so the normal path never reads a
    // clock and the loop behaves exactly as before.
    let mut pending_since: Option<Instant> = None;
    let mut last_frame_start: Option<Instant> = None;
    let mut last_delta_at: Option<Instant> = None;
    let mut deltas_since_frame: u64 = 0;
    let mut runtime_events_since_frame: u64 = 0;
    let mut requests_since_frame: u64 = 0;
    let mut recorder = crate::record::Recorder::from_env();

    // Termination signals are only useful when a client owns the loop: a
    // closing terminal or a plain `kill` must run the same stop path instead of
    // leaving goal work running (or the terminal raw) behind a default kill.
    // A second signal stops waiting for cleanup.
    let (signal_tx, mut signal_rx) = mpsc::unbounded_channel::<()>();
    spawn_termination_forwarder(signal_tx.clone());
    // Keep a sender alive: with every sender gone, `recv()` would answer `None`
    // immediately and the select would spin.
    let _signal_tx = signal_tx;

    paint(&mut alt, &mut stdout, &mut state, &mut tab_title)?;

    while state.running {
        let mut effects: Vec<Effect> = std::mem::take(&mut carried_effects);
        let mut paint_now = false;
        let mut ticked = false;

        if !effects.is_empty() {
            paint_now = true;
        } else if !state.terminal_lost
            && let Some(action) = pending_terminal_actions.pop_front()
        {
            if let Action::Resize(cols, rows) = &action {
                state.size = (*cols, *rows);
            }
            effects = reduce(&mut state, action);
            paint_now = true;
        } else {
            tokio::select! {
                maybe = term_events.next(), if !state.terminal_lost => {
                match maybe {
                    Some(Ok(action)) => {
                        if let Action::Resize(cols, rows) = &action {
                            state.size = (*cols, *rows);
                        }
                        let action = maybe_coalesce_text_input(
                            action,
                            &state,
                            &mut term_events,
                            &mut pending_terminal_actions,
                        )
                        .await?;
                        effects = reduce(&mut state, action);
                        paint_now = true;
                        requests_since_frame += 1;
                    }
                    // A read error is not recoverable here; the terminal is
                    // gone just as it is on EOF. Run the bounded stop path
                    // instead of returning while runtime-owned work continues.
                    other => {
                        if let Some(Err(error)) = other {
                            tracing::warn!(%error, "terminal input stream failed");
                        }
                        effects = reduce(&mut state, Action::TerminalLost);
                        paint_now = false;
                    }
                }
                },
                Some(()) = signal_rx.recv() => {
                    if state.terminal_lost {
                        // The user or the OS asked a second time. Stop waiting
                        // for cleanup; `guard.restore()` still runs.
                        state.running = false;
                    } else {
                        effects = reduce(&mut state, Action::TerminalLost);
                    }
                    paint_now = false;
                },
                received = events.recv(), if events_open => match received {
                    Ok(event) => {
                        if let RuntimeEvent::SessionOpened { session } = &event
                            && session.id != state.session_id
                        {
                            events = client.subscribe_session(&session.id);
                            events_open = true;
                        }
                        // Read the trigger from the event before it is moved
                        // into the reducer; a background chunk is coalescable.
                        let is_background_output =
                            matches!(&event, RuntimeEvent::BackgroundTaskOutput { .. });
                        let delta_bytes = match &event {
                            RuntimeEvent::AssistantTextDelta { delta, .. } => Some(delta.len()),
                            _ => None,
                        };
                        if let Some(recorder) = recorder.as_mut() {
                            recorder.record(&event);
                        }
                        let apply_start = crate::profile::start();
                        effects = reduce(&mut state, Action::Runtime(event));
                        crate::profile::stop(apply_start, "tui.state_apply_ms");
                        crate::profile::add("tui.runtime_event_count", 1);
                        if let Some(bytes) = delta_bytes {
                            crate::profile::add("stream.delta_count", 1);
                            crate::profile::add("stream.delta_bytes", bytes as u64);
                            if crate::profile::enabled() {
                                if let Some(prev) = last_delta_at {
                                    crate::profile::record_elapsed(
                                        prev,
                                        "stream.delta_interval_ms",
                                    );
                                }
                                last_delta_at = Some(Instant::now());
                            }
                            deltas_since_frame += 1;
                        }
                        runtime_events_since_frame += 1;
                        requests_since_frame += 1;
                        if pending_since.is_none() {
                            pending_since = crate::profile::start();
                        }
                        if is_background_output {
                            pending_background_paint = true;
                        } else {
                            pending_immediate_paint = true;
                        }
                        pending_runtime_paint = true;
                    }
                    Err(RecvError::Lagged(_)) => {
                        match client.snapshot(&state.session_id).await {
                            Ok(snapshot) => {
                                effects = reduce(
                                    &mut state,
                                    Action::Runtime(RuntimeEvent::SessionOpened {
                                        session: snapshot,
                                    }),
                                );
                            }
                            Err(error) => {
                                // Resync failed: surface it instead of silently
                                // dropping to a stale view. A later event or a
                                // manual refresh retries. The raw failure is
                                // diagnostic; the notice states only what the
                                // user can rely on.
                                tracing::warn!(%error, "event stream lag resync failed");
                                let message = state.t().stream_resync_failed.to_string();
                                state.notification = Some(Notification {
                                    level: NotificationLevel::Error,
                                    message,
                                });
                            }
                        }
                        pending_runtime_paint = true;
                        pending_immediate_paint = true;
                    }
                    Err(RecvError::Closed) => {
                        events_open = false;
                        note_disconnected(&mut state);
                        pending_runtime_paint = true;
                    }
                },
                Some(action) = completion_rx.recv() => {
                    effects = reduce(&mut state, action);
                    paint_now = true;
                    requests_since_frame += 1;
                }
                _ = tick.tick() => {
                    effects = reduce(&mut state, Action::IdleTick(Instant::now()));
                    ticked = true;
                }
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
            &client,
            &completion_tx,
            &delivery_tx,
            &url_opener,
            &clean_host,
            &mut alt,
            &mut stdout,
        );

        // Busy spinner + elapsed clock.
        state.tick = state.tick.wrapping_add(1);
        state.motion_clock = motion_origin.elapsed();
        if state.is_executing() {
            let start = *state.turn_started_at.get_or_insert_with(Instant::now);
            state.elapsed_secs = start.elapsed().as_secs();
        } else {
            state.turn_started_at = None;
            state.elapsed_secs = 0;
        }

        // A completed goal fades itself out. The tick already wakes this loop,
        // so the boundary is checked here instead of on a sleep or a second
        // timer; a failure never expires and needs no work.
        if state
            .active_goal
            .as_ref()
            .is_some_and(|goal| goal.is_expired(Instant::now()))
        {
            state.active_goal = None;
            paint_now = true;
        }

        // Wall clock in the footer — repaint when the minute rolls over.
        let clock = crate::status_line::fmt_clock(chrono::Local::now().time());
        if clock != state.clock_label {
            state.clock_label = clock;
            paint_now = true;
        }

        // Fade a stale notification so old notices don't linger forever.
        if expire_notification(&mut state, &mut note_shown, Instant::now()) {
            paint_now = true;
        }
        if state.conv.reply_copy.expire_feedback(Instant::now()) {
            paint_now = true;
        }

        // End / Ctrl+End returns to the conversation's live edge.
        if state.jump_to_bottom {
            state.jump_to_bottom = false;
            state.active_screen = Screen::Conversation;
            paint_now = true;
        }

        // Conversation viewport scroll is synced inside the paint branch below,
        // not here: syncing rebuilds the projected lines, and a streaming turn
        // bumps the transcript on every delta. Running it per loop iteration
        // re-parsed the whole live message and re-wrapped the whole transcript
        // once per delta instead of once per painted frame. Every reason sync
        // could request a repaint (content growth, resize, scroll intent)
        // already sets one of the paint conditions evaluated below.

        // Repaint on input, on runtime updates, and — while busy — on the tick
        // (spinner/elapsed). Idle ticks only repaint when the wall clock minute
        // changes (handled above via paint_now).
        if !state.terminal_lost
            && (paint_now
                || (ticked && (state.is_busy() || state.running_user_shell().is_some()))
                || (ticked && pending_runtime_paint)
                || (!state.is_busy() && pending_immediate_paint)
                || (!state.is_busy()
                    && pending_background_paint
                    && last_paint.elapsed() >= BACKGROUND_PAINT_INTERVAL))
        {
            if state.active_screen == Screen::Conversation {
                crate::conversation::sync_scroll(&mut state);
            }
            crate::profile::add("tui.render_count", 1);
            crate::profile::add("tui.render_requested_count", requests_since_frame);
            crate::profile::add(
                "tui.render_coalesced_count",
                requests_since_frame.saturating_sub(1),
            );
            if ticked && requests_since_frame == 0 && deltas_since_frame == 0 {
                crate::profile::add("tui.spinner_only_render_count", 1);
            }
            crate::profile::record_us("tui.deltas_per_frame", deltas_since_frame);
            crate::profile::record_us("tui.runtime_events_per_frame", runtime_events_since_frame);
            if let Some(t0) = pending_since {
                crate::profile::record_elapsed(t0, "tui.render_wait_ms");
            }
            if let Some(prev) = last_frame_start {
                crate::profile::record_elapsed(prev, "tui.frame_interval_ms");
            }
            let paint_start = crate::profile::start();
            paint(&mut alt, &mut stdout, &mut state, &mut tab_title)?;
            crate::profile::stop(paint_start, "tui.render_ms");
            if let Some(t0) = pending_since {
                crate::profile::record_elapsed(t0, "tui.event_to_render_end_ms");
            }
            pending_since = None;
            last_frame_start = crate::profile::start();
            last_paint = Instant::now();
            deltas_since_frame = 0;
            runtime_events_since_frame = 0;
            requests_since_frame = 0;
            pending_runtime_paint = false;
            pending_immediate_paint = false;
            pending_background_paint = false;
        }
    }

    // Leave the alternate screen before restoring terminal state. Kitty image
    // data lives outside the character grid, so it is deleted by id while the
    // alternate screen that owns the splash is still active.
    if alt.is_some() {
        if let Some(image) = &mut state.welcome_image {
            image.retire();
        }
        crate::image_preview::retire(&mut state);
        let _ = flush_welcome_cleanup(&mut stdout, &mut state);
        let _ = execute!(stdout, LeaveAlternateScreen);
    }

    crate::pending_steer::restore_draft(&mut state);
    // Persist (or clear) the composer draft for next launch (spec §24).
    if let Some(path) = state.draft_path() {
        let draft = state.composer.canonical_text();
        // A draft the detector calls a high-confidence credential, or that
        // contains a value already registered for this session, is not
        // persisted. Losing an unsubmitted draft is recoverable; writing a
        // plaintext credential to `~/.leveler` is not.
        let unsafe_draft = leveler_core::detect_secrets(&draft)
            .iter()
            .any(|finding| finding.level == leveler_core::SecretLevel::HighConfidence)
            || leveler_core::scrub_registered_secrets(state.session_id.as_str(), &draft) != draft;
        if draft.trim().is_empty() || unsafe_draft {
            let _ = std::fs::remove_file(path);
        } else if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
            write_private_state_file(path, &draft);
        }
    }
    // Persist the input history (last HISTORY_CAP entries) for ↑ recall. The
    // same rule applies: a high-confidence value is masked and a registered
    // session secret is scrubbed, so recall cannot replay a credential from
    // disk.
    if let Some(path) = state.history_path() {
        let history = state.composer.history();
        let tail: Vec<String> = history[history.len().saturating_sub(HISTORY_CAP)..]
            .iter()
            .map(|entry| {
                let masked = mask_high_confidence(entry);
                leveler_core::scrub_registered_secrets(state.session_id.as_str(), &masked)
            })
            .collect();
        if let Ok(json) = serde_json::to_string(&tail) {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            write_private_state_file(path, &json);
        }
    }

    let session_id = state.session_id.clone();
    // A completed `/update` installed a new binary; the CLI replaces this
    // process once the terminal is back. Everything else is a normal quit.
    let exit = if state.restart_requested {
        TuiExit::Restart
    } else {
        TuiExit::Quit
    };
    guard.restore();
    crate::profile::emit_report();
    // Facts the alternate screen can no longer show. Both are stated after it
    // is restored, and neither is claimed as a success that was not proved.
    // Writes are fallible on a terminal that just disappeared; a diagnostic
    // must not panic on the way out.
    if let Some(message) = state.exit.stop_unconfirmed.take() {
        let _ = writeln!(
            io::stderr(),
            "{}",
            state.t().exit_stop_unconfirmed.replace("{}", &message)
        );
    }
    if state.exit.continued_count > 0 {
        let _ = writeln!(
            io::stdout(),
            "{}",
            state
                .t()
                .exit_continue_hint
                .replace("{}", &state.exit.continued_count.to_string())
        );
    }
    if exit == TuiExit::Quit {
        // After raw mode is off: print full resume command so the user can reconnect.
        // A terminal that just disappeared is a failed write, not a panic.
        let _ = writeln!(io::stdout(), "{}", session_exit_hint(session_id.as_str()));
    }
    Ok(exit)
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

/// Stop through the existing session-scoped authority, then observe actual
/// settlement. A client disconnect never calls this explicit-exit path.
async fn stop_session_before_quit(
    client: &Arc<dyn InteractiveRuntimeClient>,
    session_id: &SessionId,
    timeout: Duration,
) -> Result<(), String> {
    // Drop the observation at the deadline so it can never issue new stops
    // after the UI has reported failure and accepted another user action.
    tokio::time::timeout(timeout, observe_session_cleanup(client, session_id))
        .await
        .map_err(|_| "timed out waiting for the runtime to confirm cleanup".to_string())?
}

async fn issue_quit_command(
    client: &Arc<dyn InteractiveRuntimeClient>,
    session_id: &SessionId,
    command: ClientCommand,
) -> Result<(), String> {
    let client = Arc::clone(client);
    let session_id = session_id.clone();
    // Only an already-issued authority operation may outlive observation.
    // Never drop an in-process stop halfway through process reclamation.
    tokio::spawn(async move { client.issue(session_id, command).await })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

async fn observe_session_cleanup(
    client: &Arc<dyn InteractiveRuntimeClient>,
    session_id: &SessionId,
) -> Result<(), String> {
    // Subscribe before inspecting or cancelling so a fast terminal is not lost.
    let mut events = client.subscribe_session(session_id);
    let mut turn_cancelled = false;
    let mut requested = std::collections::HashSet::new();
    let mut requested_shells = std::collections::HashSet::new();
    loop {
        let snapshot = client
            .snapshot(session_id)
            .await
            .map_err(|e| e.to_string())?;
        if snapshot.id != *session_id {
            return Err("runtime returned a snapshot for a different session".into());
        }
        let running = crate::exit::has_live_turn(&snapshot);
        // A foreground user shell is the user's own process; exiting stops it
        // unless the user explicitly chose to leave work running. A shell
        // already handed to the background is registry-owned and excluded here
        // by the same predicate the registry uses.
        let active_shell = snapshot
            .user_shells
            .iter()
            .find(|shell| shell.status == "running" && shell.task_id.is_none())
            .map(|shell| shell.id.clone());
        let has_active_shell = active_shell.is_some();
        // A client exit owns only `goal` work. `session`, `runtime` and
        // `persistent` tasks outlive it by contract: a user shell handed to the
        // background stays the Execution Host's, and the next client re-attaches
        // to it. An absent lifetime is an older runtime; keep today's stop.
        let owned: Vec<_> = snapshot
            .active_background_tasks
            .into_iter()
            .filter(|task| crate::exit::affected_background_lifetime(task.lifetime.as_deref()))
            .collect();
        let has_owned = !owned.is_empty();
        let mut issued = false;
        if let Some(execution_id) = active_shell
            && requested_shells.insert(execution_id.clone())
        {
            issue_quit_command(
                client,
                session_id,
                ClientCommand::CancelUserShell {
                    session_id: session_id.clone(),
                    execution_id,
                },
            )
            .await
            .map_err(|e| e.to_string())?;
            issued = true;
        }
        if running && !turn_cancelled {
            // A cancelling turn may still launch a final background process;
            // the loop re-enumerates after it settles. Known tasks are stopped
            // in the same pass so a hung turn cannot delay their exit.
            issue_quit_command(
                client,
                session_id,
                ClientCommand::CancelCurrentTurn {
                    session_id: session_id.clone(),
                },
            )
            .await
            .map_err(|e| e.to_string())?;
            turn_cancelled = true;
            issued = true;
        }
        for task in owned {
            if requested.insert(task.task_id.clone()) {
                issue_quit_command(
                    client,
                    session_id,
                    ClientCommand::CancelBackgroundTask {
                        session_id: session_id.clone(),
                        task_id: task.task_id,
                    },
                )
                .await
                .map_err(|e| e.to_string())?;
                issued = true;
            }
        }
        if !running && !has_active_shell && !has_owned {
            return Ok(());
        }
        if issued {
            // An ACK only confirms dispatch. Re-read the registry before
            // deciding whether a terminal event is still owed, and catch a
            // task the stopping turn launched in the meantime.
            continue;
        }
        match events.recv().await {
            Ok(_) | Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => {
                return Err("runtime disconnected before cleanup was confirmed".into());
            }
        }
    }
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
    /// Rejected, in the runtime's own words. `recovery` marks a refusal that
    /// must not be presented as a generic failure: an earlier tool call's
    /// outcome is unconfirmed.
    Rejected {
        message: String,
        recovery: bool,
    },
    /// Admitted, with an outcome the runtime proves it cannot recover.
    Unresolvable,
    /// No answer — nothing may be concluded.
    None,
}

async fn deliver_command(
    delivery_client: &Arc<dyn InteractiveRuntimeClient>,
    session_id: SessionId,
    command: ClientCommand,
    expected_version: Option<i64>,
) -> EffectCompletion {
    let client = Arc::clone(delivery_client);
    let envelope = CommandEnvelope {
        command_id: leveler_client_protocol::CommandId::generate(),
        session_id: session_id.clone(),
        expected_version,
        issued_at: chrono::Utc::now().to_rfc3339(),
        command: command.clone(),
    };
    let mut attempt = tokio::spawn(async move { client.deliver(envelope).await });
    let answer = match tokio::time::timeout(DELIVERY_TIMEOUT, &mut attempt).await {
        Ok(joined) => delivery_answer(joined),
        Err(_) => DeliveryAnswer::None,
    };
    match answer {
        DeliveryAnswer::Delivered if matches!(command, ClientCommand::ManageSteer { .. }) => {
            EffectCompletion::SteerCommandDelivered(command)
        }
        DeliveryAnswer::Delivered => EffectCompletion::CommandDelivered,
        DeliveryAnswer::Rejected { message, .. } => EffectCompletion::CommandRejected {
            command,
            message,
            snapshot: snapshot_within(delivery_client, &session_id).await,
        },
        // A fresh id per issue never meets an unsettled receipt;
        // were it to, it is still no delivery this client saw.
        DeliveryAnswer::Unresolvable | DeliveryAnswer::None => EffectCompletion::CommandUncertain {
            command,
            snapshot: snapshot_within(delivery_client, &session_id).await,
        },
    }
}

fn delivery_answer(
    joined: Result<Result<(), ClientError>, tokio::task::JoinError>,
) -> DeliveryAnswer {
    match joined {
        Ok(Ok(())) => DeliveryAnswer::Delivered,
        Ok(Err(ClientError::OutcomeUnknown(_))) | Err(_) => DeliveryAnswer::None,
        Ok(Err(ClientError::Unresolvable(_))) => DeliveryAnswer::Unresolvable,
        Ok(Err(ClientError::Runtime(message))) => DeliveryAnswer::Rejected {
            message,
            recovery: false,
        },
        Ok(Err(ClientError::RecoveryRequired(message))) => DeliveryAnswer::Rejected {
            message,
            recovery: true,
        },
        Ok(Err(error)) => DeliveryAnswer::Rejected {
            message: error.to_string(),
            recovery: false,
        },
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
            snapshot: snapshot_within(client, &envelope.session_id).await,
        }),
        DeliveryAnswer::Rejected { message, recovery } => {
            Ok(EffectCompletion::SubmissionRejected {
                command_id: envelope.command_id.clone(),
                message,
                recovery,
                snapshot: snapshot_within(client, &envelope.session_id).await,
            })
        }
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
            DeliveryAnswer::Rejected { message, recovery } => {
                EffectCompletion::SubmissionRejected {
                    command_id,
                    message,
                    recovery,
                    snapshot,
                }
            }
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

/// Read one fresh snapshot off the paint loop and project the exit impact.
///
/// Both the initial check and the pre-continue re-check use the same read, so
/// they cannot disagree about the affected set. The answer carries the
/// request's generation; the reducer drops one whose dialog is gone.
fn spawn_exit_impact_read(
    client: Arc<dyn InteractiveRuntimeClient>,
    state: &AppState,
    completion_tx: &mpsc::UnboundedSender<Action>,
    generation: u64,
    recheck: bool,
) {
    let session_id = state.session_id.clone();
    let scope = if state.runtime_outlives_client {
        crate::exit::ClientExitScope::DetachesFromRuntime
    } else {
        crate::exit::ClientExitScope::EndsWithClient
    };
    let turn_cancelled = state.turn_cancel_requested;
    let tx = completion_tx.clone();
    tokio::spawn(async move {
        let result = tokio::time::timeout(DELIVERY_TIMEOUT, client.snapshot(&session_id))
            .await
            .map(|answer| answer.map_err(|error| error.to_string()))
            .unwrap_or_else(|_| Err("timed out reading the session".into()))
            .map(|snapshot| crate::exit::project(&snapshot, scope, turn_cancelled));
        let action = if recheck {
            Action::ExitRechecked { generation, result }
        } else {
            Action::ExitImpactLoaded { generation, result }
        };
        let _ = tx.send(action);
    });
}

/// Envelope session for one outgoing command.
///
/// The runtime binds an envelope to the command's own `session_id` and rejects
/// a mismatch. Session management from the Sessions view targets stored
/// sessions that need not be the client's current one; a command without a
/// session (a global query) still belongs to the client's current session.
fn delivery_session_id(command: &ClientCommand, current: &SessionId) -> SessionId {
    command
        .session_id()
        .cloned()
        .unwrap_or_else(|| current.clone())
}

/// Mark every runtime-owned read as no longer in flight. Without this a
/// disconnect leaves pages spinning on a latch that only a matching answer
/// could clear, and a form latched on "saving" with no way to Escape.
fn note_disconnected(state: &mut AppState) {
    let message = state.t().connection_lost.to_string();
    state.runtime_connected = false;
    state.sessions_loading = false;
    state.sessions_error = Some(message.clone());
    state.sessions.clear();
    state.all_sessions.clear();
    state.diff = None;
    state.diff_pending = false;
    state.diff_error = Some(message.clone());
    state.stats_error = Some(message.clone());
    state.context.loaded = None;
    state.usage.loaded = None;
    state.trace.loaded = None;
    state.trace.error = Some(message.clone());
    state.context.track.reset();
    state.usage.track.reset();
    state.trace.track.reset();
    state.memory.status = None;
    state.memory.preview = None;
    state.memory.status_query = None;
    state.memory.preview_query = None;
    state.memory.action_query = None;
    state.memory.preview_loading = false;
    if state.active_screen == Screen::Memory {
        state.memory.error = Some(message.clone());
    }
    state.foreign_sessions.loading = false;
    state.foreign_sessions.importing = false;
    state.foreign_sessions.sessions.clear();
    if state.foreign_sessions.error.is_none() {
        state.foreign_sessions.error = Some(message.clone());
    }
    match state.active_screen {
        Screen::Mcp => crate::mcp::observe_command_failure(state, Some(&message)),
        Screen::Schedules => crate::schedules::observe_command_failure(state, Some(&message)),
        Screen::Settings => {
            state.settings.query = None;
            state.settings.query_started = None;
            state.settings.error = Some(message.clone());
        }
        _ => {}
    }
    state.notification = Some(Notification {
        level: NotificationLevel::Error,
        message,
    });
}

/// Carry out the reducer's effects. A failed send means the runtime side is
/// gone — surface that instead of pretending the action happened.
#[allow(clippy::too_many_arguments)]
fn dispatch_effects(
    state: &mut AppState,
    effects: Vec<Effect>,
    client: &Arc<dyn InteractiveRuntimeClient>,
    completion_tx: &mpsc::UnboundedSender<Action>,
    delivery_tx: &mpsc::UnboundedSender<DeliveryJob>,
    url_opener: &Option<UrlOpener>,
    clean_host: &Option<CleanHost>,
    // `$EDITOR` takes the terminal over, so it needs the alternate-screen
    // handle (dropped for the duration) and stdout to restore modes on.
    alt: &mut Option<Terminal<CrosstermBackend<Stdout>>>,
    stdout: &mut Stdout,
) {
    for effect in effects {
        match effect {
            command_effect @ (Effect::Send(_) | Effect::SendThen { .. }) => {
                let (command, then) = match command_effect {
                    Effect::Send(command) => (command, None),
                    Effect::SendThen { command, then } => (command, Some(then)),
                    _ => unreachable!(),
                };
                if !state.runtime_connected {
                    note_disconnected(state);
                    continue;
                }
                let expected_version =
                    match observed_command_version(&command, state.snapshot_version) {
                        Ok(version) => version,
                        Err(message) => {
                            state.notification = Some(Notification {
                                level: NotificationLevel::Error,
                                message,
                            });
                            continue;
                        }
                    };
                // Bind the envelope to the command's own target. The Sessions
                // view manages stored sessions (archive/unarchive/rename/
                // delete/fork) that need not be the client's current session;
                // the runtime rejects an envelope whose session differs from
                // the command target. Session-less global queries keep the
                // client's current session.
                let session_id = delivery_session_id(&command, &state.session_id);
                let _ = delivery_tx.send(DeliveryJob::Command {
                    session_id,
                    command,
                    expected_version,
                    then,
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
                        message: state.t().interaction_held_offline.to_string(),
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
            Effect::ShellComplete { request } => {
                let tx = completion_tx.clone();
                tokio::spawn(async move {
                    let result =
                        tokio::task::spawn_blocking(move || crate::shell_mode::complete(&request))
                            .await;
                    if let Ok(result) = result {
                        let _ = tx.send(Action::ShellCompletionsLoaded(result));
                    }
                });
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
            Effect::StartUpdate => {
                // Detached: the update is network-bound and must not block the
                // event loop. Progress and the terminal outcome come back as
                // `Action`s on the completion channel. The reducer only emits
                // this while idle, so no task is running under the swap.
                let tx = completion_tx.clone();
                tokio::spawn(async move {
                    let outcome = match leveler_update::UpdateService::production() {
                        Ok(service) => service
                            .run(false, |step| {
                                let _ = tx.send(Action::UpdateStep(step));
                            })
                            .await
                            .map_err(|error| error.to_string()),
                        Err(error) => Err(error.to_string()),
                    };
                    let _ = tx.send(Action::UpdateFinished(outcome));
                });
            }
            Effect::InspectExit { generation } => {
                spawn_exit_impact_read(Arc::clone(client), state, completion_tx, generation, false);
            }
            Effect::ContinueExit { generation } => {
                spawn_exit_impact_read(Arc::clone(client), state, completion_tx, generation, true);
            }
            Effect::QuitSignal => {
                if delivery_tx
                    .send(DeliveryJob::Quit {
                        session_id: state.session_id.clone(),
                        timeout: QUIT_SIGNAL_TIMEOUT,
                        terminal_lost: true,
                    })
                    .is_err()
                {
                    let _ = completion_tx.send(Action::EffectCompleted(
                        EffectCompletion::TerminalLostCleanupFinished(Err(
                            "runtime command queue is closed".into(),
                        )),
                    ));
                }
            }
            Effect::Quit => {
                if !state.quit_pending {
                    state.quit_pending = true;
                    let paused = state.t().quit_input_paused.to_string();
                    for input in &mut state.pending_inputs {
                        if input.session_id == state.session_id && input.is_queued() {
                            input.state =
                                crate::pending_inputs::PendingInputState::Failed(paused.clone());
                        }
                    }
                    state.notification = Some(Notification {
                        level: NotificationLevel::Info,
                        message: state.t().quit_stopping.to_string(),
                    });
                    if delivery_tx
                        .send(DeliveryJob::Quit {
                            session_id: state.session_id.clone(),
                            timeout: QUIT_CLEANUP_TIMEOUT,
                            terminal_lost: false,
                        })
                        .is_err()
                    {
                        let _ = completion_tx.send(Action::EffectCompleted(
                            EffectCompletion::QuitFinished(Err(
                                "runtime command queue is closed".into()
                            )),
                        ));
                    }
                }
            }
            Effect::StartCleanScan => {
                let tx = completion_tx.clone();
                match clean_host.clone() {
                    Some(host) => {
                        tokio::spawn(async move {
                            let result = tokio::task::spawn_blocking(move || (host.scan)())
                                .await
                                .map_err(|error| error.to_string());
                            let _ = tx.send(Action::CleanScanned(result));
                        });
                    }
                    None => {
                        let _ = completion_tx
                            .send(Action::CleanScanned(Err(CLEAN_UNAVAILABLE.to_string())));
                    }
                }
            }
            Effect::RunCleanSafe => {
                let tx = completion_tx.clone();
                match clean_host.clone() {
                    Some(host) => {
                        tokio::spawn(async move {
                            let result = tokio::task::spawn_blocking(move || (host.run_safe)())
                                .await
                                .map_err(|error| error.to_string());
                            let _ = tx.send(Action::CleanRan(result));
                        });
                    }
                    None => {
                        let _ = completion_tx
                            .send(Action::CleanRan(Err(CLEAN_UNAVAILABLE.to_string())));
                    }
                }
            }
            Effect::CopyReply {
                session_id,
                target,
                text,
            } => {
                let tx = completion_tx.clone();
                tokio::spawn(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        clipboard_result(&text).map(|(receipt, _)| receipt)
                    })
                    .await
                    .unwrap_or_else(|error| Err(error.to_string()));
                    let _ = tx.send(Action::ReplyCopied {
                        session_id,
                        target,
                        result,
                    });
                });
            }
            Effect::CopyUserShellOutput {
                directory,
                retained,
            } => {
                // File read + clipboard write can both block (network
                // clipboard, tmux). Keep them off the paint loop.
                let tx = completion_tx.clone();
                tokio::spawn(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        let text = match directory {
                            Some(directory) => {
                                crate::shell_render::read_complete_capture(&directory)?
                            }
                            None => crate::shell_render::copy_text(&retained),
                        };
                        if text.is_empty() {
                            return Err("输出为空，未写入剪贴板".to_string());
                        }
                        let count = text.chars().count();
                        let receipt = crate::clipboard::copy(&text)?;
                        Ok((receipt, count))
                    })
                    .await
                    .unwrap_or_else(|error| Err(error.to_string()));
                    let _ = tx.send(Action::UserShellOutputCopied(result));
                });
            }
            Effect::OpenOutputPager { source } => {
                let result = run_output_pager(alt, stdout, source);
                let _ = completion_tx.send(Action::OutputPagerFinished(result));
            }
            Effect::LoadOutputIndex {
                source,
                width,
                generation,
            } => {
                let tx = completion_tx.clone();
                tokio::task::spawn_blocking(move || {
                    let result = crate::output_view::OutputIndex::build(&source, width)
                        .map(std::sync::Arc::new);
                    let _ = tx.send(Action::OutputIndexReady { generation, result });
                });
            }
            Effect::CopyOutputSelection {
                index,
                source,
                start,
                end,
            } => {
                let tx = completion_tx.clone();
                tokio::task::spawn_blocking(move || {
                    let mut selection = crate::selection::TextSelection::default();
                    selection.begin(crate::selection::TextPos {
                        row: start.0,
                        col: start.1,
                    });
                    selection.extend(crate::selection::TextPos {
                        row: end.0,
                        col: end.1,
                    });
                    let text = crate::output_view::selection_rows(&index, &source, &selection)
                        .map(|rows| crate::output_view::selection_text(&rows, &selection))
                        .and_then(|text| clipboard_result(&text));
                    let _ = tx.send(Action::UserShellOutputCopied(text));
                });
            }
            Effect::CopyOutputAll { source } => {
                let tx = completion_tx.clone();
                tokio::task::spawn_blocking(move || {
                    let text = source.copy_all().and_then(|text| clipboard_result(&text));
                    let _ = tx.send(Action::UserShellOutputCopied(text));
                });
            }
            Effect::RunFindTranscript {
                generation,
                corpus,
                query,
            } => {
                let tx = completion_tx.clone();
                tokio::task::spawn_blocking(move || {
                    let (hits, capped) = crate::find::search_corpus(&corpus, &query);
                    let _ = tx.send(Action::FindTranscriptReady {
                        generation,
                        hits,
                        capped,
                    });
                });
            }
            Effect::RunFindArtifacts {
                generation,
                cancel,
                requests,
                query,
            } => {
                // One worker walks every artifact: a keystroke starts ONE
                // scan, and the generation check stops it at the next chunk
                // when the query moves on. Nothing is loaded whole.
                let tx = completion_tx.clone();
                tokio::task::spawn_blocking(move || {
                    for request in requests {
                        if cancel.load() != generation {
                            break;
                        }
                        let outcome = crate::find::artifacts::scan_file(
                            &request.path,
                            &query,
                            cancel.atomic(),
                            generation,
                        );
                        let cancelled =
                            outcome.status == crate::find::artifacts::ArtifactStatus::Cancelled;
                        let _ = tx.send(Action::FindArtifactReady {
                            generation,
                            request,
                            outcome,
                        });
                        if cancelled {
                            break;
                        }
                    }
                });
            }
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

/// Write already-semantic output text to the clipboard, with the same empty
/// guard every copy path uses.
fn clipboard_result(text: &str) -> Result<(crate::clipboard::CopyReceipt, usize), String> {
    if text.is_empty() {
        return Err("输出为空，未写入剪贴板".to_string());
    }
    let count = text.chars().count();
    crate::clipboard::copy(text).map(|receipt| (receipt, count))
}

/// Hand the terminal to `$PAGER` for the full output, then take it back.
///
/// A capture is passed as a path so the pager reads it on demand; only a
/// complete retained result without a capture is written to a temporary file.
fn run_output_pager(
    alt: &mut Option<Terminal<CrosstermBackend<Stdout>>>,
    stdout: &mut Stdout,
    source: crate::action::PagerSource,
) -> Result<(), String> {
    *alt = None;
    crate::external_editor::suspend_terminal(stdout).map_err(|e| format!("无法让出终端：{e}"))?;
    let result = match source {
        crate::action::PagerSource::File(path) => crate::external_editor::view_file(&path),
        crate::action::PagerSource::Text(text) => crate::external_editor::view_text(&text),
    };
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
        if let Some(image) = &mut state.welcome_image {
            image.reset();
        }
        execute!(stdout, EnterAlternateScreen)?;
        let mut t = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        t.clear()?;
        *alt = Some(t);
    }
    // The splash is permanent for this session once real content exists; its
    // terminal-side image data must not outlive the card.
    if !crate::splash::conversation_is_empty(state)
        && let Some(image) = &mut state.welcome_image
    {
        image.retire();
    }
    flush_welcome_cleanup(stdout, state)?;
    if let Some(t) = alt {
        t.draw(|f| render(f, state))?;
    }
    // An encoding failure is discovered during draw and retires the image then.
    flush_welcome_cleanup(stdout, state)?;
    // Terminal tab title: a best-effort projection of the same structured
    // state this frame just rendered. Deduplicated inside; a terminal that
    // ignores OSC titles degrades silently.
    title.maybe_apply(state, stdout);
    Ok(())
}

/// Deletes Kitty image data by id. iTerm2 images are cell-anchored and are
/// torn down with the alternate screen, so they yield no sequences.
fn flush_welcome_cleanup(stdout: &mut Stdout, state: &mut AppState) -> io::Result<()> {
    let mut pending = Vec::new();
    if let Some(image) = &mut state.welcome_image {
        pending.extend(image.take_cleanup());
    }
    pending.extend(crate::image_preview::take_cleanup(state));
    if pending.is_empty() {
        return Ok(());
    }
    for sequence in pending {
        stdout.write_all(sequence.as_bytes())?;
    }
    stdout.flush()
}

/// Forward handled termination signals into the event loop.
///
/// A default SIGTERM/SIGHUP kills the process without restoring the terminal or
/// stopping the goal work this client owns. The loop receives each signal and
/// runs the same stop path once; a second signal stops waiting for cleanup.
/// Windows has no SIGHUP/SIGTERM equivalent for a console close; Ctrl+C is
/// forwarded when it reaches the process as a signal rather than a raw-mode key.
fn spawn_termination_forwarder(tx: mpsc::UnboundedSender<()>) {
    #[cfg(unix)]
    tokio::spawn(async move {
        use tokio::signal::unix::{SignalKind, signal};
        let install = |kind| signal(kind).map_err(|error| error.to_string());
        let (mut term, mut hup, mut interrupt) = match (
            install(SignalKind::terminate()),
            install(SignalKind::hangup()),
            install(SignalKind::interrupt()),
        ) {
            (Ok(term), Ok(hup), Ok(interrupt)) => (term, hup, interrupt),
            (term, hup, interrupt) => {
                for error in [term.err(), hup.err(), interrupt.err()]
                    .into_iter()
                    .flatten()
                {
                    tracing::warn!(%error, "could not install a termination signal handler");
                }
                return;
            }
        };
        loop {
            tokio::select! {
                _ = term.recv() => {}
                _ = hup.recv() => {}
                _ = interrupt.recv() => {}
            }
            if tx.send(()).is_err() {
                break;
            }
        }
    });
    #[cfg(windows)]
    tokio::spawn(async move {
        loop {
            if tokio::signal::ctrl_c().await.is_err() {
                break;
            }
            if tx.send(()).is_err() {
                break;
            }
        }
    });
    #[cfg(not(any(unix, windows)))]
    let _ = tx;
}

/// Write a state file and make it owner-only. The draft and history can carry
/// restored user text; they must not be readable by another local account.
/// Windows inherits the user-profile ACL, so the Unix mode is a no-op there.
fn write_private_state_file(path: &std::path::Path, contents: &str) {
    if std::fs::write(path, contents).is_ok() {
        let _ = leveler_core::private_fs::restrict_file(path);
    }
}

/// Replace every high-confidence value in a history entry with its mask. The
/// entry stays recognizable without becoming a credential store.
fn mask_high_confidence(text: &str) -> String {
    let findings = leveler_core::detect_secrets(text);
    let mut out = text.to_string();
    for finding in findings.iter().rev() {
        if finding.level != leveler_core::SecretLevel::HighConfidence {
            continue;
        }
        out.replace_range(
            finding.start..finding.end,
            &leveler_core::masked_secret(finding.value(text)),
        );
    }
    out
}

async fn maybe_coalesce_text_input(
    first: Action,
    state: &AppState,
    term_events: &mut TerminalInput,
    pending: &mut VecDeque<Action>,
) -> io::Result<Action> {
    let Action::Key(key) = first else {
        return Ok(first);
    };
    let Some(first_char) = plain_text_char(&key) else {
        return Ok(Action::Key(key));
    };
    // A coalesced burst ends in the composer. Only the composer may claim an
    // ordinary character: another workbench focus binds letters itself (the
    // Command focus stops with `x`), and turning that key into text before
    // the reducer ran swallowed the binding.
    if !coalescing_owns_key(state) {
        return Ok(Action::Key(key));
    }
    // `!` on an empty Normal composer is a mode transition, not text. The
    // reducer must see the key itself before any following characters are
    // folded into a typing burst.
    if first_char == '!'
        && state.composer.is_empty()
        && state.input_mode == crate::shell_mode::InputMode::Normal
    {
        return Ok(Action::Key(key));
    }

    let mut text = String::new();
    text.push(first_char);
    loop {
        let next = tokio::time::timeout(INPUT_BURST_WINDOW, term_events.next()).await;
        let action = match next {
            Err(_) => break,
            Ok(Some(Ok(action))) => action,
            Ok(Some(Err(error))) => return Err(error),
            Ok(None) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "terminal input closed",
                ));
            }
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
    Ok(Action::TextInput(text))
}

/// Whether this surface currently treats ordinary characters as composer
/// text. The reducer owns every other letter binding, so it must receive the
/// key event itself.
fn coalescing_owns_key(state: &AppState) -> bool {
    state.overlay.is_none()
        && state.active_screen == Screen::Conversation
        && state.workbench_focus == crate::state::WorkbenchFocus::Input
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
    #[test]
    fn settings_use_the_observed_snapshot_while_cancellation_keeps_its_identity_contract() {
        let rename = leveler_client_protocol::ClientCommand::RenameSession {
            session_id: leveler_client_protocol::SessionId::new("s1"),
            name: "new".into(),
        };
        assert_eq!(
            super::observed_command_version(&rename, Some(17)).unwrap(),
            Some(17)
        );
        assert!(
            super::observed_command_version(&rename, None)
                .unwrap_err()
                .contains("snapshot version required")
        );
        let cancel = leveler_client_protocol::ClientCommand::CancelTask {
            session_id: leveler_client_protocol::SessionId::new("s1"),
        };
        assert_eq!(
            super::observed_command_version(&cancel, Some(17)).unwrap(),
            None
        );
    }

    use super::*;

    #[test]
    fn explicit_quit_waits_for_runtime_cleanup_instead_of_disconnecting() {
        let mut s = state();
        let (completion_tx, _completion_rx) = mpsc::unbounded_channel();
        let (delivery_tx, mut delivery_rx) = mpsc::unbounded_channel();
        let client: Arc<dyn InteractiveRuntimeClient> = Arc::new(
            leveler_client_protocol::mock::MockRuntimeClient::new(s.session_id.clone()),
        );
        dispatch_effects(
            &mut s,
            vec![Effect::Quit],
            &client,
            &completion_tx,
            &delivery_tx,
            &None,
            &None,
            &mut None,
            &mut io::stdout(),
        );
        assert!(
            s.running,
            "a quit intent must wait for authoritative cleanup"
        );
        assert!(
            delivery_rx.try_recv().is_ok(),
            "the runtime must own cleanup"
        );
    }

    struct QuitRuntime {
        inner: leveler_client_protocol::mock::MockRuntimeClient,
        command_seen: tokio::sync::Notify,
        observed_after_stop: tokio::sync::Notify,
        snapshots: std::sync::atomic::AtomicUsize,
        fail_stop: bool,
    }

    impl QuitRuntime {
        fn new(snapshot: UiSessionSnapshot, fail_stop: bool) -> Arc<Self> {
            let inner = leveler_client_protocol::mock::MockRuntimeClient::new(snapshot.id.clone());
            inner.set_snapshot(snapshot);
            Arc::new(Self {
                inner,
                command_seen: tokio::sync::Notify::new(),
                observed_after_stop: tokio::sync::Notify::new(),
                snapshots: std::sync::atomic::AtomicUsize::new(0),
                fail_stop,
            })
        }
        fn publish(&self, snapshot: UiSessionSnapshot) {
            self.inner.set_snapshot(snapshot.clone());
            self.inner
                .emit(RuntimeEvent::SessionOpened { session: snapshot });
        }
    }

    #[async_trait::async_trait]
    impl InteractiveRuntimeClient for QuitRuntime {
        async fn send(&self, command: ClientCommand) -> Result<(), ClientError> {
            self.inner.send(command).await?;
            self.command_seen.notify_one();
            if self.fail_stop {
                Err(ClientError::Runtime("permission denied".into()))
            } else {
                Ok(())
            }
        }
        fn subscribe(&self) -> tokio::sync::broadcast::Receiver<RuntimeEvent> {
            self.inner.subscribe()
        }
        async fn snapshot(&self, session: &SessionId) -> Result<UiSessionSnapshot, ClientError> {
            let snapshot = self.inner.snapshot(session).await?;
            if self
                .snapshots
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                > 0
            {
                self.observed_after_stop.notify_one();
            }
            Ok(snapshot)
        }
    }

    async fn quit_snapshot() -> UiSessionSnapshot {
        let inner = leveler_client_protocol::mock::MockRuntimeClient::new(SessionId::new("s1"));
        let mut snapshot = inner.snapshot(&SessionId::new("s1")).await.unwrap();
        snapshot
            .active_background_tasks
            .push(leveler_client_protocol::UiActiveBackgroundTask {
                task_id: "bg-owned".into(),
                program: "sleep".into(),
                args: vec!["30".into()],
                elapsed_ms: 1,
                pid: Some(100),
                ready_evidence: None,
                lifetime: Some("goal".into()),
            });
        snapshot
    }

    #[tokio::test]
    async fn explicit_quit_waits_beyond_the_stop_ack_for_the_owned_background_terminal() {
        let snapshot = quit_snapshot().await;
        let runtime = QuitRuntime::new(snapshot.clone(), false);
        let client: Arc<dyn InteractiveRuntimeClient> = runtime.clone();
        let work = tokio::spawn(async move {
            stop_session_before_quit(&client, &SessionId::new("s1"), QUIT_CLEANUP_TIMEOUT).await
        });
        tokio::time::timeout(
            Duration::from_secs(1),
            runtime.observed_after_stop.notified(),
        )
        .await
        .unwrap();
        assert!(
            !work.is_finished(),
            "a stop ACK does not prove the process has gone"
        );
        assert!(
            matches!(runtime.inner.commands().as_slice(), [ClientCommand::CancelBackgroundTask {session_id, task_id}] if session_id.as_str()=="s1" && task_id=="bg-owned")
        );
        let mut terminal = snapshot;
        terminal.active_background_tasks.clear();
        runtime.publish(terminal);
        tokio::time::timeout(Duration::from_secs(1), work)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn explicit_quit_waits_for_the_turn_then_stops_its_late_background_task() {
        let mut snapshot = quit_snapshot().await;
        snapshot.status = "running".into();
        snapshot.active_background_tasks.clear();
        let runtime = QuitRuntime::new(snapshot.clone(), false);
        let client: Arc<dyn InteractiveRuntimeClient> = runtime.clone();
        let work = tokio::spawn(async move {
            stop_session_before_quit(&client, &SessionId::new("s1"), QUIT_CLEANUP_TIMEOUT).await
        });
        tokio::time::timeout(Duration::from_secs(1), runtime.command_seen.notified())
            .await
            .unwrap();
        assert!(
            matches!(runtime.inner.commands().as_slice(), [ClientCommand::CancelCurrentTurn {session_id}] if session_id.as_str()=="s1")
        );
        let mut late = quit_snapshot().await;
        late.active_background_tasks[0].task_id = "bg-late".into();
        runtime.publish(late.clone());
        tokio::time::timeout(Duration::from_secs(1), runtime.command_seen.notified())
            .await
            .unwrap();
        assert!(
            matches!(runtime.inner.commands().as_slice(), [ClientCommand::CancelCurrentTurn {..}, ClientCommand::CancelBackgroundTask {session_id, task_id}] if session_id.as_str()=="s1" && task_id=="bg-late")
        );
        late.active_background_tasks.clear();
        runtime.publish(late);
        tokio::time::timeout(Duration::from_secs(1), work)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn explicit_quit_stops_a_foreground_user_shell_only() {
        let mut snapshot = quit_snapshot().await;
        snapshot.active_background_tasks.clear();
        snapshot
            .user_shells
            .push(leveler_client_protocol::UiUserShell {
                id: leveler_core::UserShellId::new("ush-live"),
                origin_command_id: None,
                command: "npm run dev".into(),
                cwd: "/repo".into(),
                status: "running".into(),
                elapsed_secs: 3,
                exit_code: None,
                output_tail: String::new(),
                output_truncated: false,
                full_output: None,
                task_id: None,
            });
        snapshot
            .user_shells
            .push(leveler_client_protocol::UiUserShell {
                id: leveler_core::UserShellId::new("ush-moved"),
                origin_command_id: None,
                command: "make watch".into(),
                cwd: "/repo".into(),
                status: "background".into(),
                elapsed_secs: 9,
                exit_code: None,
                output_tail: String::new(),
                output_truncated: false,
                full_output: None,
                task_id: Some("bg-hosted".into()),
            });
        let runtime = QuitRuntime::new(snapshot.clone(), false);
        let client: Arc<dyn InteractiveRuntimeClient> = runtime.clone();
        let work = tokio::spawn(async move {
            stop_session_before_quit(&client, &SessionId::new("s1"), QUIT_CLEANUP_TIMEOUT).await
        });
        tokio::time::timeout(Duration::from_secs(1), runtime.command_seen.notified())
            .await
            .unwrap();
        assert!(
            matches!(
                runtime.inner.commands().as_slice(),
                [ClientCommand::CancelUserShell { session_id, execution_id }]
                    if session_id.as_str() == "s1" && execution_id.as_str() == "ush-live"
            ),
            "the foreground shell is stopped; the already-backgrounded one is untouched"
        );
        assert!(
            !work.is_finished(),
            "an ACK is not proof the shell has ended"
        );
        let mut stopped = snapshot;
        stopped
            .user_shells
            .retain(|shell| shell.id.as_str() != "ush-live");
        runtime.publish(stopped);
        tokio::time::timeout(Duration::from_secs(1), work)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn explicit_quit_ignores_a_shell_already_handed_to_the_background() {
        let mut snapshot = quit_snapshot().await;
        snapshot.active_background_tasks.clear();
        snapshot
            .user_shells
            .push(leveler_client_protocol::UiUserShell {
                id: leveler_core::UserShellId::new("ush-moved"),
                origin_command_id: None,
                command: "make watch".into(),
                cwd: "/repo".into(),
                status: "background".into(),
                elapsed_secs: 9,
                exit_code: None,
                output_tail: String::new(),
                output_truncated: false,
                full_output: None,
                task_id: Some("bg-hosted".into()),
            });
        let runtime = QuitRuntime::new(snapshot, false);
        let client: Arc<dyn InteractiveRuntimeClient> = runtime.clone();
        stop_session_before_quit(&client, &SessionId::new("s1"), QUIT_CLEANUP_TIMEOUT)
            .await
            .unwrap();
        assert!(
            runtime.inner.commands().is_empty(),
            "a backgrounded shell's process belongs to its registry lifetime"
        );
    }

    #[tokio::test]
    async fn explicit_quit_leaves_a_runtime_owned_background_task_alone() {
        let mut snapshot = quit_snapshot().await;
        snapshot.active_background_tasks[0].lifetime = Some("runtime".into());
        let runtime = QuitRuntime::new(snapshot, false);
        let client: Arc<dyn InteractiveRuntimeClient> = runtime.clone();
        stop_session_before_quit(&client, &SessionId::new("s1"), QUIT_CLEANUP_TIMEOUT)
            .await
            .unwrap();
        assert!(
            runtime.inner.commands().is_empty(),
            "a runtime-owned task outlives the client that launched it"
        );
    }

    #[tokio::test]
    async fn explicit_quit_cleanup_error_keeps_the_client_open_for_retry() {
        let runtime = QuitRuntime::new(quit_snapshot().await, true);
        let client: Arc<dyn InteractiveRuntimeClient> = runtime;
        let result =
            stop_session_before_quit(&client, &SessionId::new("s1"), QUIT_CLEANUP_TIMEOUT).await;
        assert!(result.as_ref().unwrap_err().contains("permission denied"));
        let mut s = state();
        s.quit_pending = true;
        reduce(
            &mut s,
            Action::EffectCompleted(EffectCompletion::QuitFinished(result)),
        );
        assert!(s.running);
        assert!(!s.quit_pending);
        assert!(
            s.notification
                .as_ref()
                .unwrap()
                .message
                .contains("permission denied")
        );
        assert_eq!(
            s.notification.as_ref().unwrap().level,
            NotificationLevel::Error
        );
    }

    #[test]
    fn explicit_quit_blocks_late_input_and_exits_only_on_successful_cleanup() {
        let mut s = state();
        s.quit_pending = true;
        assert!(reduce(&mut s, Action::TextInput("new work".into())).is_empty());
        assert!(
            reduce(
                &mut s,
                Action::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            )
            .is_empty()
        );
        assert!(
            s.composer.is_empty(),
            "no new submission may follow the exit intent"
        );
        assert!(s.running);
        reduce(
            &mut s,
            Action::EffectCompleted(EffectCompletion::QuitFinished(Ok(()))),
        );
        assert!(!s.running);
        assert!(!s.quit_pending);
    }

    /// A plain letter is composer text only while the composer owns the keys.
    /// The Command focus binds `x` to stop; coalescing it into `TextInput`
    /// before the reducer ran made the advertised key type an `x` instead.
    #[test]
    fn a_key_burst_is_only_composer_text_while_the_composer_owns_keys() {
        let mut s = state();
        assert!(super::coalescing_owns_key(&s));
        s.workbench_focus = crate::state::WorkbenchFocus::Command;
        assert!(
            !super::coalescing_owns_key(&s),
            "a contextual letter binding must reach the reducer"
        );
        s.workbench_focus = crate::state::WorkbenchFocus::Input;
        s.active_screen = Screen::Help;
        assert!(!super::coalescing_owns_key(&s));
    }

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
                model_notice: None,
                thinking: None,
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
    fn a_repeated_warning_never_resets_its_clock() {
        let mut s = state();
        let warning = Notification {
            level: NotificationLevel::Warning,
            message: "同样的提醒".into(),
        };
        let base = Instant::now();
        let mut shown = None;
        s.notification = Some(warning.clone());
        assert!(!expire_notification(&mut s, &mut shown, base));
        // The same warning arriving again is the same fact, not a new one: it
        // must not buy another eight seconds.
        s.notification = Some(warning);
        assert!(!expire_notification(
            &mut s,
            &mut shown,
            base + std::time::Duration::from_secs(7)
        ));
        assert!(expire_notification(
            &mut s,
            &mut shown,
            base + NOTIFICATION_TTL_WARNING
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
    fn session_management_commands_bind_the_session_they_target() {
        let current = SessionId::new("current");
        let target = SessionId::new("stored");
        let commands = [
            ClientCommand::ArchiveSession {
                session_id: target.clone(),
            },
            ClientCommand::UnarchiveSession {
                session_id: target.clone(),
            },
            ClientCommand::RenameSession {
                session_id: target.clone(),
                name: "renamed".into(),
            },
            ClientCommand::DeleteSession {
                session_id: target.clone(),
            },
            ClientCommand::ForkSession {
                session_id: target.clone(),
            },
        ];
        for command in commands {
            assert_eq!(
                delivery_session_id(&command, &current).as_str(),
                "stored",
                "{command:?}"
            );
        }
        assert_eq!(
            delivery_session_id(&ClientCommand::RequestSessionList, &current).as_str(),
            "current"
        );
        // A command that acts for the current client keeps the requester as
        // its envelope even when it names another session to open.
        assert_eq!(
            delivery_session_id(
                &ClientCommand::OpenSessionFor {
                    requester_session_id: current.clone(),
                    session_id: target,
                },
                &current
            )
            .as_str(),
            "current"
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

#[cfg(test)]
mod secret_persistence_tests {
    use super::{mask_high_confidence, write_private_state_file};

    #[test]
    fn history_entries_never_persist_a_high_confidence_value() {
        let raw = "DEEPSEEK_API_KEY=sk-CANARY-SECRET-DO-NOT-EXPOSE-001";
        let masked = mask_high_confidence(raw);
        assert!(!masked.contains("sk-CANARY"), "{masked}");
        assert!(masked.contains("DEEPSEEK_API_KEY"), "{masked}");
        assert!(masked.contains("••••"), "{masked}");
        // Ordinary text is byte-identical.
        let ordinary = "let password = config.password;";
        assert_eq!(mask_high_confidence(ordinary), ordinary);
    }

    #[test]
    fn a_bare_key_handoff_is_masked() {
        let raw = "然后这是key:mgzk1gr8k7bfm6z6t8ft6tdxd8ydmm5t 要用search api 再测试一次";
        let masked = mask_high_confidence(raw);
        assert!(
            !masked.contains("mgzk1gr8k7bfm6z6t8ft6tdxd8ydmm5t"),
            "{masked}"
        );
        assert!(masked.contains("key:"), "{masked}");
        assert!(masked.contains("••••"), "{masked}");
    }

    #[cfg(unix)]
    #[test]
    fn state_files_are_written_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("input_history.json");
        write_private_state_file(&path, "[]");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
