use leveler_client_protocol::{ClientCommand, NotificationLevel, PermissionProfile};

use crate::action::Effect;
use crate::screen::Screen;
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
}

pub(super) fn submit(state: &mut AppState) -> Vec<Effect> {
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
        if is_known_slash(name) {
            state.composer.take();
            return handle_slash(state, rest.trim());
        }
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
    if state.is_busy() {
        // Queue the input instead of rejecting it; queued items run in order when
        // the current turn finishes.
        let text = state.composer.text().trim().to_string();
        state.composer.take();
        if !text.is_empty() {
            state.input_queues.push_queued(text);
            crate::footer_queue::on_queue_changed(state);
        }
        state.notification = Some(Notification {
            level: NotificationLevel::Info,
            message: state.t().queued_n.replacen(
                "{}",
                &state.input_queues.waiting_len().to_string(),
                1,
            ),
        });
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

/// Run any input that was queued while the previous turn was busy. Called by
/// the event loop once the runtime goes idle; a no-op otherwise.
pub fn drain_queued(state: &mut AppState) -> Vec<Effect> {
    if state.is_busy() || !state.input_queues.pending.is_empty() {
        return Vec::new();
    }
    let Some(text) = state.input_queues.pop_next_waiting() else {
        return Vec::new();
    };
    if text.trim().is_empty() {
        return Vec::new();
    }
    // Submit via the composer, but preserve any draft the user has started
    // typing since queuing: stash it, and restore it once the queued text has
    // actually been taken (the vision gate can leave the buffer occupied).
    let draft = state.composer.text().to_string();
    state.input_queues.mark_pending(text.clone());
    state.composer.replace(text);
    crate::footer_queue::on_queue_changed(state);
    // Drop the "queued" notice once the last queued item starts running.
    if state.input_queues.waiting_len() == 0 {
        state.notification = None;
    }
    let effects = submit(state);
    if effects.is_empty() {
        state.input_queues.reject_pending();
    }
    if state.composer.is_empty() && !draft.is_empty() {
        state.composer.replace(draft);
    }
    effects
}

/// Complete a partial slash command to the highlighted match (Tab/Enter, §29).
pub(super) fn complete_slash(state: &mut AppState) {
    let matches = crate::screen::visible_slash_popup(state);
    if matches.is_empty() {
        return;
    }
    let idx = state.slash_selected.min(matches.len() - 1);
    let (name, _) = matches[idx];
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

/// Whether `name` (without the leading `/`) is a command `handle_slash` accepts.
fn is_known_slash(name: &str) -> bool {
    matches!(
        name,
        "model"
            | "mode"
            | "tools"
            | "goal"
            | "btw"
            | "steps"
            | "plan" // legacy alias of /steps
            | "verify"
            | "diff"
            | "sessions"
            | "context"
            | "agents"
            | "restore"
            | "checkpoint"
            | "compact"
            | "paste"
            | "theme"
            | "image"
            | "attach"
            | "workflow"
            | "wf"
            | "orchestrate" // legacy alias of /workflow
            | "orch"
            | "work-mode"
            | "work_mode"
            | "collab"
            | "confirm-plan"
            | "confirm_plan"
            | "memory"
            | "skill"
            | "clear"
            | "new"
            | "quit"
            | "q"
            | "help"
            | ""
    )
}

/// Handle a `/command` typed in the composer.
fn handle_slash(state: &mut AppState, command: &str) -> Vec<Effect> {
    let name = command.split_whitespace().next().unwrap_or("");
    match name {
        "model" => {
            open_model_picker(state);
            Vec::new()
        }
        "mode" => {
            open_mode_picker(state);
            Vec::new()
        }
        "goal" => run_goal(state, command),
        "btw" => run_btw(state, command),
        "tools" => toggle_screen(state, Screen::Tools),
        // /steps = task plan screen. /plan alone = collaboration Plan mode.
        "steps" => toggle_screen(state, Screen::Plan),
        "plan" => set_collab(state, "plan"),
        "work-mode" | "work_mode" => set_work_mode(state, command),
        "collab" => set_collab_cmd(state, command),
        "confirm-plan" | "confirm_plan" => confirm_plan_to_goal(state),
        "memory" => memory_slash(state, command),
        "skill" => skill_slash(state, command),
        "verify" => toggle_screen(state, Screen::Verification),
        "diff" => open_diff_screen(state),
        "sessions" => open_sessions_screen(state),
        "context" => toggle_screen(state, Screen::Context),
        "agents" => toggle_screen(state, Screen::Agents),
        "restore" | "checkpoint" => {
            open_checkpoint_picker(state);
            Vec::new()
        }
        "compact" => vec![Effect::Send(ClientCommand::CompactContext {
            session_id: state.session_id.clone(),
        })],
        "paste" => vec![Effect::Send(ClientCommand::AddClipboardImage {
            session_id: state.session_id.clone(),
        })],
        "theme" => {
            let arg = command.split_whitespace().nth(1).unwrap_or("").trim();
            if arg.is_empty() {
                open_theme_picker(state);
            } else {
                apply_theme_id(state, arg);
            }
            Vec::new()
        }
        "image" | "attach" => {
            let path = command.split_whitespace().nth(1).unwrap_or("");
            if path.is_empty() {
                state.notification = Some(Notification {
                    level: NotificationLevel::Warning,
                    message: format!("用法: /{name} <文件路径>"),
                });
                Vec::new()
            } else {
                vec![Effect::Send(ClientCommand::AddAttachment {
                    session_id: state.session_id.clone(),
                    path: path.to_string(),
                })]
            }
        }
        // /workflow is the real name; /wf is short. /orchestrate|/orch kept as
        // transitional aliases (not listed in the slash menu).
        "workflow" | "wf" | "orchestrate" | "orch" => {
            state.orchestrate = !state.orchestrate;
            let t = state.t();
            state.notification = Some(Notification {
                level: NotificationLevel::Info,
                message: if state.orchestrate {
                    t.mode_workflow_on.to_string()
                } else {
                    t.mode_workflow_off.to_string()
                },
            });
            vec![Effect::Send(ClientCommand::SetAgentMode {
                session_id: state.session_id.clone(),
                orchestrate: state.orchestrate,
            })]
        }
        "clear" | "new" => {
            // New conversation: clear the display AND drop the model-side history
            // so the next message starts with no prior context.
            state.transcript.clear();
            state.context_tokens = 0;
            state.token_input = 0;
            state.token_output = 0;
            state.turn_tool_calls = 0;
            vec![Effect::Send(ClientCommand::ClearConversation {
                session_id: state.session_id.clone(),
            })]
        }
        "quit" | "q" => vec![Effect::Quit],
        "help" | "" => toggle_screen(state, Screen::Help),
        other => {
            state.notification = Some(Notification {
                level: NotificationLevel::Warning,
                message: format!("未知命令: /{other}"),
            });
            Vec::new()
        }
    }
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
    if state.is_busy() {
        state.notification = Some(Notification {
            level: NotificationLevel::Warning,
            message: "idle only: wait for the turn to finish before /work-mode".into(),
        });
        return Vec::new();
    }
    let arg = command
        .split_whitespace()
        .nth(1)
        .unwrap_or("")
        .to_ascii_lowercase();
    if !matches!(arg.as_str(), "economy" | "balanced" | "delivery") {
        state.notification = Some(Notification {
            level: NotificationLevel::Warning,
            message: "用法: /work-mode economy|balanced|delivery".into(),
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
            message: "用法: /collab chat|plan|goal".into(),
        });
        return Vec::new();
    }
    set_collab(state, &arg)
}

fn set_collab(state: &mut AppState, collab: &str) -> Vec<Effect> {
    if state.is_busy() {
        state.notification = Some(Notification {
            level: NotificationLevel::Warning,
            message: "idle only: wait for the turn to finish before /collab".into(),
        });
        return Vec::new();
    }
    if !matches!(collab, "chat" | "plan" | "goal") {
        state.notification = Some(Notification {
            level: NotificationLevel::Warning,
            message: "用法: /collab chat|plan|goal".into(),
        });
        return Vec::new();
    }
    state.collaboration = collab.to_string();
    if collab == "plan" {
        state.mode = PermissionProfile::RequestApproval;
        state.notification = Some(Notification {
            level: NotificationLevel::Info,
            message: "协作=计划（只读）。确认方案后输入 /confirm-plan 自动进入 goal".into(),
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

/// `/memory` — list active (+archived); `/memory forget <id>` archives.
/// `/skill` — list available skills, or select one (rewrites to `$name` and
/// submits so the agent turn-injection path matches typing `$name`).
fn skill_slash(state: &mut AppState, command: &str) -> Vec<Effect> {
    let rest = command.strip_prefix("skill").unwrap_or(command).trim();
    let root = if state.repository.is_empty() {
        leveler_core::environment().current_dir().to_path_buf()
    } else {
        // repository may be display form (`~/…`); expand home for discover.
        let raw = state.repository.as_str();
        if let Some(rest) = raw.strip_prefix("~/") {
            if let Some(home) = leveler_core::environment()
                .var_os("HOME")
                .or_else(|| leveler_core::environment().var_os("USERPROFILE"))
            {
                std::path::PathBuf::from(home).join(rest)
            } else {
                std::path::PathBuf::from(raw)
            }
        } else {
            std::path::PathBuf::from(raw)
        }
    };

    if rest.is_empty() {
        let skills = leveler_skills::discover(&root);
        let message = if skills.is_empty() {
            "暂无技能。在 .leveler/skills/<name>/SKILL.md 或 ~/.leveler/skills/ 添加；\
             用法: /skill <name> [任务]"
                .to_string()
        } else {
            let mut lines =
                vec!["可用技能（/skill <name> [任务] ≡ 发送 $name；本轮注入全文）：".to_string()];
            for s in &skills {
                lines.push(format!(
                    "  ${} [{}] — {}",
                    s.name,
                    s.source.as_str(),
                    s.description
                ));
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
            message: "用法: /skill <name> [任务说明]".into(),
        });
        return Vec::new();
    }

    // Same inject signal as typing `$name` in free text (S1/S2 shared path).
    let content = crate::screen::skill_mention_message(name, task);
    if state.is_busy() {
        state.input_queues.push_queued(content);
        crate::footer_queue::on_queue_changed(state);
        state.notification = Some(Notification {
            level: NotificationLevel::Info,
            message: state.t().queued_n.replacen(
                "{}",
                &state.input_queues.waiting_len().to_string(),
                1,
            ),
        });
        return Vec::new();
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
    if let Some(id) = rest.strip_prefix("forget").map(str::trim) {
        if id.is_empty() {
            state.notification = Some(Notification {
                level: NotificationLevel::Warning,
                message: "usage: /memory forget <id>".into(),
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
        message: "usage: /memory | /memory forget <id>".into(),
    });
    Vec::new()
}

/// K24: confirm collaboration plan → auto goal (anti-misclick: require idle + proposal).
fn confirm_plan_to_goal(state: &mut AppState) -> Vec<Effect> {
    if state.is_busy() {
        state.notification = Some(Notification {
            level: NotificationLevel::Warning,
            message: "idle only: finish the current turn before /confirm-plan".into(),
        });
        return Vec::new();
    }
    if state.collaboration != "plan" {
        state.notification = Some(Notification {
            level: NotificationLevel::Warning,
            message: "当前不是协作 plan 模式；先 /plan 或 /collab plan".into(),
        });
        return Vec::new();
    }
    let content = state
        .pending_plan_proposal
        .clone()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            // Fall back to last assistant message as the proposed plan.
            state
                .transcript
                .items()
                .iter()
                .rev()
                .find_map(|item| match item {
                    crate::transcript::TranscriptItem::Assistant(b) => Some(b.text.clone()),
                    _ => None,
                })
        })
        .unwrap_or_else(|| "Execute the confirmed plan".into());
    state.collaboration = "goal".into();
    state.mode = PermissionProfile::Assisted;
    state.pending_plan_proposal = None;
    state.goal_mode_active = true;
    state.notification = Some(Notification {
        level: NotificationLevel::Info,
        message: "已确认计划 → 自动进入 goal（将开始改代码）".into(),
    });
    start_turn(state);
    vec![Effect::Send(ClientCommand::ConfirmPlanToGoal {
        session_id: state.session_id.clone(),
        content,
    })]
}

fn run_goal(state: &mut AppState, command: &str) -> Vec<Effect> {
    let goal = command
        .strip_prefix("goal")
        .unwrap_or(command)
        .trim()
        .to_string();
    if goal.is_empty() {
        state.notification = Some(Notification {
            level: NotificationLevel::Warning,
            message: "用法: /goal <任务目标>".to_string(),
        });
        return Vec::new();
    }
    if state.is_busy() {
        state.input_queues.push_queued(format!("/goal {goal}"));
        crate::footer_queue::on_queue_changed(state);
        state.notification = Some(Notification {
            level: NotificationLevel::Info,
            message: state.t().queued_n.replacen(
                "{}",
                &state.input_queues.waiting_len().to_string(),
                1,
            ),
        });
        return Vec::new();
    }
    state.transcript.push_user_if_new(goal.clone());
    start_turn(state);
    state.goal_mode_active = true;
    vec![Effect::Send(ClientCommand::RunGoal {
        session_id: state.session_id.clone(),
        content: goal,
    })]
}
