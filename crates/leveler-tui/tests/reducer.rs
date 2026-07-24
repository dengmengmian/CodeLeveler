//! Reducer tests: runtime events fold into state; keys edit/submit/cancel (§69.1).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use leveler_client_protocol::ToolCallId;
use leveler_client_protocol::{
    ApprovalDecision, ApprovalId, ClientCommand, MessageId, PermissionProfile, RuntimeEvent,
    RuntimeStatus, SessionId, UiActiveToolCall, UiApprovalRequest, UiCheckpoint,
    UiCompletionReport, UiMessage, UiPlan, UiPlanStep, UiRole, UiSessionSnapshot,
};
use leveler_tui::action::{Action, Effect, EffectCompletion};
use leveler_tui::overlay::Overlay;
use leveler_tui::reducer::reduce;
use leveler_tui::screen::Screen;
use leveler_tui::state::{AppState, Boot, PendingInteraction};
use leveler_tui::theme::Theme;
use leveler_tui::transcript::{ToolStatus, TranscriptItem, TurnEndStatus};

/// Assert a single SendInteraction, ignoring the generated `command_id`.
fn assert_send_interaction(
    effects: &[Effect],
    expected_command: ClientCommand,
    expected_restore: PendingInteraction,
) {
    assert_eq!(effects.len(), 1, "effects: {effects:?}");
    match &effects[0] {
        Effect::SendInteraction {
            command,
            restore,
            command_id,
        } => {
            assert_eq!(command, &expected_command);
            assert_eq!(restore, &expected_restore);
            assert!(
                !command_id.as_str().is_empty(),
                "command_id must be non-empty"
            );
        }
        other => panic!("expected SendInteraction, got {other:?}"),
    }
}

fn state() -> AppState {
    AppState::new(
        Theme::no_color(),
        Boot {
            session_id: SessionId::new("s1"),
            user: "麻凡".to_string(),
            version: "0.1.0".to_string(),
            show_welcome: true,
            draft_path: None,
            history_path: None,
            context_window: 0,
            locale: leveler_tui::Locale::Zh,
        },
    )
}

fn snapshot() -> UiSessionSnapshot {
    UiSessionSnapshot {
        id: SessionId::new("s1"),
        repository: "/repo".to_string(),
        goal: "interactive session".to_string(),
        model: leveler_client_protocol::ModelRef::parse("deepseek/v3"),
        mode: PermissionProfile::Assisted,
        branch: Some("main".to_string()),
        status: "idle".to_string(),
        messages: Vec::new(),
        pending_interactions: Vec::new(),
        available_models: vec![
            leveler_client_protocol::ModelRef::parse("deepseek/v3").unwrap(),
            leveler_client_protocol::ModelRef::parse("glm/5").unwrap(),
        ],
        vision: false,
        last_sequence: None,
        active_tools: Vec::new(),
        plan: None,
        verification: None,
        diff: None,
        checkpoints: Vec::new(),
        completion_report: None,
    }
}

fn key(code: KeyCode) -> Action {
    Action::Key(KeyEvent::new(code, KeyModifiers::empty()))
}

fn ctrl(c: char) -> Action {
    Action::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
}

fn raw_char(c: char) -> Action {
    Action::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::empty()))
}

#[test]
fn session_opened_sets_labels_without_welcome_card() {
    let mut s = state();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot(),
        }),
    );
    assert_eq!(s.model_label, "deepseek/v3");
    assert_eq!(s.mode_label, "Assisted");
    assert_eq!(s.branch.as_deref(), Some("main"));
    assert!(
        !s.transcript
            .items()
            .iter()
            .any(|i| matches!(i, TranscriptItem::Welcome(_))),
        "welcome card must not be injected"
    );
}

#[test]
fn assistant_streaming_accumulates_into_one_block() {
    let mut s = state();
    let id = MessageId::new("m1");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AssistantMessageStarted {
            message_id: id.clone(),
        }),
    );
    assert_eq!(s.status, RuntimeStatus::Busy);
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AssistantTextDelta {
            message_id: id.clone(),
            delta: "你好".into(),
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AssistantTextDelta {
            message_id: id.clone(),
            delta: "，世界".into(),
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AssistantMessageCompleted {
            message_id: id.clone(),
        }),
    );

    let blocks: Vec<_> = s
        .transcript
        .items()
        .iter()
        .filter_map(|i| match i {
            TranscriptItem::Assistant(b) => Some(b),
            _ => None,
        })
        .collect();
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].text, "你好，世界");
    assert!(blocks[0].done);
}

#[test]
fn user_message_added_appends_user_block() {
    let mut s = state();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::UserMessageAdded {
            message: UiMessage {
                id: MessageId::new("u1"),
                role: UiRole::User,
                text: "hi".into(),
            },
        }),
    );
    assert!(matches!(s.transcript.items().last(), Some(TranscriptItem::User(t)) if t == "hi"));
}

#[test]
fn token_usage_updates_context_gauge() {
    let mut s = state();
    assert_eq!(s.context_tokens, 0);
    assert_eq!(s.token_input, 0);
    assert_eq!(s.token_output, 0);
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::TokenUsage {
            input_tokens: 1200,
            output_tokens: 300,
            cached_input_tokens: 0,
        }),
    );
    // Window in use = input + output; latest round replaces, not accumulates.
    assert_eq!(s.context_tokens, 1500);
    assert_eq!(s.token_input, 1200);
    assert_eq!(s.token_output, 300);
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::TokenUsage {
            input_tokens: 2000,
            output_tokens: 500,
            cached_input_tokens: 0,
        }),
    );
    assert_eq!(s.context_tokens, 2500);
    assert_eq!(s.token_input, 2000);
    assert_eq!(s.token_output, 500);
}

#[test]
fn btw_slash_sends_btw_command_without_user_transcript() {
    let mut s = opened();
    s.composer.replace("/btw 这个函数做什么？");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(
        matches!(
            &effects[..],
            [Effect::Send(ClientCommand::Btw { question, .. })]
                if question == "这个函数做什么？"
        ),
        "effects: {effects:?}"
    );
    assert!(
        !s.transcript
            .items()
            .iter()
            .any(|i| matches!(i, TranscriptItem::User(_))),
        "btw must not add a main user turn"
    );
}

#[test]
fn incomplete_turn_keeps_reason_on_turn_end_marker() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::TurnIncomplete {
            reason: "完整性检查未通过：缺少边界说明".into(),
        }),
    );
    let Some(TranscriptItem::TurnEnd(end)) = s
        .transcript
        .items()
        .iter()
        .rev()
        .find(|item| matches!(item, TranscriptItem::TurnEnd(_)))
    else {
        panic!("expected turn end");
    };
    assert_eq!(end.status, TurnEndStatus::Incomplete);
    assert_eq!(
        end.detail.as_deref(),
        Some("完整性检查未通过：缺少边界说明")
    );
    assert!(
        s.notification.is_none(),
        "the durable turn-end reason must not be duplicated as a notification"
    );
}

#[test]
fn unverified_turn_keeps_reason_without_duplicate_notification() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::TurnCompletedUnverified {
            reason: "没有验证门可独立确认".into(),
        }),
    );
    let Some(TranscriptItem::TurnEnd(end)) = s
        .transcript
        .items()
        .iter()
        .rev()
        .find(|item| matches!(item, TranscriptItem::TurnEnd(_)))
    else {
        panic!("expected turn end");
    };
    assert_eq!(end.status, TurnEndStatus::Unverified);
    assert_eq!(end.detail.as_deref(), Some("没有验证门可独立确认"));
    assert!(
        s.notification.is_none(),
        "the durable turn-end reason must not be duplicated as a notification"
    );
}

/// Unverified turn must never show success `verify ✓` even when gate `passed`
/// is true (passed means !Failed, not task Verified).
#[test]
fn unverified_turn_end_omits_success_verify_chrome() {
    use leveler_client_protocol::{CheckState, UiCheck, UiVerification};
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::VerificationUpdated {
            verification: UiVerification {
                checks: vec![UiCheck {
                    name: "cargo test".into(),
                    status: CheckState::Passed,
                    evidence: None,
                }],
                passed: Some(true),
            },
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::TurnCompletedUnverified {
            reason: "有改动但缺少系统级验收背书".into(),
        }),
    );
    let Some(TranscriptItem::TurnEnd(end)) = s
        .transcript
        .items()
        .iter()
        .rev()
        .find(|item| matches!(item, TranscriptItem::TurnEnd(_)))
    else {
        panic!("expected turn end");
    };
    assert_eq!(end.status, TurnEndStatus::Unverified);
    let summary = end.summary.as_deref().unwrap_or("");
    assert!(
        !summary.contains("verify ✓"),
        "Unverified must not show verify ✓; summary={summary:?}"
    );
}

#[test]
fn completed_turn_end_may_show_success_verify_chrome() {
    use leveler_client_protocol::{CheckState, UiCheck, UiVerification};
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::VerificationUpdated {
            verification: UiVerification {
                checks: vec![UiCheck {
                    name: "cargo test".into(),
                    status: CheckState::Passed,
                    evidence: None,
                }],
                passed: Some(true),
            },
        }),
    );
    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCompleted));
    let Some(TranscriptItem::TurnEnd(end)) = s
        .transcript
        .items()
        .iter()
        .rev()
        .find(|item| matches!(item, TranscriptItem::TurnEnd(_)))
    else {
        panic!("expected turn end");
    };
    assert_eq!(end.status, TurnEndStatus::Completed);
    let summary = end.summary.as_deref().unwrap_or("");
    assert!(
        summary.contains("verify ✓"),
        "Completed with green gates may show verify ✓; summary={summary:?}"
    );
}

#[test]
fn btw_events_fill_ephemeral_block() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::BtwStarted {
            question: "why?".into(),
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::BtwTextDelta {
            delta: "because".into(),
        }),
    );
    reduce(&mut s, Action::Runtime(RuntimeEvent::BtwCompleted));
    let Some(TranscriptItem::Btw(b)) = s.transcript.items().last() else {
        panic!("expected btw block");
    };
    assert_eq!(b.question, "why?");
    assert_eq!(b.answer, "because");
    assert!(b.done);
    assert!(!b.failed);
}

#[test]
fn esc_dismisses_finished_btw_card() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::BtwStarted {
            question: "q".into(),
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::BtwTextDelta { delta: "a".into() }),
    );
    // Still running: Esc must not remove the card.
    reduce(&mut s, key(KeyCode::Esc));
    assert!(
        s.transcript
            .items()
            .iter()
            .any(|i| matches!(i, TranscriptItem::Btw(b) if !b.done)),
        "running btw must stay"
    );
    reduce(&mut s, Action::Runtime(RuntimeEvent::BtwCompleted));
    assert!(s.transcript.has_finished_btw());
    reduce(&mut s, key(KeyCode::Esc));
    assert!(
        !s.transcript
            .items()
            .iter()
            .any(|i| matches!(i, TranscriptItem::Btw(_))),
        "finished btw must dismiss on Esc"
    );
}

#[test]
fn ctrl_o_expands_only_the_latest_tool_group() {
    let mut s = opened();
    // Older group.
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ToolCallStarted {
            id: ToolCallId::new("a1"),
            name: "read_file".into(),
            arguments: r#"{"path":"old.rs"}"#.into(),
            parallel: false,
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            id: ToolCallId::new("a1"),
            ok: true,
            preview: "old-line\n".into(),
            duration_ms: 5,
        }),
    );
    // Assistant text closes the older tool group, then a new group starts.
    let mid = MessageId::new("m1");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AssistantMessageStarted {
            message_id: mid.clone(),
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AssistantTextDelta {
            message_id: mid.clone(),
            delta: "ok".into(),
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AssistantMessageCompleted { message_id: mid }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ToolCallStarted {
            id: ToolCallId::new("b1"),
            name: "run_command".into(),
            arguments: r#"{"program":"ls"}"#.into(),
            parallel: false,
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            id: ToolCallId::new("b1"),
            ok: true,
            preview: "new-line\n".into(),
            duration_ms: 5,
        }),
    );

    let groups: Vec<_> = s
        .transcript
        .items()
        .iter()
        .filter_map(|i| match i {
            TranscriptItem::ToolGroup(g) => Some(g.expanded),
            _ => None,
        })
        .collect();
    assert!(groups.len() >= 2, "need two groups, got {groups:?}");
    assert!(groups.iter().all(|e| !*e), "all collapsed initially");

    reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL)),
    );

    let flags: Vec<_> = s
        .transcript
        .items()
        .iter()
        .filter_map(|i| match i {
            TranscriptItem::ToolGroup(g) => Some(g.expanded),
            _ => None,
        })
        .collect();
    assert_eq!(
        flags.last().copied(),
        Some(true),
        "latest group expanded: {flags:?}"
    );
    assert!(
        flags[..flags.len() - 1].iter().all(|e| !*e),
        "older groups stay collapsed: {flags:?}"
    );
}

#[test]
fn ctrl_o_prefers_live_reasoning_over_tool_groups() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ToolCallStarted {
            id: ToolCallId::new("t1"),
            name: "read_file".into(),
            arguments: r#"{"path":"a.rs"}"#.into(),
            parallel: false,
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            id: ToolCallId::new("t1"),
            ok: true,
            preview: "ok\n".into(),
            duration_ms: 1,
        }),
    );
    s.reasoning = "thinking hard about the fix".into();
    s.reasoning_expanded = false;

    reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL)),
    );
    assert!(
        s.reasoning_expanded,
        "Ctrl+O must expand live reasoning first"
    );
    let groups: Vec<_> = s
        .transcript
        .items()
        .iter()
        .filter_map(|i| match i {
            TranscriptItem::ToolGroup(g) => Some(g.expanded),
            _ => None,
        })
        .collect();
    assert!(
        groups.iter().all(|e| !*e),
        "tool groups stay collapsed while reasoning is the target: {groups:?}"
    );

    // Second toggle collapses reasoning only.
    reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL)),
    );
    assert!(!s.reasoning_expanded);
}

