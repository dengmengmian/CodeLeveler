use leveler_client_protocol::{ClientCommand, CommandId, MessageId, NotificationLevel};

use crate::action::Effect;
use crate::screen::{BusyPolicy, Screen};
use crate::state::{AppState, Notification, PendingSubmission, WorkbenchFocus};
use crate::transcript::InputDelivery;

use super::overlay_keys::{
    apply_theme_id, open_checkpoint_picker, open_collab_picker, open_mode_picker,
    open_model_picker, open_recovery_confirm, open_theme_picker, open_unsupported_media,
};
use super::runtime_apply::start_turn;
use super::screen_nav::{
    open_clean, open_diff_screen, open_sessions_screen, open_stats, open_trace, toggle_screen,
};

/// "The composer text just changed" hook: re-arm the slash popup and drop any
/// contextual next-step ghost, because the user has now said something with
/// their own hands. Every editing path (typing, paste, delete, kill, `$EDITOR`
/// result, completion) funnels through here, which is why the ghost is
/// destroyed in one place instead of at two dozen call sites.
pub(super) fn touch_slash_filter(state: &mut AppState) {
    // The side thread is a plain conversation: no slash popup, no completions,
    // no next-step ghost. Leaving the main popup state untouched also means
    // returning to Main restores it exactly as it was.
    if state.surface == crate::btw::SurfaceFocus::Btw {
        return;
    }
    state.slash_selected = 0;
    state.slash_popup_dismissed = false;
    // Any edit invalidates the fetched candidate set and its replacement
    // ranges. A response that lands later is dropped by request identity.
    state.completion.reset_on_edit();
    crate::suggestion::dismiss(state);
    if crate::screen::skill_mention_query(state).is_some() {
        refresh_skill_catalog(state);
    }
}

pub(super) fn submit(state: &mut AppState) -> Vec<Effect> {
    // Submitting real input spends the ghost. An Enter on an EMPTY composer
    // submits nothing and must leave the offer standing: the ghost is not
    // input, so that keystroke is a no-op, not a rejection of the suggestion.
    if !state.composer.is_empty() {
        crate::suggestion::dismiss(state);
    }
    // Shell Mode is explicit input state; the `!` marker never enters the
    // buffer, so the canonical text is the command itself. An empty shell
    // submission gets its own hint instead of the chat no-op.
    if state.input_mode == crate::shell_mode::InputMode::Shell {
        let cmd = state.composer.canonical_text();
        return submit_user_shell(state, cmd);
    }
    let raw = state.composer.text().trim().to_string();
    // A picture is a message. Only an empty composer with nothing staged is
    // not one.
    if raw.is_empty() && state.pending_attachments.is_empty() {
        return Vec::new();
    }
    // A FIRST LINE starting with a KNOWN `/command` is a local slash command,
    // parsed locally and never sent to the model — its argument may span
    // multiple lines (a small pasted goal below the chip threshold must not
    // silently degrade `/goal` into a chat message; R004 F1-adjacent). The
    // unknown-`/xxx` guard keeps its single-line scope so a typo or path-like
    // multiline message is never swallowed.
    //
    // A LARGE paste is represented in the buffer by `[Pasted: N lines]`, so
    // the command prefix lives only in the canonical text. Detecting on the
    // raw buffer alone let a pasted `/goal ...` blob become a Chat message
    // whose first line was the literal command — the same degradation this
    // branch exists to prevent, one threshold higher.
    let canonical;
    let command_text = if raw.starts_with('/') {
        raw.as_str()
    } else {
        canonical = state.composer.canonical_text();
        canonical.trim()
    };
    if let Some(rest) = command_text.strip_prefix('/') {
        let name = rest
            .lines()
            .next()
            .unwrap_or("")
            .split_whitespace()
            .next()
            .unwrap_or("");
        if crate::screen::is_known_slash_token(name) {
            // What a command does lands at the end of the conversation. A user
            // who ran /compact while reading history saw only a notification
            // fade, and the line it wrote stayed a page below them.
            crate::conversation::interaction::jump_to_live_edge(state);
            // `take()` returns the canonical content (paste chips expanded);
            // slash ARGUMENTS must come from it, not from the raw buffer, or
            // a pasted goal/question degrades to its placeholder (R004 F1).
            let expanded = state.composer.take();
            let expanded = expanded.trim();
            let rest_expanded = expanded.strip_prefix('/').unwrap_or(expanded);
            return handle_slash(state, rest_expanded.trim());
        }
        // Reserve unknown-command feedback for SINGLE-LINE command-shaped
        // typos such as `/hlep`. Absolute paths (`/Users/...`), file names,
        // and multiline slash-prefixed prose are ordinary messages.
        if !command_text.contains('\n') && looks_like_unknown_slash_command(name) {
            state.notification = Some(Notification {
                level: NotificationLevel::Warning,
                message: format!("未知命令: /{name}（内容已保留，/help 查看命令）"),
            });
            return Vec::new();
        }
    }
    if state.is_busy() {
        // Images have no mid-turn delivery lane: a steer is text only, and
        // sending the text without them would detach the message from the
        // picture the user staged. Say so and leave the draft untouched.
        if !state.pending_attachments.is_empty() {
            state.notification = Some(Notification {
                level: NotificationLevel::Warning,
                message: state.t().steer_attachments_unsupported.to_string(),
            });
            return Vec::new();
        }
        if turn_input_held(state) {
            return Vec::new();
        }
        // The product default: a message typed while a turn runs joins that
        // turn at its next model round. The row appears immediately as
        // Sending; only the runtime's admission answer promotes it to saved,
        // and only its settlement says the model actually took it.
        let text = state.composer.take().trim().to_string();
        if text.is_empty() {
            return Vec::new();
        }
        let message_id = MessageId::new(leveler_core::new_uuid_string());
        state
            .transcript
            .push_input(message_id.clone(), text.clone(), InputDelivery::Sending);
        let command = ClientCommand::SteerCurrentTurn {
            session_id: state.session_id.clone(),
            content: text,
            message_id: Some(message_id),
        };
        return submit_turn_input(state, command);
    }
    // Vision gate: block sending images to a non-vision model until the user
    // chooses how to proceed (spec §42). Handled before the request is built.
    if !state.pending_attachments.is_empty() && !state.vision {
        open_unsupported_media(state);
        return Vec::new();
    }
    send_message(state)
}

/// `/thinking [auto|off|minimal|low|medium|high|max|reset]`.
///
/// No argument opens the picker. A level applies to this session only; `reset`
/// clears the override so the session goes back to the configured level, which
/// is a different request from `auto` — a session that explicitly asked for no
/// override at all.
fn thinking_slash(state: &mut AppState, command: &str) -> Vec<Effect> {
    let arg = command.split_whitespace().nth(1).unwrap_or("").trim();
    let t = state.t();
    if arg.is_empty() {
        crate::reducer::overlay_keys::open_thinking_picker(state);
        return Vec::new();
    }
    let level = if arg.eq_ignore_ascii_case("reset") {
        None
    } else {
        match leveler_model::ThinkingLevel::parse(arg) {
            Some(level) => Some(level),
            None => {
                state.notification = Some(Notification {
                    level: NotificationLevel::Warning,
                    message: t.thinking_invalid.replacen("{}", arg, 1).replacen(
                        "{}",
                        &leveler_model::ThinkingLevel::values(),
                        1,
                    ),
                });
                return Vec::new();
            }
        }
    };
    state.notification = Some(Notification {
        level: NotificationLevel::Info,
        message: t.thinking_requested.replacen(
            "{}",
            &level.map_or("reset".to_string(), |l| l.to_string()),
            1,
        ),
    });
    vec![Effect::Send(ClientCommand::SetThinkingLevel {
        session_id: state.session_id.clone(),
        level,
    })]
}

fn looks_like_unknown_slash_command(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '-' | '_'))
}

