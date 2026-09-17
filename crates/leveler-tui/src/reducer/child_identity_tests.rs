//! A child spawned from a declared agent is named the same way everywhere it
//! appears: the conversation's agent tree, the activity lane and the roster.

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
            reasoning_effort: None,
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

/// Dogfood: the tree read `├─ Euclid` while the roster said `rust-reviewer`.
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
        text.iter().any(|l| l.contains("Euclid · rust-reviewer")),
        "{text:#?}"
    );
    assert!(
        text.iter()
            .any(|l| l.contains("Newton") && !l.contains("Newton ·")),
        "a built-in child keeps its nickname alone: {text:#?}"
    );

    let rows = crate::activity::summaries(&s);
    assert!(
        rows.iter().any(|r| r.title == "Euclid · rust-reviewer"),
        "{rows:#?}"
    );
    assert!(rows.iter().any(|r| r.title == "Newton"), "{rows:#?}");
}