#[test]
fn help_expand_copy_matches_latest_group_semantics() {
    let s = state();
    let t = s.t();
    assert!(
        t.key_expand.contains("最新")
            || t.key_expand.contains("latest")
            || t.key_expand.contains("当前思考"),
        "help must describe latest-group/thinking priority: {}",
        t.key_expand
    );
}

#[test]
fn zero_token_usage_does_not_wipe_gauge() {
    let mut s = state();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::TokenUsage {
            input_tokens: 100,
            output_tokens: 20,
            cached_input_tokens: 0,
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::TokenUsage {
            input_tokens: 0,
            output_tokens: 0,
            cached_input_tokens: 0,
        }),
    );
    assert_eq!(s.context_tokens, 120);
    assert_eq!(s.token_input, 100);
}

#[test]
fn turn_end_estimates_context_when_provider_reports_no_usage() {
    let mut s = opened();
    s.transcript
        .push_user("你好，这是一段用于估算上下文的测试文本。".into());
    s.context_window_tokens = 1_000_000;
    assert_eq!(s.context_tokens, 0);
    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnAnswered));
    assert!(
        s.context_tokens > 0,
        "finish without TokenUsage must still estimate from transcript"
    );
}

#[test]
fn typing_edits_composer() {
    let mut s = state();
    reduce(&mut s, key(KeyCode::Char('你')));
    reduce(&mut s, key(KeyCode::Char('好')));
    assert_eq!(s.composer.text(), "你好");
}

#[test]
fn batched_text_input_edits_composer_once() {
    let mut s = state();
    reduce(&mut s, Action::TextInput("hello\nworld".to_string()));
    assert_eq!(s.composer.text(), "hello\nworld");
}

#[test]
fn typing_reclaims_input_focus_after_mousing_into_conversation() {
    use leveler_tui::state::WorkbenchFocus;
    let mut s = opened();
    // User moused / scrolled into the conversation, so focus left the input.
    s.workbench_focus = WorkbenchFocus::Conversation;
    // Normal human typing arrives as coalesced TextInput bursts. It must both
    // insert AND pull focus back to the input — matching the single-key path's
    // "typing always claims Input focus" rule. Otherwise the composer stays
    // muted and ↑/↓ keep scrolling, so typing "feels" dead.
    reduce(&mut s, Action::TextInput("hi".to_string()));
    assert_eq!(s.composer.text(), "hi");
    assert_eq!(s.workbench_focus, WorkbenchFocus::Input);
}

#[test]
fn raw_control_chars_edit_composer_like_control_keys() {
    let mut s = state();
    for ch in "hello world".chars() {
        reduce(&mut s, key(KeyCode::Char(ch)));
    }

    reduce(&mut s, raw_char('\u{17}')); // Ctrl+W
    assert_eq!(s.composer.text(), "hello ");

    reduce(&mut s, raw_char('\u{15}')); // Ctrl+U
    assert_eq!(s.composer.text(), "");

    for ch in "abcdef".chars() {
        reduce(&mut s, key(KeyCode::Char(ch)));
    }
    reduce(&mut s, raw_char('\u{1}')); // Ctrl+A
    reduce(&mut s, raw_char('\u{b}')); // Ctrl+K
    assert_eq!(s.composer.text(), "");

    for ch in "xy".chars() {
        reduce(&mut s, key(KeyCode::Char(ch)));
    }
    reduce(&mut s, raw_char('\u{8}')); // Backspace in some PTYs.
    reduce(&mut s, raw_char('\u{7f}')); // DEL/backspace in others.
    assert_eq!(s.composer.text(), "");
}

#[test]
fn enter_submits_and_clears_composer() {
    let mut s = state();
    for ch in "fix bug".chars() {
        reduce(&mut s, key(KeyCode::Char(ch)));
    }
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert_eq!(
        effects,
        vec![Effect::Send(ClientCommand::SubmitMessage {
            session_id: SessionId::new("s1"),
            content: "fix bug".to_string(),
            attachments: Vec::new(),
        })]
    );
    assert!(s.composer.is_empty(), "composer cleared after submit");
    assert!(matches!(
        s.transcript.items().last(),
        Some(TranscriptItem::User(text)) if text == "fix bug"
    ));
}

#[test]
fn runtime_user_echo_does_not_duplicate_local_echo() {
    let mut s = state();
    for ch in "fix bug".chars() {
        reduce(&mut s, key(KeyCode::Char(ch)));
    }
    reduce(&mut s, key(KeyCode::Enter));
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::UserMessageAdded {
            message: UiMessage {
                id: MessageId::new("u1"),
                role: UiRole::User,
                text: "fix bug".into(),
            },
        }),
    );

    let users = s
        .transcript
        .items()
        .iter()
        .filter(|item| matches!(item, TranscriptItem::User(_)))
        .count();
    assert_eq!(users, 1);
}

#[test]
fn enter_on_empty_composer_does_nothing() {
    let mut s = state();
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(effects.is_empty());
}

#[test]
fn ctrl_j_inserts_newline_not_submit() {
    let mut s = state();
    reduce(&mut s, key(KeyCode::Char('a')));
    let effects = reduce(&mut s, ctrl('j'));
    assert!(effects.is_empty());
    assert_eq!(s.composer.line_count(), 2);
}

#[test]
fn ctrl_c_idle_quits_on_second_press() {
    let mut s = state();
    let first = reduce(&mut s, ctrl('c'));
    assert!(first.is_empty());
    assert!(s.quit_armed);
    assert!(s.notification.is_some());
    let second = reduce(&mut s, ctrl('c'));
    assert_eq!(second, vec![Effect::Quit]);
    assert!(
        s.notification.is_none(),
        "final quit should not repaint the stale 'press again' prompt"
    );
}

#[test]
fn ctrl_c_accepts_terminal_variants() {
    let mut s = state();
    assert!(reduce(&mut s, raw_char('\u{3}')).is_empty());
    assert_eq!(reduce(&mut s, raw_char('\u{3}')), vec![Effect::Quit]);

    let mut s = state();
    assert!(reduce(&mut s, ctrl('C')).is_empty());
    assert_eq!(reduce(&mut s, ctrl('C')), vec![Effect::Quit]);
}

#[test]
fn any_key_disarms_pending_quit() {
    let mut s = state();
    reduce(&mut s, ctrl('c')); // arm quit
    assert!(s.quit_armed);
    reduce(&mut s, key(KeyCode::Char('x'))); // disarm
    assert!(!s.quit_armed);
    let next = reduce(&mut s, ctrl('c'));
    assert!(next.is_empty(), "quit must re-arm, not fire");
}

#[test]
fn non_printing_control_key_does_not_disarm_pending_quit() {
    let mut s = state();
    reduce(&mut s, ctrl('c'));
    assert!(s.quit_armed);
    reduce(&mut s, key(KeyCode::Null));
    assert!(s.quit_armed);
    assert_eq!(reduce(&mut s, ctrl('c')), vec![Effect::Quit]);
}

#[test]
fn visible_quit_prompt_is_enough_to_confirm_quit() {
    let mut s = state();
    reduce(&mut s, ctrl('c'));
    s.quit_armed = false;
    assert_eq!(reduce(&mut s, ctrl('c')), vec![Effect::Quit]);
}

#[test]
fn command_progress_heartbeat_names_the_running_command_with_elapsed() {
    // Long-command heartbeat: the status line must read "运行 cargo test · <mm:ss>"
    // instead of a bare "等待模型" while a command runs (runtime observability).
    let mut s = state();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::CommandProgress {
            label: "cargo test".into(),
            elapsed_ms: 151_000, // 2m31s
        }),
    );
    assert_eq!(s.status, RuntimeStatus::Busy);
    let activity = s.activity.clone().unwrap_or_default();
    assert!(activity.contains("cargo test"), "activity: {activity}");
    assert!(activity.contains("2m 31s"), "activity: {activity}");
}

#[test]
fn enter_on_queued_item_starts_now_and_interrupts_turn() {
    let mut s = state();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AgentActivity {
            label: "run".into(),
        }),
    );
    assert_eq!(s.status, RuntimeStatus::Busy);
    s.input_queues.queued = vec!["first".into(), "second".into()];
    s.queue_collapsed = false;
    s.queue_selected = Some(1); // waiting index 1 = "second"

    let effects = reduce(&mut s, key(KeyCode::Enter));
    // Running turn is interrupted so the runtime idles and drains this item.
    assert_eq!(
        effects,
        vec![Effect::Send(ClientCommand::CancelCurrentTurn {
            session_id: SessionId::new("s1"),
        })]
    );
    // "second" jumped to the front so it runs next.
    assert_eq!(
        s.input_queues.queued.first().map(String::as_str),
        Some("second")
    );
}

#[test]
fn delete_cancels_selected_queued_item() {
    let mut s = state();
    s.input_queues.queued = vec!["a".into(), "b".into()];
    s.queue_collapsed = false;
    s.queue_selected = Some(0); // first waiting = "a"
    reduce(&mut s, key(KeyCode::Delete));
    assert_eq!(s.input_queues.queued, vec!["b".to_string()]);
}

#[test]
fn ctrl_c_busy_cancels_then_force_cancels() {
    let mut s = state();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AgentActivity {
            label: "run".into(),
        }),
    );
    assert_eq!(s.status, RuntimeStatus::Busy);

    let first = reduce(&mut s, ctrl('c'));
    assert_eq!(
        first,
        vec![Effect::Send(ClientCommand::CancelCurrentTurn {
            session_id: SessionId::new("s1"),
        })]
    );
    assert!(s.cancel_armed);
    assert!(!s.force_cancel_armed);

    let second = reduce(&mut s, ctrl('c'));
    assert_eq!(
        second,
        vec![Effect::Send(ClientCommand::ForceCancelCurrentTurn {
            session_id: SessionId::new("s1"),
        })]
    );
    assert!(s.force_cancel_armed);

    // Third press while still busy: force-cancel did not free the turn — quit.
    assert_eq!(reduce(&mut s, ctrl('c')), vec![Effect::Quit]);
}

#[test]
fn turn_cancelled_clears_force_cancel_arm() {
    let mut s = state();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AgentActivity {
            label: "run".into(),
        }),
    );
    reduce(&mut s, ctrl('c'));
    reduce(&mut s, ctrl('c'));
    assert!(s.force_cancel_armed);
    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCancelled));
    assert!(
        !s.force_cancel_armed && !s.cancel_armed,
        "cancel arms must clear at turn end, else next busy turn escalates too fast"
    );
}

#[test]
fn turn_failed_records_error_and_status() {
    let mut s = state();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::TurnFailed {
            error: "boom".into(),
        }),
    );
    assert_eq!(s.status, RuntimeStatus::Error);
    assert!(
        s.transcript
            .items()
            .iter()
            .any(|item| matches!(item, TranscriptItem::Error(e) if e == "boom"))
    );
    assert!(matches!(
        s.transcript.items().last(),
        Some(TranscriptItem::TurnEnd(end)) if end.status == TurnEndStatus::Failed
    ));
}

#[test]
fn answer_end_is_distinct_from_verified_task_completion() {
    let mut s = state();
    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnAnswered));

    assert_eq!(s.status, RuntimeStatus::Idle);
    assert!(matches!(
        s.transcript.items().last(),
        Some(TranscriptItem::TurnEnd(end)) if end.status == TurnEndStatus::Answered
    ));
}

#[test]
fn truncated_turn_is_visible_but_leaves_input_available() {
    let mut s = state();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::TurnTruncated {
            error: "达到输出上限".into(),
        }),
    );

    assert_eq!(s.status, RuntimeStatus::Idle);
    assert!(matches!(
        s.transcript
            .items()
            .iter()
            .rev()
            .find(|item| matches!(item, TranscriptItem::TurnEnd(_))),
        Some(TranscriptItem::TurnEnd(end)) if end.status == TurnEndStatus::Truncated
    ));
    assert!(
        s.notification.is_none(),
        "the durable truncation reason must not be duplicated as a notification"
    );
}

#[test]
fn resize_updates_size() {
    let mut s = state();
    reduce(&mut s, Action::Resize(120, 40));
    assert_eq!(s.size, (120, 40));
}

// ---- Phase 2: overlays, pickers, approval, slash ---------------------------

fn opened() -> AppState {
    let mut s = state();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot(),
        }),
    );
    s
}

fn typed(s: &mut AppState, text: &str) -> Vec<Effect> {
    let mut last = Vec::new();
    for ch in text.chars() {
        last = reduce(s, key(KeyCode::Char(ch)));
    }
    last
}

fn approval_req() -> UiApprovalRequest {
    UiApprovalRequest {
        id: ApprovalId::new("r1"),
        tool: "run_command".into(),
        summary: "git push".into(),
        command: Some("git push".into()),
        risks: vec!["将访问网络".into()],
    }
}

#[test]
fn slash_model_opens_picker_and_is_not_sent() {
    let mut s = opened();
    typed(&mut s, "/model");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(
        effects.is_empty(),
        "slash command must not be sent to runtime"
    );
    assert!(matches!(s.overlay, Some(Overlay::ModelPicker(_))));
    assert!(s.composer.is_empty());
}

#[test]
fn model_picker_confirm_sends_select_model_and_closes() {
    let mut s = opened();
    typed(&mut s, "/model");
    reduce(&mut s, key(KeyCode::Enter)); // open picker
    // Two models (not searchable): quick-select the 2nd → glm/5.
    let effects = reduce(&mut s, key(KeyCode::Char('2')));
    assert_eq!(
        effects,
        vec![Effect::Send(ClientCommand::SelectModel {
            session_id: SessionId::new("s1"),
            model: leveler_client_protocol::ModelRef::parse("glm/5").unwrap(),
        })]
    );
    assert!(s.overlay.is_none());
    assert_eq!(s.model_label, "glm/5");
    let note = s.notification.as_ref().expect("default-model notice");
    assert!(
        note.message.contains("默认") && note.message.contains("glm/5"),
        "{}",
        note.message
    );
}