/// Build and send the current composer message with its attachments, clearing
/// both. Assumes vision gating has already passed.
pub(super) fn send_message(state: &mut AppState) -> Vec<Effect> {
    if turn_input_held(state) {
        return Vec::new();
    }
    let content = state.composer.take();
    let attachments = std::mem::take(&mut state.pending_attachments);
    let shown = state.message_with_images(attachments.len(), &content);
    state.transcript.push_user_if_new(shown);
    // Stage the turn's goal identity before it flips to Busy. In Goal mode
    // every turn belongs to the goal on screen; in Chat the runtime's own
    // structured intent decides whether this message re-enters a goal, and the
    // client projects that fact instead of guessing it from the text.
    let continuation = attachments.is_empty() && resumes_goal(state);
    state.staged_goal = Some(crate::active_goal::StagedGoal {
        title: crate::active_goal::short_title(&content),
        objective: Some(content.clone()),
        continuation,
    });
    // Go Busy immediately (not on the first runtime event): closes the
    // submit→first-event window where a second submit would send instead of
    // queue, double-driving the runtime.
    start_turn(state);
    // Mirror session collaboration on the busy chrome (footer/plan shell).
    // Runtime maps collaboration=goal SubmitMessage → goal turn profile.
    state.goal_mode_active =
        state.collaboration.eq_ignore_ascii_case("goal") && attachments.is_empty();
    let command = turn_input_command(state, content, attachments);
    submit_turn_input(state, command)
}

/// Whether this submitted text belongs to the goal already on screen.
///
/// Only the session's collaboration axis answers that on the client: in Goal
/// mode every turn is the goal's. In Chat, whether an ordinary message
/// continues a goal is the model's reading, declared through the runtime's
/// structured intent tool; the client never classifies the user's words.
fn resumes_goal(state: &AppState) -> bool {
    state.collaboration.eq_ignore_ascii_case("goal")
}

/// The command a user's turn input becomes.
///
/// An ordinary message stays [`ClientCommand::SubmitMessage`]. The runtime —
/// not the client — decides whether the model's structured declaration makes it
/// a continuation, so any language and phrasing takes the same path.
fn turn_input_command(
    state: &AppState,
    content: String,
    attachments: Vec<leveler_client_protocol::AttachmentRef>,
) -> ClientCommand {
    ClientCommand::SubmitMessage {
        session_id: state.session_id.clone(),
        content,
        attachments,
    }
}

/// Send one 待发送 item through the ordinary turn-input delivery. While a turn
/// runs it steers that turn; otherwise it starts one. The item stays in the
/// list — marked sending — until the runtime answers for its id.
pub(super) fn send_pending_input(state: &mut AppState, index: usize) -> Vec<Effect> {
    let Some(item) = state.pending_inputs.get(index) else {
        return Vec::new();
    };
    if !item.is_unsent() {
        return Vec::new();
    }
    // The list is shared bottom-control state; an item written for another
    // session is not this session's to send.
    if item.session_id != state.session_id {
        return Vec::new();
    }
    let content = item.text.clone();
    let message_id = item.message_id.clone();
    if turn_input_held(state) {
        return Vec::new();
    }
    let command = if state.is_busy() {
        ClientCommand::SteerCurrentTurn {
            session_id: state.session_id.clone(),
            content,
            message_id: Some(message_id),
        }
    } else {
        // The runtime announces an admitted turn's message itself
        // (`UserMessageAdded`); only the optimistic Busy is ours to set.
        let continuation = resumes_goal(state);
        state.staged_goal = Some(crate::active_goal::StagedGoal {
            title: crate::active_goal::short_title(&content),
            objective: Some(content.clone()),
            continuation,
        });
        start_turn(state);
        turn_input_command(state, content, Vec::new())
    };
    let effects = submit_turn_input(state, command);
    if let Some(Effect::Submit { command_id, .. }) = effects.first() {
        state.pending_inputs[index].state =
            crate::pending_inputs::PendingInputState::Submitting(command_id.clone());
    }
    effects
}

/// Move the composer's text into 待发送: the explicit "process after the
/// running task" lane.
///
/// Ordinary Enter during a turn steers. This is the other choice, and it is
/// only reachable by an explicit gesture (Tab with a typed message), so the
/// two lanes never compete for the same keystroke. The item is client-local:
/// it is not in the runtime's journal until the client actually sends it.
pub(super) fn queue_for_after_turn(state: &mut AppState) -> Vec<Effect> {
    let text = state.composer.take().trim().to_string();
    if text.is_empty() {
        return Vec::new();
    }
    state
        .pending_inputs
        .push(crate::pending_inputs::PendingInput::queued(
            text,
            state.session_id.clone(),
        ));
    state.pending_selected = state.pending_inputs.len() - 1;
    Vec::new()
}

/// Take the head of the 待发送 queue when the runtime is genuinely ready for
/// the next turn, and submit it as that turn.
///
/// This is the only automatic drain, and it lives here because `reduce` is the
/// single authority over `AppState`: every action — a terminal runtime event, an
/// idle snapshot, a reconnect, an effect completion, a keystroke — runs through
/// `reduce`, which calls this once at its tail. A second event in the same ready
/// window therefore cannot send the same item twice: the first call moved the
/// item to `Submitting`, emptied `pending_submissions`, and made the status
/// `Busy`, and any of those three alone is enough to stop the next call.
///
/// Ready means the runtime's own state, not a presentation event: connected,
/// not `Busy`, no turn input still in flight, and nothing blocking on the user
/// (an overlay / pending interaction). The runtime status is the authority — a
/// `TurnCompleted` presentation event is never the trigger by itself.
///
/// Only the FIFO head is considered, and only while it is still `Queued`. An
/// in-flight head (`Submitting` / `DeliveryUnknown`) is already a pending
/// submission, so the in-flight check stops the drain; a `Failed` head pauses
/// the queue until the user decides, because retrying it automatically would
/// loop and skipping past it would reorder the user's own messages.
pub(super) fn drain_pending_input(state: &mut AppState) -> Option<Effect> {
    if state.quit_pending || !state.runtime_connected || state.is_busy() {
        return None;
    }
    // A question the runtime is blocked on means it is not ready for a new
    // turn, whatever the coarse status says.
    if state.overlay.is_some() || !state.pending_interactions.is_empty() {
        return None;
    }
    // An earlier turn input has not been answered yet and may already be
    // running; starting a turn behind it would double-drive the runtime.
    if !state.pending_submissions.is_empty() {
        return None;
    }
    if !state
        .pending_inputs
        .first()
        .is_some_and(|item| item.is_queued() && item.session_id == state.session_id)
    {
        return None;
    }
    // Not busy by the checks above, so this submits a new turn (never a steer).
    send_pending_input(state, 0).into_iter().next()
}

/// Return one unsent item to the composer without creating a delivery.
pub(super) fn edit_pending_input(state: &mut AppState, index: usize) {
    let Some(item) = state.pending_inputs.get(index) else {
        return;
    };
    if !item.is_unsent()
        || item.session_id != state.session_id
        || state.surface != crate::btw::SurfaceFocus::Main
    {
        return;
    }
    if !state.composer.is_empty() || !state.pending_attachments.is_empty() {
        state.notification = Some(Notification {
            level: NotificationLevel::Warning,
            message: state.t().pending_input_edit_draft.to_string(),
        });
        return;
    }
    let text = item.text.clone();
    delete_pending_input(state, index);
    state.pending_hits.clear();
    state.composer.replace(text);
    super::shell::reset_after_submit(state);
    state.workbench_focus = WorkbenchFocus::Input;
    touch_slash_filter(state);
}

/// Delete one unsent 待发送 item. Local only: it never reached the runtime.
pub(super) fn delete_pending_input(state: &mut AppState, index: usize) {
    if state
        .pending_inputs
        .get(index)
        .is_some_and(crate::pending_inputs::PendingInput::is_unsent)
    {
        state.pending_inputs.remove(index);
        state.pending_hover = None;
        state.pending_selected = state
            .pending_selected
            .min(state.pending_inputs.len().saturating_sub(1));
        if state.pending_inputs.is_empty() && state.workbench_focus == WorkbenchFocus::Pending {
            state.workbench_focus = WorkbenchFocus::Input;
        }
    }
}

/// Refuse a new turn input while an earlier one to this session has no answer
/// from the runtime (it may already be running), or while nothing can reach
/// the runtime at all. The caller leaves the typed text in place.
fn turn_input_held(state: &mut AppState) -> bool {
    let message = if !state.runtime_connected {
        state.t().input_held_offline
    } else if state
        .pending_submissions
        .iter()
        .any(|p| p.unconfirmed && p.command.session_id() == Some(&state.session_id))
    {
        state.t().submission_held
    } else {
        return false;
    };
    state.notification = Some(Notification {
        level: NotificationLevel::Warning,
        message: message.to_string(),
    });
    true
}

