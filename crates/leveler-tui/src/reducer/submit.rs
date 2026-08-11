use leveler_client_protocol::{ClientCommand, NotificationLevel, PermissionProfile};

use crate::action::Effect;
use crate::screen::{BusyPolicy, Screen};
use crate::state::{AppState, Notification};

use super::overlay_keys::{
    apply_theme_id, open_checkpoint_picker, open_mode_picker, open_model_picker, open_theme_picker,
    open_unsupported_media,
};
use super::runtime_apply::start_turn;
use super::screen_nav::{open_diff_screen, open_sessions_screen, toggle_screen};

/// Composer text changed: re-enable the slash popup and reset its highlight.
pub(super) fn touch_slash_filter(state: &mut AppState) {
    state.slash_selected = 0;
    state.slash_popup_dismissed = false;
    if state.composer.text().starts_with('/') {
        refresh_skill_catalog(state);
    }
}

pub(super) fn submit(state: &mut AppState) -> Vec<Effect> {
    // User shell escape: the RAW composer's first character is `!` — no
    // leading-whitespace trim, so " !cargo test" stays a normal message and
    // ordinary prose can never execute. Never reaches the model.
    if let Some(cmd) = state.composer.text().strip_prefix('!') {
        let cmd = cmd.to_string();
        return submit_user_shell(state, cmd);
    }
    let text = state.composer.text().trim().to_string();
    if text.is_empty() {
        return Vec::new();
    }
    // A single line starting with a KNOWN `/command` is a local slash command,
    // parsed locally and never sent to the model. An unknown `/xxx` keeps the
    // composer content (so a typo or a path-like message is never swallowed).
    if let Some(rest) = text.strip_prefix('/')
        && !text.contains('\n')
    {
        let name = rest.split_whitespace().next().unwrap_or("");
        refresh_skill_catalog(state);
        if crate::screen::is_known_slash_token(name) {
            // /clear confirm arming is handled inside; other commands disarm.
            if name != "clear" && name != "new" {
                state.clear_confirm_armed = false;
            }
            state.composer.take();
            return handle_slash(state, rest.trim());
        }
        // Project/user skills are first-class: `/code-review` ≡ `/skill code-review`.
        if crate::screen::is_skill_slash_token(state, name) {
            state.clear_confirm_armed = false;
            state.composer.take();
            let task = rest.strip_prefix(name).unwrap_or("").trim().to_string();
            return run_named_skill(state, name, &task);
        }
        state.clear_confirm_armed = false;
        // Reserve unknown-command feedback for command-shaped typos such as
        // `/hlep`. Absolute paths (`/Users/...`), file names, and other
        // slash-prefixed prose are ordinary messages.
        if looks_like_unknown_slash_command(name) {
            state.notification = Some(Notification {
                level: NotificationLevel::Warning,
                message: format!("未知命令: /{name}（内容已保留，/help 查看命令）"),
            });
            return Vec::new();
        }
    }
    state.clear_confirm_armed = false;
    if state.is_busy() {
        // Steer the running turn instead of queuing behind it: a correction
        // ("actually use the other module") is worthless once the work is
        // finished. The runtime injects it at the top of the next round, and
        // falls back to an ordinary submission if the turn ended in the
        // meantime — so nothing typed is ever lost.
        let text = state.composer.text().trim().to_string();
        state.composer.take();
        if text.is_empty() {
            return Vec::new();
        }
        // The user typed it, so it belongs in the transcript like any message.
        state.transcript.push_user_if_new(text.clone());
        state.notification = Some(Notification {
            level: NotificationLevel::Info,
            message: state.t().steering_sent.to_string(),
        });
        return vec![Effect::Send(ClientCommand::SteerCurrentTurn {
            session_id: state.session_id.clone(),
            content: text,
        })];
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
    let content = state.composer.take();
    let attachments = std::mem::take(&mut state.pending_attachments);
    state.transcript.push_user_if_new(content.clone());
    // Go Busy immediately (not on the first runtime event): closes the
    // submit→first-event window where a second submit would send instead of
    // queue, double-driving the runtime.
    start_turn(state);
    // Mirror session collaboration on the busy chrome (footer/plan shell).
    // Runtime maps collaboration=goal SubmitMessage → goal turn profile.
    state.goal_mode_active =
        state.collaboration.eq_ignore_ascii_case("goal") && attachments.is_empty();
    vec![Effect::Send(ClientCommand::SubmitMessage {
        session_id: state.session_id.clone(),
        content,
        attachments,
    })]
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
        "btw" => run_btw(state, command),
        "tools" => toggle_screen(state, Screen::Tools),
        // /plan = collaboration Plan mode (read-only planning).
        "plan" => set_collab(state, "plan"),
        "work-mode" => set_work_mode(state, command),
        "collab" => set_collab_cmd(state, command),
        "memory" => memory_slash(state, command),
        "skill" => skill_slash(state, command),
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
        "export" => export_conversation(state, command),
        "paste" => vec![Effect::Send(ClientCommand::AddClipboardImage {
            session_id: state.session_id.clone(),
        })],
        // The composer was already taken above, so this opens on an empty
        // buffer — the command itself is never carried into the editor.
        "editor" => super::open_external_editor(state),
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
                })]
            }
        }
        "clear" => clear_conversation(state),
        "doctor" => doctor_slash(state),
        "quit" => vec![Effect::Quit],
        "help" => toggle_screen(state, Screen::Help),
        other => {
            state.notification = Some(Notification {
                level: NotificationLevel::Warning,
                message: format!("未知命令: /{other}"),
            });
            Vec::new()
        }
    }
}