#[test]
fn slash_popup_down_arrow_selects_and_tab_completes() {
    let mut s = opened();
    typed(&mut s, "/"); // popup lists all commands
    reduce(&mut s, key(KeyCode::Down)); // move highlight from /model to /mode
    assert_eq!(s.slash_selected, 1);
    reduce(&mut s, key(KeyCode::Tab)); // complete to the highlighted command
    assert_eq!(s.composer.text(), "/mode ");
    assert_eq!(s.slash_selected, 0, "selection resets after completing");
}

/// Typing `/mode` still lists `/model` + `/mode`; ↑/↓ must move selection
/// (regression: workbench popup rendered both rows without a highlight).
#[test]
fn mode_prefix_popup_allows_up_down_selection() {
    let mut s = opened();
    typed(&mut s, "/mode");
    let matches = leveler_tui::screen::visible_slash_popup(&s);
    assert!(
        matches.len() >= 2,
        "expected /model and /mode under prefix /mode, got {matches:?}"
    );
    assert_eq!(matches[0].0, "/model");
    assert_eq!(matches[1].0, "/mode");
    assert_eq!(s.slash_selected, 0);

    reduce(&mut s, key(KeyCode::Down));
    assert_eq!(s.slash_selected, 1, "Down must highlight /mode");
    reduce(&mut s, key(KeyCode::Up));
    assert_eq!(s.slash_selected, 0, "Up must return to /model");
    reduce(&mut s, key(KeyCode::Down));
    reduce(&mut s, key(KeyCode::Tab));
    assert_eq!(
        s.composer.text(),
        "/mode ",
        "Tab completes the highlighted /mode row"
    );
}

#[test]
fn at_file_popup_filters_candidates_and_tab_inserts_the_selected_path() {
    let mut s = opened();
    s.context_files = vec![
        "src/main.rs".into(),
        "src/model.rs".into(),
        "tests/main_test.rs".into(),
    ];
    typed(&mut s, "请检查 @src/ma");

    reduce(&mut s, key(KeyCode::Tab));

    assert_eq!(s.composer.text(), "请检查 @src/main.rs ");
}

#[test]
fn typing_at_requests_the_repository_file_index_once() {
    let mut s = opened();

    let first = reduce(&mut s, key(KeyCode::Char('@')));
    let second = reduce(&mut s, key(KeyCode::Char('s')));

    assert_eq!(
        first,
        vec![Effect::LoadFileCandidates {
            repository: "/repo".into()
        }]
    );
    assert!(second.is_empty(), "the index request must be deduplicated");

    reduce(
        &mut s,
        Action::FileCandidatesLoaded(vec!["src/lib.rs".into()]),
    );
    assert_eq!(
        leveler_tui::screen::visible_file_popup(&s),
        vec!["src/lib.rs"]
    );
}

#[test]
fn slash_popup_esc_dismisses_without_submitting() {
    let mut s = opened();
    typed(&mut s, "/");
    assert!(
        !leveler_tui::screen::visible_slash_popup(&s).is_empty(),
        "typing / should open the popup"
    );
    let effects = reduce(&mut s, key(KeyCode::Esc));
    assert!(effects.is_empty(), "Esc must not submit a command");
    assert!(
        leveler_tui::screen::visible_slash_popup(&s).is_empty(),
        "Esc should hide the slash popup"
    );
    // Draft stays so the user can keep editing if they want.
    assert_eq!(s.composer.text(), "/");
    // Next keystroke brings the popup back.
    reduce(&mut s, key(KeyCode::Char('h')));
    assert!(
        !leveler_tui::screen::visible_slash_popup(&s).is_empty(),
        "editing after dismiss should re-open the popup"
    );
}

#[test]
fn typing_resets_slash_selection() {
    let mut s = opened();
    typed(&mut s, "/");
    reduce(&mut s, key(KeyCode::Down));
    reduce(&mut s, key(KeyCode::Char('d')));
    assert_eq!(s.slash_selected, 0, "filtering resets the highlight");
}

#[test]
fn mode_picker_confirm_sends_set_permission_profile() {
    let mut s = opened();
    typed(&mut s, "/mode");
    reduce(&mut s, key(KeyCode::Enter)); // open picker
    let effects = reduce(&mut s, key(KeyCode::Char('1'))); // Plan
    assert_eq!(
        effects,
        vec![Effect::Send(ClientCommand::SetPermissionProfile {
            session_id: SessionId::new("s1"),
            mode: PermissionProfile::RequestApproval,
        })]
    );
    assert_eq!(s.mode, PermissionProfile::RequestApproval);
    assert!(s.overlay.is_none());
}

#[test]
fn approval_request_opens_overlay() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ApprovalRequested {
            request: approval_req(),
        }),
    );
    assert!(matches!(s.overlay, Some(Overlay::Approval(_))));
}

#[test]
fn reconnect_snapshot_restores_live_pending_interactions() {
    let mut s = state();
    let mut snap = snapshot();
    snap.pending_interactions = vec![
        leveler_client_protocol::UiPendingInteraction::Approval(approval_req()),
        leveler_client_protocol::UiPendingInteraction::Clarification(
            leveler_client_protocol::UiClarificationRequest {
                id: leveler_client_protocol::ClarificationId::new("c1"),
                question: "which?".into(),
                options: vec!["a".into(), "b".into()],
            },
        ),
    ];

    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened { session: snap }),
    );

    assert!(matches!(s.overlay, Some(Overlay::Approval(_))));
    assert_eq!(s.pending_interactions.len(), 1);
}

#[test]
fn reconnect_snapshot_restores_running_turn_render_state() {
    let mut s = state();
    let mut snap = snapshot();
    snap.status = "running".to_string();
    snap.active_tools = vec![UiActiveToolCall {
        id: ToolCallId::new("tool-1"),
        name: "run_command".to_string(),
        arguments: r#"{"cmd":"cargo test"}"#.to_string(),
    }];
    snap.plan = Some(UiPlan {
        steps: vec![UiPlanStep {
            index: 0,
            description: "run tests".to_string(),
            status: leveler_client_protocol::PlanStepStatus::Running,
        }],
    });
    snap.checkpoints = vec![UiCheckpoint {
        id: leveler_client_protocol::CheckpointId::new("cp-1"),
        label: "before tests".to_string(),
        ordinal: 1,
    }];
    snap.completion_report = Some(UiCompletionReport {
        files_changed: 1,
        added: 2,
        removed: 0,
        checks_passed: 1,
        checks_total: 1,
        success: true,
    });

    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened { session: snap }),
    );

    assert_eq!(s.status, RuntimeStatus::Busy);
    assert_eq!(s.transcript.tool_calls().len(), 1);
    assert_eq!(s.transcript.tool_calls()[0].status, ToolStatus::Running);
    assert_eq!(s.plan.as_ref().unwrap().steps.len(), 1);
    assert_eq!(s.checkpoints.len(), 1);
    assert!(
        s.transcript
            .items()
            .iter()
            .any(|item| matches!(item, TranscriptItem::Completion(report) if report.success))
    );
}

#[test]
fn second_approval_queues_and_advances_after_first_resolved() {
    let mut s = opened();
    // First approval → becomes the active overlay.
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ApprovalRequested {
            request: approval_req(),
        }),
    );
    // Second approval arrives while the first is unanswered → must not clobber it.
    let second = UiApprovalRequest {
        id: ApprovalId::new("r2"),
        tool: "run_command".into(),
        summary: "rm -rf tmp".into(),
        command: Some("rm -rf tmp".into()),
        risks: vec![],
    };
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ApprovalRequested { request: second }),
    );

    // First is still the active overlay; second is parked in the queue.
    let Some(Overlay::Approval(ov)) = &s.overlay else {
        panic!("expected the first approval to stay active");
    };
    assert_eq!(ov.request.id, ApprovalId::new("r1"));
    assert_eq!(s.pending_interactions.len(), 1);

    // Answer the first (Enter → Deny). The decision targets r1; the second
    // stays parked until the event loop confirms delivery (ACK), so a failed
    // send can restore the first overlay instead of losing both.
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert_send_interaction(
        &effects,
        ClientCommand::ApprovalDecision {
            request_id: ApprovalId::new("r1"),
            decision: ApprovalDecision::Deny,
        },
        PendingInteraction::Approval(approval_req()),
    );
    assert!(s.overlay.is_none(), "overlay waits for transport ACK");
    assert_eq!(s.pending_interactions.len(), 1);
    // Simulate successful delivery: promote the parked second approval.
    leveler_tui::reducer::overlay_keys::advance_overlay(&mut s);
    let Some(Overlay::Approval(ov)) = &s.overlay else {
        panic!("expected the second approval to become active after ACK");
    };
    assert_eq!(ov.request.id, ApprovalId::new("r2"));
    assert!(s.pending_interactions.is_empty());
}

#[test]
fn clarification_queues_behind_open_approval_and_both_get_answered() {
    use leveler_client_protocol::{ClarificationId, UiClarificationRequest};
    let mut s = opened();
    // An approval is showing…
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ApprovalRequested {
            request: approval_req(),
        }),
    );
    // …when a clarification arrives. It must park, not clobber the approval —
    // a clobbered approval's decision is never sent and the tool call hangs.
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ClarificationRequested {
            request: UiClarificationRequest {
                id: ClarificationId::new("c1"),
                question: "选哪个方案？".into(),
                options: vec![],
            },
        }),
    );
    let Some(Overlay::Approval(ov)) = &s.overlay else {
        panic!("expected the approval to stay active");
    };
    assert_eq!(ov.request.id, ApprovalId::new("r1"));

    // Answering the approval emits a SendInteraction; promotion waits for ACK.
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert_send_interaction(
        &effects,
        ClientCommand::ApprovalDecision {
            request_id: ApprovalId::new("r1"),
            decision: ApprovalDecision::Deny,
        },
        PendingInteraction::Approval(approval_req()),
    );
    assert!(s.overlay.is_none());
    leveler_tui::reducer::overlay_keys::advance_overlay(&mut s);
    let Some(Overlay::Clarification(cov)) = &s.overlay else {
        panic!("expected the parked clarification to become active after ACK");
    };
    assert_eq!(cov.request.id, ClarificationId::new("c1"));

    // The clarification is answerable (Esc = skip with empty answer).
    let effects = reduce(&mut s, key(KeyCode::Esc));
    assert_send_interaction(
        &effects,
        ClientCommand::AnswerClarification {
            request_id: ClarificationId::new("c1"),
            answer: String::new(),
        },
        PendingInteraction::Clarification(UiClarificationRequest {
            id: ClarificationId::new("c1"),
            question: "选哪个方案？".into(),
            options: vec![],
        }),
    );
    assert!(s.overlay.is_none());
}

#[test]
fn approval_enter_denies_by_default() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ApprovalRequested {
            request: approval_req(),
        }),
    );
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert_send_interaction(
        &effects,
        ClientCommand::ApprovalDecision {
            request_id: ApprovalId::new("r1"),
            decision: ApprovalDecision::Deny,
        },
        PendingInteraction::Approval(approval_req()),
    );
    assert!(s.overlay.is_none());
}

#[test]
fn approval_y_approves_once() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ApprovalRequested {
            request: approval_req(),
        }),
    );
    let effects = reduce(&mut s, key(KeyCode::Char('y')));
    assert_send_interaction(
        &effects,
        ClientCommand::ApprovalDecision {
            request_id: ApprovalId::new("r1"),
            decision: ApprovalDecision::ApproveOnce,
        },
        PendingInteraction::Approval(approval_req()),
    );
}

#[test]
fn approval_ctrl_c_denies_safely() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ApprovalRequested {
            request: approval_req(),
        }),
    );
    let effects = reduce(&mut s, ctrl('c'));
    assert_send_interaction(
        &effects,
        ClientCommand::ApprovalDecision {
            request_id: ApprovalId::new("r1"),
            decision: ApprovalDecision::Deny,
        },
        PendingInteraction::Approval(approval_req()),
    );
}

#[test]
fn approval_retry_reuses_the_same_command_id() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ApprovalRequested {
            request: approval_req(),
        }),
    );
    let first = reduce(&mut s, key(KeyCode::Enter));
    let first_id = match &first[0] {
        Effect::SendInteraction { command_id, .. } => command_id.clone(),
        other => panic!("expected SendInteraction, got {other:?}"),
    };
    // Simulate delivery-unknown restore: overlay back, sticky id retained.
    s.overlay = Some(Overlay::Approval(Box::new(
        leveler_tui::overlay::ApprovalOverlay::new(approval_req()),
    )));
    let second = reduce(&mut s, key(KeyCode::Enter));
    let second_id = match &second[0] {
        Effect::SendInteraction { command_id, .. } => command_id.clone(),
        other => panic!("expected SendInteraction, got {other:?}"),
    };
    assert_eq!(
        first_id, second_id,
        "retry must reuse CommandId for receipt dedup"
    );
}

#[test]
fn uncertain_interaction_completion_restores_overlay_and_sticky_id() {
    let mut s = opened();
    let request = approval_req();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ApprovalRequested {
            request: request.clone(),
        }),
    );
    let first = reduce(&mut s, key(KeyCode::Enter));
    let command_id = match &first[0] {
        Effect::SendInteraction { command_id, .. } => command_id.clone(),
        other => panic!("expected SendInteraction, got {other:?}"),
    };
    reduce(
        &mut s,
        Action::EffectCompleted(EffectCompletion::InteractionUncertain {
            key: format!("a:{}", request.id.as_str()),
            restore: PendingInteraction::Approval(request),
            snapshot: None,
        }),
    );
    assert!(matches!(s.overlay, Some(Overlay::Approval(_))));
    let retry = reduce(&mut s, key(KeyCode::Enter));
    assert!(matches!(
        &retry[0],
        Effect::SendInteraction { command_id: retry_id, .. } if retry_id == &command_id
    ));
}

#[test]
fn whitespace_only_paste_is_inserted_as_text() {
    let mut s = opened();
    let effects = reduce(&mut s, Action::Paste(" \n\t".into()));
    assert!(effects.is_empty());
    assert_eq!(s.composer.take(), " \n\t");
}