/// A turn input gets its id here, where the logical command is created — not
/// per transport attempt — and the client keeps it until the runtime answers.
fn submit_turn_input(state: &mut AppState, command: ClientCommand) -> Vec<Effect> {
    // What the user just sent, and the turn it drives, are the live edge.
    crate::conversation::interaction::jump_to_live_edge(state);
    let command_id = CommandId::generate();
    state.pending_submissions.push(PendingSubmission {
        command_id: command_id.clone(),
        command: command.clone(),
        unconfirmed: false,
        settled_turns: state.settled_turns,
    });
    vec![Effect::Submit {
        command,
        command_id,
    }]
}

/// The text a turn input showed in the conversation when it was sent.
pub(super) fn turn_input_text(command: &ClientCommand) -> Option<&str> {
    match command {
        ClientCommand::SubmitMessage { content, .. }
        | ClientCommand::SteerCurrentTurn { content, .. }
        | ClientCommand::RunGoal { content, .. }
        | ClientCommand::ResumeTask { content, .. } => Some(content),
        _ => None,
    }
}

/// Put a turn input the runtime did not run back where the user typed it,
/// ahead of anything typed since.
pub(super) fn restore_turn_input(state: &mut AppState, command: ClientCommand) {
    let (text, attachments) = match command {
        ClientCommand::SubmitMessage {
            content,
            attachments,
            ..
        } => (content, attachments),
        ClientCommand::SteerCurrentTurn { content, .. } => (content, Vec::new()),
        ClientCommand::RunGoal { content, .. } => (format!("/goal {content}"), Vec::new()),
        ClientCommand::ResumeTask { content, .. } => (content, Vec::new()),
        _ => return,
    };
    if state.composer.canonical_text().trim().is_empty() {
        state.composer.replace(text);
    } else {
        // The restored message's images go back at the head of the list, so
        // the names in the draft written since move down to make room.
        state.composer.shift_image_token_numbers(attachments.len());
        let draft = state.composer.canonical_text();
        state.composer.replace(format!("{text}\n{draft}"));
    }
    state.pending_attachments.splice(0..0, attachments);
}

/// Complete a partial slash command to the highlighted match (Tab/Enter, §29).
pub(super) fn complete_slash(state: &mut AppState) {
    let matches = crate::screen::visible_slash_popup(state);
    if matches.is_empty() {
        return;
    }
    let idx = state.slash_selected.min(matches.len() - 1);
    let name = matches[idx].0.clone();
    state.composer.replace(format!("{name} "));
    touch_slash_filter(state);
}

pub(super) fn complete_skill_mention(state: &mut AppState) {
    let matches = crate::screen::visible_skill_popup(state);
    let Some((name, _)) = matches.get(state.slash_selected.min(matches.len().saturating_sub(1)))
    else {
        return;
    };
    let name = name.clone();
    state
        .composer
        .replace_token_before_cursor(&format!("{name} "));
    touch_slash_filter(state);
}

pub(super) fn complete_file_mention(state: &mut AppState) {
    let matches = crate::screen::visible_file_popup(state);
    let Some(path) = matches
        .get(state.slash_selected.min(matches.len().saturating_sub(1)))
        .map(|path| (*path).to_string())
    else {
        return;
    };
    state
        .composer
        .replace_token_before_cursor(&format!("@{path} "));
    touch_slash_filter(state);
}

pub(super) fn request_file_candidates(state: &mut AppState) -> Vec<Effect> {
    if state.input_mode == crate::shell_mode::InputMode::Normal
        && crate::screen::file_mention_query(state).is_some()
        && !state.file_index_requested
        && let Some(repository) = state.repository.clone()
    {
        state.file_index_requested = true;
        vec![Effect::LoadFileCandidates { repository }]
    } else {
        Vec::new()
    }
}

/// Start (or re-surface) the embedded browser Web UI. The server is bound and
/// served at the event-loop edge via the injected `WebLauncher`; here we only
/// guard against launching twice and give immediate feedback.
fn start_web(state: &mut AppState) -> Vec<Effect> {
    if let Some(url) = &state.web_url {
        state.notification = Some(Notification {
            level: NotificationLevel::Info,
            message: format!("Web UI 已在运行：{url}"),
        });
        // The tab that opened on first launch may be long closed — re-open the
        // browser every time instead of stranding the user with a URL to copy.
        return vec![Effect::OpenWebUrl(url.clone())];
    }
    if state.web_starting {
        return Vec::new();
    }
    state.web_starting = true;
    state.notification = Some(Notification {
        level: NotificationLevel::Info,
        message: "正在启动 Web UI…".to_string(),
    });
    vec![Effect::StartWeb]
}

/// Handle a `/command` typed in the composer.
pub(super) fn handle_slash(state: &mut AppState, command: &str) -> Vec<Effect> {
    let name = command.split_whitespace().next().unwrap_or("");
    // Busy gate from the registry (IdleOnly refuses while a turn is running).
    if state.is_busy()
        && let Some(def) = crate::screen::slash_def(name)
        && def.busy == BusyPolicy::IdleOnly
    {
        let primary = def.name.trim_start_matches('/');
        state.notification = Some(Notification {
            level: NotificationLevel::Warning,
            message: state.t().idle_only_cmd.replacen("{}", primary, 1),
        });
        return Vec::new();
    }

    // Canonical primary without leading `/` (aliases collapse here).
    let primary = crate::screen::slash_primary(name)
        .map(|p| p.trim_start_matches('/'))
        .unwrap_or(name);

    match primary {
        "model" => {
            open_model_picker(state);
            Vec::new()
        }
        "permission" => {
            open_mode_picker(state);
            Vec::new()
        }
        "thinking" => thinking_slash(state, command),
        "goal" => run_goal(state, command),
        "btw" => run_btw(state, command),
        "trace" => {
            let id = command.split_whitespace().nth(1).map(str::to_string);
            // A slash command is typed in the conversation, so that is where
            // the observatory's `←` goes back to.
            open_trace(state, id, crate::screen::Screen::Conversation)
        }
        "context" => open_stats(state, crate::screen::StatsSection::Context),
        // Same page, other section: one implementation, two entry points.
        "usage" => open_stats(state, crate::screen::StatsSection::Usage),
        "clean" => open_clean(state),
        "schedules" => crate::schedules::open(state),
        "memory" => crate::memory::open(state),
        "remember" => remember_memory(state, command),
        "dream" => run_memory_dream(state, command),
        "flush" => run_memory_flush(state),
        "collab" => set_collab_cmd(state, command),
        "agents" => agents_slash(state, command),
        "skills" => skills_slash(state, command),
        "web" => start_web(state),
        "copy" => copy_slash(state, command),
        "output" => output_slash(state),
        "diff" => open_diff_screen(state),
        "sessions" => open_sessions_screen(state),
        // Exact id imports straight away; an omitted id opens that source's
        // picker. Independent of native `/resume`: the import always creates a
        // NEW CodeLeveler session.
        "resume-claude" => crate::foreign_session::resume(
            state,
            command,
            leveler_client_protocol::UiForeignTool::ClaudeCode,
        ),
        "resume-codex" => crate::foreign_session::resume(
            state,
            command,
            leveler_client_protocol::UiForeignTool::Codex,
        ),
        "restore" => {
            open_checkpoint_picker(state);
            Vec::new()
        }
        // Review the crash window and explicitly acknowledge that the
        // interrupted calls' outcomes are unknown. It never replays them and
        // never claims they succeeded.
        "recover" => open_recovery_confirm(state),
        // The runtime copies record + transcript into a fresh session and
        // leaves this one open, so there is nothing to confirm here.
        "fork" => vec![Effect::Send(ClientCommand::ForkSession {
            session_id: state.session_id.clone(),
        })],
        "compact" => vec![Effect::Send(ClientCommand::CompactContext {
            session_id: state.session_id.clone(),
        })],
        // Re-read the configured sources into the RUNNING runtime. Not a model
        // message: the answer is the runtime's ConfigReloaded event.
        "reload" => vec![Effect::Send(ClientCommand::ReloadConfig { query_id: None })],
        // Long-goal P3: the runtime cuts a durable goal checkpoint and
        // answers with GoalRecapCreated — the Recap is persisted truth, not
        // a client-side transcript summary.
        "recap" => vec![Effect::Send(ClientCommand::Recap {
            session_id: state.session_id.clone(),
        })],
        "export" => export_conversation(state, command),
        "theme" => {
            let arg = command.split_whitespace().nth(1).unwrap_or("").trim();
            if arg.is_empty() {
                open_theme_picker(state);
            } else {
                apply_theme_id(state, arg);
            }
            Vec::new()
        }
        "attach" => {
            let path = command.split_whitespace().nth(1).unwrap_or("").trim();
            if path.is_empty() {
                state.notification = Some(Notification {
                    level: NotificationLevel::Warning,
                    message: "用法: /attach <文件路径>".to_string(),
                });
                Vec::new()
            } else {
                vec![Effect::Send(ClientCommand::AddAttachment {
                    session_id: state.session_id.clone(),
                    path: path.to_string(),
                    name: None,
                })]
            }
        }
        "new" => new_conversation(state),
        "help" => toggle_screen(state, Screen::Help),
        "doctor" => toggle_screen(state, Screen::Doctor),
        "update" => start_update(state),
        other => {
            state.notification = Some(Notification {
                level: NotificationLevel::Warning,
                message: format!("未知命令: /{other}"),
            });
            Vec::new()
        }
    }
}