/// `/clear` requires a second confirmation so a fat-finger does not wipe history.
/// `/clear` (alias `/new`): start a fresh conversation.
///
/// This starts a NEW session and switches to it; the current one keeps its
/// transcript and checkpoints and stays in `/sessions`. It used to wipe the
/// current session in place — unrecoverable, checkpoints and all — which is
/// the only reason it needed a two-step confirmation. Starting fresh loses
/// nothing, so there is nothing to confirm.
fn clear_conversation(state: &mut AppState) -> Vec<Effect> {
    state.clear_confirm_armed = false;
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

fn run_btw(state: &mut AppState, command: &str) -> Vec<Effect> {
    let question = command
        .strip_prefix("btw")
        .unwrap_or(command)
        .trim()
        .to_string();
    if question.is_empty() {
        state.notification = Some(Notification {
            level: NotificationLevel::Warning,
            message: state.t().btw_usage.to_string(),
        });
        return Vec::new();
    }
    // Do not push to the main user transcript or start a main turn — side Q only.
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
    if !matches!(arg.as_str(), "economy" | "balanced" | "delivery") {
        state.notification = Some(Notification {
            level: NotificationLevel::Warning,
            message: state.t().work_mode_usage.to_string(),
        });
        return Vec::new();
    }
    state.work_profile = arg.clone();
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
        state.notification = Some(Notification {
            level: NotificationLevel::Warning,
            message: state.t().collab_usage.to_string(),
        });
        return Vec::new();
    }
    set_collab(state, &arg)
}