#[test]
fn uncertain_async_command_fails_closed_until_snapshot_resync() {
    let mut s = opened();
    s.status = RuntimeStatus::Busy;
    s.input_queues.mark_pending("retry me".into());
    reduce(
        &mut s,
        Action::EffectCompleted(EffectCompletion::CommandFailed { snapshot: None }),
    );
    assert_eq!(s.status, RuntimeStatus::Busy);
    assert!(!s.runtime_connected);
    assert_eq!(s.input_queues.pending, vec!["retry me"]);
    assert!(s.input_queues.rejected.is_empty());
}

#[test]
fn overlay_captures_keys_away_from_composer() {
    let mut s = opened();
    typed(&mut s, "/model");
    reduce(&mut s, key(KeyCode::Enter)); // open picker
    reduce(&mut s, key(KeyCode::Char('x'))); // would type 'x' if composer had focus
    assert!(s.composer.is_empty(), "overlay must capture key input");
}

#[test]
fn slash_clear_empties_transcript() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::UserMessageAdded {
            message: UiMessage {
                id: MessageId::new("u1"),
                role: UiRole::User,
                text: "hi".into(),
            },
        }),
    );
    assert!(!s.transcript.is_empty());
    typed(&mut s, "/clear");
    reduce(&mut s, key(KeyCode::Enter));
    assert!(s.transcript.is_empty());
}

#[test]
fn slash_unknown_notifies_and_does_not_send() {
    let mut s = opened();
    typed(&mut s, "/bogus");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(effects.is_empty());
    assert!(s.notification.is_some());
}

#[test]
fn absolute_path_at_message_start_is_sent_instead_of_parsed_as_a_command() {
    let mut s = opened();
    let message = "/Users/example/projects/sample-project 和这个项目对比，那个更好";
    typed(&mut s, message);

    let effects = reduce(&mut s, key(KeyCode::Enter));

    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Send(ClientCommand::SubmitMessage { content, .. })] if content == message
        ),
        "absolute path should be submitted as ordinary text: {effects:?}"
    );
    assert!(s.notification.is_none());
    assert!(s.composer.is_empty());
}

#[test]
fn ctrl_m_opens_model_picker() {
    let mut s = opened();
    reduce(&mut s, ctrl('m'));
    assert!(matches!(s.overlay, Some(Overlay::ModelPicker(_))));
}

// ---- Phase 3: tool blocks + Tools screen -----------------------------------

fn tool_started(s: &mut AppState, id: &str, name: &str, args: &str) {
    reduce(
        s,
        Action::Runtime(RuntimeEvent::ToolCallStarted {
            id: ToolCallId::new(id),
            name: name.into(),
            arguments: args.into(),
            parallel: false,
        }),
    );
}

fn tool_completed(s: &mut AppState, id: &str, ok: bool) {
    reduce(
        s,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            id: ToolCallId::new(id),
            ok,
            preview: if ok { "done".into() } else { "boom".into() },
            duration_ms: 82,
        }),
    );
}

#[test]
fn submit_goes_busy_immediately_so_a_second_submit_queues() {
    let mut s = opened();
    for ch in "first".chars() {
        reduce(&mut s, key(KeyCode::Char(ch)));
    }
    let e1 = reduce(&mut s, key(KeyCode::Enter));
    // Busy the instant we submit — before any runtime event arrives.
    assert_eq!(s.status, RuntimeStatus::Busy);
    assert!(matches!(
        e1.as_slice(),
        [Effect::Send(ClientCommand::SubmitMessage { .. })]
    ));

    // A second submit in that window must QUEUE, not double-drive the runtime.
    for ch in "second".chars() {
        reduce(&mut s, key(KeyCode::Char(ch)));
    }
    let e2 = reduce(&mut s, key(KeyCode::Enter));
    assert!(e2.is_empty(), "second submit must not send a second turn");
    assert_eq!(s.input_queues.waiting_len(), 1, "it must be queued");
}

#[test]
fn text_appended_after_completion_is_visible() {
    use leveler_client_protocol::MessageId;
    let mut s = opened();
    let id = MessageId::new("m1");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AssistantMessageStarted {
            message_id: id.clone(),
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AssistantTextDelta {
            message_id: id.clone(),
            delta: "part one".into(),
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AssistantMessageCompleted {
            message_id: id.clone(),
        }),
    );
    // A late delta for the same message (e.g. a stream retry) must reopen it so
    // the appended text is not hidden behind the cached render.
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AssistantTextDelta {
            message_id: id.clone(),
            delta: " part two".into(),
        }),
    );
    let block = s
        .transcript
        .items()
        .iter()
        .find_map(|i| match i {
            TranscriptItem::Assistant(b) => Some(b),
            _ => None,
        })
        .unwrap();
    assert_eq!(block.text, "part one part two");
    assert!(
        !block.done,
        "late delta must reopen the block for rendering"
    );
}

#[test]
fn switching_session_resets_per_session_state_but_resync_keeps_it() {
    let mut s = opened(); // session "s1"
    s.context_tokens = 4242;
    s.token_input = 100;
    s.diff_selected = 3;
    s.reasoning = "old thinking".into();

    // Open a DIFFERENT session → per-session view state must reset.
    let mut other = snapshot();
    other.id = SessionId::new("s2");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened { session: other }),
    );
    assert_eq!(s.context_tokens, 0, "context gauge must reset on switch");
    assert_eq!(s.token_input, 0);
    assert_eq!(s.diff_selected, 0);
    assert!(s.reasoning.is_empty());

    // A same-session resync (lag recovery) must NOT wipe live state.
    s.context_tokens = 777;
    let mut same = snapshot();
    same.id = SessionId::new("s2");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened { session: same }),
    );
    assert_eq!(
        s.context_tokens, 777,
        "resync of the same session keeps live state"
    );
}

#[test]
fn cancel_arm_clears_on_turn_end_and_checkpoints_dedup() {
    use leveler_client_protocol::{CheckpointId, UiCheckpoint};
    let mut s = opened();
    for ch in "go".chars() {
        reduce(&mut s, key(KeyCode::Char(ch)));
    }
    reduce(&mut s, key(KeyCode::Enter)); // Busy
    reduce(&mut s, ctrl('c')); // first Ctrl+C while busy arms cancel
    assert!(s.cancel_armed);
    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCancelled));
    assert!(
        !s.cancel_armed,
        "a leftover cancel-arm must clear at turn end, else next turn's first Ctrl+C force-cancels"
    );

    let cp = UiCheckpoint {
        id: CheckpointId::new("c1"),
        label: "x".into(),
        ordinal: 0,
    };
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::CheckpointCreated {
            checkpoint: cp.clone(),
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::CheckpointCreated { checkpoint: cp }),
    );
    assert_eq!(
        s.checkpoints.len(),
        1,
        "duplicate checkpoint events must not stack"
    );
}

#[test]
fn context_estimate_does_not_clobber_real_token_usage() {
    let mut s = opened();
    // Real usage reported first.
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::TokenUsage {
            input_tokens: 5000,
            output_tokens: 200,
            cached_input_tokens: 0,
        }),
    );
    assert_eq!(s.context_tokens, 5200);
    // A later pre-run estimate must not overwrite the live gauge.
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ContextUpdated {
            candidate_files: vec!["a.rs".into()],
            estimated_tokens: 99,
        }),
    );
    assert_eq!(
        s.context_tokens, 5200,
        "estimate must not clobber real usage"
    );
    assert_eq!(
        s.context_files,
        vec!["a.rs".to_string()],
        "but files still update"
    );
}

#[test]
fn compaction_summary_renders_as_summary_not_user_message() {
    use leveler_client_protocol::{COMPACTION_SUMMARY_PREFIX, UiMessage};
    let mut s = opened();
    let mut snap = snapshot();
    snap.messages = vec![UiMessage {
        id: MessageId::new("m1"),
        role: UiRole::User,
        text: format!("{COMPACTION_SUMMARY_PREFIX}：\n## Briefing\n做了一些事"),
    }];
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened { session: snap }),
    );
    let has_user = s
        .transcript
        .items()
        .iter()
        .any(|i| matches!(i, TranscriptItem::User(_)));
    let has_summary = s
        .transcript
        .items()
        .iter()
        .any(|i| matches!(i, TranscriptItem::Assistant(b) if b.text.contains("Briefing")));
    assert!(
        !has_user,
        "compaction summary must not render as a user turn"
    );
    assert!(
        has_summary,
        "compaction summary must render as a distinct block"
    );
}

#[test]
fn turn_end_finalizes_in_flight_blocks() {
    use leveler_client_protocol::MessageId;
    let mut s = opened();
    // A streaming assistant, a running tool, and a running sub-agent, none of
    // which received their completion event before the turn was cancelled.
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AssistantMessageStarted {
            message_id: MessageId::new("m1"),
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AssistantTextDelta {
            message_id: MessageId::new("m1"),
            delta: "half a thought".into(),
        }),
    );
    tool_started(&mut s, "t1", "grep", "{}");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SubAgentUpdated {
            id: "a1".into(),
            nickname: "Newton".into(),
            role: "explorer".into(),
            done: false,
            ok: false,
            detail: "investigating".into(),
        }),
    );

    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCancelled));

    // Nothing may be left "running" — else it never commits to scrollback and
    // shows a stuck spinner/cursor forever.
    let assistant_done =
        s.transcript.items().iter().any(
            |i| matches!(i, TranscriptItem::Assistant(b) if b.done && b.text == "half a thought"),
        );
    assert!(assistant_done, "unfinished assistant must be finalized");
    assert_eq!(s.transcript.tool_calls()[0].status, ToolStatus::Failed);
    let sub_running = s
        .transcript
        .items()
        .iter()
        .any(|i| matches!(i, TranscriptItem::SubAgent(b) if b.status == ToolStatus::Running));
    assert!(!sub_running, "running sub-agent must be finalized");
}

#[test]
fn tool_preview_control_chars_are_neutralized() {
    let mut s = opened();
    tool_started(&mut s, "t1", "grep", "{}");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            id: ToolCallId::new("t1"),
            ok: true,
            preview: "file.go:1\tfunc x(\rmore".into(),
            duration_ms: 5,
        }),
    );
    let p = s.transcript.tool_calls()[0].preview.clone().unwrap();
    assert!(
        !p.contains('\t') && !p.contains('\r'),
        "tab/CR must be neutralized: {p:?}"
    );
}

#[test]
fn tool_preview_ansi_color_codes_are_stripped() {
    let mut s = opened();
    tool_started(&mut s, "t1", "run_command", "{}");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            id: ToolCallId::new("t1"),
            ok: true,
            preview: "\u{1b}[32m✓\u{1b}[39m test passed \u{1b}[1m[30m leftover".into(),
            duration_ms: 12,
        }),
    );
    let p = s.transcript.tool_calls()[0].preview.clone().unwrap();
    assert!(
        p.contains('✓') && p.contains("test passed"),
        "kept text: {p:?}"
    );
    assert!(
        !p.contains('\u{1b}') && !p.contains("[32m") && !p.contains("[1m") && !p.contains("[30m"),
        "ANSI must not remain: {p:?}"
    );
}

#[test]
fn tool_call_starts_running_then_completes() {
    let mut s = opened();
    tool_started(&mut s, "t1", "grep", "{\"q\":\"x\"}");
    let running = s.transcript.tool_calls();
    assert_eq!(running.len(), 1);
    assert_eq!(running[0].status, ToolStatus::Running);
    assert_eq!(s.status, RuntimeStatus::Busy);

    tool_completed(&mut s, "t1", true);
    let done = s.transcript.tool_calls();
    assert_eq!(done[0].status, ToolStatus::Ok);
    assert_eq!(done[0].preview.as_deref(), Some("done"));
    assert_eq!(done[0].duration_ms, Some(82));
}

fn sub_agents(s: &AppState) -> Vec<&leveler_tui::transcript::SubAgentBlock> {
    s.transcript
        .items()
        .iter()
        .filter_map(|i| match i {
            TranscriptItem::SubAgent(b) => Some(b),
            _ => None,
        })
        .collect()
}

#[test]
fn repeated_running_sub_agent_updates_in_place_not_duplicated() {
    let mut s = opened();
    let running = |detail: &str| {
        Action::Runtime(RuntimeEvent::SubAgentUpdated {
            id: "a1".into(),
            nickname: "Newton".into(),
            role: "explorer".into(),
            done: false,
            ok: false,
            detail: detail.into(),
        })
    };
    reduce(&mut s, running("step 1"));
    reduce(&mut s, running("step 2")); // progress refresh, same id
    let blocks = sub_agents(&s);
    assert_eq!(blocks.len(), 1, "same id must not create a second block");
    assert!(
        blocks[0].detail.contains("step 2"),
        "detail refreshed in place"
    );
}

#[test]
fn sub_agent_finish_before_start_still_renders() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SubAgentUpdated {
            id: "a9".into(),
            nickname: "Euclid".into(),
            role: String::new(),
            done: true,
            ok: true,
            detail: "already done".into(),
        }),
    );
    let blocks = sub_agents(&s);
    assert_eq!(
        blocks.len(),
        1,
        "a finish with no prior start must still show"
    );
    assert_eq!(blocks[0].status, ToolStatus::Ok);
    assert_eq!(blocks[0].nickname, "Euclid");
}

#[test]
fn sub_agent_block_updates_in_place_from_running_to_done() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SubAgentUpdated {
            id: "a1".into(),
            nickname: "Newton".into(),
            role: "explorer".into(),
            done: false,
            ok: false,
            detail: "investigate module A".into(),
        }),
    );
    let running = sub_agents(&s);
    assert_eq!(running.len(), 1);
    assert_eq!(running[0].nickname, "Newton");
    assert_eq!(running[0].role, "explorer");
    assert_eq!(running[0].status, ToolStatus::Running);
    assert!(running[0].detail.contains("investigate module A"));

    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SubAgentUpdated {
            id: "a1".into(),
            nickname: "Newton".into(),
            role: String::new(),
            done: true,
            ok: true,
            detail: "found 12 crates".into(),
        }),
    );
    let done = sub_agents(&s);
    assert_eq!(
        done.len(),
        1,
        "the same agent updates in place, not appended"
    );
    assert_eq!(done[0].status, ToolStatus::Ok);
    assert!(done[0].detail.contains("found 12 crates"));
}

