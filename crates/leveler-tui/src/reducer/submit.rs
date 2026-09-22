use leveler_client_protocol::{ClientCommand, CommandId, NotificationLevel};

use crate::action::Effect;
use crate::screen::{BusyPolicy, Screen};
use crate::state::{AppState, Notification, PendingSubmission, WorkbenchFocus};

use super::overlay_keys::{
    apply_theme_id, open_checkpoint_picker, open_collab_picker, open_mode_picker,
    open_model_picker, open_theme_picker, open_unsupported_media, open_work_mode_picker,
};
use super::runtime_apply::start_turn;
use super::screen_nav::{
    open_clean, open_context, open_diff_screen, open_sessions_screen, open_trace, toggle_screen,
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
    crate::suggestion::dismiss(state);
    crate::away_summary::cancel(state);
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
    // User shell escape: the RAW composer's first character is `!` — no
    // leading-whitespace trim, so " !cargo test" stays a normal message and
    // ordinary prose can never execute. Never reaches the model.
    if state.composer.text().starts_with('!') {
        // Command content comes from the canonical form so a pasted script
        // runs as pasted, not as its `[Pasted: N lines]` chip (R004 F1).
        let canonical = state.composer.canonical_text();
        let cmd = canonical
            .strip_prefix('!')
            .unwrap_or(&canonical)
            .to_string();
        return submit_user_shell(state, cmd);
    }
    let text = state.composer.text().trim().to_string();
    // A picture is a message. Only an empty composer with nothing staged is
    // not one.
    if text.is_empty() && state.pending_attachments.is_empty() {
        return Vec::new();
    }
    // A FIRST LINE starting with a KNOWN `/command` is a local slash command,
    // parsed locally and never sent to the model — its argument may span
    // multiple lines (a small pasted goal below the chip threshold must not
    // silently degrade `/goal` into a chat message; R004 F1-adjacent). The
    // unknown-`/xxx` guard keeps its single-line scope so a typo or path-like
    // multiline message is never swallowed.
    if let Some(rest) = text.strip_prefix('/') {
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
            // Detection above stays on the raw single-line presentation.
            let expanded = state.composer.take();
            let expanded = expanded.trim();
            let rest_expanded = expanded.strip_prefix('/').unwrap_or(expanded);
            return handle_slash(state, rest_expanded.trim());
        }
        // Reserve unknown-command feedback for SINGLE-LINE command-shaped
        // typos such as `/hlep`. Absolute paths (`/Users/...`), file names,
        // and multiline slash-prefixed prose are ordinary messages.
        if !text.contains('\n') && looks_like_unknown_slash_command(name) {
            state.notification = Some(Notification {
                level: NotificationLevel::Warning,
                message: format!("未知命令: /{name}（内容已保留，/help 查看命令）"),
            });
            return Vec::new();
        }
    }
    if state.is_busy() {
        // Hold it in 待发送 rather than sending it: the user decides which
        // input steers the running turn, and when. Nothing unsent is shown in
        // the conversation. Once this turn reaches its terminal event and the
        // runtime is ready again, `drain_pending_input` sends it as the next
        // turn — the queue is a continuation queue, not a draft box.
        let text = state.composer.take().trim().to_string();
        if !text.is_empty() {
            state
                .pending_inputs
                .push(crate::pending_inputs::PendingInput::queued(
                    text,
                    state.session_id.clone(),
                ));
            state.pending_selected = state.pending_inputs.len() - 1;
        }
        return Vec::new();
    }
    // Vision gate: block sending images to a non-vision model until the user
    // chooses how to proceed (spec §42). Handled before the request is built.
    if !state.pending_attachments.is_empty() && !state.vision {
        open_unsupported_media(state);
        return Vec::new();
    }
    send_message(state)
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
    // Stage the turn's goal identity before it flips to Busy. A continuation
    // phrase re-enters the goal already shown (and the runtime's own resume
    // path); any other input opens a new one.
    let continuation =
        attachments.is_empty() && leveler_client_protocol::parse_continuation(&content).is_some();
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

