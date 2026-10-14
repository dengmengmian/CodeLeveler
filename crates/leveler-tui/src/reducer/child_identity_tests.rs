//! A child spawned from a declared agent carries ONE identity everywhere it
//! appears — the runtime's role, nickname and spawn title. Each surface formats
//! that identity for its own width (the conversation's tree is the longest, the
//! activity lane the shortest), so these tests pin the shared FACTS rather than
//! a shared string.

use leveler_client_protocol::{RuntimeEvent, RuntimeStatus, SessionId, UiChildAgentIdentity};

use super::reduce;
use crate::action::Action;
use crate::state::{AppState, Boot};

fn state() -> AppState {
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
            model_notice: None,
            thinking: None,
        },
    );
    s.size = (120, 40);
    s.conv.rect = Some((0, 2, 120, 30));
    s.status = RuntimeStatus::Busy;
    s
}

fn spawned(s: &mut AppState, id: &str, nickname: &str, agent: Option<&str>) {
    reduce(
        s,
        Action::Runtime(RuntimeEvent::SubAgentUpdated {
            id: id.into(),
            nickname: nickname.into(),
            role: "explorer".into(),
            title: None,
            done: false,
            ok: false,
            detail: "审查 src/lib.rs".into(),
            profile_id: agent.map(str::to_string),
            profile_role: Some("explorer".into()),
            read_only: true,
            agent: agent.map(|name| UiChildAgentIdentity {
                name: name.into(),
                source: "project".into(),
                capability: "read_only".into(),
                fingerprint: "sha256:abc".into(),
                model: None,
                reasoning_effort: None,
                skills: Vec::new(),
            }),
            contribution: None,
            outcome: None,
            stop: None,
            limit: None,
            background: Some(true),
            scope: Vec::new(),
        }),
    );
}

/// The durable conversation summary no longer duplicates live identities;
/// the activity projection remains the identity source for detail navigation.
#[test]
fn a_declared_agent_child_carries_its_agent_name_in_the_tree_and_the_activity_lane() {
    let mut s = state();
    s.transcript.push_user("派两个子 agent".into());
    spawned(&mut s, "a1", "Euclid", Some("rust-reviewer"));
    spawned(&mut s, "a2", "Newton", None);

    let text: Vec<String> = s
        .conversation_lines(120)
        .iter()
        .map(crate::selection::line_to_plain)
        .collect();
    assert!(
        text.iter().any(|l| l.contains("2 个 agents 正在运行")),
        "{text:#?}"
    );
    assert!(
        text.iter()
            .all(|l| !l.contains("Euclid") && !l.contains("Newton")),
        "live identities do not duplicate into the conversation: {text:#?}"
    );

    let rows = crate::activity::summaries(&s);
    assert!(
        rows.iter().any(|r| r.title == "Euclid · rust-reviewer"),
        "{rows:#?}"
    );
    assert!(rows.iter().any(|r| r.title == "Newton"), "{rows:#?}");
}

/// The text under the agent header is the child's prompt (`task`). The
/// conversation shows the short spawn title and leaves the prompt out.
#[test]
fn the_conversation_shows_the_spawn_title_and_not_the_agent_prompt() {
    let mut s = state();
    reduce(
        &mut s,
        Action::Runtime(spawn_with(
            "a1",
            "Euclid",
            Some("摸清 GitCode 事件入口"),
            "你是只读探查。目标仓库：/tmp/gitcode-npc\n1. HTTP 路由在哪里注册",
        )),
    );
    reduce(
        &mut s,
        Action::Runtime(spawn_with(
            "a2",
            "Newton",
            Some("摸清审查任务执行链"),
            "你是只读探查。目标仓库：/tmp/gitcode-npc\n1. 一次任务在数据库里怎么表示",
        )),
    );

    let text = s
        .conversation_lines(120)
        .iter()
        .map(crate::selection::line_to_plain)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("摸清 GitCode 事件入口"), "{text}");
    assert!(text.contains("摸清审查任务执行链"), "{text}");
    assert!(
        !text.contains("你是只读探查") && !text.contains("目标仓库") && !text.contains("HTTP 路由"),
        "the agent prompt is not conversation text: {text}"
    );
}

fn spawn_with(id: &str, nickname: &str, title: Option<&str>, prompt: &str) -> RuntimeEvent {
    RuntimeEvent::SubAgentUpdated {
        id: id.into(),
        nickname: nickname.into(),
        role: "explorer".into(),
        title: title.map(str::to_string),
        done: false,
        ok: false,
        detail: prompt.into(),
        profile_id: None,
        profile_role: Some("explorer".into()),
        read_only: true,
        agent: None,
        contribution: None,
        outcome: None,
        stop: None,
        limit: None,
        background: Some(true),
        scope: Vec::new(),
    }
}