#[test]
fn ctrl_t_toggles_tools_screen() {
    let mut s = opened();
    reduce(&mut s, ctrl('t'));
    assert_eq!(s.active_screen, Screen::Tools);
    reduce(&mut s, ctrl('t'));
    assert_eq!(s.active_screen, Screen::Conversation);
}

#[test]
fn tools_screen_navigates_and_esc_returns() {
    let mut s = opened();
    tool_started(&mut s, "t1", "read_file", "a.rs");
    tool_completed(&mut s, "t1", true);
    tool_started(&mut s, "t2", "run_command", "cargo test");
    tool_completed(&mut s, "t2", true);

    reduce(&mut s, ctrl('t')); // open Tools
    assert_eq!(s.tools_screen.selected, 0);
    reduce(&mut s, key(KeyCode::Down));
    assert_eq!(s.tools_screen.selected, 1);
    reduce(&mut s, key(KeyCode::Up));
    assert_eq!(s.tools_screen.selected, 0);
    reduce(&mut s, key(KeyCode::Esc));
    assert_eq!(s.active_screen, Screen::Conversation);
}

// ---- Phase 6: sessions + context -------------------------------------------

fn summary(id: &str, goal: &str) -> leveler_client_protocol::UiSessionSummary {
    leveler_client_protocol::UiSessionSummary {
        id: SessionId::new(id),
        goal: goal.into(),
        status: "completed".into(),
        model: "deepseek/v3".into(),
        updated_at: "2026-07-08".into(),
        repository: None,
    }
}

#[test]
fn ctrl_s_opens_sessions_and_requests_list() {
    let mut s = opened();
    let effects = reduce(&mut s, ctrl('s'));
    assert_eq!(s.active_screen, Screen::Sessions);
    assert_eq!(
        effects,
        vec![Effect::Send(ClientCommand::RequestSessionListFor {
            requester_session_id: s.session_id.clone(),
        })]
    );
}

#[test]
fn session_list_event_populates_and_enter_opens() {
    let mut s = opened();
    reduce(&mut s, ctrl('s'));
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionList {
            sessions: vec![summary("a", "first"), summary("b", "second")],
        }),
    );
    assert_eq!(s.sessions.len(), 2);
    reduce(&mut s, key(KeyCode::Down));
    assert_eq!(s.sessions_selected, 1);
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert_eq!(
        effects,
        vec![Effect::Send(ClientCommand::OpenSessionFor {
            requester_session_id: s.session_id.clone(),
            session_id: SessionId::new("b"),
        })]
    );
    assert_eq!(s.active_screen, Screen::Conversation);
}

#[test]
fn sessions_d_deletes_selected() {
    let mut s = opened();
    reduce(&mut s, ctrl('s'));
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionList {
            sessions: vec![summary("a", "first")],
        }),
    );
    let effects = reduce(&mut s, key(KeyCode::Char('d')));
    assert_eq!(
        effects,
        vec![Effect::Send(ClientCommand::DeleteSessionFor {
            requester_session_id: s.session_id.clone(),
            session_id: SessionId::new("a"),
        })]
    );
}

#[test]
fn open_session_rebuilds_transcript() {
    let mut s = opened();
    // Seed some content, then open a different session snapshot.
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::UserMessageAdded {
            message: UiMessage {
                id: MessageId::new("u1"),
                role: UiRole::User,
                text: "old".into(),
            },
        }),
    );
    let mut snap = snapshot();
    snap.id = SessionId::new("other");
    snap.messages = vec![UiMessage {
        id: MessageId::new("m1"),
        role: UiRole::User,
        text: "loaded".into(),
    }];
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened { session: snap }),
    );
    assert_eq!(s.session_id, SessionId::new("other"));
    let has_loaded = s
        .transcript
        .items()
        .iter()
        .any(|i| matches!(i, TranscriptItem::User(t) if t == "loaded"));
    let has_old = s
        .transcript
        .items()
        .iter()
        .any(|i| matches!(i, TranscriptItem::User(t) if t == "old"));
    assert!(
        has_loaded && !has_old,
        "transcript replaced by opened session"
    );
}

#[test]
fn context_updated_event_sets_state() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ContextUpdated {
            candidate_files: vec!["src/a.rs".into()],
            estimated_tokens: 5200,
        }),
    );
    assert_eq!(s.context_tokens, 5200);
    assert_eq!(s.context_files, vec!["src/a.rs".to_string()]);
}

#[test]
fn slash_compact_sends_compact_context() {
    let mut s = opened();
    typed(&mut s, "/compact");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert_eq!(
        effects,
        vec![Effect::Send(ClientCommand::CompactContext {
            session_id: SessionId::new("s1"),
        })]
    );
}

// ---- Checkpoint / restore --------------------------------------------------

#[test]
fn checkpoint_created_event_is_recorded_and_restore_picker_works() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::CheckpointCreated {
            checkpoint: leveler_client_protocol::UiCheckpoint {
                id: leveler_client_protocol::CheckpointId::new("k1"),
                label: "第一条".into(),
                ordinal: 0,
            },
        }),
    );
    assert_eq!(s.checkpoints.len(), 1);

    typed(&mut s, "/restore");
    reduce(&mut s, key(KeyCode::Enter));
    assert!(matches!(s.overlay, Some(Overlay::CheckpointPicker(_))));

    // Only one checkpoint → number 1 restores it.
    let effects = reduce(&mut s, key(KeyCode::Char('1')));
    assert_eq!(
        effects,
        vec![Effect::Send(ClientCommand::RestoreCheckpoint {
            session_id: SessionId::new("s1"),
            checkpoint_id: leveler_client_protocol::CheckpointId::new("k1"),
        })]
    );
}

// ---- Clarification (ask_user) ----------------------------------------------

fn clarify_req() -> leveler_client_protocol::UiClarificationRequest {
    leveler_client_protocol::UiClarificationRequest {
        id: leveler_client_protocol::ClarificationId::new("c1"),
        question: "保留旧字段还是替换？".into(),
        options: vec!["保留".into(), "替换".into()],
    }
}

#[test]
fn clarification_event_opens_overlay() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ClarificationRequested {
            request: clarify_req(),
        }),
    );
    assert!(matches!(s.overlay, Some(Overlay::Clarification(_))));
}

#[test]
fn clarification_digit_answers_option() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ClarificationRequested {
            request: clarify_req(),
        }),
    );
    let effects = reduce(&mut s, key(KeyCode::Char('2')));
    assert_send_interaction(
        &effects,
        ClientCommand::AnswerClarification {
            request_id: leveler_client_protocol::ClarificationId::new("c1"),
            answer: "替换".to_string(),
        },
        PendingInteraction::Clarification(clarify_req()),
    );
    assert!(s.overlay.is_none());
}

#[test]
fn clarification_esc_skips_with_empty_answer() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ClarificationRequested {
            request: clarify_req(),
        }),
    );
    let effects = reduce(&mut s, key(KeyCode::Esc));
    assert_send_interaction(
        &effects,
        ClientCommand::AnswerClarification {
            request_id: leveler_client_protocol::ClarificationId::new("c1"),
            answer: String::new(),
        },
        PendingInteraction::Clarification(clarify_req()),
    );
}

// ---- Phase 7 + 8: agents, help, theme, completion, scroll ------------------

#[test]
fn ctrl_g_toggles_agents_screen() {
    let mut s = opened();
    reduce(&mut s, ctrl('g'));
    assert_eq!(s.active_screen, Screen::Agents);
    reduce(&mut s, key(KeyCode::Esc));
    assert_eq!(s.active_screen, Screen::Conversation);
}

#[test]
fn slash_help_opens_help_screen() {
    let mut s = opened();
    typed(&mut s, "/help");
    reduce(&mut s, key(KeyCode::Enter));
    assert_eq!(s.active_screen, Screen::Help);
}

#[test]
fn slash_theme_opens_picker_and_named_arg_sets() {
    use leveler_tui::ThemeId;
    use leveler_tui::overlay::Overlay;
    let mut s = opened();
    assert_eq!(s.theme.id, ThemeId::Ion);
    typed(&mut s, "/theme");
    reduce(&mut s, key(KeyCode::Enter));
    assert!(
        matches!(s.overlay, Some(Overlay::ThemePicker(_))),
        "bare /theme opens the theme picker"
    );
    // Cursor starts on current (ion); Down → night, Enter confirms.
    reduce(&mut s, key(KeyCode::Down));
    reduce(&mut s, key(KeyCode::Enter));
    assert_eq!(s.theme.id, ThemeId::Night);
    assert!(s.overlay.is_none());
    typed(&mut s, "/theme day");
    reduce(&mut s, key(KeyCode::Enter));
    assert_eq!(s.theme.id, ThemeId::Day);
    assert!(!s.dark, "day theme clears the dark flag");
    typed(&mut s, "/theme ion");
    reduce(&mut s, key(KeyCode::Enter));
    assert_eq!(s.theme.id, ThemeId::Ion);
}

#[test]
fn tab_completes_slash_command() {
    let mut s = opened();
    typed(&mut s, "/mod");
    reduce(&mut s, key(KeyCode::Tab));
    // "/mod" matches both /mode and /model; first declared is /model.
    assert_eq!(s.composer.text(), "/model ");
}

#[test]
fn enter_on_partial_slash_only_completes_before_executing() {
    let mut s = opened();
    typed(&mut s, "/mod");

    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(effects.is_empty());
    assert_eq!(s.composer.text(), "/model ");
    assert!(
        s.overlay.is_none(),
        "partial slash completion must not open a picker on the first Enter"
    );

    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(effects.is_empty());
    assert!(matches!(s.overlay, Some(Overlay::ModelPicker(_))));
    assert!(s.composer.is_empty());
}

#[test]
fn info_screens_scroll_with_keys_and_reset_on_exit() {
    let mut s = opened();
    reduce(&mut s, key(KeyCode::Esc)); // clear any notification
    typed(&mut s, "/help");
    reduce(&mut s, key(KeyCode::Enter));
    assert_eq!(s.active_screen, Screen::Help);

    reduce(&mut s, key(KeyCode::Down));
    reduce(&mut s, key(KeyCode::Down));
    assert_eq!(s.screen_scroll, 2, "Down scrolls an info screen");
    reduce(&mut s, key(KeyCode::PageDown));
    assert!(s.screen_scroll > 2, "PageDown scrolls a page");
    reduce(&mut s, key(KeyCode::Up));
    let before = s.screen_scroll;
    reduce(&mut s, key(KeyCode::PageUp));
    assert!(s.screen_scroll < before, "PageUp scrolls back");

    reduce(&mut s, key(KeyCode::Esc));
    assert_eq!(s.active_screen, Screen::Conversation);
    typed(&mut s, "/help");
    reduce(&mut s, key(KeyCode::Enter));
    assert_eq!(s.screen_scroll, 0, "scroll resets when reopening a screen");
}

#[test]
fn diff_screen_page_keys_scroll_detail_and_selection_resets_it() {
    let mut s = opened();
    s.diff = Some(leveler_client_protocol::UiDiff {
        files: vec![
            leveler_client_protocol::UiDiffFile {
                path: "a.rs".into(),
                added: 1,
                removed: 0,
                patch: Some("+a".into()),
            },
            leveler_client_protocol::UiDiffFile {
                path: "b.rs".into(),
                added: 1,
                removed: 0,
                patch: Some("+b".into()),
            },
        ],
    });
    s.active_screen = Screen::Diff;
    reduce(&mut s, key(KeyCode::PageDown));
    assert!(s.screen_scroll > 0, "PageDown scrolls the patch pane");
    reduce(&mut s, key(KeyCode::Down)); // select next file
    assert_eq!(s.diff_selected, 1);
    assert_eq!(
        s.screen_scroll, 0,
        "switching files resets the patch scroll"
    );
}

// ---- Phase 5: attachments + vision gating ----------------------------------

fn attachment(name: &str) -> leveler_client_protocol::AttachmentRef {
    leveler_client_protocol::AttachmentRef {
        id: leveler_client_protocol::AttachmentId::new("a1"),
        kind: leveler_client_protocol::AttachmentKind::Image,
        name: name.into(),
        mime_type: "image/png".into(),
        size_bytes: 1000,
        sha256: "deadbeef".into(),
        width: Some(100),
        height: Some(80),
    }
}

#[test]
fn slash_image_sends_add_attachment() {
    let mut s = opened();
    typed(&mut s, "/image assets/login.png");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert_eq!(
        effects,
        vec![Effect::Send(ClientCommand::AddAttachment {
            session_id: SessionId::new("s1"),
            path: "assets/login.png".to_string(),
        })]
    );
}

#[test]
fn attachment_added_event_stages_it() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AttachmentAdded {
            attachment: attachment("login.png"),
        }),
    );
    assert_eq!(s.pending_attachments.len(), 1);
}

#[test]
fn submit_with_image_on_non_vision_model_opens_gate() {
    let mut s = opened(); // snapshot vision=false
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AttachmentAdded {
            attachment: attachment("login.png"),
        }),
    );
    typed(&mut s, "看看这张图");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(effects.is_empty(), "must not send to a non-vision model");
    assert!(matches!(s.overlay, Some(Overlay::UnsupportedMedia(_))));
}

#[test]
fn unsupported_media_text_only_sends_without_images() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AttachmentAdded {
            attachment: attachment("login.png"),
        }),
    );
    typed(&mut s, "hi");
    reduce(&mut s, key(KeyCode::Enter)); // opens gate
    // Choose option 3 (仅发送文字).
    let effects = reduce(&mut s, key(KeyCode::Char('3')));
    assert_eq!(
        effects,
        vec![Effect::Send(ClientCommand::SubmitMessage {
            session_id: SessionId::new("s1"),
            content: "hi".to_string(),
            attachments: Vec::new(),
        })]
    );
    assert!(s.pending_attachments.is_empty());
}