/// The command a user's turn input becomes.
///
/// A continuation phrase (`继续`, `继续，但是先不要跑测试`, `go on`, …) is sent as
/// [`ClientCommand::ResumeTask`] so the runtime can re-enter the session's
/// logical task through its resume path. The runtime remains the authority on
/// whether a resumable task exists: when none does, it treats the same text as
/// an ordinary message. Attachments stay ordinary messages — a continuation is
/// text.
fn turn_input_command(
    state: &AppState,
    content: String,
    attachments: Vec<leveler_client_protocol::AttachmentRef>,
) -> ClientCommand {
    if attachments.is_empty() && leveler_client_protocol::parse_continuation(&content).is_some() {
        return ClientCommand::ResumeTask {
            session_id: state.session_id.clone(),
            content,
        };
    }
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
    if turn_input_held(state) {
        return Vec::new();
    }
    let command = if state.is_busy() {
        ClientCommand::SteerCurrentTurn {
            session_id: state.session_id.clone(),
            content,
        }
    } else {
        // The runtime announces an admitted turn's message itself
        // (`UserMessageAdded`); only the optimistic Busy is ours to set.
        let continuation = leveler_client_protocol::parse_continuation(&content).is_some();
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
    if !state.runtime_connected || state.is_busy() {
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
        state.t().commands_disabled_disconnected
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
    if crate::screen::file_mention_query(state).is_some()
        && !state.file_index_requested
        && !state.repository.is_empty()
    {
        state.file_index_requested = true;
        vec![Effect::LoadFileCandidates {
            repository: state.repository.clone(),
        }]
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

/// `/remote` — make this machine reachable from a phone.
///
/// `/remote-loc` is the same thing with the relay bound to this machine's own
/// network address instead of one on the internet, so a phone on the same
/// Wi-Fi can reach it — the difference between "works once you rent a server"
/// and "works now".
///
/// A separate name rather than an argument to `/remote`: as `/remote loc` the
/// local path was one typo away from being taken for the real command, on a
/// command whose whole job is to open this machine to a network. It stays out
/// of the command list — it is a testing and same-network path, not the shape
/// the product is aiming at.
fn start_remote(state: &mut AppState, local: bool) -> Vec<Effect> {
    state.notification = Some(Notification {
        level: NotificationLevel::Info,
        message: if local {
            "正在把这台机器开放给同一 Wi-Fi 下的手机…".to_string()
        } else {
            "正在生成配对二维码…".to_string()
        },
    });
    vec![Effect::StartRemote { local }]
}

/// Handle a `/command` typed in the composer.
fn handle_slash(state: &mut AppState, command: &str) -> Vec<Effect> {
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
        "goal" => run_goal(state, command),
        "develop" => run_develop(state, command),
        "btw" => run_btw(state, command),
        "trace" => {
            let id = command.split_whitespace().nth(1).map(str::to_string);
            open_trace(state, id)
        }
        "context" => open_context(state),
        "clean" => open_clean(state),
        "work-mode" => set_work_mode(state, command),
        "collab" => set_collab_cmd(state, command),
        "memory" => memory_slash(state, command),
        "agents" => agents_slash(state, command),
        "skills" => skills_slash(state, command),
        "remember" => remember_slash(state, command),
        "web" => start_web(state),
        "remote" => start_remote(state, false),
        "remote-loc" => start_remote(state, true),
        "diff" => open_diff_screen(state),
        "sessions" => open_sessions_screen(state),
        "restore" => {
            open_checkpoint_picker(state);
            Vec::new()
        }
        // The runtime copies record + transcript into a fresh session and
        // leaves this one open, so there is nothing to confirm here.
        "fork" => vec![Effect::Send(ClientCommand::ForkSession {
            session_id: state.session_id.clone(),
        })],
        "compact" => vec![Effect::Send(ClientCommand::CompactContext {
            session_id: state.session_id.clone(),
        })],
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
    let has_dialogue = state
        .transcript
        .items()
        .iter()
        .any(|i| matches!(i, TranscriptItem::User(_) | TranscriptItem::Assistant(_)));
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
    if !state.repository.is_empty() {
        out.push_str(&format!("- 项目: {}\n", state.repository));
    }
    out.push('\n');
    for item in state.transcript.items() {
        match item {
            TranscriptItem::User(text) => {
                out.push_str("## 你\n\n");
                out.push_str(text.trim());
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

fn set_work_mode(state: &mut AppState, command: &str) -> Vec<Effect> {
    let arg = command
        .split_whitespace()
        .nth(1)
        .unwrap_or("")
        .to_ascii_lowercase();
    if arg.is_empty() {
        open_work_mode_picker(state);
        return Vec::new();
    }
    apply_work_profile(state, &arg)
}

pub(super) fn apply_work_profile(state: &mut AppState, arg: &str) -> Vec<Effect> {
    // Only profiles with distinct runtime behavior are user-selectable. The
    // legacy `delivery` value had none (it was identical to `balanced`).
    if !matches!(arg, "economy" | "balanced") {
        state.notification = Some(Notification {
            level: NotificationLevel::Warning,
            message: state.t().work_mode_usage.to_string(),
        });
        return Vec::new();
    }
    state.work_profile = arg.to_string();
    state.notification = Some(Notification {
        level: NotificationLevel::Info,
        message: format!("work-mode → {arg}"),
    });
    vec![Effect::Send(ClientCommand::SetProductAxes {
        session_id: state.session_id.clone(),
        work_profile: state.work_profile.clone(),
        collaboration: state.collaboration.clone(),
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
        work_profile: state.work_profile.clone(),
        collaboration: state.collaboration.clone(),
    })]
}

/// Expand `~/…` repository display paths for filesystem discovery.
fn skill_root(state: &AppState) -> std::path::PathBuf {
    if state.repository.is_empty() {
        return leveler_core::environment().current_dir().to_path_buf();
    }
    let raw = state.repository.as_str();
    if let Some(rest) = raw.strip_prefix("~/")
        && let Some(home) = leveler_core::environment()
            .var_os("HOME")
            .or_else(|| leveler_core::environment().var_os("USERPROFILE"))
    {
        return std::path::PathBuf::from(home).join(rest);
    }
    std::path::PathBuf::from(raw)
}

/// Rescan project + user skills when the root changes (or first `$` keystroke).
pub(super) fn refresh_skill_catalog(state: &mut AppState) {
    let root = skill_root(state);
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
    let root = skill_root(state);
    let registry = leveler_skills::describe(&root);
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
        return vec![Effect::Send(ClientCommand::ListAgents {
            session_id: state.session_id.clone(),
            query_id: None,
        })];
    }
    vec![Effect::Send(ClientCommand::GetAgent {
        session_id: state.session_id.clone(),
        name: name.to_string(),
        query_id: None,
    })]
}

/// `/memory` — list active (+archived); `/memory forget <id>` archives.
fn memory_slash(state: &mut AppState, command: &str) -> Vec<Effect> {
    let rest = command.strip_prefix("memory").unwrap_or(command).trim();
    if rest.is_empty() || rest == "list" {
        return vec![Effect::Send(ClientCommand::ListMemory {
            session_id: state.session_id.clone(),
            include_archived: true,
        })];
    }
    // Accepting a pending candidate is the user's consent (K36) — the reason
    // the runtime never lets the model do it.
    if let Some(id) = rest.strip_prefix("accept").map(str::trim) {
        if id.is_empty() {
            state.notification = Some(Notification {
                level: NotificationLevel::Warning,
                message: state.t().memory_accept_usage.to_string(),
            });
            return Vec::new();
        }
        return vec![Effect::Send(ClientCommand::AcceptMemory {
            session_id: state.session_id.clone(),
            id: id.to_string(),
        })];
    }
    // Rejecting a candidate is consent WITHHELD. It is a different operation
    // from forgetting an active entry, and conflating them meant a pending id
    // sent to forget did nothing at all.
    if let Some(id) = rest.strip_prefix("reject").map(str::trim) {
        if id.is_empty() {
            state.notification = Some(Notification {
                level: NotificationLevel::Warning,
                message: state.t().memory_reject_usage.to_string(),
            });
            return Vec::new();
        }
        return vec![Effect::Send(ClientCommand::RejectMemory {
            session_id: state.session_id.clone(),
            id: id.to_string(),
        })];
    }
    if let Some(id) = rest.strip_prefix("forget").map(str::trim) {
        if id.is_empty() {
            state.notification = Some(Notification {
                level: NotificationLevel::Warning,
                message: state.t().memory_forget_usage.to_string(),
            });
            return Vec::new();
        }
        return vec![Effect::Send(ClientCommand::ForgetMemory {
            session_id: state.session_id.clone(),
            id: id.to_string(),
        })];
    }
    state.notification = Some(Notification {
        level: NotificationLevel::Info,
        message: state.t().memory_usage.to_string(),
    });
    Vec::new()
}

/// `/remember <text>` — the user's own direct write.
///
/// It never reaches the model and never starts a turn: the command IS the
/// authorization. That is the whole reason it exists next to the
/// natural-language path, which can only ever propose, because a prefix parser
/// must not silently rewrite someone's long-term state.
///
/// Defaults to `preference` because "remember this" almost always means "apply
/// it from now on", and only preferences are injected every turn.
fn remember_slash(state: &mut AppState, command: &str) -> Vec<Effect> {
    let rest = command.strip_prefix("remember").unwrap_or(command).trim();
    let (kind, body) = parse_remember_kind(rest);
    let body = body.trim();
    if body.is_empty() {
        state.notification = Some(Notification {
            level: NotificationLevel::Warning,
            message: state.t().remember_usage.to_string(),
        });
        return Vec::new();
    }
    // A write mid-turn is accepted, but the running turn's context was already
    // assembled, so say when it actually takes effect rather than implying now.
    if state.is_busy() {
        state.notification = Some(Notification {
            level: NotificationLevel::Info,
            message: state.t().remember_busy_hint.to_string(),
        });
    }
    vec![Effect::Send(ClientCommand::RememberMemory {
        session_id: state.session_id.clone(),
        body: body.to_string(),
        kind: Some(kind),
    })]
}

/// Split an optional leading `--kind <k>` off the body. Unknown kinds are a
/// usage error rather than a silent fallback, so a typo cannot quietly store a
/// note as a standing preference.
fn parse_remember_kind(rest: &str) -> (leveler_client_protocol::UiMemoryKind, &str) {
    use leveler_client_protocol::UiMemoryKind;
    let Some(after) = rest.strip_prefix("--kind") else {
        return (UiMemoryKind::Preference, rest);
    };
    let after = after.trim_start();
    let (word, tail) = match after.split_once(char::is_whitespace) {
        Some((w, t)) => (w, t),
        None => (after, ""),
    };
    let kind = match word.trim() {
        "decision" => UiMemoryKind::Decision,
        "note" => UiMemoryKind::Note,
        // Includes an explicit "preference" and anything unrecognised; an
        // unrecognised word stays part of the body so the usage hint fires
        // instead of storing it under a guessed kind.
        "preference" => UiMemoryKind::Preference,
        _ => return (UiMemoryKind::Preference, rest),
    };
    (kind, tail)
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
            let command = ClientCommand::RunGoal {
                session_id: state.session_id.clone(),
                content: rest,
            };
            submit_turn_input(state, command)
        }
    }
}

/// `/develop <task>` runs the full development workflow on one goal.
///
/// The command word is not part of the goal: what reaches the runtime is what
/// the user actually asked for, so Analyze reads a task and not a transcript
/// of how it was typed.
///
/// There is no `status` or `clear` sub-command here. `/goal` needs them
/// because goal mode is a session state the user turns on and off; a Develop
/// workflow is one task that runs to a terminal, and the session's ordinary
/// cancel already stops it.
fn run_develop(state: &mut AppState, command: &str) -> Vec<Effect> {
    let goal = command
        .strip_prefix("develop")
        .unwrap_or(command)
        .trim()
        .to_string();
    if goal.is_empty() {
        state.notification = Some(Notification {
            level: NotificationLevel::Warning,
            message: state.t().develop_usage.to_string(),
        });
        return Vec::new();
    }
    if turn_input_held(state) {
        // The slash parser already took the composer; hand it back.
        state.composer.replace(format!("/develop {goal}"));
        return Vec::new();
    }
    state.transcript.push_user_if_new(goal.clone());
    state.staged_goal = Some(crate::active_goal::StagedGoal {
        title: crate::active_goal::short_title(&goal),
        objective: Some(goal.clone()),
        continuation: false,
    });
    start_turn(state);
    let command = ClientCommand::RunDevelop {
        session_id: state.session_id.clone(),
        content: goal,
    };
    submit_turn_input(state, command)
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
            work_profile: state.work_profile.clone(),
            collaboration: state.collaboration.clone(),
        }));
    }
    // `/goal cancel` is the explicit-task-cancellation entry: it cancels the
    // logical task, not just the current window, so a later `继续` cannot
    // reopen it. With nothing to cancel it is only a goal-mode clear.
    if was_busy_goal || state.resumable_task {
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
/// conversation or the model; opens Shell Details immediately.
fn submit_user_shell(state: &mut AppState, cmd: String) -> Vec<Effect> {
    if cmd.trim().is_empty() {
        // Bare `!`: keep the composer content so the user can continue
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
    state.composer.take();
    // The Details screen opens now; UserShellStarted fills it in.
    state.active_screen = crate::screen::Screen::Shell;
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
                reasoning_effort: None,
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
                reasoning_effort: None,
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
    fn the_local_relay_needs_its_own_command_not_an_argument() {
        let mut s = state_with_dialogue();

        // The real command never binds a local relay, whatever follows it. As
        // an argument this was one typo from being taken for `/remote` itself,
        // on a command whose whole job is to open this machine to a network.
        assert_eq!(
            handle_slash(&mut s, "remote"),
            vec![Effect::StartRemote { local: false }]
        );
        assert_eq!(
            handle_slash(&mut s, "remote loc"),
            vec![Effect::StartRemote { local: false }],
            "a trailing word must not change what /remote does"
        );

        assert_eq!(
            handle_slash(&mut s, "remote-loc"),
            vec![Effect::StartRemote { local: true }]
        );
    }

    #[test]
    fn the_local_command_can_be_found_by_typing_it() {
        // It used to be hidden — a testing path that shows up while browsing is
        // a testing path users will try. That was the wrong trade: the person
        // who needs this command is told to type it, types it, sees no
        // completion and nothing in the list, and concludes it does not exist.
        // A command nobody can confirm is real is worse than one a stranger
        // might try; the description says plainly what it is for.
        assert!(crate::screen::is_known_slash_token("remote-loc"));

        for locale in [crate::i18n::Locale::Zh, crate::i18n::Locale::En] {
            let text = locale.text();
            let listed = crate::screen::slash_commands(text);
            assert!(
                listed.iter().any(|(name, _)| *name == "/remote-loc"),
                "typing /remote- must offer it"
            );

            // Enter submits a fully typed command instead of completing to the
            // highlighted suggestion, and that turns on this list. Without the
            // entry, `/remote-loc` + Enter would run `/remote` — the one
            // command it must never be mistaken for.
            assert!(crate::screen::is_exact_slash_token("/remote-loc"));

            // And it says which one is which, rather than leaving two
            // near-identical names to be told apart by guessing.
            let (_, description) = listed
                .iter()
                .find(|(name, _)| *name == "/remote-loc")
                .expect("just asserted it is there");
            assert!(!description.is_empty());
            assert_ne!(*description, text.slash.remote);
        }
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