/// The applied patch of the most recent edit node, verbatim, for `/copy diff`.
///
/// The node is the unit the conversation paints as one edit, so the answer is
/// the patch set of that node and nothing else: every member shares the tool,
/// the touched files and the status (`tool_cell::patch_files_key` + name), so
/// concatenating their own patches cannot claim a continuity the runtime never
/// reported. A call with no applied diff contributes nothing, and when no call
/// has one there is no patch to copy.
fn latest_applied_patch(state: &AppState) -> Option<String> {
    use crate::transcript::TranscriptItem;
    let calls = state
        .transcript
        .items()
        .iter()
        .rev()
        .find_map(|item| match item {
            TranscriptItem::ToolGroup(group)
                if group.calls.iter().any(|c| {
                    crate::activity_stream::is_edit_call(c) && applied_diff(c).is_some()
                }) =>
            {
                Some(group.calls.as_slice())
            }
            _ => None,
        })?;
    let anchor = calls
        .iter()
        .rev()
        .find(|c| crate::activity_stream::is_edit_call(c) && applied_diff(c).is_some())?;
    let key = (
        anchor.name.clone(),
        crate::tool_cell::patch_files_key(&anchor.arguments),
        anchor.status,
    );
    let patches: Vec<&str> = calls
        .iter()
        .filter(|c| {
            (
                c.name.clone(),
                crate::tool_cell::patch_files_key(&c.arguments),
                c.status,
            ) == key
        })
        .filter_map(applied_diff)
        .collect();
    (!patches.is_empty()).then(|| patches.join("\n"))
}

/// The patch the runtime reported as APPLIED for one call. Never the request.
fn applied_diff(call: &crate::transcript::ToolCallBlock) -> Option<&str> {
    call.applied_diff
        .as_deref()
        .map(str::trim_end)
        .filter(|d| !d.trim().is_empty())
}

/// `/copy diff|output|command|selection`.
///
/// Both output paths copy the authored content, never painted rows: the
/// command is the exact string the user ran, and the output comes from the
/// raw capture (or the complete retained text when it was never capped). A
/// truncated tail is refused rather than silently passed off as the whole
/// result.
///
/// `/copy diff` copies the runtime's own applied patch, verbatim. It is a
/// different thing from dragging over the diff body: a selection yields the
/// source lines (no gutter, no `+`/`-` marker), while this entry yields a
/// unified diff a reader can apply. A request the runtime never reported as
/// applied is not a candidate — the model's intent is not a change.
fn copy_slash(state: &mut AppState, command: &str) -> Vec<Effect> {
    use crate::transcript::{TranscriptItem, UserShellStatus};
    let arg = command.split_whitespace().nth(1).unwrap_or("");
    let latest = state
        .transcript
        .items()
        .iter()
        .rev()
        .find_map(|item| match item {
            TranscriptItem::UserShell(shell) => Some((
                shell.status,
                shell.command.clone(),
                shell.output.clone(),
                shell.output_truncated,
                shell.full_output.clone(),
            )),
            _ => None,
        });
    match arg {
        "diff" => {
            let Some(patch) = latest_applied_patch(state) else {
                state.notification = Some(Notification {
                    level: NotificationLevel::Warning,
                    message: state.t().copy_no_diff.to_string(),
                });
                return Vec::new();
            };
            copy_now(state, &patch)
        }
        "command" => {
            let Some((_, command, _, _, _)) = latest else {
                state.notification = Some(Notification {
                    level: NotificationLevel::Warning,
                    message: state.t().copy_no_shell.to_string(),
                });
                return Vec::new();
            };
            copy_now(state, &command)
        }
        "output" => {
            let Some((status, _, retained, truncated, full_output)) = latest else {
                state.notification = Some(Notification {
                    level: NotificationLevel::Warning,
                    message: state.t().copy_no_shell.to_string(),
                });
                return Vec::new();
            };
            if matches!(
                status,
                UserShellStatus::Running | UserShellStatus::Background
            ) {
                state.notification = Some(Notification {
                    level: NotificationLevel::Warning,
                    message: state.t().copy_still_running.to_string(),
                });
                return Vec::new();
            }
            if let Some(directory) = full_output {
                match valid_capture(state, &directory) {
                    Ok(capture) => {
                        return vec![Effect::CopyUserShellOutput {
                            directory: Some(capture.directory),
                            retained,
                        }];
                    }
                    Err(error) if truncated => {
                        state.notification = Some(Notification {
                            level: NotificationLevel::Warning,
                            message: error,
                        });
                        return Vec::new();
                    }
                    // The capture is gone but the retained text is complete;
                    // copy what is actually known rather than refusing.
                    Err(_) => {}
                }
            }
            if truncated {
                state.notification = Some(Notification {
                    level: NotificationLevel::Warning,
                    message: state.t().copy_output_unavailable.to_string(),
                });
                return Vec::new();
            }
            vec![Effect::CopyUserShellOutput {
                directory: None,
                retained,
            }]
        }
        "selection" => {
            crate::conversation::interaction::ensure_plain(state);
            let text = crate::selection::extract_selected_text_with_layout(
                &state.conv.plain,
                &state.conv.plain_layout,
                &state.conv.selection,
            );
            if text.is_empty() {
                state.notification = Some(Notification {
                    level: NotificationLevel::Warning,
                    message: state.t().copy_selection_empty.to_string(),
                });
                return Vec::new();
            }
            copy_now(state, &text)
        }
        _ => {
            state.notification = Some(Notification {
                level: NotificationLevel::Warning,
                message: state.t().copy_usage.to_string(),
            });
            Vec::new()
        }
    }
}

/// `/output`: page the latest command's complete output.
///
/// A capture is handed to the pager as a file path, so even a 10 MiB result is
/// read on demand. Only a complete result that never had a capture is written
/// to a temporary file. A truncated tail with no capture is refused.
fn output_slash(state: &mut AppState) -> Vec<Effect> {
    use crate::action::PagerSource;
    use crate::transcript::TranscriptItem;
    let latest = state
        .transcript
        .items()
        .iter()
        .rev()
        .find_map(|item| match item {
            TranscriptItem::UserShell(shell) => Some((
                shell.output.clone(),
                shell.output_truncated,
                shell.full_output.clone(),
            )),
            _ => None,
        });
    let Some((retained, truncated, full_output)) = latest else {
        state.notification = Some(Notification {
            level: NotificationLevel::Warning,
            message: state.t().copy_no_shell.to_string(),
        });
        return Vec::new();
    };
    if let Some(directory) = full_output
        && let Ok(capture) = valid_capture(state, &directory)
    {
        return vec![Effect::OpenOutputPager {
            source: PagerSource::File(capture.combined),
        }];
    }
    // The capture was pruned, removed or outside the local state root. Fall
    // through only when the retained text still holds the whole result.
    if truncated {
        state.notification = Some(Notification {
            level: NotificationLevel::Warning,
            message: state.t().copy_output_unavailable.to_string(),
        });
        return Vec::new();
    }
    vec![Effect::OpenOutputPager {
        source: PagerSource::Text(crate::shell_render::copy_text(&retained)),
    }]
}

/// Validate a persisted capture path against the local state root.
fn valid_capture(state: &AppState, directory: &str) -> Result<crate::output_view::Capture, String> {
    crate::output_view::Capture::open(
        std::path::Path::new(directory),
        state.shell_output_root().as_deref(),
    )
}