#[test]
fn submit_with_image_on_vision_model_sends_attachment() {
    let mut s = opened();
    s.vision = true;
    let att = attachment("login.png");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AttachmentAdded {
            attachment: att.clone(),
        }),
    );
    typed(&mut s, "hi");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert_eq!(
        effects,
        vec![Effect::Send(ClientCommand::SubmitMessage {
            session_id: SessionId::new("s1"),
            content: "hi".to_string(),
            attachments: vec![att],
        })]
    );
    assert!(s.pending_attachments.is_empty());
}

#[test]
fn plain_backspace_on_empty_composer_removes_last_attachment() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AttachmentAdded {
            attachment: attachment("login.png"),
        }),
    );
    assert_eq!(s.pending_attachments.len(), 1);
    reduce(&mut s, key(KeyCode::Backspace));
    assert!(s.pending_attachments.is_empty());
}

// ---- Phase 4: plan / diff / verification / completion ----------------------

#[test]
fn plan_and_verification_events_update_state() {
    use leveler_client_protocol::{
        CheckState, PlanStepStatus, UiCheck, UiPlan, UiPlanStep, UiVerification,
    };
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::PlanUpdated {
            plan: UiPlan {
                steps: vec![UiPlanStep {
                    index: 0,
                    description: "定位代码".into(),
                    status: PlanStepStatus::Running,
                }],
            },
        }),
    );
    assert_eq!(s.plan.as_ref().unwrap().steps[0].description, "定位代码");

    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::VerificationUpdated {
            verification: UiVerification {
                checks: vec![UiCheck {
                    name: "cargo test".into(),
                    status: CheckState::Passed,
                    evidence: None,
                }],
                passed: Some(true),
            },
        }),
    );
    assert_eq!(s.verification.as_ref().unwrap().passed, Some(true));
}

#[test]
fn ctrl_p_and_ctrl_r_toggle_screens() {
    let mut s = opened();
    reduce(&mut s, ctrl('p'));
    assert_eq!(s.active_screen, Screen::Plan);
    reduce(&mut s, ctrl('p'));
    assert_eq!(s.active_screen, Screen::Conversation);
    reduce(&mut s, ctrl('r'));
    assert_eq!(s.active_screen, Screen::Verification);
}

#[test]
fn ctrl_d_opens_diff_and_requests_it() {
    let mut s = opened();
    let effects = reduce(&mut s, ctrl('d'));
    assert_eq!(s.active_screen, Screen::Diff);
    assert_eq!(
        effects,
        vec![Effect::Send(ClientCommand::RequestDiff {
            session_id: SessionId::new("s1"),
        })]
    );
}

#[test]
fn diff_updated_sets_files_and_nav_clamps() {
    use leveler_client_protocol::{UiDiff, UiDiffFile};
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::DiffUpdated {
            diff: UiDiff {
                files: vec![
                    UiDiffFile {
                        path: "a.rs".into(),
                        added: 3,
                        removed: 1,
                        patch: None,
                    },
                    UiDiffFile {
                        path: "b.rs".into(),
                        added: 0,
                        removed: 5,
                        patch: None,
                    },
                ],
            },
        }),
    );
    assert_eq!(s.diff.as_ref().unwrap().files.len(), 2);
}

#[test]
fn session_completed_pushes_completion_block() {
    use leveler_client_protocol::UiCompletionReport;
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionCompleted {
            report: UiCompletionReport {
                files_changed: 3,
                added: 86,
                removed: 31,
                checks_passed: 4,
                checks_total: 4,
                success: true,
            },
        }),
    );
    assert!(matches!(
        s.transcript.items().last(),
        Some(TranscriptItem::Completion(_))
    ));
}

#[test]
fn slash_workflow_toggles_and_sends_command() {
    let mut s = opened();
    typed(&mut s, "/workflow");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(s.orchestrate);
    assert_eq!(
        effects,
        vec![Effect::Send(ClientCommand::SetAgentMode {
            session_id: SessionId::new("s1"),
            orchestrate: true,
        })]
    );
}

#[test]
fn slash_wf_short_alias_toggles_workflow() {
    let mut s = opened();
    typed(&mut s, "/wf");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(s.orchestrate);
    assert_eq!(
        effects,
        vec![Effect::Send(ClientCommand::SetAgentMode {
            session_id: SessionId::new("s1"),
            orchestrate: true,
        })]
    );
}

#[test]
fn slash_memory_list_sends_list_memory_with_archived() {
    let mut s = opened();
    typed(&mut s, "/memory");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(
        effects.iter().any(|e| matches!(
            e,
            Effect::Send(ClientCommand::ListMemory {
                include_archived: true,
                ..
            })
        )),
        "effects={effects:?}"
    );
}

#[test]
fn slash_memory_forget_sends_forget_memory() {
    let mut s = opened();
    typed(&mut s, "/memory forget prefer-ws");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(
        effects.iter().any(|e| matches!(
            e,
            Effect::Send(ClientCommand::ForgetMemory { id, .. }) if id == "prefer-ws"
        )),
        "effects={effects:?}"
    );
}

#[test]
fn memory_list_event_pushes_multiline_transcript_note() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::MemoryList {
            memory_dir: "/proj/memory".into(),
            active: vec![leveler_client_protocol::UiMemoryEntry {
                id: "prefer-ws".into(),
                title: "prefer workspace write".into(),
            }],
            archived: vec![leveler_client_protocol::UiMemoryEntry {
                id: "old-fact".into(),
                title: "old".into(),
            }],
        }),
    );
    let note = s.transcript.items().iter().find_map(|i| match i {
        TranscriptItem::Note(t) => Some(t.as_str()),
        _ => None,
    });
    let note = note.expect("MemoryList must push TranscriptItem::Note");
    assert!(note.contains("memory_dir=/proj/memory"), "{note}");
    assert!(note.contains("[prefer-ws]"), "{note}");
    assert!(note.contains("prefer workspace write"), "{note}");
    assert!(note.contains("[old-fact]"), "{note}");
    assert!(
        note.lines().count() >= 5,
        "multi-line list expected, got {} lines: {note}",
        note.lines().count()
    );
    // Status notification is a short one-liner only (not the full list).
    let n = s.notification.as_ref().expect("short count toast");
    assert!(
        !n.message.contains("[prefer-ws]"),
        "full list must not be status-line only: {}",
        n.message
    );
    assert!(n.message.contains("active=1"), "{}", n.message);
}

#[test]
fn slash_work_mode_sends_product_axes() {
    let mut s = opened();
    // Default collaboration is chat; /work-mode only changes the work profile.
    assert_eq!(s.collaboration, "chat");
    typed(&mut s, "/work-mode delivery");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert_eq!(s.work_profile, "delivery");
    assert!(
        effects.iter().any(|e| matches!(
            e,
            Effect::Send(ClientCommand::SetProductAxes {
                work_profile,
                collaboration,
                ..
            }) if work_profile == "delivery" && collaboration == "chat"
        )),
        "effects={effects:?}"
    );
}

#[test]
fn slash_collab_plan_forces_readonly_mode() {
    let mut s = opened();
    typed(&mut s, "/collab plan");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert_eq!(s.collaboration, "plan");
    assert_eq!(s.mode, PermissionProfile::RequestApproval);
    assert!(
        effects.iter().any(|e| matches!(
            e,
            Effect::Send(ClientCommand::SetProductAxes {
                collaboration,
                ..
            }) if collaboration == "plan"
        )),
        "effects={effects:?}"
    );
}

#[test]
fn confirm_plan_auto_enters_goal() {
    let mut s = opened();
    typed(&mut s, "/collab plan");
    reduce(&mut s, key(KeyCode::Enter));
    // Seed an assistant plan proposal.
    s.pending_plan_proposal = Some("1. fix bug\n2. test".into());
    typed(&mut s, "/confirm-plan");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert_eq!(s.collaboration, "goal");
    assert_eq!(s.mode, PermissionProfile::Assisted);
    assert!(s.goal_mode_active);
    assert!(
        effects.iter().any(|e| matches!(
            e,
            Effect::Send(ClientCommand::ConfirmPlanToGoal { content, .. })
                if content.contains("fix bug")
        )),
        "effects={effects:?}"
    );
}

#[test]
fn tools_screen_filter_narrows_to_shell() {
    let mut s = opened();
    tool_started(&mut s, "t1", "read_file", "a.rs");
    tool_completed(&mut s, "t1", true);
    tool_started(&mut s, "t2", "run_command", "cargo test");
    tool_completed(&mut s, "t2", true);

    reduce(&mut s, ctrl('t'));
    // All -> Read -> Write -> Shell
    reduce(&mut s, key(KeyCode::Tab));
    reduce(&mut s, key(KeyCode::Tab));
    reduce(&mut s, key(KeyCode::Tab));
    use leveler_tui::screen::ToolFilter;
    assert_eq!(s.tools_screen.filter, ToolFilter::Shell);
}

#[test]
fn input_submitted_while_busy_is_queued_then_drained_when_idle() {
    let mut s = opened();
    s.status = RuntimeStatus::Busy;
    typed(&mut s, "next task");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(effects.is_empty(), "must not send while busy");
    assert_eq!(s.input_queues.queued, vec!["next task".to_string()]);
    assert!(s.composer.is_empty(), "composer cleared after queuing");

    // The turn finishes: draining submits the queued message.
    s.status = RuntimeStatus::Idle;
    let effects = leveler_tui::reducer::drain_queued(&mut s);
    assert!(
        matches!(
            effects.first(),
            Some(Effect::Send(ClientCommand::SubmitMessage { .. }))
        ),
        "queued input should submit when idle: {effects:?}"
    );
    assert!(
        s.input_queues.queued.is_empty(),
        "queue cleared after draining"
    );
    assert_eq!(
        s.input_queues.pending,
        vec!["next task".to_string()],
        "drained input waits for a runtime turn-start signal"
    );
}

#[test]
fn multiple_queued_items_are_fifo_and_backspace_deletes_last() {
    let mut s = opened();
    s.status = RuntimeStatus::Busy;
    for msg in ["first", "second", "third"] {
        typed(&mut s, msg);
        reduce(&mut s, key(KeyCode::Enter));
    }
    assert_eq!(
        s.input_queues.queued,
        vec![
            "first".to_string(),
            "second".to_string(),
            "third".to_string()
        ]
    );

    // Backspace on an empty composer removes the MOST RECENT queued item.
    reduce(&mut s, key(KeyCode::Backspace));
    assert_eq!(
        s.input_queues.queued,
        vec!["first".to_string(), "second".to_string()]
    );

    // Draining runs them OLDEST-first (FIFO).
    s.status = RuntimeStatus::Idle;
    let effects = leveler_tui::reducer::drain_queued(&mut s);
    assert!(
        matches!(
            effects.first(),
            Some(Effect::Send(ClientCommand::SubmitMessage { content, .. })) if content == "first"
        ),
        "FIFO: oldest runs first: {effects:?}"
    );
    assert_eq!(s.input_queues.queued, vec!["second".to_string()]);
    assert_eq!(s.input_queues.pending, vec!["first".to_string()]);
}

#[test]
fn pending_input_is_cleared_when_runtime_turn_starts() {
    let mut s = opened();
    s.status = RuntimeStatus::Busy;
    typed(&mut s, "next");
    reduce(&mut s, key(KeyCode::Enter));
    s.status = RuntimeStatus::Idle;
    let effects = leveler_tui::reducer::drain_queued(&mut s);
    assert!(!effects.is_empty());
    assert_eq!(s.input_queues.pending, vec!["next".to_string()]);

    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AssistantMessageStarted {
            message_id: MessageId::new("m1"),
        }),
    );

    assert!(s.input_queues.pending.is_empty());
}

#[test]
fn runtime_user_echo_does_not_clear_pending_input() {
    let mut s = opened();
    s.status = RuntimeStatus::Busy;
    typed(&mut s, "next");
    reduce(&mut s, key(KeyCode::Enter));
    s.status = RuntimeStatus::Idle;
    let effects = leveler_tui::reducer::drain_queued(&mut s);
    assert!(!effects.is_empty());
    assert_eq!(s.input_queues.pending, vec!["next".to_string()]);

    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::UserMessageAdded {
            message: UiMessage {
                id: MessageId::new("u1"),
                role: UiRole::User,
                text: "next".into(),
            },
        }),
    );

    assert_eq!(
        s.input_queues.pending,
        vec!["next".to_string()],
        "user echo is not a turn-start signal"
    );
}

#[test]
fn turn_failed_does_not_auto_retry_pending_input() {
    let mut s = opened();
    s.status = RuntimeStatus::Busy;
    for msg in ["first", "second"] {
        typed(&mut s, msg);
        reduce(&mut s, key(KeyCode::Enter));
    }

    s.status = RuntimeStatus::Idle;
    let first = leveler_tui::reducer::drain_queued(&mut s);
    assert!(
        matches!(
            first.as_slice(),
            [Effect::Send(ClientCommand::SubmitMessage { content, .. })] if content == "first"
        ),
        "first queued input sends: {first:?}"
    );

    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::TurnFailed {
            error: "transport failed before start".into(),
        }),
    );
    s.status = RuntimeStatus::Idle;
    assert!(s.input_queues.pending.is_empty());
    assert!(
        s.input_queues.rejected.is_empty(),
        "failed model turns must not auto-retry the same prompt"
    );
    assert_eq!(s.input_queues.queued, vec!["second".to_string()]);

    let next = leveler_tui::reducer::drain_queued(&mut s);
    assert!(
        matches!(
            next.as_slice(),
            [Effect::Send(ClientCommand::SubmitMessage { content, .. })] if content == "second"
        ),
        "failure should not replay the failed prompt before queued input: {next:?}"
    );
    assert!(s.input_queues.queued.is_empty());
}

