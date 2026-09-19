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
        images: 0,
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
        active_background_tasks: Vec::new(),
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
fn replayed_background_lifecycle_is_history_and_cannot_replace_live_activity() {
    let mut s = state();
    let effects = open(&mut s, vec![message(UiRole::User, "继续")]);
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::BackgroundTaskStarted {
            task_id: "live-now".into(),
            program: "cargo".into(),
            args: vec!["test".into()],
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionHistoryLoaded {
            query_id: history_query(&effects),
            session_id: SessionId::new("s1"),
            omitted_turns: 0,
            entries: vec![
                leveler_client_protocol::UiHistoryEntry {
                    turn_elapsed_ms: 0,
                    turn_start: true,
                    event: RuntimeEvent::BackgroundTaskStarted {
                        task_id: "old".into(),
                        program: "false".into(),
                        args: vec![],
                    },
                },
                leveler_client_protocol::UiHistoryEntry {
                    turn_elapsed_ms: 10,
                    turn_start: false,
                    event: RuntimeEvent::BackgroundTaskExited {
                        task_id: "old".into(),
                        exit_code: Some(1),
                        duration_ms: 10,
                        ok: false,
                        stopped: false,
                        output: String::new(),
                    },
                },
            ],
        }),
    );

    assert!(s.background_task_labels.contains_key("live-now"));
    assert!(!s.background_task_labels.contains_key("old"));
    assert!(
        s.transcript
            .items()
            .iter()
            .any(|item| matches!(item, TranscriptItem::Note(note) if note.contains("后台任务")))
    );
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

/// Every send is confirmed with a session snapshot, and a snapshot carries the
/// conversation text only. Adopting one must not erase the tool rows the live
/// session already showed: otherwise each new message wipes the record of
/// everything that ran before it.
#[test]
fn a_delivery_snapshot_keeps_the_tool_rows_already_on_screen() {
    let mut s = state();
    open(&mut s, vec![message(UiRole::User, "运行 soak")]);
    for event in [
        RuntimeEvent::ToolCallStarted {
            id: ToolCallId::new("c1"),
            name: "run_command".into(),
            arguments: r#"{"program":"./scripts/soak.sh"}"#.into(),
            parallel: false,
        },
        RuntimeEvent::ToolCallCompleted {
            id: ToolCallId::new("c1"),
            ok: true,
            preview: "ok".into(),
            duration_ms: 3000,
            applied_diff: None,
            exit_code: Some(0),
            stop: None,
        },
    ] {
        reduce(&mut s, Action::Runtime(event));
    }
    assert!(tool_rows(&s) == 1, "{:#?}", s.transcript.items());

    s.status = RuntimeStatus::Idle;
    s.composer.replace("再跑一次".to_string());
    let effects = reduce(
        &mut s,
        Action::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::empty(),
        )),
    );
    let command_id = effects
        .iter()
        .find_map(|e| match e {
            Effect::Submit { command_id, .. } => Some(command_id.clone()),
            _ => None,
        })
        .expect("a submission");
    reduce(
        &mut s,
        Action::EffectCompleted(crate::action::EffectCompletion::SubmissionDelivered {
            command_id,
            snapshot: Some(Box::new(snapshot(vec![
                message(UiRole::User, "运行 soak"),
                message(UiRole::Assistant, "跑完了。"),
                message(UiRole::User, "再跑一次"),
            ]))),
        }),
    );
    assert_eq!(tool_rows(&s), 1, "{:#?}", s.transcript.items());
}