/// Copy a small, already-clean string (a command, or a selection). Output
/// captures go through [`Effect::CopyUserShellOutput`] so a large file never
/// blocks this loop.
fn copy_now(state: &mut AppState, text: &str) -> Vec<Effect> {
    match crate::selection::copy_to_clipboard(text) {
        Ok(receipt) if !text.is_empty() => {
            let count = text.chars().count();
            state.notification = Some(Notification {
                level: NotificationLevel::Info,
                message: super::clipboard_notice(&receipt, count, state.locale),
            });
        }
        Ok(_) => {}
        Err(message) => {
            state.notification = Some(Notification {
                level: NotificationLevel::Warning,
                message,
            });
        }
    }
    Vec::new()
}

/// `/new`: start a fresh conversation.
///
/// This starts a NEW session and switches to it; the current one keeps its
/// transcript and checkpoints and stays in `/sessions`. Starting fresh loses
/// nothing, so there is nothing to confirm.
fn new_conversation(state: &mut AppState) -> Vec<Effect> {
    // Ask, do not act. Clearing the view here would show success before the
    // host has created anything — and if creation or config persistence
    // fails, the user is left staring at an empty screen with their old
    // conversation apparently gone. The switch happens when `SessionOpened`
    // arrives (see `runtime_apply`), which is the host confirming it.
    vec![Effect::Send(ClientCommand::NewSessionFor {
        requester_session_id: state.session_id.clone(),
    })]
}

/// `/export [path]`: write the visible conversation to a markdown file. Default
/// path is `<cwd>/leveler-chat-<timestamp>.md`; an argument overrides it. Only
/// the dialogue (user + assistant prose) is written — tool activity is skipped.
/// Expand a leading `~` / `~/` to the user's home. `~user` is not supported
/// (rare, and needs passwd lookup); anything else is returned verbatim.
fn expand_tilde(arg: &str) -> std::path::PathBuf {
    let home = || {
        leveler_core::environment()
            .var_os("HOME")
            .or_else(|| leveler_core::environment().var_os("USERPROFILE"))
    };
    if arg == "~"
        && let Some(h) = home()
    {
        return std::path::PathBuf::from(h);
    }
    if let Some(rest) = arg.strip_prefix("~/")
        && let Some(h) = home()
    {
        return std::path::PathBuf::from(h).join(rest);
    }
    std::path::PathBuf::from(arg)
}

fn export_conversation(state: &mut AppState, command: &str) -> Vec<Effect> {
    use crate::transcript::TranscriptItem;
    // Nothing to write if there is no actual dialogue yet — say so instead of
    // dropping an empty file.
    let has_dialogue = state.transcript.items().iter().any(|i| {
        matches!(
            i,
            TranscriptItem::User(_) | TranscriptItem::UserInput(_) | TranscriptItem::Assistant(_)
        )
    });
    if !has_dialogue {
        state.notification = Some(Notification {
            level: NotificationLevel::Warning,
            message: "没有可导出的内容".to_string(),
        });
        return Vec::new();
    }
    // Everything after the command word is the path, so paths with spaces
    // survive and the `/export` / `/save` aliases are both handled.
    let arg = command
        .split_once(char::is_whitespace)
        .map(|(_, rest)| rest)
        .unwrap_or("")
        .trim();
    let path: std::path::PathBuf = if arg.is_empty() {
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        leveler_core::environment()
            .current_dir()
            .join(format!("leveler-chat-{stamp}.md"))
    } else {
        // `~` is a literal char here (no shell), so expand it ourselves.
        expand_tilde(arg)
    };
    // Create missing parent dirs so `~/Desktop/notes/x.md` does not just fail.
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        let _ = std::fs::create_dir_all(parent);
    }
    let md = build_export_markdown(state);
    match std::fs::write(&path, md) {
        Ok(()) => {
            // A persistent transcript note keeps the path on screen (and
            // selectable) rather than a toast that vanishes in a few seconds.
            let msg = format!("已导出对话到 {}", path.display());
            state.transcript.push_note(msg.clone());
            state.notification = Some(Notification {
                level: NotificationLevel::Info,
                message: msg,
            });
        }
        Err(e) => {
            state.notification = Some(Notification {
                level: NotificationLevel::Warning,
                message: format!("导出失败: {e}"),
            });
        }
    }
    Vec::new()
}

/// Render the visible transcript to markdown: user turns and assistant answers
/// only, with `---` between turns. Tool activity, side questions (`/btw`), and
/// transient chrome are intentionally excluded.
fn build_export_markdown(state: &AppState) -> String {
    use crate::transcript::TranscriptItem;
    let mut out = String::from("# CodeLeveler 对话导出\n\n");
    out.push_str(&format!("- 会话: {}\n", state.session_id.as_str()));
    if let Some(repository) = state.repository.as_deref() {
        out.push_str(&format!("- 项目: {repository}\n"));
    }
    out.push('\n');
    for item in state.transcript.items() {
        match item {
            TranscriptItem::User(text) => {
                out.push_str("## 你\n\n");
                out.push_str(text.trim());
                out.push_str("\n\n");
            }
            TranscriptItem::UserInput(block) => {
                out.push_str("## 你\n\n");
                out.push_str(block.text.trim());
                out.push_str("\n\n");
            }
            TranscriptItem::Assistant(block) => {
                out.push_str("## 助手\n\n");
                out.push_str(block.text.trim());
                out.push_str("\n\n");
            }
            TranscriptItem::TurnEnd(_) => out.push_str("---\n\n"),
            _ => {}
        }
    }
    out
}

/// Start `/update`: open the panel and hand the work to the event loop, which
/// spawns the shared update service. The busy gate in `handle_slash` is what
/// keeps this from running under an active task.
fn start_update(state: &mut AppState) -> Vec<Effect> {
    state.update = Some(crate::update::UpdateView::new());
    vec![Effect::StartUpdate]
}

fn run_btw(state: &mut AppState, command: &str) -> Vec<Effect> {
    let question = command
        .strip_prefix("btw")
        .unwrap_or(command)
        .trim()
        .to_string();
    // `/btw` always lands on the side thread. With a question it starts one;
    // without, it re-opens the existing thread (never a second one) so a
    // follow-up does not require retyping the command or re-entering context.
    super::enter_btw(state);
    if question.is_empty() {
        return Vec::new();
    }
    if state.btw.generating {
        state.notification = Some(Notification {
            level: NotificationLevel::Warning,
            message: state.t().btw_busy.to_string(),
        });
        return Vec::new();
    }
    // No main user transcript and no main turn — a side question only.
    state.btw.scroll = 0;
    vec![Effect::Send(ClientCommand::Btw {
        session_id: state.session_id.clone(),
        question,
    })]
}

/// `/remember <text>`: a typed write to global memory. No model turn, no
/// prompt; the runtime stores the note and answers with the settled outcome.
fn remember_memory(state: &mut AppState, command: &str) -> Vec<Effect> {
    let content = command
        .strip_prefix("remember")
        .unwrap_or(command)
        .trim()
        .to_owned();
    if content.is_empty() {
        state.notification = Some(Notification {
            level: NotificationLevel::Warning,
            message: state.t().memory.remember_usage.to_string(),
        });
        return Vec::new();
    }
    let query_id = CommandId::generate();
    state.memory.action_query = Some(query_id.clone());
    vec![Effect::Send(ClientCommand::RememberMemory {
        session_id: state.session_id.clone(),
        query_id: Some(query_id),
        content,
    })]
}

/// `/dream [batch]`: consolidate pending observations now. The runtime decides
/// eligibility and owns the lease; the client only names the mode.
fn run_memory_dream(state: &mut AppState, command: &str) -> Vec<Effect> {
    let argument = command
        .strip_prefix("dream")
        .unwrap_or(command)
        .trim()
        .to_owned();
    let batch = if argument.is_empty() {
        false
    } else if argument.eq_ignore_ascii_case("batch") {
        true
    } else {
        state.notification = Some(Notification {
            level: NotificationLevel::Warning,
            message: state.t().memory.dream_usage.to_string(),
        });
        return Vec::new();
    };
    if state.memory.action_query.is_some() {
        state.notification = Some(Notification {
            level: NotificationLevel::Warning,
            message: state.t().memory.action_busy.to_string(),
        });
        return Vec::new();
    }
    let query_id = CommandId::generate();
    state.memory.action_query = Some(query_id.clone());
    state.notification = Some(Notification {
        level: NotificationLevel::Info,
        message: state.t().memory.action_pending.to_string(),
    });
    vec![Effect::Send(ClientCommand::RunMemoryDream {
        session_id: state.session_id.clone(),
        query_id: Some(query_id),
        batch,
    })]
}