#[test]
fn every_menu_slash_command_is_handled() {
    // Guard against menu/handler drift: every command advertised in the slash
    // popup must be wired in the reducer (not fall through to "未知命令").
    // This caught `/tools` being listed but unhandled.
    for name in leveler_tui::screen::SLASH_NAMES {
        let mut s = state();
        reduce(&mut s, Action::Paste((*name).to_string()));
        reduce(&mut s, key(KeyCode::Enter));
        let unknown = s
            .notification
            .as_ref()
            .map(|n| n.message.contains("未知命令"))
            .unwrap_or(false);
        assert!(
            !unknown,
            "menu command {name} is advertised but not handled"
        );
    }
}

#[test]
fn skill_slash_select_sends_dollar_mention() {
    // S2: /skill <name> [task] must SubmitMessage with `$name …` so agent S1 injects.
    let mut s = state();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot(),
        }),
    );
    reduce(&mut s, Action::Paste("/skill demo please ship".into()));
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Send(ClientCommand::SubmitMessage { content, .. })]
                if content == "$demo please ship"
        ),
        "expected $demo submit, got {effects:?}"
    );
    assert!(s.composer.is_empty());
}

#[test]
fn skill_slash_list_does_not_send_when_no_args() {
    let mut s = state();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot(),
        }),
    );
    reduce(&mut s, Action::Paste("/skill".into()));
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(
        effects.is_empty(),
        "list-only /skill should not submit: {effects:?}"
    );
    let msg = s
        .notification
        .as_ref()
        .map(|n| n.message.as_str())
        .unwrap_or("");
    assert!(
        msg.contains("技能") || msg.contains("skill") || msg.contains("/skill"),
        "should notify about skills list/usage: {msg}"
    );
}

#[test]
fn backspace_on_empty_composer_removes_last_attachment() {
    use leveler_client_protocol::{AttachmentId, AttachmentKind, AttachmentRef};
    let att = |n: &str| AttachmentRef {
        id: AttachmentId::new(n),
        kind: AttachmentKind::Image,
        name: n.to_string(),
        mime_type: "image/png".into(),
        size_bytes: 1,
        sha256: "x".into(),
        width: None,
        height: None,
    };
    let mut s = state();
    s.pending_attachments = vec![att("a"), att("b")];

    // Empty composer: Backspace removes the last attachment chip.
    reduce(&mut s, key(KeyCode::Backspace));
    assert_eq!(s.pending_attachments.len(), 1);
    reduce(&mut s, key(KeyCode::Backspace));
    assert!(s.pending_attachments.is_empty());

    // With typed text present, Backspace edits text and leaves attachments alone.
    s.pending_attachments = vec![att("c")];
    reduce(&mut s, key(KeyCode::Char('x')));
    reduce(&mut s, key(KeyCode::Backspace));
    assert_eq!(
        s.pending_attachments.len(),
        1,
        "text backspace must not drop attachments"
    );
    assert!(s.composer.is_empty());
}

// ---- 修复回归锁定：排队草稿 / 未知命令 / Alt+Backspace ----------------------

#[test]
fn drain_queued_preserves_in_progress_draft() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AssistantMessageStarted {
            message_id: MessageId::new("m1"),
        }),
    );
    assert_eq!(s.status, RuntimeStatus::Busy);
    typed(&mut s, "queued msg");
    reduce(&mut s, key(KeyCode::Enter));
    assert_eq!(s.input_queues.queued, vec!["queued msg".to_string()]);

    // The user starts typing the NEXT message while the turn is still busy.
    typed(&mut s, "draft");
    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCompleted));

    let effects = leveler_tui::reducer::drain_queued(&mut s);
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Send(ClientCommand::SubmitMessage { content, .. })] if content == "queued msg"
        ),
        "queued input submits: {effects:?}"
    );
    assert_eq!(
        s.composer.text(),
        "draft",
        "draining the queue must not clobber the draft being typed"
    );
}

#[test]
fn completed_turn_does_not_guess_input_suggestion_from_freeform_answer() {
    let mut s = opened();
    let id = MessageId::new("handoff");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AssistantMessageStarted {
            message_id: id.clone(),
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AssistantTextDelta {
            message_id: id.clone(),
            delta: "权限检查已经完成。\n\n下一步：运行标签预置脚本。".into(),
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AssistantMessageCompleted { message_id: id }),
    );

    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCompleted));

    assert!(matches!(
        s.transcript.items().last(),
        Some(TranscriptItem::TurnEnd(_))
    ));
    assert!(
        s.composer.is_empty(),
        "freeform assistant text must not be promoted into user input"
    );
}

#[test]
fn turn_handoff_never_overwrites_a_draft_typed_while_busy() {
    let mut s = opened();
    let id = MessageId::new("handoff");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AssistantMessageStarted {
            message_id: id.clone(),
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AssistantTextDelta {
            message_id: id.clone(),
            delta: "已完成。下一步：运行完整测试。".into(),
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AssistantMessageCompleted { message_id: id }),
    );
    typed(&mut s, "我自己的下一条消息");

    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCompleted));

    assert_eq!(s.composer.text(), "我自己的下一条消息");
    assert!(matches!(
        s.transcript.items().last(),
        Some(TranscriptItem::TurnEnd(_))
    ));
}

#[test]
fn incomplete_turn_prefills_continue_when_no_specific_next_step_exists() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::TurnIncomplete {
            reason: "本轮资源窗口已用完，当前改动已经保留".into(),
        }),
    );

    assert_eq!(s.composer.text(), "继续");
    assert!(matches!(
        s.transcript.items().last(),
        Some(TranscriptItem::TurnEnd(end))
            if end.detail.as_deref() == Some("本轮资源窗口已用完，当前改动已经保留")
    ));
}

#[test]
fn goal_completion_uses_structured_summary_only_for_the_input_suggestion() {
    let mut s = opened();
    let id = ToolCallId::new("goal");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ToolCallStarted {
            id: id.clone(),
            name: "update_goal".into(),
            arguments: serde_json::json!({
                "status": "complete",
                "summary": "实现和测试都已完成。",
                "next_step": "提交当前改动"
            })
            .to_string(),
            parallel: false,
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            id,
            ok: true,
            preview: "目标已完成".into(),
            duration_ms: 10,
        }),
    );

    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCompleted));

    assert!(matches!(
        s.transcript.items().last(),
        Some(TranscriptItem::Recap(block))
            if block.next_step == "提交当前改动"
    ));
    assert_eq!(s.composer.text(), "提交当前改动");
}

#[test]
fn goal_completion_without_structured_next_step_has_no_suggestion() {
    let mut s = opened();
    let id = ToolCallId::new("goal-no-next");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ToolCallStarted {
            id: id.clone(),
            name: "update_goal".into(),
            arguments: serde_json::json!({
                "status": "complete",
                "summary": "实现和测试都已完成。"
            })
            .to_string(),
            parallel: false,
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            id,
            ok: true,
            preview: "目标已完成".into(),
            duration_ms: 10,
        }),
    );

    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCompleted));

    assert!(s.composer.is_empty());
}

#[test]
fn unknown_slash_command_keeps_composer_content() {
    let mut s = opened();
    typed(&mut s, "/tmp 目录是干嘛的");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(effects.is_empty());
    assert_eq!(
        s.composer.text(),
        "/tmp 目录是干嘛的",
        "unknown command must not swallow the typed text"
    );
    assert!(s.notification.is_some(), "warn about the unknown command");
}

#[test]
fn known_slash_command_still_runs() {
    let mut s = opened();
    typed(&mut s, "/help");
    reduce(&mut s, key(KeyCode::Enter));
    assert_eq!(s.active_screen, Screen::Help);
    assert!(s.composer.is_empty());
}

#[test]
fn plain_message_with_default_chat_collab_stays_chat() {
    let mut s = opened();
    // Default chat mode has no update_goal chrome on plain Enter.
    assert_eq!(s.collaboration, "chat");
    typed(&mut s, "你好");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Send(ClientCommand::SubmitMessage { content, .. })] if content == "你好"
        ),
        "submit still uses SubmitMessage (runtime maps collab→profile): {effects:?}"
    );
    assert!(
        !s.goal_mode_active,
        "default collaboration=chat must not mark goal_mode_active"
    );
}

#[test]
fn plain_message_with_goal_collab_marks_goal_mode() {
    let mut s = opened();
    s.collaboration = "goal".into();
    typed(&mut s, "实现登录");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Send(ClientCommand::SubmitMessage { content, .. })] if content == "实现登录"
        ),
        "{effects:?}"
    );
    assert!(
        s.goal_mode_active,
        "explicit collaboration=goal must mark goal_mode_active"
    );
}

#[test]
fn plain_message_with_chat_collab_stays_chat() {
    let mut s = opened();
    s.collaboration = "chat".into();
    typed(&mut s, "随便聊聊");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(matches!(
        effects.as_slice(),
        [Effect::Send(ClientCommand::SubmitMessage { content, .. })] if content == "随便聊聊"
    ));
    assert!(!s.goal_mode_active);
}

#[test]
fn slash_goal_runs_explicit_goal_command() {
    let mut s = opened();
    typed(&mut s, "/goal 修复红叉显示");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Send(ClientCommand::RunGoal { content, .. })]
                if content == "修复红叉显示"
        ),
        "/goal should run the explicit goal path: {effects:?}"
    );
    assert!(s.composer.is_empty());
    assert!(s.goal_mode_active);
    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCompleted));
    assert!(!s.goal_mode_active);
}

#[test]
fn slash_goal_requires_a_goal() {
    let mut s = opened();
    typed(&mut s, "/goal");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(effects.is_empty());
    assert!(
        s.notification
            .as_ref()
            .is_some_and(|n| n.message.contains("用法: /goal"))
    );
}

#[test]
fn alt_backspace_deletes_word_not_attachment() {
    let mut s = opened();
    s.pending_attachments = vec![leveler_client_protocol::AttachmentRef {
        id: leveler_client_protocol::AttachmentId::new("a1"),
        kind: leveler_client_protocol::AttachmentKind::Image,
        name: "a1".to_string(),
        mime_type: "image/png".into(),
        size_bytes: 1,
        sha256: "x".into(),
        width: None,
        height: None,
    }];
    typed(&mut s, "hello world");
    reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT)),
    );
    assert_eq!(s.composer.text(), "hello ", "Alt+Backspace deletes a word");
    assert_eq!(
        s.pending_attachments.len(),
        1,
        "attachments only pop via plain Backspace on an empty composer"
    );
}

#[test]
fn activity_clears_when_the_tool_completes() {
    let mut s = opened();
    s.reasoning = "stale previous-step reasoning".into();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ToolCallStarted {
            id: ToolCallId::new("t1"),
            name: "read_file".into(),
            arguments: r#"{"path":"src/lib.rs"}"#.into(),
            parallel: false,
        }),
    );
    assert!(s.activity.is_some());
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            id: ToolCallId::new("t1"),
            ok: true,
            preview: "ok".into(),
            duration_ms: 3,
        }),
    );
    assert!(
        s.activity.is_none(),
        "a finished tool must not linger in the status line while the model thinks"
    );
    assert!(
        s.reasoning.is_empty(),
        "completed tools must clear stale reasoning before waiting for the next model step"
    );
}

#[test]
fn empty_end_requests_jump_to_bottom() {
    let mut s = opened();
    assert!(s.composer.is_empty());
    assert!(!s.jump_to_bottom);
    reduce(&mut s, key(KeyCode::End));
    assert!(
        s.jump_to_bottom,
        "empty End should request live-edge rebuild"
    );
    assert_eq!(
        s.notification.as_ref().map(|n| n.message.as_str()),
        Some("已回到底部")
    );
}

#[test]
fn end_with_composer_text_moves_to_line_end_not_jump() {
    let mut s = opened();
    s.composer.insert_char('a');
    s.composer.insert_char('b');
    s.composer.move_left();
    reduce(&mut s, key(KeyCode::End));
    assert!(
        !s.jump_to_bottom,
        "End with text must keep end-of-line editing"
    );
    let (_row, col) = s.composer.cursor_row_col_display();
    assert_eq!(col, 2, "cursor should sit at end of 'ab'");
}

#[test]
fn ctrl_end_requests_jump_to_bottom_even_with_text() {
    let mut s = opened();
    s.composer.insert_char('x');
    reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL)),
    );
    assert!(
        s.jump_to_bottom,
        "Ctrl+End always rebuilds the live edge (Approach A)"
    );
}

#[test]
fn ctrl_down_also_requests_jump_to_bottom() {
    let mut s = opened();
    s.composer.insert_char('x');
    reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::CONTROL)),
    );
    assert!(
        s.jump_to_bottom,
        "Ctrl+↓ is the macOS-friendly jump-to-bottom shortcut"
    );
}

#[test]
fn a_new_model_step_replaces_the_previous_step_reasoning() {
    let mut s = opened();
    // Step 1: the model thinks, then calls a tool. A tool-only step never emits
    // assistant text, so nothing closes the thought except the tool call itself.
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ReasoningDelta {
            delta: "先读一遍源码".into(),
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ToolCallStarted {
            id: ToolCallId::new("t1"),
            name: "read_file".into(),
            arguments: "{}".into(),
            parallel: false,
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            id: ToolCallId::new("t1"),
            ok: true,
            preview: "ok".into(),
            duration_ms: 5,
        }),
    );

    // Step 2: the model thinks again. This is a new thought, not a continuation
    // of the last one — it must replace it, not concatenate onto it.
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ReasoningDelta {
            delta: "再补测试".into(),
        }),
    );

    assert_eq!(
        s.reasoning, "再补测试",
        "each model step's reasoning replaces the previous step's, \
         otherwise the thinking footer reads as one run-on paragraph"
    );
}