fn set_collab(state: &mut AppState, collab: &str) -> Vec<Effect> {
    if !matches!(collab, "chat" | "plan" | "goal") {
        state.notification = Some(Notification {
            level: NotificationLevel::Warning,
            message: state.t().collab_usage.to_string(),
        });
        return Vec::new();
    }
    state.collaboration = collab.to_string();
    if collab == "plan" {
        state.mode = PermissionProfile::RequestApproval;
        state.notification = Some(Notification {
            level: NotificationLevel::Info,
            message: "协作=计划（只读）。确认后用 /collab goal 或 /goal <任务> 开始执行".into(),
        });
    } else {
        state.notification = Some(Notification {
            level: NotificationLevel::Info,
            message: format!("协作 → {collab}"),
        });
    }
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

/// Rescan project + user skills when the root changes (or first `/` keystroke).
pub(super) fn refresh_skill_catalog(state: &mut AppState) {
    let root = skill_root(state);
    let key = root.display().to_string();
    if state.skill_catalog_root.as_deref() == Some(key.as_str()) {
        return;
    }
    state.skill_catalog = leveler_skills::discover(&root)
        .into_iter()
        // Builtins always win the name; keep only skill-shaped tokens.
        .filter(|s| {
            looks_like_unknown_slash_command(&s.name)
                && !crate::screen::is_known_slash_token(&s.name)
        })
        .map(|s| (s.name, s.description))
        .collect();
    state.skill_catalog_root = Some(key);
}

/// `/memory` — list active (+archived); `/memory forget <id>` archives.
/// `/skill` — list available skills, or select one (rewrites to `$name` and
/// submits so the agent turn-injection path matches typing `$name`).
fn skill_slash(state: &mut AppState, command: &str) -> Vec<Effect> {
    let rest = command.strip_prefix("skill").unwrap_or(command).trim();
    refresh_skill_catalog(state);

    if rest.is_empty() {
        let message = if state.skill_catalog.is_empty() {
            "暂无技能。在 .leveler/skills/<name>/SKILL.md 或 ~/.leveler/skills/ 添加；\
             用法: /skill <name> [任务]  或直接  /<name> [任务]"
                .to_string()
        } else {
            let mut lines = vec![
                "可用技能（/<name> 或 /skill <name> [任务] ≡ 发送 $name；本轮注入全文）："
                    .to_string(),
            ];
            for (name, desc) in &state.skill_catalog {
                if desc.is_empty() {
                    lines.push(format!("  /{name}"));
                } else {
                    lines.push(format!("  /{name} — {desc}"));
                }
            }
            lines.join("\n")
        };
        state.notification = Some(Notification {
            level: NotificationLevel::Info,
            message,
        });
        return Vec::new();
    }

    let mut parts = rest.splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or("").trim();
    let task = parts.next().unwrap_or("").trim();
    if name.is_empty() {
        state.notification = Some(Notification {
            level: NotificationLevel::Warning,
            message: "用法: /skill <name> [任务说明]  或  /<name> [任务说明]".into(),
        });
        return Vec::new();
    }

    run_named_skill(state, name, task)
}

/// Inject and submit a skill (same path as `$name` / `/skill name`).
fn run_named_skill(state: &mut AppState, name: &str, task: &str) -> Vec<Effect> {
    let content = crate::screen::skill_mention_message(name, task);
    if state.is_busy() {
        // Same as an ordinary message: steer the running turn.
        state.transcript.push_user_if_new(content.clone());
        state.notification = Some(Notification {
            level: NotificationLevel::Info,
            message: state.t().steering_sent.to_string(),
        });
        return vec![Effect::Send(ClientCommand::SteerCurrentTurn {
            session_id: state.session_id.clone(),
            content,
        })];
    }
    state.transcript.push_user_if_new(content.clone());
    start_turn(state);
    vec![Effect::Send(ClientCommand::SubmitMessage {
        session_id: state.session_id.clone(),
        content,
        attachments: Vec::new(),
    })]
}

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
            if state.is_busy() {
                // Steer the running turn, same as an ordinary message: the
                // runtime falls back to a fresh submission if it already ended.
                let content = format!("/goal {rest}");
                state.transcript.push_user_if_new(content.clone());
                state.notification = Some(Notification {
                    level: NotificationLevel::Info,
                    message: state.t().steering_sent.to_string(),
                });
                return vec![Effect::Send(ClientCommand::SteerCurrentTurn {
                    session_id: state.session_id.clone(),
                    content,
                })];
            }
            state.transcript.push_user_if_new(rest.clone());
            start_turn(state);
            state.goal_mode_active = true;
            vec![Effect::Send(ClientCommand::RunGoal {
                session_id: state.session_id.clone(),
                content: rest,
            })]
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
    if was_busy_goal {
        state.notification = Some(Notification {
            level: NotificationLevel::Info,
            message: t.goal_cleared_and_cancel.to_string(),
        });
        effects.push(Effect::Send(ClientCommand::CancelCurrentTurn {
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

/// Local self-check: connection, model, permission, repo, skills, checkpoints.
fn doctor_slash(state: &mut AppState) -> Vec<Effect> {
    refresh_skill_catalog(state);
    let conn = if state.runtime_connected {
        "ok"
    } else {
        "断开"
    };
    let busy = if state.is_busy() { "busy" } else { "idle" };
    let repo = if state.repository.is_empty() {
        "—"
    } else {
        state.repository.as_str()
    };
    let branch = state.branch.as_deref().unwrap_or("—");
    let lines = [
        "Doctor".to_string(),
        format!("session     {}", state.session_id.as_str()),
        format!("connected   {conn}"),
        format!("status      {busy}"),
        format!("model       {}", state.model_label),
        format!("vision      {}", state.vision),
        format!("permission  {} ({:?})", state.mode_label, state.mode),
        format!(
            "axes        collab={} work={}",
            state.collaboration, state.work_profile
        ),
        format!("goal_mode   {}", state.goal_mode_active),
        format!("repo        {repo}"),
        format!("branch      {branch}"),
        format!(
            "context     {} / {} tokens (in={} out={})",
            state.context_tokens,
            state.context_window_tokens,
            state.token_input,
            state.token_output
        ),
        format!("skills      {}", state.skill_catalog.len()),
        format!("checkpoints {}", state.checkpoints.len()),
        format!("attachments {}", state.pending_attachments.len()),
    ];
    let mut body = lines.join("\n");
    if !state.untrusted_config.is_empty() {
        body.push_str("\nuntrusted_config:\n");
        for p in &state.untrusted_config {
            body.push_str(&format!("  - {p}\n"));
        }
    }
    state.transcript.push_note(body);
    state.notification = Some(Notification {
        level: NotificationLevel::Info,
        message: format!(
            "doctor · {conn} · {} · skills={}",
            state.model_label,
            state.skill_catalog.len()
        ),
    });
    Vec::new()
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
            assert!(crate::screen::SLASH_NAMES.contains(&"/remote-loc"));

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
        crate::reducer::reduce(
            &mut s,
            crate::action::Action::WebLaunched(Ok("http://127.0.0.1:9/?token=abc".to_string())),
        );
        assert!(!s.web_starting);
        assert_eq!(s.web_url.as_deref(), Some("http://127.0.0.1:9/?token=abc"));

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
}