/// `/flush`: run the pending capture and promote what it produced into the
/// visible index. The runtime's worker owns the model call.
fn run_memory_flush(state: &mut AppState) -> Vec<Effect> {
    if state.memory.action_query.is_some() {
        state.notification = Some(Notification {
            level: NotificationLevel::Warning,
            message: state.t().memory.action_busy.to_string(),
        });
        return Vec::new();
    }
    let query_id = CommandId::generate();
    state.memory.action_query = Some(query_id.clone());
    state.notification = Some(Notification {
        level: NotificationLevel::Info,
        message: state.t().memory.action_pending.to_string(),
    });
    vec![Effect::Send(ClientCommand::FlushMemory {
        session_id: state.session_id.clone(),
        query_id: Some(query_id),
    })]
}

fn set_collab_cmd(state: &mut AppState, command: &str) -> Vec<Effect> {
    let arg = command
        .split_whitespace()
        .nth(1)
        .unwrap_or("")
        .to_ascii_lowercase();
    if arg.is_empty() {
        open_collab_picker(state);
        return Vec::new();
    }
    apply_collab(state, &arg)
}

pub(super) fn apply_collab(state: &mut AppState, collab: &str) -> Vec<Effect> {
    if !matches!(collab, "chat" | "plan" | "goal") {
        state.notification = Some(Notification {
            level: NotificationLevel::Warning,
            message: state.t().collab_usage.to_string(),
        });
        return Vec::new();
    }
    state.collaboration = collab.to_string();
    // `plan` is a runtime-derived read-only TOOL overlay: `collaboration=plan`
    // makes the runtime compute `read_only`. It does NOT change the permission
    // profile, and the TUI must not fabricate one it never sent.
    let message = if collab == "plan" {
        "协作=计划（只读）。确认后用 /collab goal 或 /goal <任务> 开始执行".to_string()
    } else {
        format!("协作 → {collab}")
    };
    state.notification = Some(Notification {
        level: NotificationLevel::Info,
        message,
    });
    vec![Effect::Send(ClientCommand::SetProductAxes {
        session_id: state.session_id.clone(),
        work_profile: "single".into(), // Legacy wire field; no product axis.
        collaboration: state.collaboration.clone(),
    })]
}

/// Expand `~/…` repository display paths for filesystem discovery.
fn skill_root(state: &AppState) -> Option<std::path::PathBuf> {
    let raw = state.repository.as_deref()?;
    if let Some(rest) = raw.strip_prefix("~/")
        && let Some(home) = leveler_core::environment()
            .var_os("HOME")
            .or_else(|| leveler_core::environment().var_os("USERPROFILE"))
    {
        return Some(std::path::PathBuf::from(home).join(rest));
    }
    Some(std::path::PathBuf::from(raw))
}

/// Rescan project + user skills when the root changes (or first `$` keystroke).
pub(super) fn refresh_skill_catalog(state: &mut AppState) {
    let Some(root) = skill_root(state) else {
        state.skill_catalog.clear();
        state.skill_catalog_root = None;
        return;
    };
    let key = root.display().to_string();
    if state.skill_catalog_root.as_deref() == Some(key.as_str()) {
        return;
    }
    state.skill_catalog = leveler_skills::discover(&root)
        .into_iter()
        // Keep only names a `$name` mention can spell.
        .filter(|s| looks_like_unknown_slash_command(&s.name))
        .map(|s| (s.name, s.description))
        .collect();
    state.skill_catalog_root = Some(key);
}

/// `/skills` lists the resolved skill registry; `/skills <name>` shows one
/// entry. Read-only, and local: it reads the same registry `$name` and
/// `load_skill` use, so the listing cannot drift from what loads.
fn skills_slash(state: &mut AppState, command: &str) -> Vec<Effect> {
    let registry = match skill_root(state) {
        Some(root) => leveler_skills::describe(&root),
        None => leveler_skills::SkillRegistry::load(&leveler_skills::SkillRoots::empty()),
    };
    let name = command.strip_prefix("skills").unwrap_or(command).trim();
    let t = state.t();
    let note = if name.is_empty() {
        crate::skills_view::listing_note(&registry, t)
    } else {
        crate::skills_view::detail_note(&registry, name, t)
    };
    state.transcript.push_note(note);
    Vec::new()
}

/// `/agents` lists the agents this project resolves; `/agents <name>` shows one
/// definition. Read-only: the TUI does not write agent files.
fn agents_slash(state: &mut AppState, command: &str) -> Vec<Effect> {
    let name = command.strip_prefix("agents").unwrap_or(command).trim();
    if name.is_empty() || name == "list" {
        let query_id = leveler_client_protocol::CommandId::generate();
        state.agents_list_query = Some(query_id.clone());
        return vec![Effect::Send(ClientCommand::ListAgents {
            session_id: state.session_id.clone(),
            query_id: Some(query_id),
        })];
    }
    let query_id = leveler_client_protocol::CommandId::generate();
    state.agent_detail_query = Some(query_id.clone());
    vec![Effect::Send(ClientCommand::GetAgent {
        session_id: state.session_id.clone(),
        name: name.to_string(),
        query_id: Some(query_id),
    })]
}

/// `/goal <task>` starts a goal turn; `/goal status` and `/goal clear` manage it.
fn run_goal(state: &mut AppState, command: &str) -> Vec<Effect> {
    let rest = command
        .strip_prefix("goal")
        .unwrap_or(command)
        .trim()
        .to_string();
    let head = rest
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match head.as_str() {
        "" => {
            state.notification = Some(Notification {
                level: NotificationLevel::Warning,
                message: state.t().goal_usage.to_string(),
            });
            Vec::new()
        }
        "status" => {
            goal_status(state);
            Vec::new()
        }
        "clear" | "cancel" | "stop" => clear_goal(state),
        _ => {
            if turn_input_held(state) {
                // The slash parser already took the composer; hand it back.
                state.composer.replace(format!("/goal {rest}"));
                return Vec::new();
            }
            if state.is_busy() {
                // Steer the running turn, same as an ordinary message: the
                // runtime falls back to a fresh submission if it already ended.
                let content = format!("/goal {rest}");
                state.transcript.push_user_if_new(content.clone());
                state.notification = Some(Notification {
                    level: NotificationLevel::Info,
                    message: state.t().steering_sent.to_string(),
                });
                let command = ClientCommand::SteerCurrentTurn {
                    session_id: state.session_id.clone(),
                    content,
                    // A goal declaration has its own durable identity; it is
                    // not one of the accepted-input journal rows.
                    message_id: None,
                };
                return submit_turn_input(state, command);
            }
            state.transcript.push_user_if_new(rest.clone());
            state.staged_goal = Some(crate::active_goal::StagedGoal {
                title: crate::active_goal::short_title(&rest),
                objective: Some(rest.clone()),
                continuation: false,
            });
            start_turn(state);
            state.goal_mode_active = true;
            // Mirror the durable axis the runtime writes for `/goal <task>`:
            // the session is now a goal session, not a one-turn override, so
            // the status chip and `/goal clear` read the same value the row
            // holds. The runtime is the authority; this is the client's view.
            state.collaboration = "goal".into();
            let command = ClientCommand::RunGoal {
                session_id: state.session_id.clone(),
                content: rest,
                require_new_turn: false,
            };
            submit_turn_input(state, command)
        }
    }
}

fn goal_status(state: &mut AppState) {
    let t = state.t();
    let message = if state.goal_mode_active || state.collaboration == "goal" {
        let phase = if state.is_busy() {
            t.goal_status_busy
        } else {
            t.goal_status_waiting
        };
        t.goal_status_active
            .replacen("{}", &state.collaboration, 1)
            .replacen("{}", &state.mode_label, 1)
            .replacen("{}", phase, 1)
    } else {
        t.goal_status_idle
            .replacen("{}", &state.collaboration, 1)
            .replacen("{}", &state.mode_label, 1)
    };
    state.notification = Some(Notification {
        level: NotificationLevel::Info,
        message,
    });
}