#[test]
fn retry_attempt_reset_removes_divergent_transient_output() {
    let mut s = opened();
    let stale = MessageId::new("stale");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AssistantMessageStarted {
            message_id: stale.clone(),
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AssistantTextDelta {
            message_id: stale.clone(),
            delta: "wrong prefix".into(),
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ReasoningDelta {
            delta: "wrong thought".into(),
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AssistantAttemptReset {
            message_id: Some(stale),
        }),
    );

    assert!(s.transcript.items().iter().all(|item| {
        !matches!(item, TranscriptItem::Assistant(block) if block.text == "wrong prefix")
    }));
    assert!(s.reasoning.is_empty());
}

#[test]
fn shift_up_down_navigates_user_turns_without_clearing_draft() {
    let mut s = opened();
    s.transcript.push_user("first question".into());
    s.transcript.push_user("second question".into());
    s.transcript.push_user("third question".into());
    // User is typing a new draft mid-navigation.
    typed(&mut s, "unsent draft text");
    assert_eq!(s.composer.text(), "unsent draft text");
    assert!(s.turn_nav.is_none());

    reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT)),
    );
    assert_eq!(s.turn_nav, Some(2), "Shift+Up lands on newest user turn");
    assert_eq!(
        s.composer.text(),
        "unsent draft text",
        "draft must survive turn nav"
    );

    reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT)),
    );
    assert_eq!(s.turn_nav, Some(1));
    assert_eq!(s.composer.text(), "unsent draft text");

    reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT)),
    );
    assert_eq!(s.turn_nav, Some(2));

    // Past newest → live edge.
    reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT)),
    );
    assert_eq!(s.turn_nav, None, "Shift+Down past end returns to live");
    assert_eq!(s.composer.text(), "unsent draft text");

    // Esc while reviewing clears nav.
    reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT)),
    );
    assert!(s.turn_nav.is_some());
    reduce(&mut s, key(KeyCode::Esc));
    assert!(s.turn_nav.is_none());
    assert_eq!(s.composer.text(), "unsent draft text");
}

#[test]
fn page_up_down_on_conversation_preserves_draft() {
    let mut s = opened();
    typed(&mut s, "keep me");
    reduce(&mut s, key(KeyCode::PageUp));
    reduce(&mut s, key(KeyCode::PageDown));
    assert_eq!(s.composer.text(), "keep me");
}

// ---- Input history (↑/↓) ---------------------------------------------------

#[test]
fn empty_up_recalls_submission_history_not_scroll() {
    let mut s = opened();
    // Submit two tasks into history via Enter path.
    typed(&mut s, "修复登录问题");
    reduce(&mut s, key(KeyCode::Enter));
    assert!(s.composer.is_empty());
    typed(&mut s, "增加测试");
    reduce(&mut s, key(KeyCode::Enter));
    assert!(s.composer.is_empty());

    // Input focus (default): empty ↑ recalls history, not conversation scroll.
    assert_eq!(s.workbench_focus, leveler_tui::state::WorkbenchFocus::Input);
    reduce(&mut s, key(KeyCode::Up));
    assert_eq!(s.composer.text(), "增加测试");
    reduce(&mut s, key(KeyCode::Up));
    assert_eq!(s.composer.text(), "修复登录问题");

    // ↓ restores forward through history then empty draft.
    reduce(&mut s, key(KeyCode::Down));
    assert_eq!(s.composer.text(), "增加测试");
    reduce(&mut s, key(KeyCode::Down));
    assert!(s.composer.is_empty());
}

#[test]
fn up_stashes_in_progress_draft() {
    let mut s = opened();
    typed(&mut s, "历史任务");
    reduce(&mut s, key(KeyCode::Enter));
    typed(&mut s, "修复");
    reduce(&mut s, key(KeyCode::Up));
    assert_eq!(s.composer.text(), "历史任务");
    reduce(&mut s, key(KeyCode::Down));
    assert_eq!(s.composer.text(), "修复");
}

#[test]
fn tab_toggles_workbench_focus_and_arrows_diverge() {
    use leveler_tui::state::WorkbenchFocus;
    let mut s = opened();
    typed(&mut s, "历史任务");
    reduce(&mut s, key(KeyCode::Enter));
    assert_eq!(s.workbench_focus, WorkbenchFocus::Input);

    // Tab → Conversation: ↑ scrolls (does not rewrite composer history).
    reduce(&mut s, key(KeyCode::Tab));
    assert_eq!(s.workbench_focus, WorkbenchFocus::Conversation);
    s.conversation_auto_scroll = true;
    s.conversation_scroll = 0;
    // Force non-empty content so scroll math can leave bottom.
    s.conversation_auto_scroll = false;
    s.conversation_scroll = 0;
    reduce(&mut s, key(KeyCode::Up));
    assert!(
        s.composer.is_empty(),
        "conversation ↑ must not fill history"
    );
    assert!(!s.conversation_auto_scroll);

    // Tab back → Input: ↑ recalls history.
    reduce(&mut s, key(KeyCode::Tab));
    assert_eq!(s.workbench_focus, WorkbenchFocus::Input);
    reduce(&mut s, key(KeyCode::Up));
    assert_eq!(s.composer.text(), "历史任务");
}

#[test]
fn page_up_pins_away_from_bottom_and_enter_jumps_back() {
    use leveler_tui::state::WorkbenchFocus;
    let mut s = opened();
    s.size = (100, 40);
    // Seed enough transcript height to allow scroll-away.
    for i in 0..30 {
        s.transcript.push_user(format!("msg {i}"));
    }
    reduce(&mut s, key(KeyCode::PageUp));
    assert_eq!(s.workbench_focus, WorkbenchFocus::Conversation);
    assert!(!s.conversation_auto_scroll);

    // Enter while Conversation focus + not at bottom → jump live.
    reduce(&mut s, key(KeyCode::Enter));
    assert!(s.conversation_auto_scroll);
    assert_eq!(s.conversation_unread, 0);
}

#[test]
fn mouse_scroll_moves_conversation_and_disables_auto_follow() {
    use crossterm::event::{KeyModifiers, MouseEvent, MouseEventKind};
    use leveler_tui::state::WorkbenchFocus;
    let mut s = opened();
    s.size = (100, 40);
    for i in 0..40 {
        s.transcript.push_user(format!("line {i}"));
    }
    // Publish a conversation rect so drag/click paths work; scroll wheel does not need it.
    s.conversation_rect = Some((0, 3, 100, 20));
    s.conversation_auto_scroll = true;
    let mouse = MouseEvent {
        kind: MouseEventKind::ScrollUp,
        column: 10,
        row: 10,
        modifiers: KeyModifiers::empty(),
    };
    reduce(&mut s, Action::Mouse(mouse));
    assert!(
        !s.conversation_auto_scroll,
        "wheel must pin away from live edge"
    );
    assert_eq!(s.workbench_focus, WorkbenchFocus::Conversation);
}

#[test]
fn mouse_drag_select_disables_auto_follow_and_sets_anchor() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    let mut s = opened();
    s.size = (80, 40);
    for i in 0..50 {
        s.transcript.push_user(format!("row content {i:02}"));
    }
    s.conversation_rect = Some((0, 2, 80, 20));
    s.conversation_auto_scroll = true;
    s.conversation_scroll = 0;
    // Warm plain cache for width.
    reduce(
        &mut s,
        Action::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 5,
            modifiers: KeyModifiers::empty(),
        }),
    );
    assert!(
        !s.conversation_auto_scroll,
        "selection must pin viewport away from live edge"
    );
    assert!(s.selection.dragging);
    assert!(s.selection.anchor.is_some());
    assert_eq!(s.selection_last_mouse, Some((5, 5)));
}

#[test]
fn mouse_drag_bottom_edge_arms_auto_scroll_and_tick_extends() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    let mut s = opened();
    s.size = (80, 40);
    for i in 0..80 {
        s.transcript.push_user(format!("line-{i:03}-padding-text"));
    }
    // Conversation at rows 2..22 (height 20). Bottom edge = last 2 rows → 20,21.
    s.conversation_rect = Some((0, 2, 80, 20));
    s.conversation_auto_scroll = false;
    s.conversation_scroll = 0;

    reduce(
        &mut s,
        Action::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 4,
            row: 5,
            modifiers: KeyModifiers::empty(),
        }),
    );
    let start = s.selection.anchor.expect("anchor");

    // Drag into bottom edge hot zone.
    reduce(
        &mut s,
        Action::Mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 4,
            row: 21,
            modifiers: KeyModifiers::empty(),
        }),
    );
    assert_eq!(
        s.selection_edge_dir, 1,
        "bottom edge must arm downward scroll"
    );
    assert!(s.selection.dragging);
    let before_scroll = s.conversation_scroll;

    // Continuous tick should scroll and keep auto_follow off.
    for _ in 0..5 {
        reduce(&mut s, Action::SelectionTick);
    }
    assert!(
        s.conversation_scroll > before_scroll,
        "edge ticks must advance scroll: before={before_scroll} after={}",
        s.conversation_scroll
    );
    assert!(
        !s.conversation_auto_scroll,
        "must stay pinned while selecting"
    );
    let focus = s.selection.focus.expect("focus");
    assert!(
        focus.row >= start.row,
        "selection must extend with scroll: start={start:?} focus={focus:?}"
    );
}

#[test]
fn mouse_drag_top_edge_arms_upward_scroll() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    let mut s = opened();
    s.size = (80, 40);
    for i in 0..80 {
        s.transcript.push_user(format!("up-line-{i:03}"));
    }
    s.conversation_rect = Some((0, 2, 80, 20));
    s.conversation_auto_scroll = false;
    s.conversation_scroll = 30;

    reduce(
        &mut s,
        Action::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 4,
            row: 10,
            modifiers: KeyModifiers::empty(),
        }),
    );
    reduce(
        &mut s,
        Action::Mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 4,
            row: 2, // top edge
            modifiers: KeyModifiers::empty(),
        }),
    );
    assert_eq!(s.selection_edge_dir, -1);
    let before = s.conversation_scroll;
    reduce(&mut s, Action::SelectionTick);
    assert!(
        s.conversation_scroll < before,
        "top-edge tick scrolls up: before={before} after={}",
        s.conversation_scroll
    );
}

#[test]
fn shift_mouse_does_not_start_app_selection() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    let mut s = opened();
    s.conversation_rect = Some((0, 2, 80, 20));
    reduce(
        &mut s,
        Action::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 5,
            modifiers: KeyModifiers::SHIFT,
        }),
    );
    assert!(
        !s.selection.dragging,
        "Shift+mouse is reserved for terminal-native selection"
    );
}

#[test]
fn selection_tick_noop_when_not_dragging() {
    let mut s = opened();
    s.selection_edge_dir = 1;
    s.conversation_scroll = 0;
    reduce(&mut s, Action::SelectionTick);
    assert_eq!(s.conversation_scroll, 0);
}

#[test]
fn mouse_click_scroll_bottom_restores_auto_follow() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    let mut s = opened();
    s.conversation_auto_scroll = false;
    s.conversation_scroll = 0;
    s.conversation_unread = 3;
    s.scroll_bottom_rect = Some((40, 20, 6, 1));
    let mouse = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 42,
        row: 20,
        modifiers: KeyModifiers::empty(),
    };
    reduce(&mut s, Action::Mouse(mouse));
    assert!(s.conversation_auto_scroll);
    assert_eq!(s.conversation_unread, 0);
}

#[test]
fn mouse_scroll_over_input_keeps_input_focus() {
    use crossterm::event::{KeyModifiers, MouseEvent, MouseEventKind};
    use leveler_tui::state::WorkbenchFocus;
    let mut s = opened();
    s.workbench_focus = WorkbenchFocus::Input;
    s.input_rect = Some((0, 20, 100, 4));
    s.conversation_rect = Some((0, 3, 100, 15));
    let mouse = MouseEvent {
        kind: MouseEventKind::ScrollUp,
        column: 10,
        row: 21,
        modifiers: KeyModifiers::empty(),
    };
    reduce(&mut s, Action::Mouse(mouse));
    assert_eq!(
        s.workbench_focus,
        WorkbenchFocus::Input,
        "wheel over input must not steal focus from history/typing"
    );
}

#[test]
fn mouse_click_input_restores_input_focus_for_history() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use leveler_tui::state::WorkbenchFocus;
    let mut s = opened();
    typed(&mut s, "历史任务");
    reduce(&mut s, key(KeyCode::Enter));
    s.workbench_focus = WorkbenchFocus::Conversation;
    s.input_rect = Some((0, 20, 100, 4));
    s.conversation_rect = Some((0, 3, 100, 15));
    let click = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 5,
        row: 21,
        modifiers: KeyModifiers::empty(),
    };
    reduce(&mut s, Action::Mouse(click));
    assert_eq!(s.workbench_focus, WorkbenchFocus::Input);
    reduce(&mut s, key(KeyCode::Up));
    assert_eq!(s.composer.text(), "历史任务");
}

#[test]
fn mouse_drag_selects_conversation_text() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    let mut s = opened();
    s.size = (80, 30);
    s.transcript.push_user("hello world selection".into());
    s.conversation_rect = Some((0, 3, 80, 20));
    s.conversation_auto_scroll = true;
    s.conversation_plain = vec!["hello world selection".into()];
    s.conversation_plain_width = 80;

    let down = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 0,
        row: 3,
        modifiers: KeyModifiers::empty(),
    };
    reduce(&mut s, Action::Mouse(down));
    assert!(s.selection.dragging);

    let drag = MouseEvent {
        kind: MouseEventKind::Drag(MouseButton::Left),
        column: 11,
        row: 3,
        modifiers: KeyModifiers::empty(),
    };
    reduce(&mut s, Action::Mouse(drag));
    let range = s.selection.range().expect("range");
    assert!(
        range.0.col < range.1.col || range.0.row != range.1.row,
        "drag should span columns"
    );

    let up = MouseEvent {
        kind: MouseEventKind::Up(MouseButton::Left),
        column: 11,
        row: 3,
        modifiers: KeyModifiers::empty(),
    };
    reduce(&mut s, Action::Mouse(up));
    assert!(!s.selection.dragging);
    assert!(!s.selection.is_empty());
}
