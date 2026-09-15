//! A reopened session reads the way it ran: the TUI asks the runtime for the
//! session's durable history and rebuilds its transcript from it — tool rows
//! and how they ended, the turn terminals — instead of the snapshot's text.

use leveler_client_protocol::{
    ClientCommand, CommandId, MessageId, PermissionProfile, RuntimeEvent, RuntimeStatus, SessionId,
    ToolCallId, UiCommandStop, UiHistoryEntry, UiMessage, UiRole, UiSessionSnapshot,
};

use super::reduce;
use crate::action::{Action, Effect};
use crate::state::{AppState, Boot};
use crate::transcript::{ToolStatus, TranscriptItem, TurnEndStatus};

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
    s
}

fn message(role: UiRole, text: &str) -> UiMessage {
    UiMessage {
        id: MessageId::new(format!("m-{text}")),
        role,
        text: text.into(),
        ordinal: None,
        kind: None,
    }
}

fn snapshot(messages: Vec<UiMessage>) -> UiSessionSnapshot {
    UiSessionSnapshot {
        id: SessionId::new("s1"),
        repository: "/repo".into(),
        goal: "g".into(),
        model: None,
        mode: PermissionProfile::Assisted,
        branch: None,
        status: "idle".into(),
        finalization_stage: None,
        messages,
        pending_interactions: Vec::new(),
        available_models: Vec::new(),
        vision: false,
        last_sequence: None,
        active_tools: Vec::new(),
        plan: None,
        verification: None,
        diff: None,
        checkpoints: Vec::new(),
        recaps: Vec::new(),
        user_shells: Vec::new(),
        completion_report: None,
        reasoning: None,
        work_profile: None,
        collaboration: None,
        children: Vec::new(),
    }
}

fn open(s: &mut AppState, messages: Vec<UiMessage>) -> Vec<Effect> {
    reduce(
        s,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot(messages),
        }),
    )
}

fn entry(ms: u64, start: bool, event: RuntimeEvent) -> UiHistoryEntry {
    UiHistoryEntry {
        turn_elapsed_ms: ms,
        turn_start: start,
        event,
    }
}

fn stopped_turn() -> Vec<UiHistoryEntry> {
    let answer = MessageId::new("a1");
    vec![
        entry(
            10,
            true,
            RuntimeEvent::UserMessageAdded {
                message: message(UiRole::User, "运行 soak"),
            },
        ),
        entry(
            1000,
            false,
            RuntimeEvent::ToolCallStarted {
                id: ToolCallId::new("c1"),
                name: "run_command".into(),
                arguments: r#"{"program":"./scripts/soak.sh"}"#.into(),
                parallel: false,
            },
        ),
        entry(
            4000,
            false,
            RuntimeEvent::ToolCallCompleted {
                id: ToolCallId::new("c1"),
                ok: false,
                preview: "tool error: command was cancelled".into(),
                duration_ms: 3000,
                applied_diff: None,
                exit_code: None,
                stop: Some(UiCommandStop::Confirmed),
            },
        ),
        entry(
            5000,
            false,
            RuntimeEvent::AssistantMessageStarted {
                message_id: answer.clone(),
            },
        ),
        entry(
            5000,
            false,
            RuntimeEvent::AssistantTextDelta {
                message_id: answer.clone(),
                delta: "停在第 3 个 tick。".into(),
            },
        ),
        entry(
            5000,
            false,
            RuntimeEvent::AssistantMessageCompleted { message_id: answer },
        ),
        entry(6000, false, RuntimeEvent::TurnAnswered),
    ]
}

fn history_query(effects: &[Effect]) -> Option<CommandId> {
    effects.iter().find_map(|e| match e {
        Effect::Send(ClientCommand::QuerySessionHistory {
            session_id,
            query_id,
        }) if session_id.as_str() == "s1" => query_id.clone(),
        _ => None,
    })
}

#[test]
fn reopening_a_session_with_a_conversation_asks_for_its_history() {
    let mut s = state();
    let effects = open(
        &mut s,
        vec![
            message(UiRole::User, "运行 soak"),
            message(UiRole::Assistant, "停在第 3 个 tick。"),
        ],
    );
    assert!(history_query(&effects).is_some(), "{effects:?}");

    let mut fresh = state();
    assert!(history_query(&open(&mut fresh, Vec::new())).is_none());
}

#[test]
fn the_history_replaces_the_text_only_view_with_what_ran() {
    let mut s = state();
    let effects = open(
        &mut s,
        vec![
            message(UiRole::User, "运行 soak"),
            message(UiRole::Assistant, "停在第 3 个 tick。"),
        ],
    );
    let query_id = history_query(&effects);
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionHistoryLoaded {
            query_id,
            session_id: SessionId::new("s1"),
            entries: stopped_turn(),
            omitted_turns: 0,
        }),
    );

    let items = s.transcript.items();
    assert!(
        matches!(&items[0], TranscriptItem::User(text) if text == "运行 soak"),
        "{items:#?}"
    );
    let call = items
        .iter()
        .find_map(|i| match i {
            TranscriptItem::ToolGroup(g) => g.calls.first(),
            _ => None,
        })
        .expect("a tool row");
    assert_eq!(call.status, ToolStatus::Cancelled);
    assert_eq!(call.duration_ms, Some(3000));
    let end = items
        .iter()
        .find_map(|i| match i {
            TranscriptItem::TurnEnd(end) => Some(end),
            _ => None,
        })
        .expect("a turn end");
    assert_eq!(end.status, TurnEndStatus::Answered);
    assert_eq!(end.elapsed_secs, 6);
    assert_eq!(end.tool_calls, 1);
    // A replay is history: the live session stays as the snapshot left it.
    assert_eq!(s.status, RuntimeStatus::Idle);
    assert!(s.notification.is_none(), "{:?}", s.notification);
}

#[test]
fn a_stale_or_foreign_history_answer_is_ignored() {
    let mut s = state();
    open(&mut s, vec![message(UiRole::User, "运行 soak")]);
    let before = format!("{:?}", s.transcript.items());
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionHistoryLoaded {
            query_id: Some(CommandId::new("someone-else")),
            session_id: SessionId::new("s1"),
            entries: stopped_turn(),
            omitted_turns: 0,
        }),
    );
    assert_eq!(format!("{:?}", s.transcript.items()), before);
}

#[test]
fn a_busy_session_keeps_its_live_view() {
    let mut s = state();
    let effects = open(&mut s, vec![message(UiRole::User, "运行 soak")]);
    let query_id = history_query(&effects);
    s.status = RuntimeStatus::Busy;
    let before = format!("{:?}", s.transcript.items());
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionHistoryLoaded {
            query_id,
            session_id: SessionId::new("s1"),
            entries: stopped_turn(),
            omitted_turns: 0,
        }),
    );
    assert_eq!(format!("{:?}", s.transcript.items()), before);
}

#[test]
fn omitted_older_turns_are_said() {
    let mut s = state();
    let effects = open(&mut s, vec![message(UiRole::User, "运行 soak")]);
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionHistoryLoaded {
            query_id: history_query(&effects),
            session_id: SessionId::new("s1"),
            entries: stopped_turn(),
            omitted_turns: 3,
        }),
    );
    assert!(
        matches!(&s.transcript.items()[0], TranscriptItem::Note(note) if note.contains('3')),
        "{:#?}",
        s.transcript.items()
    );
}