fn clear_goal(state: &mut AppState) -> Vec<Effect> {
    let was_busy_goal = state.is_busy() && state.goal_mode_active;
    let flipped_collab = state.collaboration == "goal";
    state.goal_mode_active = false;
    // An explicit clear is one of the two ways the Active Goal leaves the
    // header besides completion (the other is a new goal replacing it).
    state.active_goal = None;
    state.staged_goal = None;
    if flipped_collab {
        state.collaboration = "chat".into();
    }
    let t = state.t();
    let mut effects = Vec::new();
    if flipped_collab {
        effects.push(Effect::Send(ClientCommand::SetProductAxes {
            session_id: state.session_id.clone(),
            work_profile: "single".into(), // Legacy wire field; no product axis.
            collaboration: state.collaboration.clone(),
        }));
    }
    // `/goal cancel` is the explicit-task-cancellation entry: it cancels the
    // logical task, not just the current window, so a later `继续` cannot
    // reopen it. With nothing to cancel it is only a goal-mode clear.
    //
    // The runtime's projection of owed work outranks this client's two busy
    // flags: between submitting a continuation and receiving `TurnStarted`,
    // `goal_mode_active` and `resumable_task` can both be false while the
    // durable task still owes the goal. The runtime arbitrates a cancel that
    // finds nothing left to end.
    if was_busy_goal || state.resumable_task || state.execution_task_active {
        state.notification = Some(Notification {
            level: NotificationLevel::Info,
            message: t.goal_cleared_and_cancel.to_string(),
        });
        effects.push(Effect::Send(ClientCommand::CancelTask {
            session_id: state.session_id.clone(),
        }));
    } else {
        state.notification = Some(Notification {
            level: NotificationLevel::Info,
            message: t.goal_cleared.to_string(),
        });
    }
    effects
}

/// Route `!command`: enters input history (like slash commands), never the
/// conversation or the model. Its execution block joins the conversation
/// timeline when the runtime reports it started.
fn submit_user_shell(state: &mut AppState, cmd: String) -> Vec<Effect> {
    if cmd.trim().is_empty() {
        // Empty command: keep the mode and the draft so the user can continue
        // typing; just say what is missing.
        state.notification = Some(crate::state::Notification {
            level: leveler_client_protocol::NotificationLevel::Info,
            message: state.t().user_shell_empty_hint.to_string(),
        });
        return Vec::new();
    }
    if state.is_busy() {
        // Fast local hint; the runtime enforces the same mutex.
        state.notification = Some(crate::state::Notification {
            level: leveler_client_protocol::NotificationLevel::Warning,
            message: state.t().user_shell_busy_hint.to_string(),
        });
        return Vec::new();
    }
    if state.running_user_shell().is_some() {
        // The runtime admits one foreground user shell per session. Refuse
        // locally rather than lose the typed command to a rejected RPC.
        state.notification = Some(crate::state::Notification {
            level: leveler_client_protocol::NotificationLevel::Warning,
            message: state.t().user_shell_running_hint.to_string(),
        });
        return Vec::new();
    }
    // History keeps the mode marker (`! command`), so recalling it restores
    // Shell Mode; the executed command is exactly what the user typed.
    state.composer.take_shell();
    super::shell::reset_after_submit(state);
    // The command and its result are transcript rows now. A reader who had
    // scrolled into history must see the row it just created and its output.
    crate::conversation::interaction::jump_to_live_edge(state);
    vec![Effect::Send(
        leveler_client_protocol::ClientCommand::RunUserShell {
            session_id: state.session_id.clone(),
            command: cmd.trim().to_string(),
        },
    )]
}

#[cfg(test)]
mod export_tests {
    use super::*;
    use crate::state::Boot;
    use crate::theme::Theme;
    use leveler_client_protocol::{MessageId, SessionId};

    fn state_with_dialogue() -> AppState {
        let mut s = AppState::new(
            Theme::no_color(),
            Boot {
                session_id: SessionId::new("s-export"),
                user: "u".into(),
                version: "0.1.0".into(),
                show_welcome: false,
                draft_path: None,
                history_path: None,
                context_window: 200_000,
                locale: crate::i18n::Locale::Zh,
                untrusted_config: Vec::new(),
                model_notice: None,
                thinking: None,
            },
        );
        s.transcript.push_user("帮我加个导出功能".into());
        let id = MessageId::new("m1");
        s.transcript.begin_assistant(id.clone());
        s.transcript
            .append_assistant(&id, "好的，已经加上 /export 了。");
        s.transcript.finish_assistant(&id);
        s
    }

    #[test]
    fn markdown_has_dialogue_and_omits_non_dialogue() {
        let s = state_with_dialogue();
        let md = build_export_markdown(&s);
        assert!(md.contains("# CodeLeveler 对话导出"), "{md}");
        assert!(md.contains("## 你\n\n帮我加个导出功能"), "{md}");
        assert!(
            md.contains("## 助手\n\n好的，已经加上 /export 了。"),
            "{md}"
        );
    }