/// The conversation is rebuilt from the durable log on a reopen and after a
/// /compact pushes its snapshot. A compaction recorded in that log has to
/// survive the rebuild — it is the only thing left saying the detail above it
/// is no longer what the model holds.
#[test]
fn a_replayed_compaction_keeps_its_line() {
    let mut s = state();
    let effects = open(&mut s, vec![message(UiRole::User, "运行 soak")]);
    let mut entries = stopped_turn();
    entries.insert(
        1,
        entry(
            20,
            false,
            RuntimeEvent::ContextCompacted { from: 42, to: 1 },
        ),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionHistoryLoaded {
            query_id: history_query(&effects),
            session_id: SessionId::new("s1"),
            entries,
            omitted_turns: 0,
        }),
    );
    let notes: Vec<String> = s
        .transcript
        .items()
        .iter()
        .filter_map(|i| match i {
            TranscriptItem::Note(text) => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(
        notes.iter().any(|n| n.contains("42")),
        "a replayed compaction is still on screen: {notes:#?}"
    );
}

fn tool_rows(s: &AppState) -> usize {
    s.transcript
        .items()
        .iter()
        .filter(|i| matches!(i, TranscriptItem::ToolGroup(_)))
        .count()
}

/// A tool run is how a burst READS, so it cannot be a shape that only live
/// rendering knows: reopening the session must give back the same transcript.
/// Both paths run the same events through the same reducer, and this is the
/// test that keeps it that way.
#[test]
fn a_tool_run_reads_the_same_live_and_replayed() {
    let narration = MessageId::new("a1");
    let events = vec![
        RuntimeEvent::UserMessageAdded {
            message: message(UiRole::User, "看看 app"),
        },
        read_started("c1", "crates/leveler-app/src/lib.rs"),
        read_done("c1"),
        read_started("c2", "crates/leveler-app/src/session.rs"),
        read_done("c2"),
        RuntimeEvent::AssistantMessageStarted {
            message_id: narration.clone(),
        },
        RuntimeEvent::AssistantTextDelta {
            message_id: narration.clone(),
            delta: "再看一个模块：".into(),
        },
        RuntimeEvent::AssistantMessageCompleted {
            message_id: narration,
        },
        read_started("c3", "crates/leveler-app/src/doctor.rs"),
        read_done("c3"),
        RuntimeEvent::TurnAnswered,
    ];

    // Live: the events arrive one by one as the turn runs.
    let mut live = state();
    open(&mut live, Vec::new());
    for event in events.clone() {
        reduce(&mut live, Action::Runtime(event));
    }

    // Replayed: the same events come back as the session's durable history.
    let mut replayed = state();
    let effects = open(
        &mut replayed,
        vec![
            message(UiRole::User, "看看 app"),
            message(UiRole::Assistant, "再看一个模块："),
        ],
    );
    let entries: Vec<UiHistoryEntry> = events
        .into_iter()
        .enumerate()
        .map(|(i, event)| entry(i as u64 * 100, i == 0, event))
        .collect();
    reduce(
        &mut replayed,
        Action::Runtime(RuntimeEvent::SessionHistoryLoaded {
            query_id: history_query(&effects),
            session_id: SessionId::new("s1"),
            entries,
            omitted_turns: 0,
        }),
    );

    // The turn terminal carries a wall-clock the two paths time differently;
    // everything above it is the presentation under test.
    let shape = |s: &AppState| -> Vec<String> {
        crate::conversation::build::build_conversation_lines(s, 100)
            .iter()
            .map(crate::selection::line_to_plain)
            .map(|l| l.trim_end().to_string())
            .filter(|l| !l.starts_with("\u{2500}\u{2500}"))
            .collect()
    };
    let live_lines = shape(&live);
    let replayed_lines = shape(&replayed);
    assert!(
        live_lines
            .iter()
            .any(|l| l.contains("› 读取文件") && !l.contains("lib.rs"))
            && live_lines.iter().any(|l| l.contains("├─ ")),
        "the two consecutive reads are a run: {live_lines:#?}"
    );
    assert!(
        live_lines
            .iter()
            .any(|l| l.contains("› 读取文件") && l.contains("doctor.rs")),
        "the read after the narration is a lone call, not a run: {live_lines:#?}"
    );
    assert_eq!(
        replayed_lines, live_lines,
        "history must read exactly as it ran"
    );
    // Grouping is presentation: the turn still counts three tool calls.
    for s in [&live, &replayed] {
        assert_eq!(
            s.transcript.tool_calls().len(),
            3,
            "{:#?}",
            s.transcript.items()
        );
    }
}

/// Browser parity: the per-call action label (`打开页面`, `读取页面`) is
/// presentation derived from the call's own arguments, so history must rebuild
/// it exactly as the live turn showed it. A real dogfood caught the live run
/// printing `├─ · 7.8s · 4 行`; this keeps the fix on both paths.
#[test]
fn a_browser_run_reads_the_same_live_and_replayed() {
    let events = vec![
        RuntimeEvent::UserMessageAdded {
            message: message(UiRole::User, "确认真实页面"),
        },
        RuntimeEvent::ToolCallStarted {
            id: ToolCallId::new("b1"),
            name: "browser_tab".into(),
            arguments: r#"{"action":"navigate","url":"http://localhost:3000"}"#.into(),
            parallel: false,
        },
        RuntimeEvent::ToolCallCompleted {
            id: ToolCallId::new("b1"),
            ok: true,
            preview: "navigated: true\n".into(),
            duration_ms: 7800,
            applied_diff: None,
            exit_code: None,
            stop: None,
        },
        RuntimeEvent::ToolCallStarted {
            id: ToolCallId::new("b2"),
            name: "browser_tab".into(),
            arguments: r#"{"action":"snapshot"}"#.into(),
            parallel: false,
        },
        RuntimeEvent::ToolCallCompleted {
            id: ToolCallId::new("b2"),
            ok: true,
            preview: "heading: Hello\nbutton: Submit\n".into(),
            duration_ms: 40,
            applied_diff: None,
            exit_code: None,
            stop: None,
        },
        RuntimeEvent::TurnAnswered,
    ];

    let mut live = state();
    open(&mut live, Vec::new());
    for event in events.clone() {
        reduce(&mut live, Action::Runtime(event));
    }

    let mut replayed = state();
    let effects = open(&mut replayed, vec![message(UiRole::User, "确认真实页面")]);
    let entries: Vec<UiHistoryEntry> = events
        .into_iter()
        .enumerate()
        .map(|(i, event)| entry(i as u64 * 100, i == 0, event))
        .collect();
    reduce(
        &mut replayed,
        Action::Runtime(RuntimeEvent::SessionHistoryLoaded {
            query_id: history_query(&effects),
            session_id: SessionId::new("s1"),
            entries,
            omitted_turns: 0,
        }),
    );

    let shape = |s: &AppState| -> Vec<String> {
        crate::conversation::build::build_conversation_lines(s, 100)
            .iter()
            .map(crate::selection::line_to_plain)
            .map(|l| l.trim_end().to_string())
            .filter(|l| !l.starts_with("\u{2500}\u{2500}"))
            .collect()
    };
    let live_lines = shape(&live);
    let replayed_lines = shape(&replayed);
    assert!(
        live_lines
            .iter()
            .any(|l| l.contains("打开页面") && l.contains("http://localhost:3000")),
        "the navigate child names its action and target: {live_lines:#?}"
    );
    assert!(
        live_lines.iter().any(|l| l.contains("读取页面")),
        "the snapshot child names its action: {live_lines:#?}"
    );
    for l in &live_lines {
        let t = l.trim_start();
        let is_run_child = t.starts_with("\u{251c}\u{2500}") || t.starts_with("\u{2514}\u{2500}");
        assert!(
            !is_run_child || t.contains("打开页面") || t.contains("读取页面"),
            "a browser child is never bare metadata: {l:?}"
        );
    }
    assert_eq!(
        replayed_lines, live_lines,
        "history must read exactly as it ran"
    );
}

fn read_started(id: &str, path: &str) -> RuntimeEvent {
    RuntimeEvent::ToolCallStarted {
        id: ToolCallId::new(id),
        name: "read_file".into(),
        arguments: serde_json::json!({ "path": path }).to_string(),
        parallel: false,
    }
}

fn read_done(id: &str) -> RuntimeEvent {
    RuntimeEvent::ToolCallCompleted {
        id: ToolCallId::new(id),
        ok: true,
        preview: "fn main() {}\n".into(),
        duration_ms: 12,
        applied_diff: None,
        exit_code: None,
        stop: None,
    }
}

/// The same parity the runs have, for a MIXED stretch: a group whose header
/// was dropped must be dropped on both paths, or reopening a session would
/// rewrite what the user saw while it ran.
#[test]
fn a_mixed_group_reads_the_same_live_and_replayed() {
    let narration = MessageId::new("a1");
    let events = vec![
        RuntimeEvent::UserMessageAdded {
            message: message(UiRole::User, "看看状态问题"),
        },
        RuntimeEvent::ToolCallStarted {
            id: ToolCallId::new("c1"),
            name: "grep".into(),
            arguments: r#"{"pattern":"TaskStatus"}"#.into(),
            parallel: false,
        },
        RuntimeEvent::ToolCallCompleted {
            id: ToolCallId::new("c1"),
            ok: true,
            preview: "src/app.rs:320\n".into(),
            duration_ms: 9,
            applied_diff: None,
            exit_code: None,
            stop: None,
        },
        read_started("c2", "src/app.rs"),
        read_done("c2"),
        RuntimeEvent::AssistantMessageStarted {
            message_id: narration.clone(),
        },
        RuntimeEvent::AssistantTextDelta {
            message_id: narration.clone(),
            delta: "找到了。".into(),
        },
        RuntimeEvent::AssistantMessageCompleted {
            message_id: narration,
        },
        RuntimeEvent::TurnAnswered,
    ];

    let mut live = state();
    open(&mut live, Vec::new());
    for event in events.clone() {
        reduce(&mut live, Action::Runtime(event));
    }

    let mut replayed = state();
    let effects = open(
        &mut replayed,
        vec![
            message(UiRole::User, "看看状态问题"),
            message(UiRole::Assistant, "找到了。"),
        ],
    );
    let entries: Vec<UiHistoryEntry> = events
        .into_iter()
        .enumerate()
        .map(|(i, event)| entry(i as u64 * 100, i == 0, event))
        .collect();
    reduce(
        &mut replayed,
        Action::Runtime(RuntimeEvent::SessionHistoryLoaded {
            query_id: history_query(&effects),
            session_id: SessionId::new("s1"),
            entries,
            omitted_turns: 0,
        }),
    );

    let shape = |s: &AppState| -> Vec<String> {
        crate::conversation::build::build_conversation_lines(s, 100)
            .iter()
            .map(crate::selection::line_to_plain)
            .map(|l| l.trim_end().to_string())
            .filter(|l| !l.starts_with("\u{2500}\u{2500}"))
            .collect()
    };
    let live_lines = shape(&live);
    assert!(
        live_lines.iter().any(|l| l.contains("› 搜索代码"))
            && live_lines.iter().any(|l| l.contains("› 读取文件")),
        "each tool speaks for itself: {live_lines:#?}"
    );
    assert!(
        !live_lines.iter().any(|l| l.contains("检查代码库")),
        "and nothing summarizes them again: {live_lines:#?}"
    );
    assert_eq!(
        shape(&replayed),
        live_lines,
        "history must read exactly as it ran"
    );
    for s in [&live, &replayed] {
        assert_eq!(s.transcript.tool_calls().len(), 2);
    }
}

/// A command that streamed output while it ran settles into ONE compact row,
/// live and replayed alike: the live tail was process, not history, and the
/// same logical call never comes back as two blocks.
#[test]
fn a_settled_command_reads_as_one_row_live_and_replayed() {
    let events = vec![
        RuntimeEvent::UserMessageAdded {
            message: message(UiRole::User, "跑测试"),
        },
        RuntimeEvent::ToolCallStarted {
            id: ToolCallId::new("t1"),
            name: "run_command".into(),
            arguments: r#"{"program":"cargo","args":["test","-p","leveler-update"]}"#.into(),
            parallel: false,
        },
        RuntimeEvent::ToolCallOutput {
            id: ToolCallId::new("t1"),
            stream: "stdout".into(),
            chunk: "running 27 tests\ntest select_latest_stable ... ok\n".into(),
        },
        RuntimeEvent::ToolCallCompleted {
            id: ToolCallId::new("t1"),
            ok: true,
            preview: "exit: 0\n--- stdout ---\nrunning 27 tests\ntest select_latest_stable ... ok"
                .into(),
            duration_ms: 14_200,
            applied_diff: None,
            exit_code: Some(0),
            stop: None,
        },
        RuntimeEvent::TurnAnswered,
    ];
    let mut live = state();
    open(&mut live, Vec::new());
    for event in events.clone() {
        reduce(&mut live, Action::Runtime(event));
    }
    let mut replayed = state();
    let effects = open(&mut replayed, vec![message(UiRole::User, "跑测试")]);
    reduce(
        &mut replayed,
        Action::Runtime(RuntimeEvent::SessionHistoryLoaded {
            query_id: history_query(&effects),
            session_id: SessionId::new("s1"),
            entries: events
                .into_iter()
                .enumerate()
                .map(|(i, event)| entry(i as u64 * 100, i == 0, event))
                .collect(),
            omitted_turns: 0,
        }),
    );
    let shape = |s: &AppState| -> Vec<String> {
        crate::conversation::build::build_conversation_lines(s, 100)
            .iter()
            .map(crate::selection::line_to_plain)
            .map(|l| l.trim_end().to_string())
            .filter(|l| !l.starts_with("\u{2500}\u{2500}"))
            .collect()
    };
    let live_lines = shape(&live);
    let rows: Vec<&String> = live_lines
        .iter()
        .filter(|l| l.contains("$ cargo test"))
        .collect();
    assert_eq!(rows.len(), 1, "{live_lines:#?}");
    assert!(
        rows[0].contains("✓ $ cargo test -p leveler-update · 14.2s"),
        "{live_lines:#?}"
    );
    assert!(
        !live_lines.iter().any(|l| l.contains("running 27 tests")),
        "{live_lines:#?}"
    );
    assert_eq!(
        shape(&replayed),
        live_lines,
        "history must read exactly as it ran"
    );
}