    #[test]
    fn export_with_explicit_path_writes_the_file() {
        let mut s = state_with_dialogue();
        let path =
            std::env::temp_dir().join(format!("leveler-export-test-{}.md", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let effects = export_conversation(&mut s, &format!("export {}", path.display()));
        assert!(
            effects.is_empty(),
            "export is a local action, no runtime effect"
        );
        let written = std::fs::read_to_string(&path).expect("file should exist");
        assert!(written.contains("帮我加个导出功能"), "{written}");
        assert!(
            matches!(&s.notification, Some(n) if n.message.contains("已导出")),
            "should notify success"
        );
        // The path is also appended as a persistent transcript note.
        assert!(
            s.transcript.items().iter().any(|i| matches!(
                i,
                crate::transcript::TranscriptItem::Note(t) if t.contains("已导出")
            )),
            "export path should persist as a transcript note"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_alias_writes_to_the_given_path() {
        let mut s = state_with_dialogue();
        let path =
            std::env::temp_dir().join(format!("leveler-save-alias-{}.md", std::process::id()));
        let _ = std::fs::remove_file(&path);
        // The `/save` alias must strip its own command word, not leave "save" in
        // the path.
        export_conversation(&mut s, &format!("save {}", path.display()));
        assert!(
            std::fs::read_to_string(&path).is_ok(),
            "the /save alias should write to the given path"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn expand_tilde_passes_through_non_tilde_paths() {
        assert_eq!(
            expand_tilde("/abs/x.md"),
            std::path::PathBuf::from("/abs/x.md")
        );
        assert_eq!(
            expand_tilde("rel/x.md"),
            std::path::PathBuf::from("rel/x.md")
        );
    }

    #[test]
    fn export_accepts_a_path_with_spaces() {
        let mut s = state_with_dialogue();
        let dir = std::env::temp_dir().join(format!("leveler export dir {}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("my notes.md");
        let _ = std::fs::remove_file(&path);
        export_conversation(&mut s, &format!("export {}", path.display()));
        assert!(
            std::fs::read_to_string(&path).is_ok(),
            "a path with spaces must be written verbatim, not truncated"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn export_with_no_dialogue_reports_nothing() {
        let mut s = AppState::new(
            Theme::no_color(),
            Boot {
                session_id: SessionId::new("s-empty"),
                user: "u".into(),
                version: "0.1.0".into(),
                show_welcome: false,
                draft_path: None,
                history_path: None,
                context_window: 200_000,
                locale: crate::i18n::Locale::Zh,
                untrusted_config: Vec::new(),
                model_notice: None,
                thinking: None,
            },
        );
        let effects = export_conversation(&mut s, "export");
        assert!(effects.is_empty());
        assert!(
            matches!(&s.notification, Some(n) if n.message.contains("没有可导出的内容")),
            "empty transcript should report nothing to export"
        );
    }

    #[test]
    fn web_slash_launches_once_then_re_surfaces_the_url() {
        let mut s = state_with_dialogue();

        // First /web asks the edge to start the server and arms the guard.
        assert_eq!(handle_slash(&mut s, "web"), vec![Effect::StartWeb]);
        assert!(s.web_starting);
        assert!(s.web_url.is_none());

        // A second /web while it is still starting is a no-op (no double bind).
        assert!(handle_slash(&mut s, "web").is_empty());

        // The launch completes: URL is stored and the guard clears.
        let effects = crate::reducer::reduce(
            &mut s,
            crate::action::Action::WebLaunched(Ok("http://127.0.0.1:9/?token=abc".to_string())),
        );
        assert!(!s.web_starting);
        assert_eq!(s.web_url.as_deref(), Some("http://127.0.0.1:9/?token=abc"));
        assert!(
            matches!(&s.notification, Some(n) if n.message.contains("Web UI 已启动") && !n.message.contains("浏览器打开")),
            "server readiness must not claim that the browser opener has completed"
        );
        assert_eq!(
            effects,
            vec![Effect::OpenWebUrl(
                "http://127.0.0.1:9/?token=abc".to_string()
            )],
            "the first /web must use the same opener path as later invocations"
        );

        // Now /web re-surfaces the running URL AND re-opens the browser —
        // "already running" must not strand the user with a bare URL to copy.
        assert_eq!(
            handle_slash(&mut s, "web"),
            vec![Effect::OpenWebUrl(
                "http://127.0.0.1:9/?token=abc".to_string()
            )],
            "re-invoking /web must re-open the browser, not just print the URL"
        );
        assert!(
            matches!(&s.notification, Some(n) if n.message.contains("已在运行")),
            "should report the already-running URL"
        );
    }

    #[test]
    fn web_launch_failure_clears_guard_and_warns() {
        let mut s = state_with_dialogue();
        s.web_starting = true;
        crate::reducer::reduce(
            &mut s,
            crate::action::Action::WebLaunched(Err("boom".to_string())),
        );
        assert!(!s.web_starting);
        assert!(s.web_url.is_none(), "a failed launch must not record a URL");
        assert!(
            matches!(&s.notification, Some(n) if n.message.contains("启动失败")),
            "should warn on failure"
        );
    }

    #[test]
    fn url_open_completion_reports_the_host_result() {
        let mut s = state_with_dialogue();
        let url = "https://example.com/".to_string();

        crate::reducer::reduce(
            &mut s,
            crate::action::Action::UrlOpened {
                url: url.clone(),
                result: Ok(()),
            },
        );
        assert!(
            matches!(&s.notification, Some(n) if n.message == format!("系统已接受 URL，并交给默认浏览器：{url}"))
        );

        crate::reducer::reduce(
            &mut s,
            crate::action::Action::UrlOpened {
                url: url.clone(),
                result: Err("open failed".to_string()),
            },
        );
        assert!(
            matches!(&s.notification, Some(n) if n.message.contains("无法在浏览器打开") && n.message.contains("open failed"))
        );
    }
}

/// `/copy diff` — the complete applied patch, and never a request.
#[cfg(test)]
mod copy_diff_tests {
    use super::*;
    use crate::state::Boot;
    use crate::theme::Theme;
    use leveler_client_protocol::{SessionId, ToolCallId};

    fn state() -> AppState {
        let mut s = AppState::new(
            Theme::no_color(),
            Boot {
                session_id: SessionId::new("s-copy-diff"),
                user: "u".into(),
                version: "0.1.0".into(),
                show_welcome: false,
                draft_path: None,
                history_path: None,
                context_window: 200_000,
                locale: crate::i18n::Locale::Zh,
                untrusted_config: Vec::new(),
                model_notice: None,
                thinking: None,
            },
        );
        s.transcript.push_user("change the file".into());
        s
    }

    fn whole_file_diff(path: &str, before: &str, after: &str) -> String {
        let hunks = leveler_tools::tools::applied_diff::whole_file_hunks(before, after);
        leveler_tools::tools::applied_diff::unified_diff(path, &hunks).expect("a real applied diff")
    }

    fn write(s: &mut AppState, id: &str, path: &str, after: &str, applied: Option<String>) {
        s.transcript.push_tool_started(
            ToolCallId::new(id),
            "write_file".into(),
            serde_json::json!({ "path": path, "content": after }).to_string(),
            false,
            0,
            Some(0),
            None,
        );
        s.transcript.complete_tool(
            &ToolCallId::new(id),
            true,
            format!("Wrote {path}"),
            3,
            applied,
        );
    }

    const SRC: &str = "fn main() {\n    let a = 1;\n}\n";

    #[test]
    fn copy_diff_returns_the_applied_unified_diff_verbatim() {
        let mut s = state();
        let diff = whole_file_diff("src/main.rs", "", SRC);
        write(&mut s, "e1", "src/main.rs", SRC, Some(diff.clone()));
        let copied = latest_applied_patch(&s).expect("the applied patch");
        assert_eq!(copied.trim_end(), diff.trim_end());
        assert!(copied.starts_with("--- "), "{copied:?}");
        assert!(copied.contains("+++ b/src/main.rs"), "{copied:?}");
        assert!(copied.contains("@@ "), "{copied:?}");
    }

    #[test]
    fn copy_diff_refuses_an_edit_that_reported_no_patch() {
        let mut s = state();
        // A successful write whose runtime reported no diff — the no-change
        // write. The request exists; it is not an applied change.
        write(&mut s, "e1", "src/main.rs", "fn untouched() {}\n", None);
        assert!(latest_applied_patch(&s).is_none());
    }

    #[test]
    fn copy_diff_ignores_a_failed_edit_that_never_landed() {
        let mut s = state();
        s.transcript.push_tool_started(
            ToolCallId::new("e1"),
            "write_file".into(),
            serde_json::json!({ "path": "src/main.rs", "content": SRC }).to_string(),
            false,
            0,
            Some(0),
            None,
        );
        s.transcript
            .complete_tool(&ToolCallId::new("e1"), false, "denied".into(), 3, None);
        assert!(latest_applied_patch(&s).is_none());
    }

    #[test]
    fn copy_diff_takes_the_most_recent_edit() {
        let mut s = state();
        write(
            &mut s,
            "e1",
            "src/one.rs",
            "first\n",
            Some(whole_file_diff("src/one.rs", "", "first\n")),
        );
        s.transcript.push_note("sealed".into());
        write(
            &mut s,
            "e2",
            "src/two.rs",
            "second\n",
            Some(whole_file_diff("src/two.rs", "", "second\n")),
        );
        let copied = latest_applied_patch(&s).expect("the newest patch");
        assert!(copied.contains("src/two.rs"), "{copied:?}");
        assert!(!copied.contains("src/one.rs"), "{copied:?}");
    }

    /// One node that merged several snapshots of the same file copies every
    /// member's own patch, in paint order — never a stitched hunk.
    #[test]
    fn copy_diff_of_a_merged_node_keeps_each_patch_separate() {
        let mut s = state();
        let first = whole_file_diff("src/main.rs", "", "one\n");
        let second = whole_file_diff("src/main.rs", "one\n", "two\n");
        write(&mut s, "e1", "src/main.rs", "one\n", Some(first.clone()));
        write(&mut s, "e2", "src/main.rs", "two\n", Some(second.clone()));
        let copied = latest_applied_patch(&s).expect("a patch set");
        assert_eq!(
            copied,
            format!("{}\n{}", first.trim_end(), second.trim_end())
        );
    }

    #[test]
    fn the_usage_text_names_every_argument_it_accepts() {
        for locale in [crate::i18n::Locale::Zh, crate::i18n::Locale::En] {
            let usage = locale.text().copy_usage;
            for arg in ["diff", "output", "command", "selection"] {
                assert!(
                    usage.contains(arg),
                    "{locale:?} usage misses {arg}: {usage}"
                );
            }
        }
    }
}

/// `/goal cancel` must reach the runtime whenever the session still owes a
/// goal. The client's own busy flags are a view, not the authority; between
/// submitting a continuation and receiving `TurnStarted`, a keystroke can find
/// `goal_mode_active` and `resumable_task` both false while the durable task
/// still owes the goal.
#[cfg(test)]
mod goal_clear_tests {
    use super::*;
    use crate::state::Boot;
    use crate::theme::Theme;
    use leveler_client_protocol::{RuntimeStatus, SessionId};

    fn state() -> AppState {
        AppState::new(
            Theme::no_color(),
            Boot {
                session_id: SessionId::new("s-goal-clear"),
                user: "u".into(),
                version: "0.1.0".into(),
                show_welcome: false,
                draft_path: None,
                history_path: None,
                context_window: 200_000,
                locale: crate::i18n::Locale::Zh,
                untrusted_config: Vec::new(),
                model_notice: None,
                thinking: None,
            },
        )
    }

    #[test]
    fn cancelling_a_goal_that_is_still_owed_reaches_the_runtime() {
        let mut s = state();
        // The runtime projected an owed goal; a continuation turn has been
        // submitted, so the client is busy, but its two cancel flags have not
        // converged yet.
        s.execution_task_active = true;
        s.resumable_task = false;
        s.goal_mode_active = false;
        s.status = RuntimeStatus::Busy;

        let effects = clear_goal(&mut s);

        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::Send(ClientCommand::CancelTask { .. }))),
            "`/goal cancel` must cancel the logical task, not clear a local mode: {effects:?}"
        );
    }
}
