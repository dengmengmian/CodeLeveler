//! Reducer tests: runtime events fold into state; keys edit/submit/cancel (§69.1).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use leveler_client_protocol::ToolCallId;
use leveler_client_protocol::{
    ApprovalDecision, ApprovalId, ClientCommand, FinalizationStage, MessageId, PermissionProfile,
    RuntimeEvent, RuntimeStatus, SessionId, UiActiveToolCall, UiApprovalRequest, UiCheckpoint,
    UiCompletionReport, UiMessage, UiPlan, UiPlanStep, UiReasoningState, UiRole, UiSessionSnapshot,
};
use leveler_tui::action::{Action, Effect, EffectCompletion};
use leveler_tui::btw::{BtwTurnState, SurfaceFocus};
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

/// Render the whole screen to text. Some contracts — a status line's single
/// elapsed — are only true of what the user sees, not of any one field.
fn rendered(state: &mut AppState, w: u16, h: u16) -> String {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| leveler_tui::render::render(f, state))
        .unwrap();
    let buf = term.backend().buffer();
    let mut out = String::new();
    for y in 0..h {
        let mut x = 0u16;
        while x < w {
            let sym = buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ");
            out.push_str(sym);
            x += unicode_width::UnicodeWidthStr::width(sym).max(1) as u16;
        }
        out.push('\n');
    }
    out
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
            untrusted_config: Vec::new(),
            reasoning_effort: None,
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
        finalization_stage: None,
        messages: Vec::new(),
        pending_interactions: Vec::new(),
        available_models: vec![
            leveler_client_protocol::ModelRef::parse("deepseek/v3").unwrap(),
            leveler_client_protocol::ModelRef::parse("glm/5").unwrap(),
        ],
        vision: false,
        last_sequence: None,
        active_tools: Vec::new(),
        active_background_tasks: Vec::new(),
        plan: None,
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

#[test]
fn reconnect_during_finalization_restores_busy_without_waiting_for_model() {
    let mut state = state();
    let mut snap = snapshot();
    snap.status = "running".into();
    snap.finalization_stage = Some(leveler_client_protocol::FinalizationStage::Review);

    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::SessionOpened { session: snap }),
    );

    assert_eq!(state.status, RuntimeStatus::Busy);
    assert_eq!(
        state.finalization_stage,
        Some(leveler_client_protocol::FinalizationStage::Review)
    );
    let frame = rendered(&mut state, 100, 24);
    assert!(frame.contains("正在审查"), "{frame}");
    assert!(!frame.contains("等待模型"), "{frame}");
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
        s.transcript.is_empty(),
        "no welcome card or other blocks may be injected on open"
    );
}

#[test]
fn session_updated_adopts_product_axes_from_snapshot() {
    let mut s = state();
    // Boot defaults are balanced/chat; the runtime (session record) says
    // otherwise — the snapshot value must win over the local guess.
    let mut snap = snapshot();
    snap.work_profile = Some("delivery".into());
    snap.collaboration = Some("goal".into());
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionUpdated { session: snap }),
    );
    assert_eq!(s.work_profile, "delivery");
    assert_eq!(s.collaboration, "goal");

    // An old runtime without the fields must NOT clobber the local state.
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionUpdated {
            session: snapshot(),
        }),
    );
    assert_eq!(s.work_profile, "delivery");
    assert_eq!(s.collaboration, "goal");
}

#[test]
fn session_updated_copies_effective_reasoning_from_snapshot() {
    let mut s = state();
    s.reasoning_effort = Some("low".into());
    let mut snap = snapshot();
    snap.reasoning = Some(UiReasoningState {
        effective: Some("max".into()),
    });
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionUpdated { session: snap }),
    );
    assert_eq!(s.reasoning_effort.as_deref(), Some("max"));
}

#[test]
fn session_updated_without_reasoning_keeps_boot_effort() {
    let mut s = state();
    s.reasoning_effort = Some("max".into());
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionUpdated {
            session: snapshot(),
        }),
    );
    assert_eq!(s.reasoning_effort.as_deref(), Some("max"));
}

#[test]
fn session_updated_clears_effort_when_runtime_says_none() {
    let mut s = state();
    s.reasoning_effort = Some("max".into());
    let mut snap = snapshot();
    snap.reasoning = Some(UiReasoningState { effective: None });
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionUpdated { session: snap }),
    );
    assert_eq!(s.reasoning_effort, None);
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
                ordinal: None,
                kind: None,
                images: 0,
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

#[cfg(any())]
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
#[cfg(any())]
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

/// An Unverified turn's own label already states the verification fact
/// ("not auto-verified"). A check count beside it is a second authority on the
/// same line — a real round ended `✓ 完成 · 未自动验证 · 9 次工具 · 1m 03s · 验证 1/1`,
/// which reads as "not verified" and "verified 1/1" at once.
#[cfg(any())]
#[test]
fn unverified_turn_end_omits_the_verify_count_too() {
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
                passed: None,
            },
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::TurnCompletedUnverified {
            reason: leveler_client_protocol::REASON_NO_AUTOMATIC_VERIFICATION.into(),
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
        !summary.contains("验证") && !summary.contains("verify"),
        "an unverified turn must not also count verifications; summary={summary:?}"
    );
}

#[cfg(any())]
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
    answer(&mut s, "m-final", "检查都通过了。");
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
        summary.contains(leveler_tui::Locale::Zh.text().summary_verify_ok),
        "Completed with green gates may show the success verify mark; summary={summary:?}"
    );
}

/// A verification result belongs to the turn that ran it. Dogfood: the first
/// turn's green `cargo test` put "验证 ✓" on every later turn — a script run,
/// a plain answer, even a restarted session — while each of those turns
/// recorded `verification=not_run`.
#[cfg(any())]
#[test]
fn a_later_turn_without_verification_does_not_inherit_the_verify_mark() {
    use leveler_client_protocol::{CheckState, UiCheck, UiVerification};
    let zh = leveler_tui::Locale::Zh.text();
    let last_summary = |s: &AppState| -> String {
        match s
            .transcript
            .items()
            .iter()
            .rev()
            .find(|item| matches!(item, TranscriptItem::TurnEnd(_)))
        {
            Some(TranscriptItem::TurnEnd(end)) => end.summary.clone().unwrap_or_default(),
            _ => panic!("expected turn end"),
        }
    };
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
    answer(&mut s, "m-1", "修好了。");
    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCompleted));
    assert!(last_summary(&s).contains(zh.summary_verify_ok));

    answer(&mut s, "m-2", "脚本跑完了。");
    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnAnswered));
    let second = last_summary(&s);
    assert!(
        !second.contains(zh.summary_verify_ok),
        "a turn that ran no verification must not show one: {second:?}"
    );

    // The verification screen still shows the latest result.
    assert!(s.verification.is_some());
}

/// A reopened session starts with the last verification on its screen, but
/// the next turn has not verified anything.
#[cfg(any())]
#[test]
fn a_restored_verification_is_not_the_next_turns_verification() {
    let zh = leveler_tui::Locale::Zh.text();
    let mut s = state();
    let mut restored = snapshot();
    restored.verification = Some(leveler_client_protocol::UiVerification {
        checks: vec![],
        passed: Some(true),
    });
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened { session: restored }),
    );
    answer(&mut s, "m-1", "一句话答案。");
    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnAnswered));
    let text = format!("{:?}", s.transcript.items());
    assert!(!text.contains(zh.summary_verify_ok), "{text}");
    assert!(s.verification.is_some());
}

/// Ask a side question the way the UI does: type `/btw <q>` then Enter.
fn ask_btw(s: &mut AppState, q: &str) -> Vec<Effect> {
    s.composer.replace(format!("/btw {q}"));
    reduce(s, key(KeyCode::Enter))
}

/// The runtime announced the side answer started.
fn side_answer_started(s: &mut AppState, q: &str) {
    reduce(
        s,
        Action::Runtime(RuntimeEvent::BtwStarted { question: q.into() }),
    );
}

#[test]
fn btw_events_fill_the_side_thread_not_the_main_transcript() {
    let mut s = opened();
    ask_btw(&mut s, "why?");
    side_answer_started(&mut s, "why?");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::BtwTextDelta {
            delta: "because".into(),
        }),
    );
    reduce(&mut s, Action::Runtime(RuntimeEvent::BtwCompleted));
    assert_eq!(s.btw.turns.len(), 1);
    assert_eq!(s.btw.turns[0].question, "why?");
    assert_eq!(s.btw.turns[0].answer, "because");
    assert_eq!(s.btw.turns[0].state, BtwTurnState::Done);
    assert!(
        s.transcript.items().is_empty(),
        "side turns must never enter the main transcript"
    );
    assert!(
        s.pending_submissions.is_empty(),
        "a side question must never stage a main turn"
    );
}

#[test]
fn btw_esc_returns_to_main_and_never_cancels_a_running_turn() {
    let mut s = opened();
    s.status = RuntimeStatus::Busy;
    ask_btw(&mut s, "现在做到哪了？");
    assert_eq!(s.surface, SurfaceFocus::Btw);
    let effects = reduce(&mut s, key(KeyCode::Esc));
    assert!(
        effects.is_empty(),
        "Esc on the side thread is navigation, not a command: {effects:?}"
    );
    assert_eq!(s.surface, SurfaceFocus::Main);
    assert_eq!(s.status, RuntimeStatus::Busy, "the main run must still run");
    assert!(
        !s.cancel_armed,
        "the side thread must not arm a main cancel"
    );
}

#[test]
fn btw_ctrl_c_stops_only_the_side_answer() {
    let mut s = opened();
    s.status = RuntimeStatus::Busy;
    ask_btw(&mut s, "q1");
    side_answer_started(&mut s, "q1");
    assert!(s.btw.generating);
    let effects = reduce(&mut s, ctrl('c'));
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Send(ClientCommand::CancelBtw { .. })]
        ),
        "Ctrl+C on the side thread must cancel only the side answer: {effects:?}"
    );
    assert_eq!(
        s.status,
        RuntimeStatus::Busy,
        "the main run must be untouched"
    );
    assert!(!s.cancel_armed);
    reduce(&mut s, Action::Runtime(RuntimeEvent::BtwCancelled));
    assert!(!s.btw.generating);
    assert_eq!(s.btw.turns.last().unwrap().state, BtwTurnState::Cancelled);
}

#[test]
fn btw_ctrl_c_without_an_answer_clears_the_draft_and_never_quits() {
    let mut s = opened();
    ask_btw(&mut s, "q1");
    typed(&mut s, "draft");
    let effects = reduce(&mut s, ctrl('c'));
    assert!(effects.is_empty(), "{effects:?}");
    assert!(s.composer.is_empty());
    assert!(s.running, "a side surface must never quit the app");
    assert!(!s.quit_armed);
}

#[test]
fn btw_reentry_keeps_the_side_thread() {
    let mut s = opened();
    ask_btw(&mut s, "q1");
    side_answer_started(&mut s, "q1");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::BtwTextDelta { delta: "a1".into() }),
    );
    reduce(&mut s, Action::Runtime(RuntimeEvent::BtwCompleted));
    reduce(&mut s, key(KeyCode::Esc));
    assert_eq!(s.surface, SurfaceFocus::Main);
    // `/btw` alone re-opens the existing thread, clearing nothing and sending
    // nothing.
    s.composer.replace("/btw");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(effects.is_empty(), "re-entry sends nothing: {effects:?}");
    assert_eq!(s.surface, SurfaceFocus::Btw);
    assert_eq!(s.btw.turns.len(), 1);
    assert_eq!(s.btw.turns[0].question, "q1");
    assert_eq!(s.btw.turns[0].answer, "a1");
}

#[test]
fn btw_header_tracks_the_live_main_status() {
    let mut s = opened();
    s.status = RuntimeStatus::Busy;
    ask_btw(&mut s, "q");
    let before = rendered(&mut s, 100, 24);
    assert!(
        before.contains("返回主线程"),
        "side-thread header: {before}"
    );
    // The main plan advances while the side thread holds the screen.
    s.plan = Some(UiPlan {
        steps: vec![
            UiPlanStep {
                index: 0,
                description: "一".into(),
                status: leveler_client_protocol::PlanStepStatus::Done,
            },
            UiPlanStep {
                index: 1,
                description: "二".into(),
                status: leveler_client_protocol::PlanStepStatus::Running,
            },
        ],
    });
    let after = rendered(&mut s, 100, 24);
    assert!(
        after.contains("第 2/2 步"),
        "header must project the live plan, not a snapshot: {after}"
    );
}

#[test]
fn btw_survives_main_completion_and_shows_the_verdict() {
    let mut s = opened();
    s.status = RuntimeStatus::Busy;
    ask_btw(&mut s, "q");
    side_answer_started(&mut s, "q");
    // Main finishes (with an answer) while the side thread is open: it must
    // not crash, and the header must move to the run's own terminal verdict.
    let message = MessageId::new("m1");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AssistantMessageStarted {
            message_id: message.clone(),
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AssistantTextDelta {
            message_id: message.clone(),
            delta: "done".into(),
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AssistantMessageCompleted {
            message_id: message,
        }),
    );
    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCompleted));
    assert_eq!(s.status, RuntimeStatus::Idle);
    let text = rendered(&mut s, 100, 24);
    assert!(text.contains("任务已完成"), "{text}");
    assert!(
        s.btw.generating,
        "the side answer's lifecycle is independent of the main turn"
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::BtwTextDelta { delta: "a".into() }),
    );
    reduce(&mut s, Action::Runtime(RuntimeEvent::BtwCompleted));
    assert_eq!(s.btw.turns[0].answer, "a");
}

#[test]
fn btw_reports_approval_without_taking_the_decision() {
    let mut s = opened();
    s.status = RuntimeStatus::Busy;
    ask_btw(&mut s, "q");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ApprovalRequested {
            request: approval_req(),
        }),
    );
    // The header must show the wait; the decision stays on Main.
    let text = rendered(&mut s, 100, 24);
    assert!(
        text.contains("审批"),
        "approval must be visible as state: {text}"
    );
    let effects = reduce(&mut s, key(KeyCode::Char('y')));
    assert!(
        !matches!(
            effects.as_slice(),
            [Effect::Send(ClientCommand::ApprovalDecision { .. })]
        ),
        "keys on the side thread must not answer a main approval: {effects:?}"
    );
    assert!(matches!(s.overlay, Some(Overlay::Approval(_))));
    // Returning to Main restores the decision surface intact.
    reduce(&mut s, key(KeyCode::Esc));
    assert_eq!(s.surface, SurfaceFocus::Main);
    assert!(matches!(s.overlay, Some(Overlay::Approval(_))));
}

#[test]
fn btw_stream_survives_navigation_to_main_and_back() {
    let mut s = opened();
    s.status = RuntimeStatus::Busy;
    ask_btw(&mut s, "q1");
    side_answer_started(&mut s, "q1");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::BtwTextDelta {
            delta: "part1 ".into(),
        }),
    );
    // Leave while the answer streams.
    let effects = reduce(&mut s, key(KeyCode::Esc));
    assert!(effects.is_empty());
    assert_eq!(s.status, RuntimeStatus::Busy);
    // The rest arrives while the side thread is hidden: the view being hidden
    // must not drop or reorder events.
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::BtwTextDelta {
            delta: "part2".into(),
        }),
    );
    reduce(&mut s, Action::Runtime(RuntimeEvent::BtwCompleted));
    s.composer.replace("/btw");
    reduce(&mut s, key(KeyCode::Enter));
    assert_eq!(s.surface, SurfaceFocus::Btw);
    assert_eq!(s.btw.turns.len(), 1);
    assert_eq!(s.btw.turns[0].answer, "part1 part2");
    let text = rendered(&mut s, 100, 24);
    assert_eq!(
        text.matches("part1 part2").count(),
        1,
        "the answer appears exactly once: {text}"
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
            exit_code: None,
            stop: None,
            id: ToolCallId::new("a1"),
            ok: true,
            preview: "old-line\n".into(),
            duration_ms: 5,
            applied_diff: None,
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
            exit_code: None,
            stop: None,
            id: ToolCallId::new("b1"),
            ok: true,
            preview: "new-line\n".into(),
            duration_ms: 5,
            applied_diff: None,
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
fn ctrl_o_toggles_the_latest_tool_group_even_while_analysis_streams() {
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
            exit_code: None,
            stop: None,
            id: ToolCallId::new("t1"),
            ok: true,
            preview: "ok\n".into(),
            duration_ms: 1,
            applied_diff: None,
        }),
    );
    // A live analysis block renders nothing and is not a disclosure:
    // even while reasoning streams, Ctrl+O goes straight to the latest
    // tool group.
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ReasoningDelta {
            delta: "thinking hard about the fix".into(),
        }),
    );
    reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL)),
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
    assert_eq!(
        groups.iter().filter(|e| **e).count(),
        1,
        "Ctrl+O toggles the latest tool group; analysis is not a disclosure: {groups:?}"
    );
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
        submitted(&effects).0,
        ClientCommand::SubmitMessage {
            session_id: SessionId::new("s1"),
            content: "fix bug".to_string(),
            attachments: Vec::new(),
        }
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
                ordinal: None,
                kind: None,
                images: 0,
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
    // The elapsed belongs to the command, not to the label — assert what the
    // status line actually renders.
    let activity = s.activity.clone().unwrap_or_default();
    assert!(activity.contains("cargo test"), "activity: {activity}");
    let frame = rendered(&mut s, 120, 20);
    assert!(frame.contains("cargo test"), "status: {frame}");
    assert!(frame.contains("2m 31s"), "status: {frame}");
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
            failure: None,
        }),
    );
    assert_eq!(s.status, RuntimeStatus::Error);
    // ONE terminal failure → ONE primary block (the old code pushed both an
    // Error item and a TurnEnd detail, showing the same failure twice).
    let failures: Vec<_> = s
        .transcript
        .items()
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::Failure(f) => Some(f),
            _ => None,
        })
        .collect();
    assert_eq!(failures.len(), 1, "one failure must be presented once");
    assert_eq!(failures[0].summary, "boom");
    assert!(
        !s.transcript
            .items()
            .iter()
            .any(|item| matches!(item, TranscriptItem::Error(_))),
        "the legacy error item must not repeat the failure"
    );
    assert!(matches!(
        s.transcript.items().last(),
        Some(TranscriptItem::TurnEnd(end)) if end.status == TurnEndStatus::Failed
    ));
}

#[test]
fn an_exhausted_failure_states_how_many_retries_were_spent() {
    use leveler_client_protocol::{
        FailureCategory, FailureDelivery, FailureRetryability, FailureSource, UiFailure,
    };
    let mut s = state();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::TurnFailed {
            error: "network".into(),
            failure: Some(UiFailure {
                category: FailureCategory::Network,
                source: FailureSource::Provider,
                provider: Some("deepseek".into()),
                model: None,
                provider_code: None,
                request_id: None,
                status: None,
                retries: Some(10),
                retryability: FailureRetryability::Safe,
                delivery: FailureDelivery::NotSent,
                summary: "无法连接模型服务。".into(),
                detail: "connection closed".into(),
            }),
        }),
    );
    let failure = s
        .transcript
        .items()
        .iter()
        .find_map(|item| match item {
            TranscriptItem::Failure(f) => Some(f),
            _ => None,
        })
        .expect("a failure block");
    assert!(
        failure.summary.contains("已重试 10 次"),
        "the terminal line must state the spent retries: {:?}",
        failure.summary
    );
}

/// Retry is ephemeral runtime state: a fresh state (a reopened session) never
/// resumes a stale "Reconnecting" indicator.
#[test]
fn reconnecting_never_survives_a_fresh_state() {
    assert!(state().reconnecting.is_none());
    assert!(state().reconnected_until.is_none());
}

#[test]
fn model_retrying_is_ephemeral_not_a_transcript_item() {
    let mut s = state();
    s.status = RuntimeStatus::Busy;
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ModelRetrying {
            attempt: 2,
            max_attempts: 10,
            delay_ms: 1400,
        }),
    );
    let rc = s.reconnecting.expect("the retry is live");
    assert_eq!((rc.attempt, rc.max_attempts), (2, 10));
    assert_eq!(rc.delay, std::time::Duration::from_millis(1400));
    assert!(
        !s.transcript
            .items()
            .iter()
            .any(|item| matches!(item, TranscriptItem::Failure(_) | TranscriptItem::Error(_))),
        "reconnect is ephemeral status, never a transcript item"
    );
    // A fresh attempt clears the retry and briefly confirms the reconnect.
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AssistantAttemptReset { message_id: None }),
    );
    assert_eq!(s.reconnecting, None);
    assert!(
        s.reconnected_until.is_some(),
        "a retry that starts again confirms the reconnect"
    );
}

/// §11 end to end: a real `ApprovalRequested` naming a running call makes that
/// call's transcript row say it is waiting, not that it is running.
#[test]
fn an_approval_request_stops_its_call_reading_as_running() {
    let mut s = opened();
    s.size = (120, 40);
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ToolCallStarted {
            id: ToolCallId::new("rm1"),
            name: "run_command".into(),
            arguments: r#"{"program":"rm","args":["-rf","stale"]}"#.into(),
            parallel: false,
        }),
    );
    let running = rendered(&mut s, 120, 40);
    assert!(
        running.contains('◌'),
        "the call starts out running: {running}"
    );

    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ApprovalRequested {
            request: UiApprovalRequest {
                id: ApprovalId::new("a1"),
                tool: "run_command".into(),
                summary: String::new(),
                command: Some("rm -rf stale".into()),
                risks: vec!["可能造成破坏性变更".into()],
                call_id: Some("rm1".into()),
                always_persists: true,
            },
        }),
    );
    let waiting = rendered(&mut s, 120, 40);
    assert!(
        waiting.contains("等待批准"),
        "the gated row must say what it is waiting for: {waiting}"
    );
    assert!(
        waiting.contains("rm -rf stale"),
        "and still name the command: {waiting}"
    );

    // Allowed: the row goes back to running, and the cache must have noticed.
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ApprovalResolved {
            id: ApprovalId::new("a1"),
        }),
    );
    let allowed = rendered(&mut s, 120, 40);
    assert!(
        !allowed.contains("等待批准"),
        "nothing is waiting once it is answered: {allowed}"
    );
    assert!(allowed.contains('◌'), "and the call is running: {allowed}");
}

// ── §12: the completion footer is a claim, and it needs an answer behind it ──

/// Stream and finish one assistant answer, the way a real turn commits one.
fn answer(s: &mut AppState, id: &str, text: &str) {
    reduce(
        s,
        Action::Runtime(RuntimeEvent::AssistantMessageStarted {
            message_id: MessageId::new(id),
        }),
    );
    reduce(
        s,
        Action::Runtime(RuntimeEvent::AssistantTextDelta {
            message_id: MessageId::new(id),
            delta: text.into(),
        }),
    );
    reduce(
        s,
        Action::Runtime(RuntimeEvent::AssistantMessageCompleted {
            message_id: MessageId::new(id),
        }),
    );
}

fn last_turn_end(s: &AppState) -> &leveler_tui::transcript::TurnEndBlock {
    s.transcript
        .items()
        .iter()
        .rev()
        .find_map(|item| match item {
            TranscriptItem::TurnEnd(end) => Some(end),
            _ => None,
        })
        .expect("expected a turn-end marker")
}

/// C1: the loop ended and nothing was answered. The runtime's own outcome is
/// still reported, but the marker must not read "✓ 任务已完成" — there is no
/// answer in the transcript for that claim to be about.
#[test]
fn a_turn_that_committed_no_answer_does_not_claim_completion() {
    for event in [RuntimeEvent::TurnAnswered, RuntimeEvent::TurnCompleted] {
        let mut s = opened();
        reduce(&mut s, Action::Runtime(event));
        assert_eq!(
            last_turn_end(&s).status,
            TurnEndStatus::NoFinalAnswer,
            "a turn with no answer cannot present as done"
        );
        let screen = rendered(&mut s, 100, 24);
        assert!(
            !screen.contains(leveler_tui::Locale::Zh.text().turn_end_completed),
            "no green completion wording: {screen}"
        );
    }
}

/// C2: with an answer committed, the runtime's outcome stands untouched.
#[test]
fn a_turn_that_committed_an_answer_keeps_its_runtime_outcome() {
    let mut s = opened();
    answer(&mut s, "m-final", "P1-8 已完成。");
    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCompleted));
    assert_eq!(last_turn_end(&s).status, TurnEndStatus::Completed);
}

#[test]
fn finalizing_is_busy_but_never_presented_as_waiting_for_the_model() {
    let mut s = opened();
    answer(&mut s, "m-finalizing", "修改结果如下。");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::TurnFinalizing {
            stage: FinalizationStage::Review,
        }),
    );

    assert_eq!(s.status, RuntimeStatus::Busy);
    assert_eq!(s.finalization_stage, Some(FinalizationStage::Review));
    let finalizing = rendered(&mut s, 100, 24);
    assert!(finalizing.contains("正在审查"), "screen: {finalizing}");
    assert!(!finalizing.contains("等待模型"), "screen: {finalizing}");

    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCompleted));
    assert_eq!(s.status, RuntimeStatus::Idle);
    assert_eq!(s.finalization_stage, None);
    assert_eq!(
        s.transcript
            .items()
            .iter()
            .filter(|item| matches!(item, TranscriptItem::TurnEnd(_)))
            .count(),
        1,
        "the terminal event remains the only turn-end authority"
    );
}

/// C3: prose a tool call acted on is narration, not the answer. A turn whose
/// only text was "让我先看看" and which then stopped has answered nothing.
#[test]
fn interim_narration_does_not_satisfy_the_completion_footer() {
    let mut s = opened();
    answer(&mut s, "m-interim", "让我先看看 worker.go。");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ToolCallStarted {
            id: ToolCallId::new("t1"),
            name: "read_file".into(),
            arguments: r#"{"path":"worker.go"}"#.into(),
            parallel: false,
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            exit_code: None,
            stop: None,
            id: ToolCallId::new("t1"),
            ok: true,
            preview: "ok".into(),
            duration_ms: 5,
            applied_diff: None,
        }),
    );
    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnAnswered));
    assert_eq!(last_turn_end(&s).status, TurnEndStatus::NoFinalAnswer);
}

/// Answering, then having the harness review that answer, is still answering.
///
/// A child the MODEL spawns proves the prose before it was not the answer —
/// the model was still working. The harness's own reviewer proves the
/// opposite: it only runs at closure, on an answer that already exists.
/// Treating the two the same made a completed `/develop` turn — mechanically
/// `completed` with `verification=passed` — render as "⚠ 执行已结束，但未提交
/// 最终回答".
#[test]
fn a_harness_reviewer_does_not_demote_the_answer_it_reviews() {
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
            delta: "12 个测试全部通过。".into(),
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AssistantMessageCompleted {
            message_id: id.clone(),
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(sub_agent_running("review", "reviewer")),
    );
    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCompleted));

    assert_eq!(
        last_turn_end(&s).status,
        TurnEndStatus::Completed,
        "the harness reviewing an answer must not erase it"
    );
}

/// The other half of the same rule: a child the MODEL asked for still proves
/// the prose before it was interim. This is the behaviour `/develop` must not
/// have changed.
#[test]
fn a_model_spawned_child_still_demotes_the_prose_before_it() {
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
            delta: "我先派人去看看。".into(),
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AssistantMessageCompleted {
            message_id: id.clone(),
        }),
    );
    reduce(&mut s, Action::Runtime(sub_agent_running("a1", "explorer")));
    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCompleted));

    assert_eq!(
        last_turn_end(&s).status,
        TurnEndStatus::NoFinalAnswer,
        "a model-spawned child means the model was still working"
    );
}

fn sub_agent_running(id: &str, role: &str) -> RuntimeEvent {
    RuntimeEvent::SubAgentUpdated {
        id: id.into(),
        nickname: id.into(),
        role: role.into(),
        title: None,
        done: false,
        ok: false,
        detail: "working".into(),
        profile_id: None,
        profile_role: None,
        read_only: true,
        agent: None,
        contribution: None,
        outcome: None,
        stop: None,
        limit: None,
        background: None,
        scope: Vec::new(),
    }
}

/// C4: an outcome that already says something went wrong keeps saying it. This
/// rule replaces a false "done", never a true "failed" or "incomplete".
#[test]
fn a_failing_outcome_is_not_rewritten_by_the_missing_answer_rule() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::TurnIncomplete {
            reason: "预算用尽".into(),
        }),
    );
    assert_eq!(last_turn_end(&s).status, TurnEndStatus::Incomplete);

    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::TurnFailed {
            error: "provider closed".into(),
            failure: None,
        }),
    );
    assert_eq!(last_turn_end(&s).status, TurnEndStatus::Failed);
}

#[test]
fn answer_end_is_distinct_from_verified_task_completion() {
    let mut s = state();
    answer(&mut s, "m-final", "答完了。");
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
        call_id: None,
        always_persists: true,
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
fn model_picker_confirm_selects_and_sets_default() {
    let mut s = opened();
    typed(&mut s, "/model");
    reduce(&mut s, key(KeyCode::Enter)); // open picker
    // Two models (not searchable): quick-select the 2nd → glm/5.
    let effects = reduce(&mut s, key(KeyCode::Char('2')));
    // The picker is the user's explicit selection, so it carries the authority
    // to persist the default — not the session-scoped `SelectModel`.
    assert_eq!(
        effects,
        vec![Effect::Send(ClientCommand::SetDefaultModel {
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
    reduce(&mut s, key(KeyCode::Down)); // move highlight from /model to /permission
    assert_eq!(s.slash_selected, 1);
    reduce(&mut s, key(KeyCode::Tab)); // complete to the highlighted command
    assert_eq!(s.composer.text(), "/permission ");
    assert_eq!(s.slash_selected, 0, "selection resets after completing");
}

/// Typing `/mode` still lists `/model` + `/permission` (mode is an alias);
/// ↑/↓ must move selection (regression: popup rendered without a highlight).
#[test]
fn mode_prefix_popup_allows_up_down_selection() {
    let mut s = opened();
    typed(&mut s, "/mode");
    let matches = leveler_tui::screen::visible_slash_popup(&s);
    assert!(
        matches.len() >= 2,
        "expected /model and /permission under prefix /mode, got {matches:?}"
    );
    assert_eq!(matches[0].0, "/model");
    assert_eq!(matches[1].0, "/permission");
    assert_eq!(s.slash_selected, 0);

    reduce(&mut s, key(KeyCode::Down));
    assert_eq!(s.slash_selected, 1, "Down must highlight /permission");
    reduce(&mut s, key(KeyCode::Up));
    assert_eq!(s.slash_selected, 0, "Up must return to /model");
    reduce(&mut s, key(KeyCode::Down));
    reduce(&mut s, key(KeyCode::Tab));
    assert_eq!(
        s.composer.text(),
        "/permission ",
        "Tab completes the highlighted /permission row"
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
fn at_file_popup_enter_completes_then_second_enter_submits_once() {
    let mut s = opened();
    s.context_files = vec!["src/main.rs".into()];
    typed(&mut s, "@src/mai");

    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(
        effects.is_empty(),
        "first Enter only completes: {effects:?}"
    );
    assert_eq!(s.composer.text(), "@src/main.rs ");

    // The completed mention is done: the popup closes and the next Enter
    // submits (composer cleared) instead of re-completing the same path.
    assert!(
        leveler_tui::screen::visible_file_popup(&s).is_empty(),
        "completed mention must close the popup"
    );
    reduce(&mut s, key(KeyCode::Enter));
    assert!(
        s.composer.is_empty(),
        "second Enter must submit the mention once, got {:?}",
        s.composer.text()
    );
    assert!(
        s.transcript.items().iter().any(|item| matches!(
            item,
            TranscriptItem::User(text) if text.trim() == "@src/main.rs"
        )),
        "the submitted mention must land in the transcript exactly once"
    );
}

#[test]
fn at_file_popup_tab_completes_then_second_tab_does_not_reinsert() {
    let mut s = opened();
    s.context_files = vec!["src/main.rs".into()];
    typed(&mut s, "@src/mai");

    reduce(&mut s, key(KeyCode::Tab));
    assert_eq!(s.composer.text(), "@src/main.rs ");

    reduce(&mut s, key(KeyCode::Tab));
    assert_eq!(
        s.composer.text(),
        "@src/main.rs ",
        "a completed mention must not be completed again"
    );
}

#[test]
fn completed_file_mention_hides_the_popup() {
    let mut s = opened();
    s.context_files = vec!["src/main.rs".into()];
    typed(&mut s, "@src/main.rs ");

    assert!(leveler_tui::screen::visible_file_popup(&s).is_empty());
}

#[test]
fn finished_file_mention_followed_by_prose_hides_the_popup() {
    let mut s = opened();
    s.context_files = vec!["src/main.rs".into()];
    typed(&mut s, "@src/main.rs some text");

    assert!(
        leveler_tui::screen::visible_file_popup(&s).is_empty(),
        "a finished mention followed by prose must not reopen the popup"
    );
}

#[test]
fn multi_mention_targets_only_the_token_before_the_cursor() {
    let mut s = opened();
    s.context_files = vec!["src/main.rs".into(), "src/lib.rs".into()];
    typed(&mut s, "@src/main.rs @src/li");

    let popup = leveler_tui::screen::visible_file_popup(&s);
    assert_eq!(popup, vec!["src/lib.rs"]);

    // Only the second, still-active mention is replaced in place.
    reduce(&mut s, key(KeyCode::Tab));
    assert_eq!(s.composer.text(), "@src/main.rs @src/lib.rs ");
}

#[test]
fn file_mention_popup_is_cursor_aware() {
    let mut s = opened();
    s.context_files = vec!["src/main.rs".into()];
    typed(&mut s, "@src/main.rs some text");

    // Cursor at the end (plain text): no active `@` token.
    assert!(leveler_tui::screen::visible_file_popup(&s).is_empty());

    // Move the cursor back into an unfinished `@` token and the popup returns.
    s.composer.replace("@src/mai some text");
    for _ in 0.." some text".chars().count() {
        reduce(&mut s, key(KeyCode::Left));
    }
    assert_eq!(
        leveler_tui::screen::visible_file_popup(&s),
        vec!["src/main.rs"]
    );
}

#[test]
fn at_in_a_shell_escape_is_not_a_file_mention() {
    let mut s = opened();
    s.context_files = vec!["src/main.rs".into()];
    typed(&mut s, "!echo @src/main.rs");

    assert!(
        leveler_tui::screen::visible_file_popup(&s).is_empty(),
        "shell `!` owns the `@`, not the file index"
    );
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Send(ClientCommand::RunUserShell { command, .. })]
                if command == "echo @src/main.rs"
        ),
        "Enter must run the shell command, not complete a file mention: {effects:?}"
    );
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
    assert_eq!(
        s.mode,
        PermissionProfile::Assisted,
        "the chip waits for SessionUpdated, not the keystroke"
    );
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
            leveler_client_protocol::UiClarificationRequest::single(
                leveler_client_protocol::ClarificationId::new("c1"),
                "which?",
                vec!["a".into(), "b".into()],
            ),
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
        elapsed_ms: 0,
        output_tail: String::new(),
        output_truncated: false,
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
        call_id: None,
        always_persists: true,
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
            request: UiClarificationRequest::single(
                ClarificationId::new("c1"),
                "选哪个方案？",
                vec![],
            ),
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
        PendingInteraction::Clarification(UiClarificationRequest::single(
            ClarificationId::new("c1"),
            "选哪个方案？",
            vec![],
        )),
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
fn overlay_captures_keys_away_from_composer() {
    let mut s = opened();
    typed(&mut s, "/model");
    reduce(&mut s, key(KeyCode::Enter)); // open picker
    reduce(&mut s, key(KeyCode::Char('x'))); // would type 'x' if composer had focus
    assert!(s.composer.is_empty(), "overlay must capture key input");
}

#[test]
fn slash_new_starts_a_fresh_session() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::UserMessageAdded {
            message: UiMessage {
                id: MessageId::new("u1"),
                role: UiRole::User,
                text: "hi".into(),
                ordinal: None,
                kind: None,
                images: 0,
            },
        }),
    );
    assert!(!s.transcript.is_empty());
    // `/new` starts a NEW session on the FIRST press and leaves the previous
    // one in /sessions, so there is nothing to confirm and nothing to lose.
    typed(&mut s, "/new");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Send(ClientCommand::NewSessionFor { .. })]
        ),
        "one /new must start a new session: {effects:?}"
    );
    assert!(
        !s.transcript.is_empty(),
        "the view must NOT clear before the host confirms: if creating the \
         session fails, an emptied screen is a lie about work that still exists"
    );

    // The switch happens when the host says it happened.
    let mut fresh = snapshot();
    fresh.id = SessionId::new("s-new");
    fresh.messages = vec![];
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened { session: fresh }),
    );
    assert!(
        s.transcript.is_empty(),
        "the view switches once SessionOpened arrives"
    );
}

/// Entry points closed by the command-surface closure. Each capability lives
/// elsewhere: `/new` (was `/clear`), `/collab plan`, `$skill`, Ctrl+V,
/// Ctrl+X Ctrl+E, Ctrl+T, `leveler doctor`, Ctrl+C. `/skill` is not listed
/// here: it is now the prefix of the `/skills` inspector, so Enter completes
/// it rather than reporting it unknown.
#[test]
fn closed_commands_are_unknown_and_send_nothing() {
    for command in [
        "/clear",
        "/plan",
        "/skill demo please ship",
        "/paste",
        "/editor",
        "/tools",
        "/doctor",
        "/quit",
        "/q",
    ] {
        let mut s = opened();
        s.composer.replace(command);
        let effects = reduce(&mut s, key(KeyCode::Enter));
        assert!(effects.is_empty(), "{command} must not act: {effects:?}");
        assert!(
            s.notification
                .as_ref()
                .is_some_and(|n| n.message.contains("未知命令")),
            "{command} must be reported as unknown: {:?}",
            s.notification
        );
        assert_eq!(s.active_screen, Screen::Conversation, "{command}");
        assert_eq!(s.composer.text(), command, "{command} draft is kept");
    }
}

#[test]
fn ctrl_v_attaches_the_clipboard_image() {
    let mut s = opened();
    let effects = reduce(&mut s, ctrl('v'));
    assert_eq!(
        effects,
        vec![Effect::Send(ClientCommand::AddClipboardImage {
            session_id: SessionId::new("s1"),
        })]
    );
    assert!(s.composer.is_empty(), "Ctrl+V must not type a 'v'");
}

/// With `/quit`, `/paste` and `/editor` gone, Help is where their keys are
/// discovered.
#[test]
fn help_lists_the_keys_that_replaced_closed_commands() {
    let mut s = opened();
    typed(&mut s, "/help");
    reduce(&mut s, key(KeyCode::Enter));
    assert_eq!(s.active_screen, Screen::Help);
    let frame = rendered(&mut s, 120, 80);
    for key_name in ["Ctrl+C", "Ctrl+V", "Ctrl+X Ctrl+E", "Ctrl+D/T/S"] {
        assert!(
            frame.contains(key_name),
            "help must list {key_name}: {frame}"
        );
    }
    for closed in [
        "/quit", "/paste", "/editor", "/clear",
        // `/skills` is a live inspector, so the guard is the bare command with
        // a trailing gap — `/skills` never matches this.
        "/skill ", "/plan", "/doctor",
    ] {
        assert!(
            !frame.contains(closed),
            "help still lists {closed}: {frame}"
        );
    }
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
            [Effect::Submit { command: ClientCommand::SubmitMessage { content, .. }, .. }] if content == message
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
            exit_code: None,
            stop: None,
            id: ToolCallId::new(id),
            ok,
            preview: if ok { "done".into() } else { "boom".into() },
            duration_ms: 82,
            applied_diff: None,
        }),
    );
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
    s.live_reasoning = "old thinking".into();

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
    assert!(
        s.live_reasoning.is_empty(),
        "switching sessions must not carry another session's live reasoning"
    );

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
        ordinal: None,
        kind: None,
        images: 0,
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
            title: None,
            done: false,
            ok: false,
            detail: "investigating".into(),
            profile_id: None,
            profile_role: None,
            read_only: false,
            agent: None,
            contribution: None,
            outcome: None,
            stop: None,
            limit: None,
            background: None,
            scope: Vec::new(),
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
    // Not running any more, and not "failed" either: no terminal arrived, so
    // the outcome is unknown.
    assert_eq!(s.transcript.tool_calls()[0].status, ToolStatus::Unknown);
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
            exit_code: None,
            stop: None,
            id: ToolCallId::new("t1"),
            ok: true,
            preview: "file.go:1\tfunc x(\rmore".into(),
            duration_ms: 5,
            applied_diff: None,
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
            exit_code: None,
            stop: None,
            id: ToolCallId::new("t1"),
            ok: true,
            preview: "\u{1b}[32m✓\u{1b}[39m test passed \u{1b}[1m[30m leftover".into(),
            duration_ms: 12,
            applied_diff: None,
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
            title: None,
            done: false,
            ok: false,
            detail: detail.into(),
            profile_id: None,
            profile_role: None,
            read_only: false,
            agent: None,
            contribution: None,
            outcome: None,
            stop: None,
            limit: None,
            background: None,
            scope: Vec::new(),
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
            title: None,
            done: true,
            ok: true,
            detail: "already done".into(),
            profile_id: None,
            profile_role: None,
            read_only: false,
            agent: None,
            contribution: None,
            outcome: None,
            stop: None,
            limit: None,
            background: None,
            scope: Vec::new(),
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
            title: None,
            done: false,
            ok: false,
            detail: "investigate module A".into(),
            profile_id: None,
            profile_role: None,
            read_only: false,
            agent: None,
            contribution: None,
            outcome: None,
            stop: None,
            limit: None,
            background: None,
            scope: Vec::new(),
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
            title: None,
            done: true,
            ok: true,
            detail: "found 12 crates".into(),
            profile_id: None,
            profile_role: None,
            read_only: false,
            agent: None,
            contribution: None,
            outcome: None,
            stop: None,
            limit: None,
            background: None,
            scope: Vec::new(),
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
                ordinal: None,
                kind: None,
                images: 0,
            },
        }),
    );
    let mut snap = snapshot();
    snap.id = SessionId::new("other");
    snap.messages = vec![UiMessage {
        id: MessageId::new("m1"),
        role: UiRole::User,
        text: "loaded".into(),
        ordinal: None,
        kind: None,
        images: 0,
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
    leveler_client_protocol::UiClarificationRequest::single(
        leveler_client_protocol::ClarificationId::new("c1"),
        "保留旧字段还是替换？",
        vec!["保留".into(), "替换".into()],
    )
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
fn clarification_arrows_choose_the_option_enter_submits() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ClarificationRequested {
            request: clarify_req(),
        }),
    );
    assert!(
        reduce(&mut s, key(KeyCode::Down)).is_empty(),
        "the arrow only moves the focus"
    );
    let effects = reduce(&mut s, key(KeyCode::Enter));
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

// ---- Phase 7 + 8: help, theme, completion, scroll ------------------

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
    // A session that never chose a theme starts on the one explicit default
    // (R007-F4) — not on whichever id happens to be listed first.
    assert_eq!(s.theme.id, ThemeId::DEFAULT);
    typed(&mut s, "/theme");
    reduce(&mut s, key(KeyCode::Enter));
    assert!(
        matches!(s.overlay, Some(Overlay::ThemePicker(_))),
        "bare /theme opens the theme picker"
    );
    // Cursor starts on current (auto); Down → dark, Enter confirms.
    reduce(&mut s, key(KeyCode::Down));
    reduce(&mut s, key(KeyCode::Enter));
    assert_eq!(s.theme.id, ThemeId::Dark);
    assert!(s.overlay.is_none());
    typed(&mut s, "/theme light");
    reduce(&mut s, key(KeyCode::Enter));
    assert_eq!(s.theme.id, ThemeId::Light);
    assert!(!s.dark, "light theme clears the dark flag");
    typed(&mut s, "/theme dark");
    reduce(&mut s, key(KeyCode::Enter));
    assert_eq!(s.theme.id, ThemeId::Dark);
}

#[test]
fn tab_completes_slash_command() {
    let mut s = opened();
    typed(&mut s, "/mod");
    reduce(&mut s, key(KeyCode::Tab));
    // "/mod" matches /model and /permission (via /mode alias); first is /model.
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
            name: None,
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
    // Choose the text-only option. There are two: switching model, or dropping
    // the images — "remove" and "text only" were the same outcome twice.
    let effects = reduce(&mut s, key(KeyCode::Char('2')));
    assert_eq!(
        submitted(&effects).0,
        ClientCommand::SubmitMessage {
            session_id: SessionId::new("s1"),
            content: "hi".to_string(),
            attachments: Vec::new(),
        }
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
        submitted(&effects).0,
        ClientCommand::SubmitMessage {
            session_id: SessionId::new("s1"),
            content: "[图片 #1] hi".to_string(),
            attachments: vec![att],
        }
    );
    assert!(s.pending_attachments.is_empty());
}

#[test]
fn backspace_over_an_image_token_removes_that_image() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AttachmentAdded {
            attachment: attachment("login.png"),
        }),
    );
    assert_eq!(s.composer.text(), "[图片 #1] ");
    assert_eq!(s.pending_attachments.len(), 1);
    reduce(&mut s, key(KeyCode::Backspace)); // the space
    reduce(&mut s, key(KeyCode::Backspace)); // the token, whole
    assert!(s.composer.is_empty(), "{:?}", s.composer.text());
    assert!(
        s.pending_attachments.is_empty(),
        "the name went, so the image goes"
    );
}

// ---- Phase 4: plan / diff / verification / completion ----------------------

#[test]
fn plan_events_update_state() {
    use leveler_client_protocol::{PlanStepStatus, UiPlan, UiPlanStep};
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
}

#[test]
fn plan_summary_opens_a_full_plan_page_with_keyboard_and_returns_with_esc() {
    use leveler_client_protocol::PlanStepStatus;
    let mut s = opened();
    s.status = RuntimeStatus::Busy;
    s.plan = Some(UiPlan {
        steps: vec![
            UiPlanStep {
                index: 0,
                description: "摸清现有报价模型".into(),
                status: PlanStepStatus::Done,
            },
            UiPlanStep {
                index: 1,
                description: "设计完整语义模型".into(),
                status: PlanStepStatus::Running,
            },
            UiPlanStep {
                index: 2,
                description: "写成正式设计文档".into(),
                status: PlanStepStatus::Pending,
            },
        ],
    });

    let workbench = rendered(&mut s, 100, 30);
    let plan_rows: Vec<&str> = workbench
        .lines()
        .filter(|line| line.contains("计划") || line.contains("语义模型"))
        .collect();
    assert_eq!(
        plan_rows.len(),
        1,
        "the workbench owns one plan summary row:\n{workbench}"
    );
    assert!(
        plan_rows[0].contains('↗'),
        "detail affordance: {plan_rows:?}"
    );
    assert!(
        !workbench.contains("写成正式设计文档"),
        "pending steps belong to the detail page:\n{workbench}"
    );

    reduce(&mut s, key(KeyCode::Tab));
    reduce(&mut s, key(KeyCode::Tab));
    assert_eq!(format!("{:?}", s.workbench_focus), "Plan");
    reduce(&mut s, key(KeyCode::Enter));
    assert_eq!(format!("{:?}", s.active_screen), "Plan");

    let detail = rendered(&mut s, 100, 30);
    assert!(detail.contains("← 计划"), "{detail}");
    assert!(detail.contains("摸清现有报价模型"), "{detail}");
    assert!(detail.contains("设计完整语义模型"), "{detail}");
    assert!(detail.contains("写成正式设计文档"), "{detail}");

    reduce(&mut s, key(KeyCode::Esc));
    assert_eq!(s.active_screen, Screen::Conversation);
}

#[test]
fn clicking_the_plan_summary_opens_the_same_scrollable_plan_page() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use leveler_client_protocol::PlanStepStatus;
    let mut s = opened();
    s.status = RuntimeStatus::Busy;
    s.plan = Some(UiPlan {
        steps: (0..20)
            .map(|index| UiPlanStep {
                index,
                description: format!("完整计划步骤 {}", index + 1),
                status: if index == 0 {
                    PlanStepStatus::Running
                } else {
                    PlanStepStatus::Pending
                },
            })
            .collect(),
    });

    let workbench = rendered(&mut s, 100, 24);
    let row = workbench
        .lines()
        .position(|line| line.contains("计划 ·"))
        .unwrap_or_else(|| panic!("plan summary missing:\n{workbench}")) as u16;
    reduce(
        &mut s,
        Action::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 4,
            row,
            modifiers: KeyModifiers::empty(),
        }),
    );
    assert_eq!(format!("{:?}", s.active_screen), "Plan");

    let first = rendered(&mut s, 70, 10);
    assert!(first.contains("完整计划步骤 1"), "{first}");
    reduce(&mut s, key(KeyCode::PageDown));
    assert!(s.screen_scroll > 0);
    let later = rendered(&mut s, 70, 10);
    assert!(!later.contains("● 1. 完整计划步骤 1"), "{later}");
    assert!(later.contains("完整计划步骤 20"), "{later}");
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
                kind: None,
                sensitive: false,
            }],
            archived: vec![leveler_client_protocol::UiMemoryEntry {
                id: "old-fact".into(),
                title: "old".into(),
                kind: None,
                sensitive: false,
            }],
            pending: vec![],
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
fn memory_recalled_event_pushes_an_auditable_note_without_a_toast() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::MemoryRecalled {
            count: 2,
            ids: vec!["a".into(), "b".into()],
        }),
    );
    let note = s
        .transcript
        .items()
        .iter()
        .find_map(|i| match i {
            TranscriptItem::Note(t) => Some(t.clone()),
            _ => None,
        })
        .expect("MemoryRecalled must push a note");
    assert_eq!(note, "我想起了 2 条相关记忆。");
    assert!(
        s.notification.is_none(),
        "routine recall must not duplicate the transcript note as a toast"
    );
}

#[test]
fn memory_recalled_zero_is_silent() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::MemoryRecalled {
            count: 0,
            ids: vec![],
        }),
    );
    assert!(
        !s.transcript.items().iter().any(|i| matches!(
            i,
            TranscriptItem::Note(t) if t.contains("Memory")
        )),
        "a 0-hit search must not render as a recall"
    );
}

#[test]
fn memory_superseded_event_explains_the_conflict_and_update() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::MemoryChanged {
            operation: "superseded".into(),
            id: "k-probe".into(),
            title: "发布探针代号：BLUE-4821".into(),
            authority: Some("explicit_user".into()),
        }),
    );
    let note = s
        .transcript
        .items()
        .iter()
        .find_map(|i| match i {
            TranscriptItem::Note(t) => Some(t.clone()),
            _ => None,
        })
        .expect("MemoryChanged must push a note");
    assert_eq!(note, "我更新了之前记住的内容：发布探针代号：BLUE-4821");
    assert!(note.contains("BLUE-4821"), "{note}");
    assert!(
        !note.contains("k-probe"),
        "internal id stays in memory management: {note}"
    );
    assert!(
        !note.contains("explicit_user"),
        "internal authority stays in memory management: {note}"
    );
    assert!(!note.contains("updated · updated"), "{note}");
    let toast = s
        .notification
        .as_ref()
        .expect("persistent memory changes must remain visible as a toast");
    assert_eq!(toast.message, note);
}

#[test]
fn memory_superseded_event_explains_the_conflict_in_english() {
    let mut s = opened();
    s.locale = leveler_tui::Locale::En;
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::MemoryChanged {
            operation: "superseded".into(),
            id: "k-probe".into(),
            title: "Release probe code: BLUE-4821".into(),
            authority: Some("explicit_user".into()),
        }),
    );
    let note = s
        .transcript
        .items()
        .iter()
        .find_map(|i| match i {
            TranscriptItem::Note(t) => Some(t.clone()),
            _ => None,
        })
        .expect("MemoryChanged must push a note");
    assert_eq!(
        note,
        "I updated an earlier memory: Release probe code: BLUE-4821"
    );
    let toast = s
        .notification
        .as_ref()
        .expect("persistent memory changes must remain visible as a toast");
    assert_eq!(toast.message, note);
}

#[test]
fn memory_updated_event_keeps_the_plain_updated_label() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::MemoryChanged {
            operation: "updated".into(),
            id: "k-probe".into(),
            title: "发布探针代号：BLUE-4821".into(),
            authority: Some("explicit_user".into()),
        }),
    );
    let note = s
        .transcript
        .items()
        .iter()
        .find_map(|i| match i {
            TranscriptItem::Note(t) => Some(t.clone()),
            _ => None,
        })
        .expect("MemoryChanged must push a note");
    assert_eq!(note, "我更新了记忆：发布探针代号：BLUE-4821");
}

#[test]
fn slash_work_mode_sends_product_axes() {
    let mut s = opened();
    // Default collaboration is chat; /work-mode only changes the work profile.
    assert_eq!(s.collaboration, "chat");
    typed(&mut s, "/work-mode economy");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert_eq!(s.work_profile, "economy");
    assert!(
        effects.iter().any(|e| matches!(
            e,
            Effect::Send(ClientCommand::SetProductAxes {
                work_profile,
                collaboration,
                ..
            }) if work_profile == "economy" && collaboration == "chat"
        )),
        "effects={effects:?}"
    );
}

/// `delivery` has no runtime behavior distinct from `balanced`; it is no longer
/// a user-selectable value, and typing it must not silently apply something.
#[test]
fn slash_work_mode_rejects_delivery() {
    let mut s = opened();
    typed(&mut s, "/work-mode delivery");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(effects.is_empty(), "effects={effects:?}");
    assert_eq!(s.work_profile, "balanced", "delivery must not be applied");
}

#[test]
fn bare_work_mode_opens_picker_on_current_value() {
    let mut s = opened();
    assert_eq!(s.work_profile, "balanced");
    typed(&mut s, "/work-mode");
    reduce(&mut s, key(KeyCode::Enter));
    let Some(Overlay::WorkModePicker(model)) = &s.overlay else {
        panic!(
            "bare /work-mode must open the shared picker, got {:?}",
            s.overlay
        );
    };
    let focused = model
        .visible_rows()
        .into_iter()
        .find(|(_, _, on)| *on)
        .map(|(_, o, _)| o.key.as_str());
    assert_eq!(focused, Some("balanced"));
}

#[test]
fn work_mode_picker_enter_applies_and_updates_state() {
    let mut s = opened();
    typed(&mut s, "/work-mode");
    reduce(&mut s, key(KeyCode::Enter));
    // Options: balanced, economy — Down once lands on economy.
    reduce(&mut s, key(KeyCode::Down));
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert_eq!(s.work_profile, "economy");
    assert!(s.overlay.is_none());
    assert!(
        effects.iter().any(|e| matches!(
            e,
            Effect::Send(ClientCommand::SetProductAxes {
                work_profile,
                ..
            }) if work_profile == "economy"
        )),
        "effects={effects:?}"
    );
}

#[test]
fn work_mode_picker_esc_returns_to_input() {
    let mut s = opened();
    typed(&mut s, "/work-mode");
    reduce(&mut s, key(KeyCode::Enter));
    assert!(matches!(s.overlay, Some(Overlay::WorkModePicker(_))));
    reduce(&mut s, key(KeyCode::Esc));
    assert!(s.overlay.is_none());
    assert_eq!(s.work_profile, "balanced", "Esc must not apply a mode");
}

#[test]
fn bare_collab_opens_picker() {
    let mut s = opened();
    typed(&mut s, "/collab");
    reduce(&mut s, key(KeyCode::Enter));
    let Some(Overlay::CollabPicker(model)) = &s.overlay else {
        panic!("bare /collab must open the picker, got {:?}", s.overlay);
    };
    let focused = model
        .visible_rows()
        .into_iter()
        .find(|(_, _, on)| *on)
        .map(|(_, o, _)| o.key.as_str());
    assert_eq!(focused, Some("chat"));
}

#[test]
fn permission_picker_selects_ask_and_the_chip_follows_the_ack() {
    let mut s = opened();
    typed(&mut s, "/permission");
    reduce(&mut s, key(KeyCode::Enter));
    assert!(matches!(s.overlay, Some(Overlay::ModePicker(_))));
    // Default assisted=auto; first row is ask (request_approval).
    reduce(&mut s, key(KeyCode::Up));
    reduce(&mut s, key(KeyCode::Enter));
    // The picker records the choice; the chip does not lead the runtime.
    assert_eq!(
        s.pending_permission,
        Some(PermissionProfile::RequestApproval)
    );
    assert_eq!(
        s.mode,
        PermissionProfile::Assisted,
        "the chip waits for SessionUpdated, not the selection"
    );

    let mut snap = snapshot();
    snap.mode = PermissionProfile::RequestApproval;
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionUpdated { session: snap }),
    );
    assert_eq!(s.mode, PermissionProfile::RequestApproval);
    assert_eq!(s.mode_label, "RequestApproval");
    assert_eq!(
        s.pending_permission, None,
        "the ack clears what was pending"
    );
}

#[test]
fn slash_collab_plan_sets_collaboration_without_touching_permission() {
    let mut s = opened();
    assert_eq!(s.mode, PermissionProfile::Assisted);
    typed(&mut s, "/collab plan");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert_eq!(s.collaboration, "plan");
    assert_eq!(
        s.mode,
        PermissionProfile::Assisted,
        "plan is a runtime-derived read-only TOOL overlay; the TUI must not fabricate \
         a permission change it never sent"
    );
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
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::Send(ClientCommand::SetPermissionProfile { .. }))),
        "plan must not send a permission change: {effects:?}"
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
fn every_menu_slash_command_is_handled() {
    // Guard against menu/handler drift: every command advertised in the slash
    // popup must be wired in the reducer (not fall through to "未知命令").
    // This caught `/tools` being listed but unhandled.
    for name in leveler_tui::screen::SLASH_DEFS.iter().map(|d| d.name) {
        let mut s = state();
        reduce(&mut s, Action::Paste(name.to_string()));
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
    for name in ["a", "b"] {
        reduce(
            &mut s,
            Action::Runtime(RuntimeEvent::AttachmentAdded {
                attachment: att(name),
            }),
        );
    }
    assert_eq!(s.composer.text(), "[图片 #1] [图片 #2] ");

    // Backspace over the last token removes the image it named, and the one
    // still in the sentence keeps its place.
    reduce(&mut s, key(KeyCode::Backspace));
    reduce(&mut s, key(KeyCode::Backspace));
    assert_eq!(s.pending_attachments.len(), 1);
    assert_eq!(s.pending_attachments[0].id.as_str(), "a");
    assert_eq!(s.composer.text(), "[图片 #1] ");

    // Backspace over ordinary text leaves the images alone.
    typed(&mut s, "x");
    reduce(&mut s, key(KeyCode::Backspace));
    assert_eq!(
        s.pending_attachments.len(),
        1,
        "text backspace must not drop attachments"
    );
    assert_eq!(s.composer.text(), "[图片 #1] ");
}

// ---- 修复回归锁定：排队草稿 / 未知命令 / Alt+Backspace ----------------------

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
    assert_eq!(
        s.prompt_suggestion, None,
        "freeform prose is not a structured next step"
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
    assert_eq!(
        s.prompt_suggestion, None,
        "a draft wins: no ghost is parked to ambush the user later"
    );
    assert!(matches!(
        s.transcript.items().last(),
        Some(TranscriptItem::TurnEnd(_))
    ));
}

#[test]
fn incomplete_turn_suggests_continue_when_no_specific_next_step_exists() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::TurnIncomplete {
            reason: "本轮资源窗口已用完，当前改动已经保留".into(),
        }),
    );

    assert_eq!(s.prompt_suggestion.as_deref(), Some("继续"));
    assert!(
        s.composer.is_empty(),
        "the suggestion is ghost text, never buffer content"
    );
    assert!(leveler_tui::suggestion::is_visible(&s));
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
            exit_code: None,
            stop: None,
            id,
            ok: true,
            preview: "目标已完成".into(),
            duration_ms: 10,
            applied_diff: None,
        }),
    );

    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCompleted));

    assert!(matches!(
        s.transcript.items().last(),
        Some(TranscriptItem::Recap(block))
            if block.next_step == "提交当前改动"
    ));
    assert_eq!(s.prompt_suggestion.as_deref(), Some("提交当前改动"));
    assert!(s.composer.is_empty());
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
            exit_code: None,
            stop: None,
            id,
            ok: true,
            preview: "目标已完成".into(),
            duration_ms: 10,
            applied_diff: None,
        }),
    );

    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCompleted));

    assert!(s.composer.is_empty());
    assert_eq!(s.prompt_suggestion, None);
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
            [Effect::Submit { command: ClientCommand::SubmitMessage { content, .. }, .. }] if content == "你好"
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
            [Effect::Submit { command: ClientCommand::SubmitMessage { content, .. }, .. }] if content == "实现登录"
        ),
        "{effects:?}"
    );
    assert!(
        s.goal_mode_active,
        "explicit collaboration=goal must mark goal_mode_active"
    );
}

#[test]
fn a_continuation_phrase_is_sent_as_a_resume_intent() {
    let mut s = opened();
    typed(&mut s, "继续");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Submit { command: ClientCommand::ResumeTask { content, .. }, .. }]
                if content == "继续"
        ),
        "`继续` must express resume intent, not a plain message: {effects:?}"
    );
    assert!(
        !s.goal_mode_active,
        "a resume is not a fresh goal turn; the runtime decides the profile"
    );
}

#[test]
fn a_continuation_amendment_travels_with_the_resume_intent() {
    let mut s = opened();
    typed(&mut s, "继续，但是先不要跑测试");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Submit { command: ClientCommand::ResumeTask { content, .. }, .. }]
                if content == "继续，但是先不要跑测试"
        ),
        "the runtime parses the amendment off the original text: {effects:?}"
    );
}

#[test]
fn a_new_request_starting_with_continue_is_a_plain_message() {
    let mut s = opened();
    typed(&mut s, "继续之前的退款审计任务");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Submit { command: ClientCommand::SubmitMessage { content, .. }, .. }]
                if content == "继续之前的退款审计任务"
        ),
        "a new request is not a continuation: {effects:?}"
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
        [Effect::Submit { command: ClientCommand::SubmitMessage { content, .. }, .. }] if content == "随便聊聊"
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
            [Effect::Submit { command: ClientCommand::RunGoal { content, .. }, .. }]
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
fn slash_develop_runs_the_develop_workflow_on_the_bare_goal() {
    let mut s = opened();
    typed(&mut s, "/develop 修复 TUI 中计划进度没有实时更新的问题");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Submit { command: ClientCommand::RunDevelop { content, .. }, .. }]
                if content == "修复 TUI 中计划进度没有实时更新的问题"
        ),
        "/develop must send the goal without the command word: {effects:?}"
    );
    assert!(s.composer.is_empty());
}

#[test]
fn slash_develop_requires_a_task() {
    let mut s = opened();
    typed(&mut s, "/develop");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(
        effects.is_empty(),
        "an empty /develop must not start a task: {effects:?}"
    );
    assert!(
        s.notification
            .as_ref()
            .is_some_and(|n| n.message.contains("用法: /develop")),
        "the user gets a usage hint, not an empty workflow: {:?}",
        s.notification
    );
}

#[test]
fn slash_develop_is_refused_while_a_turn_is_running() {
    let mut s = opened();
    typed(&mut s, "/develop 第一个任务");
    reduce(&mut s, key(KeyCode::Enter));
    assert!(s.is_busy(), "the first /develop starts a turn");

    typed(&mut s, "/develop 第二个任务");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(
        effects.is_empty(),
        "a running workflow must not be steered into a second one: {effects:?}"
    );
    assert!(
        s.notification
            .as_ref()
            .is_some_and(|n| n.message.contains("develop")),
        "the refusal names the command: {:?}",
        s.notification
    );
}

#[test]
fn an_ordinary_message_is_untouched_by_the_develop_command_existing() {
    let mut s = opened();
    typed(&mut s, "修复 TUI 的计划进度问题");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Submit { command: ClientCommand::SubmitMessage { content, .. }, .. }]
                if content == "修复 TUI 的计划进度问题"
        ),
        "plain text must still take the ordinary path: {effects:?}"
    );
}

#[test]
fn alt_backspace_deletes_word_not_attachment() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AttachmentAdded {
            attachment: attachment("a1"),
        }),
    );
    typed(&mut s, "hello world");
    reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT)),
    );
    assert_eq!(
        s.composer.text(),
        "[图片 #1] hello ",
        "a word, not the image"
    );
    assert_eq!(
        s.pending_attachments.len(),
        1,
        "word delete stops at the image's name"
    );
}

/// Option+← is word-left, and the Meta encoding macOS terminals send by
/// default (ESC b → Alt+b) must reach that binding instead of typing a `b`.
#[test]
fn option_left_moves_by_word_and_never_types_b() {
    let mut s = opened();
    typed(&mut s, "hello world");
    // CSI-modifier form: a terminal that reports Option+← as Alt+Left.
    reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT)),
    );
    assert_eq!(s.composer.text(), "hello world");
    assert_eq!(s.composer.cursor(), 6);
    // Meta encoding: Option+← arrives as Alt+b (readline backward-word).
    reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT)),
    );
    assert_eq!(s.composer.text(), "hello world", "Meta+b typed a b");
    assert_eq!(s.composer.cursor(), 0);
}

/// Option+→ is word-right, and the Meta encoding (ESC f → Alt+f) must not
/// insert an `f`.
#[test]
fn option_right_moves_by_word_and_never_types_f() {
    let mut s = opened();
    typed(&mut s, "hello world");
    reduce(&mut s, key(KeyCode::Home));
    reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::Right, KeyModifiers::ALT)),
    );
    assert_eq!(s.composer.text(), "hello world");
    assert_eq!(s.composer.cursor(), 5);
    reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::ALT)),
    );
    assert_eq!(s.composer.text(), "hello world", "Meta+f typed an f");
    assert_eq!(s.composer.cursor(), 11);
}

/// Rapid alternating Option+←/→ leaves the draft exactly as typed.
#[test]
fn rapid_option_word_navigation_does_not_pollute_the_draft() {
    let mut s = opened();
    typed(&mut s, "hello world test");
    for _ in 0..4 {
        reduce(
            &mut s,
            Action::Key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT)),
        );
        reduce(
            &mut s,
            Action::Key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::ALT)),
        );
    }
    assert_eq!(s.composer.text(), "hello world test");
}

/// Plain b and f are ordinary text: the word-motion bindings are Alt-only.
#[test]
fn plain_b_and_f_still_insert() {
    let mut s = opened();
    typed(&mut s, "bf");
    assert_eq!(s.composer.text(), "bf");
}

#[test]
fn activity_clears_when_the_tool_completes() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ReasoningDelta {
            delta: "previous-step analysis".into(),
        }),
    );
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
            exit_code: None,
            stop: None,
            id: ToolCallId::new("t1"),
            ok: true,
            preview: "ok".into(),
            duration_ms: 3,
            applied_diff: None,
        }),
    );
    assert!(
        s.activity.is_none(),
        "a finished tool must not linger in the status line while the model thinks"
    );
    // Raw reasoning has no conversation representation at all: the tool
    // boundary spent the status scratch, and no transcript item ever existed.
    assert!(s.live_reasoning.is_empty(), "scratch spent at the boundary");
    assert!(
        !s.transcript
            .items()
            .iter()
            .any(|i| { format!("{i:?}").contains("previous-step analysis") }),
        "raw reasoning must not survive anywhere in the transcript"
    );
}

/// One background lifecycle reads as one named activity: the start names
/// what is running, the exit names what finished, and the opaque runtime id
/// is never the user-facing label. Two concurrent tasks stay distinct, keyed
/// by that id — never by matching text.
#[test]
fn background_task_lifecycle_is_named_not_id_addressed() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::BackgroundTaskStarted {
            task_id: "bg-7f3a".into(),
            program: "cargo".into(),
            args: vec!["test".into(), "-p".into(), "leveler-tui".into()],
        }),
    );
    let msg = s.notification.as_ref().unwrap().message.clone();
    assert!(msg.contains("cargo test"), "names the command: {msg}");
    assert!(
        !msg.contains("bg-7f3a"),
        "opaque id is not the label: {msg}"
    );

    // A second task in flight: the map keeps them apart by identity.
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::BackgroundTaskStarted {
            task_id: "bg-91c2".into(),
            program: "npm".into(),
            args: vec!["run".into(), "dev".into()],
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::BackgroundTaskExited {
            task_id: "bg-7f3a".into(),
            exit_code: Some(0),
            duration_ms: 12_000,
            ok: true,
            stopped: false,
            output: String::new(),
        }),
    );
    let msg = s.notification.as_ref().unwrap().message.clone();
    assert!(msg.contains("cargo test"), "the RIGHT task settled: {msg}");
    assert!(!msg.contains("npm"), "the other task is untouched: {msg}");
    assert!(!msg.contains("bg-7f3a"), "{msg}");
    assert!(
        s.background_task_labels
            .get("bg-7f3a")
            .is_some_and(|c| !c.is_running() && c.ok == Some(true)),
        "a settled task is retained as terminal so its detail stays reopenable"
    );
    assert!(
        s.background_task_labels
            .get("bg-91c2")
            .is_some_and(|c| c.is_running()),
        "the still-running task is still live chrome"
    );
    assert!(
        s.transcript
            .items()
            .iter()
            .any(|item| format!("{item:?}").contains("cargo test")),
        "the terminal fact remains visible in transcript history"
    );

    // Failure stays truthful and prominent, with the exit code.
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::BackgroundTaskExited {
            task_id: "bg-91c2".into(),
            exit_code: Some(1),
            duration_ms: 900,
            ok: false,
            stopped: false,
            output: String::new(),
        }),
    );
    let note = s.notification.as_ref().unwrap();
    assert!(note.message.contains("npm run dev"), "{}", note.message);
    assert!(note.message.contains("exit 1"), "{}", note.message);
    assert!(matches!(
        note.level,
        leveler_client_protocol::NotificationLevel::Warning
    ));
}

/// An exit whose start was never seen (reconnect) must not invent a name.
#[test]
fn an_unlabeled_background_exit_falls_back_to_a_truthful_generic() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::BackgroundTaskExited {
            task_id: "bg-unknown".into(),
            exit_code: Some(0),
            duration_ms: 5,
            ok: true,
            stopped: false,
            output: String::new(),
        }),
    );
    let msg = s.notification.as_ref().unwrap().message.clone();
    assert!(!msg.contains("bg-unknown"), "{msg}");
    assert!(msg.contains("后台任务"), "generic but truthful: {msg}");
}

/// Live output is appended to the same projection the detail page reads, so a
/// running task's page grows as the process writes. Output for a task this
/// session never saw does not invent an entry.
#[test]
fn background_output_streams_into_the_task_projection() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::BackgroundTaskStarted {
            task_id: "bg-out".into(),
            program: "make".into(),
            args: vec!["up".into()],
        }),
    );
    for chunk in ["[+] Building web\n", "server listening on :3000\n"] {
        reduce(
            &mut s,
            Action::Runtime(RuntimeEvent::BackgroundTaskOutput {
                task_id: "bg-out".into(),
                chunk: chunk.into(),
            }),
        );
    }
    let output = &s.background_task_labels.get("bg-out").unwrap().output;
    assert!(output.contains("[+] Building web"), "{output:?}");
    assert!(output.contains("server listening on :3000"), "{output:?}");

    // The terminal log is authoritative over the streamed tail: a lagged
    // chunk can never leave the finished page short.
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::BackgroundTaskExited {
            task_id: "bg-out".into(),
            exit_code: Some(0),
            duration_ms: 1234,
            ok: true,
            stopped: false,
            output: "[+] Building web\nserver listening on :3000\nrequest 200 OK\n".into(),
        }),
    );
    assert_eq!(
        s.background_task_labels.get("bg-out").unwrap().output,
        "[+] Building web\nserver listening on :3000\nrequest 200 OK\n"
    );

    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::BackgroundTaskOutput {
            task_id: "bg-never-started".into(),
            chunk: "stray".into(),
        }),
    );
    assert!(
        !s.background_task_labels.contains_key("bg-never-started"),
        "output for an unseen task must not fabricate a row"
    );
}

/// `x` on a running background-task detail stops exactly that task, through
/// the runtime's own cancellation command — never a PID, never just closing
/// the page. Esc keeps the old non-destructive meaning.
#[test]
fn x_on_a_running_background_detail_cancels_that_task_only() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::BackgroundTaskStarted {
            task_id: "bg-2".into(),
            program: "cargo".into(),
            args: vec!["test".into()],
        }),
    );
    s.activity_selected = Some(leveler_tui::activity::ActivityId::Background("bg-2".into()));
    s.activity_open = Some(leveler_tui::activity::ActivityId::Background("bg-2".into()));
    s.active_screen = Screen::Activity;
    let effects = reduce(&mut s, raw_char('x'));
    assert!(
        effects.iter().any(|e| matches!(
            e,
            Effect::Send(
                leveler_client_protocol::ClientCommand::CancelBackgroundTask { task_id, .. }
            ) if task_id == "bg-2"
        )),
        "x must stop the shown task through the runtime: {effects:?}"
    );
    // Esc is not a stop: it only leaves the page.
    let esc = reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
    );
    assert!(esc.is_empty(), "Esc must not stop the task: {esc:?}");
    assert_eq!(s.active_screen, Screen::Conversation);
    assert!(
        s.background_task_labels
            .get("bg-2")
            .is_some_and(|c| c.is_running()),
        "leaving the page must not settle the task"
    );
}

/// A task that finishes while its detail is open keeps the detail open and
/// switches it to the terminal state; the row stays reopenable afterwards.
#[test]
fn a_finished_background_task_keeps_its_detail_and_reopens_from_the_row() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::BackgroundTaskStarted {
            task_id: "bg-2".into(),
            program: "cargo".into(),
            args: vec!["test".into(), "--workspace".into()],
        }),
    );
    s.activity_selected = Some(leveler_tui::activity::ActivityId::Background("bg-2".into()));
    s.activity_open = Some(leveler_tui::activity::ActivityId::Background("bg-2".into()));
    s.active_screen = Screen::Activity;
    s.background_task_labels
        .get_mut("bg-2")
        .unwrap()
        .output
        .push_str("test result: ok\n");

    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::BackgroundTaskExited {
            task_id: "bg-2".into(),
            exit_code: Some(0),
            duration_ms: 8_000,
            ok: true,
            stopped: false,
            output: String::new(),
        }),
    );
    assert_eq!(
        s.active_screen,
        Screen::Activity,
        "exit must not close the viewer"
    );
    assert_eq!(
        s.activity_open,
        Some(leveler_tui::activity::ActivityId::Background("bg-2".into()))
    );
    assert!(
        s.background_task_labels
            .get("bg-2")
            .is_some_and(|c| c.ok == Some(true) && !c.is_running()),
        "the entry is terminal but retained"
    );

    // Esc only closes the viewer; the task is still reopenable.
    reduce(&mut s, key(KeyCode::Esc));
    assert_eq!(s.active_screen, Screen::Conversation);
    assert!(s.activity_open.is_none());
    assert!(s.background_task_labels.contains_key("bg-2"));

    // The task is still in the projection, so the list page reopens it: the
    // footer summary is the entry point now that no task row sits in the
    // conversation body.
    s.workbench_focus = leveler_tui::state::WorkbenchFocus::Background;
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(
        effects.is_empty(),
        "opening the list sends nothing: {effects:?}"
    );
    assert_eq!(s.active_screen, Screen::ActivityList);
    // Only one task exists, so it is selected; Enter opens its detail.
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(
        effects.is_empty(),
        "opening a finished task sends nothing: {effects:?}"
    );
    assert_eq!(s.active_screen, Screen::Activity);
    let frame = render_screen_text(&mut s);
    assert!(frame.contains("Background Task"), "{frame}");
    assert!(frame.contains("test result: ok"), "{frame}");
}

#[test]
fn activity_enter_opens_detail_and_esc_closes_without_cancel() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::BackgroundTaskStarted {
            task_id: "bg-2".into(),
            program: "cargo".into(),
            args: vec!["test".into(), "--workspace".into()],
        }),
    );
    reduce(&mut s, key(KeyCode::Tab));
    reduce(&mut s, key(KeyCode::Tab));
    // A background-only session focuses the footer summary, never a strip row.
    assert_eq!(
        s.workbench_focus,
        leveler_tui::state::WorkbenchFocus::Background
    );
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert_eq!(s.active_screen, Screen::ActivityList);
    assert!(
        !effects
            .iter()
            .any(|e| format!("{e:?}").contains("Cancel") || format!("{e:?}").contains("Kill")),
        "Enter must not cancel: {effects:?}"
    );
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert_eq!(s.active_screen, Screen::Activity);
    assert!(
        effects.is_empty(),
        "opening a background detail sends nothing: {effects:?}"
    );
    let effects = reduce(&mut s, key(KeyCode::Esc));
    assert_eq!(s.active_screen, Screen::Conversation);
    assert!(s.activity_open.is_none());
    assert!(
        s.background_task_labels
            .get("bg-2")
            .is_some_and(|c| c.is_running())
    );
    assert!(
        effects.is_empty(),
        "Esc close sends no command: {effects:?}"
    );
}

/// The full three-level flow: conversation → footer summary → list → detail.
/// Opening the list acknowledges the current failures and never deletes them.
#[test]
fn the_background_list_acknowledges_failures_and_keeps_history() {
    let mut s = opened();
    s.locale = leveler_tui::Locale::En;
    for (id, program, args) in [
        ("bg-run", "make", vec!["up"]),
        ("bg-fail", "npm", vec!["start"]),
    ] {
        reduce(
            &mut s,
            Action::Runtime(RuntimeEvent::BackgroundTaskStarted {
                task_id: id.into(),
                program: program.into(),
                args: args.into_iter().map(String::from).collect(),
            }),
        );
    }
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::BackgroundTaskExited {
            task_id: "bg-fail".into(),
            exit_code: Some(1),
            duration_ms: 30_000,
            ok: false,
            stopped: false,
            output: "Error: listen EADDRINUSE\n".into(),
        }),
    );
    assert!(!s.background_failures_seen.contains("bg-fail"));

    // Footer focus → Enter opens the list; opening is the acknowledgement.
    s.workbench_focus = leveler_tui::state::WorkbenchFocus::Background;
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(effects.is_empty(), "opening sends nothing: {effects:?}");
    assert_eq!(s.active_screen, Screen::ActivityList);
    assert!(
        s.background_failures_seen.contains("bg-fail"),
        "opening the list acknowledges the failure"
    );
    // The history is untouched: the terminal record and its output survive.
    assert!(s.background_task_labels.contains_key("bg-fail"));
    assert!(s.background_task_labels.contains_key("bg-run"));

    let frame = render_screen_text(&mut s);
    assert!(frame.contains("Running"), "{frame}");
    assert!(frame.contains("Recently finished"), "{frame}");
    assert!(frame.contains("npm") && frame.contains("make"), "{frame}");

    reduce(&mut s, key(KeyCode::Esc));
    assert_eq!(s.active_screen, Screen::Conversation);
    assert!(
        s.background_task_labels.contains_key("bg-fail"),
        "returning keeps the history"
    );
}

/// `x` stops exactly the selected running task and is inert on a finished one.
#[test]
fn x_in_the_background_list_stops_only_the_selected_running_task() {
    let mut s = opened();
    for id in ["bg-run", "bg-done"] {
        reduce(
            &mut s,
            Action::Runtime(RuntimeEvent::BackgroundTaskStarted {
                task_id: id.into(),
                program: "npm".into(),
                args: vec!["start".into()],
            }),
        );
    }
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::BackgroundTaskExited {
            task_id: "bg-done".into(),
            exit_code: Some(0),
            duration_ms: 5_000,
            ok: true,
            stopped: false,
            output: String::new(),
        }),
    );
    s.workbench_focus = leveler_tui::state::WorkbenchFocus::Background;
    reduce(&mut s, key(KeyCode::Enter));
    assert_eq!(s.active_screen, Screen::ActivityList);
    // The running task leads the list and is selected first.
    assert_eq!(s.background_list_selected.as_deref(), Some("bg-run"));
    let effects = reduce(&mut s, key(KeyCode::Char('x')));
    let debug = format!("{effects:?}");
    assert!(
        debug.contains("CancelBackgroundTask") && debug.contains("bg-run"),
        "x stops the selected running task: {debug}"
    );
    // The finished row advertises no stop.
    reduce(&mut s, key(KeyCode::Down));
    assert_eq!(s.background_list_selected.as_deref(), Some("bg-done"));
    let effects = reduce(&mut s, key(KeyCode::Char('x')));
    assert!(
        effects.is_empty(),
        "a terminal row has nothing to stop: {effects:?}"
    );
}

/// A stopped task is its own terminal state: the detail says Stopped, never
/// Failed, and the footer shows no failure badge. The distinction comes from the
/// runtime's `stopped` bit, not the exit code.
#[test]
fn a_stopped_background_task_is_shown_as_stopped_not_failed() {
    let mut s = opened();
    s.locale = leveler_tui::Locale::En;
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::BackgroundTaskStarted {
            task_id: "bg-1".into(),
            program: "npm".into(),
            args: vec!["start".into()],
        }),
    );
    // A kill reads `ok: false` but `stopped: true` in the runtime's authority.
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::BackgroundTaskExited {
            task_id: "bg-1".into(),
            exit_code: None,
            duration_ms: 24_000,
            ok: false,
            stopped: true,
            output: String::new(),
        }),
    );
    s.activity_open = Some(leveler_tui::activity::ActivityId::Background("bg-1".into()));
    s.active_screen = Screen::Activity;
    let frame = render_screen_text(&mut s);
    assert!(frame.contains("Stopped"), "{frame}");
    assert!(
        !frame.contains("Failed"),
        "a stop is not a failure: {frame}"
    );
    // Back on the conversation the footer has nothing to remind about.
    s.active_screen = Screen::Conversation;
    s.activity_open = None;
    let frame = render_screen_text(&mut s);
    assert!(!frame.contains('×'), "no failure badge: {frame}");
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
            exit_code: None,
            stop: None,
            id: ToolCallId::new("t1"),
            ok: true,
            preview: "ok".into(),
            duration_ms: 5,
            applied_diff: None,
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

    // The new step's reasoning replaces the old in the status scratch —
    // the earlier step's text was spent at the tool boundary and has no
    // representation anywhere.
    assert_eq!(s.live_reasoning, "再补测试");
    assert!(
        !s.transcript
            .items()
            .iter()
            .any(|i| { format!("{i:?}").contains("先读一遍源码") }),
        "the previous step's reasoning is gone, not archived"
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
    // The retried attempt's reasoning is sealed history, not live state:
    // nothing streams until the retry produces a new delta.
    assert!(s.live_reasoning.is_empty());
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
    s.conv.auto_scroll = true;
    s.conv.scroll = 0;
    // Force non-empty content so scroll math can leave bottom.
    s.conv.auto_scroll = false;
    s.conv.scroll = 0;
    reduce(&mut s, key(KeyCode::Up));
    assert!(
        s.composer.is_empty(),
        "conversation ↑ must not fill history"
    );
    assert!(!s.conv.auto_scroll);

    // The live header goal is now its own focus stop; the next Tab returns to
    // Input when there is no plan, command, or activity row.
    reduce(&mut s, key(KeyCode::Tab));
    assert_eq!(s.workbench_focus, WorkbenchFocus::Goal);
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
    assert!(!s.conv.auto_scroll);

    // Enter while Conversation focus + not at bottom → jump live.
    reduce(&mut s, key(KeyCode::Enter));
    assert!(s.conv.auto_scroll);
    assert_eq!(s.conv.unread, 0);
}

/// Sending a message is reading the live edge. Dogfood: after scrolling up
/// once, three later messages ran their whole turns below the viewport with
/// only a "▼N" badge — the user's own input never came into view.
#[test]
fn sending_a_message_returns_the_conversation_to_the_live_edge() {
    use leveler_tui::state::WorkbenchFocus;
    let mut s = opened();
    s.size = (100, 40);
    for i in 0..30 {
        s.transcript.push_user(format!("msg {i}"));
    }
    reduce(&mut s, key(KeyCode::PageUp));
    assert!(!s.conv.auto_scroll);
    reduce(&mut s, key(KeyCode::Tab));
    assert_eq!(s.workbench_focus, WorkbenchFocus::Input);
    typed(&mut s, "下一个问题");
    reduce(&mut s, key(KeyCode::Enter));
    assert!(
        s.conv.auto_scroll,
        "a sent message must follow the live edge again"
    );
    assert_eq!(s.conv.unread, 0);
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
    s.conv.rect = Some((0, 3, 100, 20));
    s.conv.auto_scroll = true;
    let mouse = MouseEvent {
        kind: MouseEventKind::ScrollUp,
        column: 10,
        row: 10,
        modifiers: KeyModifiers::empty(),
    };
    reduce(&mut s, Action::Mouse(mouse));
    assert!(!s.conv.auto_scroll, "wheel must pin away from live edge");
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
    s.conv.rect = Some((0, 2, 80, 20));
    s.conv.auto_scroll = true;
    s.conv.scroll = 0;
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
        !s.conv.auto_scroll,
        "selection must pin viewport away from live edge"
    );
    assert!(s.conv.selection.dragging);
    assert!(s.conv.selection.anchor.is_some());
    assert_eq!(s.conv.selection_last_mouse, Some((5, 5)));
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
    s.conv.rect = Some((0, 2, 80, 20));
    s.conv.auto_scroll = false;
    s.conv.scroll = 0;

    reduce(
        &mut s,
        Action::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 4,
            row: 5,
            modifiers: KeyModifiers::empty(),
        }),
    );
    let start = s.conv.selection.anchor.expect("anchor");

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
        s.conv.selection_edge_dir, 1,
        "bottom edge must arm downward scroll"
    );
    assert!(s.conv.selection.dragging);
    let before_scroll = s.conv.scroll;

    // Continuous tick should scroll and keep auto_follow off.
    for _ in 0..5 {
        reduce(&mut s, Action::SelectionTick);
    }
    assert!(
        s.conv.scroll > before_scroll,
        "edge ticks must advance scroll: before={before_scroll} after={}",
        s.conv.scroll
    );
    assert!(!s.conv.auto_scroll, "must stay pinned while selecting");
    let focus = s.conv.selection.focus.expect("focus");
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
    s.conv.rect = Some((0, 2, 80, 20));
    s.conv.auto_scroll = false;
    s.conv.scroll = 30;

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
    assert_eq!(s.conv.selection_edge_dir, -1);
    let before = s.conv.scroll;
    reduce(&mut s, Action::SelectionTick);
    assert!(
        s.conv.scroll < before,
        "top-edge tick scrolls up: before={before} after={}",
        s.conv.scroll
    );
}

#[test]
fn shift_mouse_does_not_start_app_selection() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    let mut s = opened();
    s.conv.rect = Some((0, 2, 80, 20));
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
        !s.conv.selection.dragging,
        "Shift+mouse is reserved for terminal-native selection"
    );
}

#[test]
fn selection_tick_noop_when_not_dragging() {
    let mut s = opened();
    s.conv.selection_edge_dir = 1;
    s.conv.scroll = 0;
    reduce(&mut s, Action::SelectionTick);
    assert_eq!(s.conv.scroll, 0);
}

#[test]
fn mouse_click_scroll_bottom_restores_auto_follow() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    let mut s = opened();
    s.conv.auto_scroll = false;
    s.conv.scroll = 0;
    s.conv.unread = 3;
    s.conv.scroll_bottom_rect = Some((40, 20, 6, 1));
    let mouse = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 42,
        row: 20,
        modifiers: KeyModifiers::empty(),
    };
    reduce(&mut s, Action::Mouse(mouse));
    assert!(s.conv.auto_scroll);
    assert_eq!(s.conv.unread, 0);
}

#[test]
fn mouse_scroll_over_input_keeps_input_focus() {
    use crossterm::event::{KeyModifiers, MouseEvent, MouseEventKind};
    use leveler_tui::state::WorkbenchFocus;
    let mut s = opened();
    s.workbench_focus = WorkbenchFocus::Input;
    s.input_rect = Some((0, 20, 100, 4));
    s.conv.rect = Some((0, 3, 100, 15));
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
    s.conv.rect = Some((0, 3, 100, 15));
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
    s.conv.rect = Some((0, 3, 80, 20));
    s.conv.auto_scroll = true;
    s.conv.plain = vec!["hello world selection".into()];
    s.conv.plain_width = 80;

    let down = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 0,
        row: 3,
        modifiers: KeyModifiers::empty(),
    };
    reduce(&mut s, Action::Mouse(down));
    assert!(s.conv.selection.dragging);

    let drag = MouseEvent {
        kind: MouseEventKind::Drag(MouseButton::Left),
        column: 11,
        row: 3,
        modifiers: KeyModifiers::empty(),
    };
    reduce(&mut s, Action::Mouse(drag));
    let range = s.conv.selection.range().expect("range");
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
    assert!(!s.conv.selection.dragging);
    assert!(!s.conv.selection.is_empty());
}

#[test]
fn mouse_click_without_drag_on_url_opens_browser() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    let mut s = opened();
    s.size = (80, 30);
    // Plain line with a bare URL; conversation_plain is what hit-testing reads.
    s.conv.rect = Some((0, 3, 80, 20));
    s.conv.auto_scroll = false;
    s.conv.scroll = 0;
    s.conv.plain = vec!["  Local: http://localhost:3000".into()];
    s.conv.plain_width = 80;

    let url_col = "  Local: http://localhost:3000"
        .find("http://")
        .expect("url") as u16;
    let down = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: url_col, // rect x=0
        row: 3,
        modifiers: KeyModifiers::empty(),
    };
    reduce(&mut s, Action::Mouse(down));
    assert!(s.conv.selection.dragging);

    let up = MouseEvent {
        kind: MouseEventKind::Up(MouseButton::Left),
        column: url_col,
        row: 3,
        modifiers: KeyModifiers::empty(),
    };
    let effects = reduce(&mut s, Action::Mouse(up));
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::OpenWebUrl(u) if u == "http://localhost:3000")),
        "expected OpenWebUrl effect, got {effects:?}"
    );
    assert!(!s.conv.selection.is_active());
}

#[test]
fn mouse_click_without_drag_off_url_does_not_open() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    let mut s = opened();
    s.size = (80, 30);
    s.conv.rect = Some((0, 3, 80, 20));
    s.conv.auto_scroll = false;
    s.conv.scroll = 0;
    s.conv.plain = vec!["  Local: http://localhost:3000".into()];
    s.conv.plain_width = 80;

    let down = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 0,
        row: 3,
        modifiers: KeyModifiers::empty(),
    };
    reduce(&mut s, Action::Mouse(down));
    let up = MouseEvent {
        kind: MouseEventKind::Up(MouseButton::Left),
        column: 0,
        row: 3,
        modifiers: KeyModifiers::empty(),
    };
    let effects = reduce(&mut s, Action::Mouse(up));
    assert!(
        !effects.iter().any(|e| matches!(e, Effect::OpenWebUrl(_))),
        "click off URL must not open: {effects:?}"
    );
}

#[test]
fn mouse_click_on_url_glued_to_chinese_prose_opens_clean_host() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use unicode_width::UnicodeWidthChar;
    let mut s = opened();
    s.size = (100, 30);
    s.conv.rect = Some((0, 3, 100, 20));
    s.conv.auto_scroll = false;
    s.conv.scroll = 0;
    // Matches real assistant copy that previously produced toast:
    // "已在浏览器打开 http://localhost:8081，前端页面可以直接打开。"
    let line = "访问地址：http://localhost:8081，前端页面可以直接打开。";
    s.conv.plain = vec![line.into()];
    s.conv.plain_width = 100;

    let byte = line.find("http://").expect("url");
    let url_col: u16 = line[..byte]
        .chars()
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(0) as u16)
        .sum();
    let down = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: url_col + 5,
        row: 3,
        modifiers: KeyModifiers::empty(),
    };
    reduce(&mut s, Action::Mouse(down));
    let up = MouseEvent {
        kind: MouseEventKind::Up(MouseButton::Left),
        column: url_col + 5,
        row: 3,
        modifiers: KeyModifiers::empty(),
    };
    let effects = reduce(&mut s, Action::Mouse(up));
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::OpenWebUrl(u) if u == "http://localhost:8081")),
        "must open clean URL without Chinese glue, got {effects:?}"
    );
    if let Some(n) = &s.notification {
        assert!(
            !n.message.contains('，') && !n.message.contains("前端"),
            "toast must not claim the Chinese prose is part of the URL: {}",
            n.message
        );
    }
}

// ---------------------------------------------------------------------------
// Terminal-agent key conventions (Esc interrupt / Shift+Tab permission cycle /
// Ctrl+C clears the draft first). These are the bindings every other coding
// agent CLI shares, so muscle memory has to carry over.
// ---------------------------------------------------------------------------

fn busy_state() -> AppState {
    let mut s = state();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AgentActivity {
            label: "run".into(),
        }),
    );
    assert_eq!(s.status, RuntimeStatus::Busy);
    s
}

#[test]
fn esc_interrupts_the_running_turn() {
    let mut s = busy_state();
    let first = reduce(&mut s, key(KeyCode::Esc));
    assert_eq!(
        first,
        vec![Effect::Send(ClientCommand::CancelCurrentTurn {
            session_id: SessionId::new("s1"),
        })]
    );
    assert!(s.cancel_armed);
}

#[test]
fn esc_escalates_to_force_cancel_but_never_quits() {
    let mut s = busy_state();
    reduce(&mut s, key(KeyCode::Esc));
    let second = reduce(&mut s, key(KeyCode::Esc));
    assert_eq!(
        second,
        vec![Effect::Send(ClientCommand::ForceCancelCurrentTurn {
            session_id: SessionId::new("s1"),
        })]
    );
    // Quitting is Ctrl+C's job alone — Esc must never drop the session.
    let third = reduce(&mut s, key(KeyCode::Esc));
    assert!(
        !third.contains(&Effect::Quit),
        "Esc must not quit, got {third:?}"
    );
}

#[test]
fn esc_closes_the_slash_popup_before_it_interrupts() {
    let mut s = busy_state();
    reduce(&mut s, key(KeyCode::Char('/')));
    let effects = reduce(&mut s, key(KeyCode::Esc));
    assert!(
        effects.is_empty(),
        "popup dismissal must not cancel the turn: {effects:?}"
    );
    assert!(s.slash_popup_dismissed);
    assert!(!s.cancel_armed);
}

#[test]
fn esc_when_idle_only_clears_the_notice() {
    let mut s = state();
    let effects = reduce(&mut s, key(KeyCode::Esc));
    assert!(effects.is_empty(), "{effects:?}");
    assert!(!s.cancel_armed);
}

#[test]
fn shift_tab_cycles_the_permission_profile() {
    let mut s = state();
    assert_eq!(s.mode, PermissionProfile::Assisted);

    let effects = reduce(&mut s, key(KeyCode::BackTab));
    assert_eq!(
        s.mode,
        PermissionProfile::Assisted,
        "the displayed profile must not lead the runtime"
    );
    assert!(
        !s.mode_label.to_lowercase().contains("full"),
        "chip stays on the last acked value: {}",
        s.mode_label
    );
    assert_eq!(
        effects,
        vec![Effect::Send(ClientCommand::SetPermissionProfile {
            session_id: SessionId::new("s1"),
            mode: PermissionProfile::FullAccess,
        })]
    );

    let mut snap = snapshot();
    snap.mode = PermissionProfile::FullAccess;
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionUpdated { session: snap }),
    );
    assert_eq!(s.mode, PermissionProfile::FullAccess);
    assert!(
        s.mode_label.to_lowercase().contains("full"),
        "header label follows the runtime ack: {}",
        s.mode_label
    );
    assert!(
        s.notification.is_some(),
        "a silent permission change is unacceptable"
    );

    reduce(&mut s, key(KeyCode::BackTab));
    assert_eq!(
        s.mode,
        PermissionProfile::FullAccess,
        "chip still waits for the next ack"
    );
    let mut snap = snapshot();
    snap.mode = PermissionProfile::RequestApproval;
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionUpdated { session: snap }),
    );
    assert_eq!(s.mode, PermissionProfile::RequestApproval);
    reduce(&mut s, key(KeyCode::BackTab));
    let mut snap = snapshot();
    snap.mode = PermissionProfile::Assisted;
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionUpdated { session: snap }),
    );
    assert_eq!(s.mode, PermissionProfile::Assisted);
}

#[test]
fn session_opened_takes_the_runtime_profile_not_a_pending_keystroke() {
    let mut s = state();
    reduce(&mut s, key(KeyCode::BackTab));
    assert_eq!(s.mode, PermissionProfile::Assisted);
    assert_eq!(s.pending_permission, Some(PermissionProfile::FullAccess));

    let mut snap = snapshot();
    snap.mode = PermissionProfile::Assisted;
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened { session: snap }),
    );
    assert_eq!(s.mode, PermissionProfile::Assisted);
    assert_eq!(
        s.pending_permission, None,
        "a reconnect snapshot is runtime truth; a pending keystroke must not keep driving the cycle"
    );
}

#[test]
fn shift_tab_does_not_disturb_the_composer() {
    let mut s = state();
    reduce(&mut s, key(KeyCode::Char('a')));
    reduce(&mut s, key(KeyCode::BackTab));
    assert_eq!(s.composer.text(), "a");
}

#[test]
fn ctrl_c_clears_the_draft_before_it_offers_to_quit() {
    let mut s = state();
    reduce(&mut s, key(KeyCode::Char('h')));
    reduce(&mut s, key(KeyCode::Char('i')));

    let first = reduce(&mut s, ctrl('c'));
    assert!(first.is_empty(), "{first:?}");
    assert!(s.composer.is_empty(), "draft must be cleared first");
    assert!(
        !s.quit_armed,
        "clearing a draft must not arm quit — that is a two-key surprise exit"
    );

    // Only an empty composer means Ctrl+C is about leaving.
    let second = reduce(&mut s, ctrl('c'));
    assert!(second.is_empty());
    assert!(s.quit_armed);
    assert_eq!(reduce(&mut s, ctrl('c')), vec![Effect::Quit]);
}

#[test]
fn ctrl_c_while_busy_still_cancels_even_with_a_draft() {
    let mut s = busy_state();
    reduce(&mut s, key(KeyCode::Char('x')));
    let effects = reduce(&mut s, ctrl('c'));
    assert_eq!(
        effects,
        vec![Effect::Send(ClientCommand::CancelCurrentTurn {
            session_id: SessionId::new("s1"),
        })]
    );
    assert_eq!(
        s.composer.text(),
        "x",
        "cancelling a turn must not eat the draft"
    );
}

// ---------------------------------------------------------------------------
// External editor (Ctrl+X Ctrl+E, as in bash/readline and every other coding
// agent CLI). Writing a long prompt inside a one-line composer is the worst
// part of the input box; $EDITOR is the standard escape hatch.
// ---------------------------------------------------------------------------

#[test]
fn ctrl_x_ctrl_e_hands_the_draft_to_the_external_editor() {
    let mut s = state();
    reduce(&mut s, key(KeyCode::Char('h')));
    reduce(&mut s, key(KeyCode::Char('i')));

    let armed = reduce(&mut s, ctrl('x'));
    assert!(armed.is_empty(), "the prefix alone does nothing: {armed:?}");
    assert_eq!(s.composer.text(), "hi", "Ctrl+X must not edit the draft");

    let effects = reduce(&mut s, ctrl('e'));
    assert_eq!(
        effects,
        vec![Effect::OpenExternalEditor {
            text: "hi".to_string()
        }]
    );
}

#[test]
fn ctrl_x_then_anything_else_falls_back_to_that_key() {
    let mut s = state();
    reduce(&mut s, ctrl('x'));
    reduce(&mut s, key(KeyCode::Char('a')));
    assert_eq!(s.composer.text(), "a");

    // The prefix is spent: a later Ctrl+E is the plain end-of-line motion.
    let effects = reduce(&mut s, ctrl('e'));
    assert!(effects.is_empty(), "{effects:?}");
}

#[test]
fn ctrl_e_without_the_prefix_still_moves_to_end_of_line() {
    let mut s = state();
    reduce(&mut s, key(KeyCode::Char('a')));
    reduce(&mut s, key(KeyCode::Char('b')));
    s.composer.move_to_line_start();
    let effects = reduce(&mut s, ctrl('e'));
    assert!(effects.is_empty(), "{effects:?}");
    assert_eq!(s.composer.cursor(), 2, "Ctrl+E must keep its emacs meaning");
}

#[test]
fn editor_text_comes_back_into_the_composer() {
    let mut s = state();
    reduce(&mut s, key(KeyCode::Char('x')));
    reduce(
        &mut s,
        Action::EditorFinished(Ok("第一行\n第二行".to_string())),
    );
    assert_eq!(s.composer.text(), "第一行\n第二行");
    assert_eq!(s.composer.line_count(), 2);
}

#[test]
fn a_failed_editor_keeps_the_draft_and_says_why() {
    let mut s = state();
    reduce(&mut s, key(KeyCode::Char('x')));
    reduce(
        &mut s,
        Action::EditorFinished(Err("$EDITOR 未设置".to_string())),
    );
    assert_eq!(
        s.composer.text(),
        "x",
        "a failed edit must not eat the draft"
    );
    let note = s.notification.expect("a silent failure is unacceptable");
    assert!(note.message.contains("EDITOR"), "{}", note.message);
}

#[test]
fn an_empty_editor_buffer_clears_the_draft() {
    // Deleting everything in the editor is how you abandon a prompt — honouring
    // it is what makes the editor round trip trustworthy.
    let mut s = state();
    reduce(&mut s, key(KeyCode::Char('x')));
    reduce(&mut s, Action::EditorFinished(Ok(String::new())));
    assert!(s.composer.is_empty());
}

// ---------------------------------------------------------------------------
// Rewind. `/restore` already rolls the workspace back with the transcript
// (checkpoint_before_turn captures a git tree per turn), so the picker has to
// say so — a user who reads "files are not reverted" will press it expecting
// their edits to survive.
// ---------------------------------------------------------------------------

fn checkpoint_picker_description(s: &mut AppState) -> String {
    match s.overlay.as_ref().expect("picker must open") {
        Overlay::CheckpointPicker(model) => model.description.clone().unwrap_or_default(),
        other => panic!("expected the checkpoint picker, got {other:?}"),
    }
}

#[test]
fn the_rewind_picker_does_not_promise_files_are_left_alone() {
    let mut s = state();
    s.checkpoints = vec![UiCheckpoint {
        id: leveler_core::CheckpointId::generate(),
        label: "修登录".into(),
        ordinal: 2,
    }];
    s.composer.replace("/restore");
    reduce(&mut s, key(KeyCode::Enter));

    let description = checkpoint_picker_description(&mut s);
    assert!(
        !description.contains("不回退文件"),
        "the workspace IS rolled back; claiming otherwise invites data loss: {description}"
    );
    assert!(
        description.contains("文件"),
        "the picker must state what happens to the working tree: {description}"
    );
}

#[test]
fn rewind_is_accepted_as_the_name_the_rest_of_the_world_uses() {
    let mut s = state();
    s.checkpoints = vec![UiCheckpoint {
        id: leveler_core::CheckpointId::generate(),
        label: "修登录".into(),
        ordinal: 2,
    }];
    s.composer.replace("/rewind");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(effects.is_empty(), "the picker opens locally: {effects:?}");
    assert!(
        matches!(s.overlay, Some(Overlay::CheckpointPicker(_))),
        "/rewind must reach the same picker as /restore"
    );
    assert!(
        s.composer.is_empty(),
        "a known command must not be left in the composer"
    );
}

#[test]
fn slash_fork_branches_the_session_without_leaving_it() {
    // The runtime copies record + transcript into a fresh session and leaves
    // this one untouched, so the command is safe to fire from the composer.
    let mut s = state();
    s.composer.replace("/fork");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert_eq!(
        effects,
        vec![Effect::Send(ClientCommand::ForkSession {
            session_id: SessionId::new("s1"),
        })]
    );
    assert_eq!(
        s.session_id,
        SessionId::new("s1"),
        "forking must not switch the user away from the conversation they are in"
    );
}

#[test]
fn fork_is_refused_mid_turn_rather_than_copying_half_a_transcript() {
    let mut s = busy_state();
    s.composer.replace("/fork");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(effects.is_empty(), "{effects:?}");
    assert!(s.notification.is_some(), "the refusal must be explained");
}

// ---------------------------------------------------------------------------
// Skill-as-slash, goal lifecycle, doctor, queue
// ---------------------------------------------------------------------------

fn write_project_skill(root: &std::path::Path, name: &str, desc: &str) {
    let dir = root.join(".leveler").join("skills").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {desc}\n---\n\n# {name}\n\nDo the thing.\n"),
    )
    .unwrap();
}

/// Skills live in the `$` namespace only: `/<skill>` is not a second entry.
#[test]
fn slash_skill_name_is_not_a_command() {
    let dir = tempfile::tempdir().unwrap();
    write_project_skill(dir.path(), "code-review", "Review the change");
    let mut s = opened();
    s.repository = dir.path().display().to_string();

    typed(&mut s, "/code");
    assert!(
        leveler_tui::screen::visible_slash_popup(&s)
            .iter()
            .all(|(n, _)| n != "/code-review"),
        "the / popup must not list skills"
    );
    s.composer.replace("/code-review fix auth");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(effects.is_empty(), "{effects:?}");
    assert_eq!(s.composer.text(), "/code-review fix auth");
}

#[test]
fn dollar_popup_lists_skills_and_tab_completes_the_mention() {
    let dir = tempfile::tempdir().unwrap();
    write_project_skill(dir.path(), "code-review", "Review the change");
    let mut s = opened();
    s.repository = dir.path().display().to_string();

    typed(&mut s, "请 $code");
    let matches = leveler_tui::screen::visible_skill_popup(&s);
    assert!(
        matches
            .iter()
            .any(|(n, d)| n == "$code-review" && d.contains("Review")),
        "popup must list skill: {matches:?}"
    );
    reduce(&mut s, key(KeyCode::Tab));
    assert_eq!(s.composer.text(), "请 $code-review ");

    typed(&mut s, "fix auth");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Submit { command: ClientCommand::SubmitMessage { content, .. }, .. }]
                if content == "请 $code-review fix auth"
        ),
        "a $skill message is sent as typed: {effects:?}"
    );
}

#[test]
fn dollar_popup_enter_completes_instead_of_sending() {
    let dir = tempfile::tempdir().unwrap();
    write_project_skill(dir.path(), "code-review", "Review the change");
    let mut s = opened();
    s.repository = dir.path().display().to_string();

    typed(&mut s, "$code");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(effects.is_empty(), "{effects:?}");
    assert_eq!(s.composer.text(), "$code-review ");

    // The completed mention is done: the popup closes and the next Enter sends.
    assert!(leveler_tui::screen::visible_skill_popup(&s).is_empty());
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Submit { command: ClientCommand::SubmitMessage { content, .. }, .. }]
                if content.trim() == "$code-review"
        ),
        "second Enter must send the mention once: {effects:?}"
    );
}

#[test]
fn dollar_without_a_matching_skill_shows_no_popup() {
    let dir = tempfile::tempdir().unwrap();
    write_project_skill(dir.path(), "code-review", "Review the change");
    let mut s = opened();
    s.repository = dir.path().display().to_string();

    typed(&mut s, "costs $100");
    assert!(leveler_tui::screen::visible_skill_popup(&s).is_empty());
}

/// `$` in a shell escape is the shell's, and Enter must run the command.
#[test]
fn dollar_in_a_shell_escape_is_not_a_skill_mention() {
    let dir = tempfile::tempdir().unwrap();
    write_project_skill(dir.path(), "code-review", "Review the change");
    let mut s = opened();
    s.repository = dir.path().display().to_string();

    typed(&mut s, "!echo $");
    assert!(leveler_tui::screen::visible_skill_popup(&s).is_empty());
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Send(ClientCommand::RunUserShell { command, .. })] if command == "echo $"
        ),
        "{effects:?}"
    );
}

/// Builtins and skills no longer share a namespace, so a skill may carry a
/// builtin's name without shadowing it or being filtered out.
#[test]
fn a_skill_named_like_a_builtin_keeps_both() {
    let dir = tempfile::tempdir().unwrap();
    write_project_skill(dir.path(), "help", "Project help skill");
    let mut s = opened();
    s.repository = dir.path().display().to_string();

    typed(&mut s, "$hel");
    assert!(
        leveler_tui::screen::visible_skill_popup(&s)
            .iter()
            .any(|(n, _)| n == "$help"),
        "$help must be offered"
    );
    s.composer.replace("/help");
    reduce(&mut s, key(KeyCode::Enter));
    assert_eq!(s.active_screen, Screen::Help, "/help stays the builtin");
}

#[test]
fn slash_goal_status_and_clear() {
    let mut s = opened();
    s.goal_mode_active = true;
    s.collaboration = "goal".into();

    typed(&mut s, "/goal status");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(effects.is_empty(), "{effects:?}");
    assert!(
        s.notification
            .as_ref()
            .is_some_and(|n| n.message.contains("目标") || n.message.contains("goal")),
        "{:?}",
        s.notification
    );

    typed(&mut s, "/goal clear");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(!s.goal_mode_active);
    assert_eq!(s.collaboration, "chat");
    assert!(
        effects.iter().any(|e| matches!(
            e,
            Effect::Send(ClientCommand::SetProductAxes { collaboration, .. })
                if collaboration == "chat"
        )),
        "{effects:?}"
    );
}

#[test]
fn slash_goal_clear_cancels_a_busy_goal_turn() {
    let mut s = busy_state();
    s.goal_mode_active = true;
    s.composer.replace("/goal clear");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(!s.goal_mode_active);
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::Send(ClientCommand::CancelTask { .. }))),
        "/goal cancel must cancel the logical task, not just the turn: {effects:?}"
    );
}

/// `/goal cancel` on an interrupted (idle, resumable) task is the explicit
/// "give up on this task" entry: it sends CancelTask so a later `继续` cannot
/// reopen it.
#[test]
fn slash_goal_cancel_on_a_resumable_task_sends_cancel_task() {
    let mut s = opened();
    s.resumable_task = true;
    s.composer.replace("/goal cancel");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::Send(ClientCommand::CancelTask { .. }))),
        "{effects:?}"
    );
}

/// The runtime's terminal task-cancel withdraws the resume affordance.
#[test]
fn a_task_cancelled_event_withdraws_resumability() {
    let mut s = opened();
    s.resumable_task = true;
    s.goal_mode_active = true;
    reduce(&mut s, Action::Runtime(RuntimeEvent::TaskCancelled));
    assert!(
        !s.resumable_task,
        "an explicitly cancelled task must not stay resumable"
    );
    assert!(!s.goal_mode_active);
    assert!(
        s.notification
            .as_ref()
            .is_some_and(|n| n.message.contains("取消")),
        "{:?}",
        s.notification
    );
}

/// The consent gate (K36) is only usable if the UI it gates can act on it.
/// Before this, pending candidates were invisible in the TUI and the only way
/// to adopt one was `leveler memory accept` on the command line.
#[test]
fn slash_memory_accept_promotes_a_pending_candidate() {
    let mut s = state();
    s.composer.replace("/memory accept rust-anyhow");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert_eq!(
        effects,
        vec![Effect::Send(ClientCommand::AcceptMemory {
            session_id: SessionId::new("s1"),
            id: "rust-anyhow".to_string(),
        })]
    );
}

#[test]
fn slash_memory_accept_without_an_id_explains_itself() {
    let mut s = state();
    s.composer.replace("/memory accept");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(effects.is_empty(), "{effects:?}");
    let note = s.notification.expect("must say what is missing");
    assert!(note.message.contains("accept"), "{}", note.message);
}

/// A listing that hides what is waiting is why nobody ever accepted anything.
#[test]
fn the_memory_listing_shows_pending_candidates_and_how_to_accept() {
    let mut s = state();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::MemoryList {
            memory_dir: "/tmp/mem".into(),
            active: vec![],
            archived: vec![],
            pending: vec![leveler_client_protocol::UiMemoryCandidate {
                id: "use-pnpm".into(),
                title: "本仓库用 pnpm".into(),
                body: "安装与脚本一律用 pnpm，不要默认 npm。".into(),
                kind: "preference".into(),
                source: "user_explicit".into(),
            }],
        }),
    );
    let text = format!("{:?}", s.transcript.items());
    // The count and the id are what matter; the word beside them follows the
    // locale (IA §12 Class A).
    assert!(text.contains("待确认 (1)"), "{text}");
    assert!(text.contains("use-pnpm"), "{text}");
    assert!(
        text.contains("/memory accept"),
        "the way to adopt it must be shown: {text}"
    );
    assert!(
        text.contains("/memory reject"),
        "declining must be offered too, not only adopting: {text}"
    );
    assert!(
        text.contains("不要默认 npm"),
        "consent needs the body, not just the title: {text}"
    );
}

/// `/remember` is the user's own write: one command out, no model request, no
/// agent turn, and no pending candidate to approve afterwards.
#[test]
fn remember_slash_sends_one_direct_write_and_nothing_else() {
    let mut s = state();
    s.composer.replace("/remember 终端输出保持紧凑");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert_eq!(
        effects,
        vec![Effect::Send(ClientCommand::RememberMemory {
            session_id: SessionId::new("s1"),
            body: "终端输出保持紧凑".to_string(),
            kind: Some(leveler_client_protocol::UiMemoryKind::Preference),
        })],
        "exactly one direct write, defaulting to a lasting preference"
    );
}

/// A decision is stored but must NOT become standing context, so the kind has
/// to survive the command rather than being inferred later.
#[test]
fn remember_slash_carries_an_explicit_kind() {
    for (input, kind) in [
        (
            "/remember --kind decision 选用 SQLite",
            leveler_client_protocol::UiMemoryKind::Decision,
        ),
        (
            "/remember --kind note 顺手记一下",
            leveler_client_protocol::UiMemoryKind::Note,
        ),
        (
            "/remember --kind preference 保持紧凑",
            leveler_client_protocol::UiMemoryKind::Preference,
        ),
    ] {
        let mut s = state();
        s.composer.replace(input);
        let effects = reduce(&mut s, key(KeyCode::Enter));
        match effects.as_slice() {
            [
                Effect::Send(ClientCommand::RememberMemory {
                    kind: got, body, ..
                }),
            ] => {
                assert_eq!(*got, Some(kind), "{input}");
                assert!(
                    !body.starts_with("--kind"),
                    "the flag must be consumed: {body}"
                );
            }
            other => panic!("{input}: {other:?}"),
        }
    }
}

/// Empty content is a usage error, not an empty memory.
#[test]
fn remember_slash_refuses_empty_content() {
    let mut s = state();
    s.composer.replace("/remember   ");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(effects.is_empty(), "nothing is sent: {effects:?}");
    assert!(s.notification.is_some(), "the user is told how to use it");
}

/// A direct write during a running turn must not become steering, and must
/// not be sent to the model: it is a control-plane command, not a message.
#[test]
fn remember_while_busy_is_a_write_not_steering() {
    let mut s = busy_state();
    s.composer.replace("/remember 输出保持紧凑");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert_eq!(
        effects,
        vec![Effect::Send(ClientCommand::RememberMemory {
            session_id: SessionId::new("s1"),
            body: "输出保持紧凑".to_string(),
            kind: Some(leveler_client_protocol::UiMemoryKind::Preference),
        })],
        "a write, not SteerCurrentTurn and not SubmitMessage"
    );
    assert!(
        !effects.iter().any(|e| matches!(
            e,
            Effect::Submit {
                command: ClientCommand::SteerCurrentTurn { .. },
                ..
            } | Effect::Submit {
                command: ClientCommand::SubmitMessage { .. },
                ..
            }
        )),
        "{effects:?}"
    );
}

/// The listing must say what the model will never see, or "stored" reads as
/// "in use".
#[test]
fn the_listing_marks_a_sensitive_entry_as_withheld() {
    let mut s = state();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::MemoryList {
            memory_dir: "/tmp/mem".into(),
            active: vec![leveler_client_protocol::UiMemoryEntry {
                id: "legacy".into(),
                title: "deploy token".into(),
                kind: Some(leveler_client_protocol::UiMemoryKind::Note),
                sensitive: true,
            }],
            archived: vec![],
            pending: vec![],
        }),
    );
    let text = format!("{:?}", s.transcript.items());
    assert!(text.contains("敏感内容"), "{text}");
    assert!(text.contains("笔记"), "the kind is shown too: {text}");
}

/// `/memory reject` must send reject, not forget. Sending a pending id to
/// forget did nothing at all — the bug the Web button shipped with.
#[test]
fn memory_reject_sends_reject_not_forget() {
    let mut s = state();
    s.composer.replace("/memory reject cand-x");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert_eq!(
        effects,
        vec![Effect::Send(ClientCommand::RejectMemory {
            session_id: SessionId::new("s1"),
            id: "cand-x".to_string(),
        })]
    );
}

/// Forget still means archive-an-active-entry; the two stay distinct.
#[test]
fn memory_forget_still_targets_active_entries() {
    let mut s = state();
    s.composer.replace("/memory forget mem-1");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert_eq!(
        effects,
        vec![Effect::Send(ClientCommand::ForgetMemory {
            session_id: SessionId::new("s1"),
            id: "mem-1".to_string(),
        })]
    );
}

/// Enter while a turn runs holds the text in 待发送; it is not sent. Sending it
/// (Tab to the list, Enter) steers the running turn: the text reaches the model
/// at the next round, and the runtime falls back to an ordinary submission if
/// the turn already finished, so nothing is lost.
#[test]
fn submitting_while_busy_holds_the_input_and_sending_it_steers_the_running_turn() {
    let mut s = busy_state();
    s.composer.replace("改用另一个模块");
    assert!(
        reduce(&mut s, key(KeyCode::Enter)).is_empty(),
        "held, not sent"
    );
    assert!(s.composer.is_empty(), "the composer clears into 待发送");
    let effects = send_first_pending(&mut s);
    assert_eq!(
        submitted(&effects).0,
        ClientCommand::SteerCurrentTurn {
            session_id: SessionId::new("s1"),
            content: "改用另一个模块".to_string(),
        }
    );
}

/// Tab from the composer to 待发送, then Enter on its selected item.
fn send_first_pending(s: &mut AppState) -> Vec<Effect> {
    reduce(s, key(KeyCode::Tab));
    reduce(s, key(KeyCode::Up));
    reduce(s, key(KeyCode::Up));
    reduce(s, key(KeyCode::Enter))
}

/// A steer enters the conversation when the runtime admits it — not when it
/// is typed, and not when it is sent.
#[test]
fn steered_text_appears_in_the_conversation_once_admitted() {
    let mut s = busy_state();
    s.composer.replace("改用另一个模块");
    reduce(&mut s, key(KeyCode::Enter));
    let (_, command_id) = submitted(&send_first_pending(&mut s));
    assert!(!format!("{:?}", s.transcript.items()).contains("改用另一个模块"));
    reduce(
        &mut s,
        Action::EffectCompleted(EffectCompletion::SubmissionDelivered {
            command_id,
            snapshot: None,
        }),
    );
    let text = format!("{:?}", s.transcript.items());
    assert!(text.contains("改用另一个模块"), "{text}");
}

/// Several held corrections are each sent on their own.
#[test]
fn several_steers_are_each_sent() {
    let mut s = busy_state();
    for msg in ["先改模块", "再加测试"] {
        s.composer.replace(msg);
        reduce(&mut s, key(KeyCode::Enter));
    }
    for msg in ["先改模块", "再加测试"] {
        let effects = send_first_pending(&mut s);
        assert!(
            matches!(
                effects.as_slice(),
                [Effect::Submit { command: ClientCommand::SteerCurrentTurn { content, .. }, .. }] if content == msg
            ),
            "{effects:?}"
        );
        let (_, command_id) = submitted(&effects);
        reduce(
            &mut s,
            Action::EffectCompleted(EffectCompletion::SubmissionDelivered {
                command_id,
                snapshot: None,
            }),
        );
        reduce(&mut s, key(KeyCode::Esc));
    }
}

#[test]
fn submitting_while_idle_still_submits_normally() {
    let mut s = state();
    s.composer.replace("做个登录页");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Submit {
                command: ClientCommand::SubmitMessage { .. },
                ..
            }]
        ),
        "{effects:?}"
    );
}

// ── Turn-input delivery truth ────────────────────────────────────────────────

/// The one Submit effect and the id it carries.
fn submitted(effects: &[Effect]) -> (ClientCommand, leveler_client_protocol::CommandId) {
    match effects {
        [
            Effect::Submit {
                command,
                command_id,
            },
        ] => (command.clone(), command_id.clone()),
        other => panic!("expected one Submit, got {other:?}"),
    }
}

/// The logical command owns its id from the moment the user presses Enter; the
/// client keeps it until the runtime answers, so every retry can carry it.
#[test]
fn a_message_keeps_its_command_id_until_the_runtime_answers() {
    let mut s = state();
    s.composer.replace("做个登录页");
    let (command, command_id) = submitted(&reduce(&mut s, key(KeyCode::Enter)));
    assert!(
        matches!(command, ClientCommand::SubmitMessage { ref content, .. } if content == "做个登录页")
    );
    assert_eq!(s.pending_submissions.len(), 1);
    assert_eq!(s.pending_submissions[0].command_id, command_id);

    reduce(
        &mut s,
        Action::EffectCompleted(EffectCompletion::SubmissionDelivered {
            command_id,
            snapshot: None,
        }),
    );
    assert!(s.pending_submissions.is_empty());
    assert_eq!(s.status, RuntimeStatus::Busy, "delivered work stays busy");
}

/// Steering is a turn input too: pushed into a live turn, or started as a turn
/// when the one it aimed at just ended. It gets the same identity.
#[test]
fn a_steer_is_a_tracked_submission() {
    let mut s = busy_state();
    s.composer.replace("改用另一个模块");
    reduce(&mut s, key(KeyCode::Enter));
    let (command, command_id) = submitted(&send_first_pending(&mut s));
    assert!(matches!(command, ClientCommand::SteerCurrentTurn { .. }));
    assert_eq!(s.pending_submissions[0].command_id, command_id);
}

/// No answer is not "not delivered": the runtime may already be running it. A
/// reconnect snapshot taken before it landed must not reopen the composer, and
/// a second input must not be sent — that is how a retry becomes a second turn.
#[test]
fn an_unanswered_submission_holds_new_input_instead_of_driving_a_second_turn() {
    let mut s = state();
    s.composer.replace("第一条");
    let (_, command_id) = submitted(&reduce(&mut s, key(KeyCode::Enter)));
    reduce(
        &mut s,
        Action::EffectCompleted(EffectCompletion::SubmissionUnconfirmed { command_id }),
    );
    assert!(s.pending_submissions[0].unconfirmed);
    assert!(s.runtime_connected, "no answer is not a dead runtime");

    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot(),
        }),
    );
    assert_eq!(
        s.status,
        RuntimeStatus::Busy,
        "an idle snapshot must not expose Idle while delivery is unknown"
    );

    s.composer.replace("第二条");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(effects.is_empty(), "nothing may be sent: {effects:?}");
    assert_eq!(s.pending_inputs.len(), 1, "the new input is kept in 待发送");
    let effects = send_first_pending(&mut s);
    assert!(effects.is_empty(), "nothing may be sent: {effects:?}");
    assert!(s.pending_inputs[0].is_unsent(), "still held, still unsent");
    let note = s
        .notification
        .as_ref()
        .map(|n| n.message.as_str())
        .unwrap_or("");
    assert!(note.contains("无法确认"), "{note}");
}

/// ACK lost, runtime kept going: the answer that arrives later settles the
/// same command, syncs the runtime's state, and sends nothing new.
#[test]
fn a_submission_confirmed_after_reconnect_syncs_runtime_state_without_resending() {
    let mut s = state();
    s.composer.replace("实现登录");
    let (_, command_id) = submitted(&reduce(&mut s, key(KeyCode::Enter)));
    reduce(
        &mut s,
        Action::EffectCompleted(EffectCompletion::SubmissionUnconfirmed {
            command_id: command_id.clone(),
        }),
    );
    let mut running = snapshot();
    running.status = "running".into();
    running.messages = vec![UiMessage {
        id: MessageId::new("u1"),
        role: UiRole::User,
        text: "实现登录".into(),
        ordinal: Some(0),
        kind: None,
        images: 0,
    }];
    let effects = reduce(
        &mut s,
        Action::EffectCompleted(EffectCompletion::SubmissionDelivered {
            command_id,
            snapshot: Some(Box::new(running)),
        }),
    );
    assert!(effects.is_empty(), "{effects:?}");
    assert!(s.pending_submissions.is_empty());
    assert_eq!(s.status, RuntimeStatus::Busy);
    let users = s
        .transcript
        .items()
        .iter()
        .filter(|item| matches!(item, TranscriptItem::User(text) if text == "实现登录"))
        .count();
    assert_eq!(users, 1, "one message, from the runtime's record");
    let note = s
        .notification
        .as_ref()
        .map(|n| n.message.as_str())
        .unwrap_or("");
    assert!(note.contains("已送达"), "{note}");
}

/// The runtime answered "not run": the input goes back into the composer, the
/// optimistic busy state yields to the runtime's, and a new submission is a
/// new logical command with a new id.
#[test]
fn a_rejected_submission_returns_the_text_and_reopens_the_composer() {
    let mut s = state();
    s.composer.replace("做个登录页");
    let (_, first_id) = submitted(&reduce(&mut s, key(KeyCode::Enter)));
    reduce(
        &mut s,
        Action::EffectCompleted(EffectCompletion::SubmissionRejected {
            command_id: first_id.clone(),
            message: "session already has an active turn".into(),
            snapshot: Some(Box::new(snapshot())),
        }),
    );
    assert!(s.pending_submissions.is_empty());
    assert_eq!(s.status, RuntimeStatus::Idle);
    assert_eq!(s.composer.text(), "做个登录页");
    let note = s
        .notification
        .as_ref()
        .map(|n| n.message.as_str())
        .unwrap_or("");
    assert!(
        note.contains("active turn"),
        "the runtime's own words: {note}"
    );

    let (_, second_id) = submitted(&reduce(&mut s, key(KeyCode::Enter)));
    assert_ne!(second_id, first_id);
}

/// The answer can land after the user moved to another session. It settles the
/// command; the old session's snapshot must not pull the view back.
#[test]
fn a_late_answer_does_not_switch_the_view_back_to_its_session() {
    let mut s = state();
    s.composer.replace("第一条");
    let (_, command_id) = submitted(&reduce(&mut s, key(KeyCode::Enter)));
    reduce(
        &mut s,
        Action::EffectCompleted(EffectCompletion::SubmissionUnconfirmed {
            command_id: command_id.clone(),
        }),
    );
    let mut other = snapshot();
    other.id = SessionId::new("s2");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened { session: other }),
    );

    reduce(
        &mut s,
        Action::EffectCompleted(EffectCompletion::SubmissionDelivered {
            command_id,
            snapshot: Some(Box::new(snapshot())),
        }),
    );
    assert!(s.pending_submissions.is_empty());
    assert_eq!(s.session_id, SessionId::new("s2"));
}

/// An answer for a command this client no longer tracks changes nothing.
#[test]
fn an_answer_for_an_untracked_command_is_ignored() {
    let mut s = state();
    reduce(
        &mut s,
        Action::EffectCompleted(EffectCompletion::SubmissionRejected {
            command_id: leveler_client_protocol::CommandId::new("cmd-stale"),
            message: "busy".into(),
            snapshot: None,
        }),
    );
    assert!(s.composer.is_empty());
    assert!(s.notification.is_none());
}

/// A runtime that answered with an error is reachable. Saying "cannot connect"
/// turns a business failure into a transport one.
#[test]
fn a_runtime_rejection_is_not_reported_as_a_lost_connection() {
    let mut s = state();
    reduce(
        &mut s,
        Action::EffectCompleted(EffectCompletion::CommandRejected {
            command: ClientCommand::CompactContext {
                session_id: SessionId::new("s1"),
            },
            message: "当前有进行中的回合，请先等待完成或取消后再压缩".into(),
            snapshot: Some(Box::new(snapshot())),
        }),
    );
    assert!(s.runtime_connected);
    let note = s
        .notification
        .as_ref()
        .map(|n| n.message.as_str())
        .unwrap_or("");
    assert!(note.contains("进行中的回合"), "{note}");
    assert!(!note.contains("无法连接"), "{note}");
}

// ── User shell (`!command`) routing / lifecycle / details ───────────────────

fn shell_started(s: &mut AppState, id: &str, command: &str) {
    reduce(
        s,
        Action::Runtime(RuntimeEvent::UserShellStarted {
            execution_id: leveler_core::UserShellId::new(id),
            command: command.into(),
            cwd: "/repo".into(),
        }),
    );
}

#[test]
fn bang_routes_to_user_shell_and_never_the_model() {
    let mut s = opened();
    typed(&mut s, "!echo hi");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Send(ClientCommand::RunUserShell { command, .. })] if command == "echo hi"
        ),
        "{effects:?}"
    );
    // Details opens immediately; input history keeps it; the conversation
    // and model history never see it.
    assert_eq!(s.active_screen, Screen::Shell);
    assert!(s.composer.history().iter().any(|h| h == "!echo hi"));
    assert!(
        !s.transcript
            .items()
            .iter()
            .any(|i| matches!(i, TranscriptItem::User(text) if text.contains("echo"))),
        "a shell command must not enter the conversation"
    );
}

#[test]
fn leading_whitespace_bang_is_a_normal_message() {
    let mut s = opened();
    typed(&mut s, " !not a shell");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Submit { command: ClientCommand::SubmitMessage { content, .. }, .. }]
                if content.trim() == "!not a shell"
        ),
        "trimmed submit keeps the text as a message: {effects:?}"
    );
}

#[test]
fn bare_bang_hints_and_keeps_the_composer() {
    let mut s = opened();
    typed(&mut s, "!");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(effects.is_empty(), "{effects:?}");
    assert_eq!(s.composer.text(), "!", "content preserved for editing");
    assert!(s.notification.is_some(), "the hint names what is missing");
}

#[test]
fn busy_agent_turn_rejects_bang_locally() {
    let mut s = opened();
    s.status = leveler_client_protocol::RuntimeStatus::Busy;
    typed(&mut s, "!touch x");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(effects.is_empty(), "no command while busy: {effects:?}");
    assert!(s.notification.is_some());
}

#[test]
fn user_shell_lifecycle_updates_the_block() {
    let mut s = opened();
    shell_started(&mut s, "ush-1", "cargo test");
    let idx = s
        .transcript
        .user_shell_index(&leveler_core::UserShellId::new("ush-1"))
        .expect("block created");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::UserShellOutput {
            execution_id: leveler_core::UserShellId::new("ush-1"),
            stream: "stdout".into(),
            chunk: "\u{1b}[31mred\u{1b}[0m line\n".into(),
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::UserShellExited {
            execution_id: leveler_core::UserShellId::new("ush-1"),
            exit_code: Some(0),
            duration_ms: 4200,
            status: "success".into(),
        }),
    );
    let TranscriptItem::UserShell(shell) = &s.transcript.items()[idx] else {
        panic!("user shell block expected");
    };
    assert_eq!(
        shell.status,
        leveler_tui::transcript::UserShellStatus::Success
    );
    assert_eq!(shell.duration_ms, Some(4200));
    assert!(
        shell.output.contains("red line") && !shell.output.contains('\u{1b}'),
        "ANSI stripped before buffering: {:?}",
        shell.output
    );
}

#[test]
fn shell_details_x_cancels_only_a_running_shell() {
    let mut s = opened();
    shell_started(&mut s, "ush-run", "sleep 30");
    s.active_screen = Screen::Shell;
    let effects = reduce(&mut s, key(KeyCode::Char('x')));
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Send(ClientCommand::CancelUserShell { execution_id, .. })]
                if execution_id.as_str() == "ush-run"
        ),
        "{effects:?}"
    );
    // Finished shell: x is inert.
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::UserShellExited {
            execution_id: leveler_core::UserShellId::new("ush-run"),
            exit_code: None,
            duration_ms: 100,
            status: "cancelled".into(),
        }),
    );
    s.active_screen = Screen::Shell;
    let effects = reduce(&mut s, key(KeyCode::Char('x')));
    assert!(effects.is_empty(), "{effects:?}");
    // Esc backs out without any cancel.
    let effects = reduce(&mut s, key(KeyCode::Esc));
    assert!(effects.is_empty());
    assert_eq!(s.active_screen, Screen::Conversation);
}

#[test]
fn snapshot_restores_shell_blocks_and_running_focus() {
    let mut s = opened();
    let mut snap = snapshot();
    snap.user_shells = vec![
        leveler_client_protocol::UiUserShell {
            id: leveler_core::UserShellId::new("ush-done"),
            command: "echo done".into(),
            cwd: "/repo".into(),
            status: "success".into(),
            elapsed_secs: 2,
            exit_code: Some(0),
            output_tail: "done\n".into(),
            output_truncated: false,
        },
        leveler_client_protocol::UiUserShell {
            id: leveler_core::UserShellId::new("ush-live"),
            command: "sleep 60".into(),
            cwd: "/repo".into(),
            status: "running".into(),
            elapsed_secs: 21,
            exit_code: None,
            output_tail: String::new(),
            output_truncated: false,
        },
    ];
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened { session: snap }),
    );
    let shells: Vec<_> = s
        .transcript
        .items()
        .iter()
        .filter_map(|i| match i {
            TranscriptItem::UserShell(b) => Some(b),
            _ => None,
        })
        .collect();
    assert_eq!(shells.len(), 2);
    assert_eq!(
        shells[0].status,
        leveler_tui::transcript::UserShellStatus::Success
    );
    assert_eq!(
        shells[1].status,
        leveler_tui::transcript::UserShellStatus::Running
    );
    let focused = s.focused_user_shell().expect("running shell focused");
    assert_eq!(focused.command, "sleep 60");
    assert!(
        (s.elapsed_secs as i64 - focused.started_elapsed_secs) >= 21,
        "elapsed survives reconnect"
    );
}

// ── R004 F1: paste placeholders are presentation-only; every submit path must
// expand them back to canonical content (§R004 Harness Repair Gate) ──────────

const BIG_PASTE: &str = "line one\nline two\n第三行中文\n\n    indented\nfinal line";

#[test]
fn slash_goal_expands_large_paste_to_canonical_content() {
    let mut s = opened();
    typed(&mut s, "/goal ");
    reduce(&mut s, Action::Paste(BIG_PASTE.into()));
    // The composer shows a chip, not the content (presentation).
    assert!(
        s.composer.text().contains("[Pasted:"),
        "precondition: chip in buffer: {}",
        s.composer.text()
    );
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Submit { command: ClientCommand::RunGoal { content, .. }, .. }] if content == BIG_PASTE
        ),
        "/goal must submit the pasted content, not the placeholder: {effects:?}"
    );
}

#[test]
fn steering_expands_large_paste_to_canonical_content() {
    let mut s = busy_state();
    typed(&mut s, "context: ");
    reduce(&mut s, Action::Paste(BIG_PASTE.into()));
    reduce(&mut s, key(KeyCode::Enter));
    let effects = send_first_pending(&mut s);
    let want = format!("context: {BIG_PASTE}");
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Submit { command: ClientCommand::SteerCurrentTurn { content, .. }, .. }] if content == &want
        ),
        "steering must carry the pasted content: {effects:?}"
    );
}

#[test]
fn bang_shell_expands_large_paste_to_canonical_content() {
    let mut s = opened();
    typed(&mut s, "!");
    reduce(&mut s, Action::Paste(BIG_PASTE.into()));
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Send(ClientCommand::RunUserShell { command, .. })] if command == BIG_PASTE
        ),
        "! shell must run the pasted command text: {effects:?}"
    );
}

#[test]
fn btw_expands_large_paste_to_canonical_content() {
    let mut s = busy_state();
    typed(&mut s, "/btw ");
    reduce(&mut s, Action::Paste(BIG_PASTE.into()));
    let effects = reduce(&mut s, key(KeyCode::Enter));
    let text = format!("{effects:?}");
    assert!(
        text.contains("line two") && !text.contains("[Pasted:"),
        "/btw must carry pasted content, not the chip: {effects:?}"
    );
}

/// T2: a user who literally TYPES placeholder-looking text gets it delivered
/// verbatim — the chip is a composer artifact, not a reserved syntax.
#[test]
fn typed_placeholder_text_is_delivered_literally() {
    let mut s = opened();
    typed(&mut s, "[Pasted: 5 lines]");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Submit { command: ClientCommand::SubmitMessage { content, .. }, .. }] if content == "[Pasted: 5 lines]"
        ),
        "typed literal must stay literal: {effects:?}"
    );
}

// ── R004 F2: input while a clarification is on screen answers the question ──

#[test]
fn paste_while_clarification_overlay_open_fills_the_answer() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ClarificationRequested {
            request: leveler_client_protocol::UiClarificationRequest::single(
                leveler_core::ClarificationId::new("c1"),
                "task content?",
                vec![],
            ),
        }),
    );
    assert!(matches!(s.overlay, Some(Overlay::Clarification(_))));
    reduce(
        &mut s,
        Action::Paste("line a\nline b\nline c\nline d\nline e\nline f".into()),
    );
    // The paste must land in the overlay's answer, not the composer.
    let Some(Overlay::Clarification(ov)) = &s.overlay else {
        panic!("overlay closed unexpectedly");
    };
    assert!(
        ov.active_text().contains("line a") && ov.active_text().contains("line f"),
        "{}",
        ov.active_text()
    );
    assert!(
        s.composer.is_empty(),
        "composer must not swallow the paste: {}",
        s.composer.text()
    );
    // Enter submits the pasted answer, not an empty skip.
    let effects = reduce(&mut s, key(KeyCode::Enter));
    let text = format!("{effects:?}");
    assert!(
        text.contains("AnswerClarification") && text.contains("line a"),
        "{effects:?}"
    );
}

/// F1: a plan transition must be on screen in the very next frame. Waiting
/// for the next assistant message, the next turn, or a session refresh is what
/// leaves the dock claiming step 1 while the agent is on step 2.
#[test]
fn a_plan_transition_is_on_screen_in_the_next_frame() {
    use leveler_client_protocol::PlanStepStatus as P;
    let mut s = busy_state();
    let plan = |a, b, c| {
        Action::Runtime(RuntimeEvent::PlanUpdated {
            plan: UiPlan {
                steps: vec![
                    UiPlanStep {
                        index: 0,
                        description: "骨架".into(),
                        status: a,
                    },
                    UiPlanStep {
                        index: 1,
                        description: "首页".into(),
                        status: b,
                    },
                    UiPlanStep {
                        index: 2,
                        description: "验证".into(),
                        status: c,
                    },
                ],
            },
        })
    };
    reduce(&mut s, plan(P::Running, P::Pending, P::Pending));
    assert_eq!(s.plan.as_ref().unwrap().steps[0].status, P::Running);

    reduce(&mut s, plan(P::Done, P::Running, P::Pending));
    let statuses: Vec<char> = s
        .plan
        .as_ref()
        .unwrap()
        .steps
        .iter()
        .map(|step| match step.status {
            P::Done => '✓',
            P::Running => '●',
            _ => '○',
        })
        .collect();
    assert_eq!(
        statuses,
        vec!['✓', '●', '○'],
        "the transition is state, not a deferred repaint"
    );
}

/// F6: a child settling is not a plan transition. Only the parent can say
/// whether the step that delegated the work is finished — one step may span
/// several children plus its own integration and verification.
#[test]
fn a_settling_sub_agent_never_advances_the_plan_by_itself() {
    use leveler_client_protocol::PlanStepStatus as P;
    let mut s = busy_state();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::PlanUpdated {
            plan: UiPlan {
                steps: vec![
                    UiPlanStep {
                        index: 0,
                        description: "委派页面".into(),
                        status: P::Running,
                    },
                    UiPlanStep {
                        index: 1,
                        description: "整合".into(),
                        status: P::Pending,
                    },
                ],
            },
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SubAgentUpdated {
            id: "s1".into(),
            nickname: "Newton".into(),
            role: "worker".into(),
            title: None,
            done: true,
            ok: true,
            detail: "页面完成".into(),
            profile_id: None,
            profile_role: None,
            read_only: false,
            agent: None,
            contribution: None,
            outcome: None,
            stop: None,
            limit: None,
            background: None,
            scope: Vec::new(),
        }),
    );
    let steps = &s.plan.as_ref().unwrap().steps;
    assert_eq!(steps[0].status, P::Running, "the parent has not said so");
    assert_eq!(steps[1].status, P::Pending);
}

/// F7: once the parent DOES decide, the transition lands like any other.
#[test]
fn the_parent_advancing_after_a_child_settles_projects_normally() {
    use leveler_client_protocol::PlanStepStatus as P;
    let mut s = busy_state();
    let plan = |a, b| {
        Action::Runtime(RuntimeEvent::PlanUpdated {
            plan: UiPlan {
                steps: vec![
                    UiPlanStep {
                        index: 0,
                        description: "委派页面".into(),
                        status: a,
                    },
                    UiPlanStep {
                        index: 1,
                        description: "整合".into(),
                        status: b,
                    },
                ],
            },
        })
    };
    reduce(&mut s, plan(P::Running, P::Pending));
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SubAgentUpdated {
            id: "s1".into(),
            nickname: "Newton".into(),
            role: "worker".into(),
            title: None,
            done: true,
            ok: true,
            detail: "页面完成".into(),
            profile_id: None,
            profile_role: None,
            read_only: false,
            agent: None,
            contribution: None,
            outcome: None,
            stop: None,
            limit: None,
            background: None,
            scope: Vec::new(),
        }),
    );
    reduce(&mut s, plan(P::Done, P::Running));
    let steps = &s.plan.as_ref().unwrap().steps;
    assert_eq!(steps[0].status, P::Done);
    assert_eq!(steps[1].status, P::Running);
}

/// A plan with five done steps, one running and three pending — the shape
/// that showed "任务已完成 … 计划 6/9" next to "▼ 计划 · 第 7/9 项进行中".
fn stale_open_plan(s: &mut AppState) {
    let step = |i: usize, st| UiPlanStep {
        index: i,
        description: format!("步骤 {}", i + 1),
        status: st,
    };
    use leveler_client_protocol::PlanStepStatus as P;
    reduce(
        s,
        Action::Runtime(RuntimeEvent::PlanUpdated {
            plan: UiPlan {
                steps: (0..9)
                    .map(|i| match i {
                        0..=4 => step(i, P::Done),
                        5 => step(i, P::Running),
                        _ => step(i, P::Pending),
                    })
                    .collect(),
            },
        }),
    );
}

fn historical_plans(s: &AppState) -> Vec<&UiPlan> {
    s.transcript
        .items()
        .iter()
        .filter_map(|item| match item {
            leveler_tui::transcript::TranscriptItem::Plan(plan) => Some(plan),
            _ => None,
        })
        .collect()
}

/// A fully settled plan is execution chrome, not durable conversation content.
/// Once the turn ends there must be no 8/8-style checklist left above the
/// terminal marker.
#[test]
fn a_completed_plan_disappears_when_the_turn_ends() {
    use leveler_client_protocol::PlanStepStatus as P;
    let mut s = busy_state();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::PlanUpdated {
            plan: UiPlan {
                steps: vec![
                    UiPlanStep {
                        index: 0,
                        description: "实现".into(),
                        status: P::Done,
                    },
                    UiPlanStep {
                        index: 1,
                        description: "无需执行".into(),
                        status: P::Skipped,
                    },
                ],
            },
        }),
    );
    answer(&mut s, "m-final", "完成。");
    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCompleted));

    assert!(s.plan.is_none(), "terminal work is not active");
    assert!(
        historical_plans(&s).is_empty(),
        "a settled checklist must not remain in conversation history"
    );
    assert!(!rendered(&mut s, 100, 30).contains("计划"));
}

/// D1: a task can finish while the plan the agent declared is still open —
/// the plan is not the completion authority. The exact declaration moves to
/// history (no invented N/N) and leaves the active slot.
#[test]
fn a_completed_turn_archives_the_open_plan_as_its_last_record() {
    use leveler_client_protocol::PlanStepStatus as P;
    let mut s = busy_state();
    stale_open_plan(&mut s);
    let declared = s.plan.clone();
    answer(&mut s, "m-final", "做完了。");
    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCompleted));
    assert!(s.plan.is_none(), "terminal work is not active");
    assert_eq!(historical_plans(&s), vec![declared.as_ref().unwrap()]);
    assert_ne!(s.status, RuntimeStatus::Busy, "the turn is not running");
    let steps = &historical_plans(&s)[0].steps;
    assert_eq!(steps.iter().filter(|x| x.status == P::Done).count(), 5);
}

/// D2 / D3: the same holds for the two "done, with a caveat" outcomes.
#[cfg(any())]
#[test]
fn unverified_and_checks_failed_turns_also_archive_the_last_record() {
    for event in [
        RuntimeEvent::TurnCompletedUnverified {
            reason: "无自动验证".into(),
        },
        RuntimeEvent::TurnCompletedChecksFailed {
            reason: "验证未通过".into(),
        },
    ] {
        let mut s = busy_state();
        stale_open_plan(&mut s);
        let declared = s.plan.clone();
        reduce(&mut s, Action::Runtime(event.clone()));
        assert!(s.plan.is_none(), "{event:?}");
        assert_eq!(historical_plans(&s), vec![declared.as_ref().unwrap()]);
    }
}

/// D1b: a finished task's plan is not carried into the next task — the
/// runtime seeds a plan only into a resumed unfinished task — so the next
/// turn does not start under the old plan.
#[test]
fn the_next_turn_does_not_run_under_a_finished_tasks_plan() {
    let mut s = busy_state();
    stale_open_plan(&mut s);
    answer(&mut s, "m-final", "做完了。");
    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCompleted));
    assert!(s.plan.is_none());
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AgentActivity {
            label: "next".into(),
        }),
    );
    assert_eq!(s.status, RuntimeStatus::Busy);
    assert!(
        s.plan.is_none(),
        "the old task's plan is not the new turn's"
    );
}

/// D4b: a terminal event closes the old activity even when its outcome is
/// incomplete. A later runtime resume must explicitly restore a live plan.
#[test]
fn the_next_turn_does_not_resurrect_an_incomplete_turns_plan() {
    let mut s = busy_state();
    stale_open_plan(&mut s);
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::TurnIncomplete {
            reason: "预算用尽".into(),
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AgentActivity {
            label: "continue".into(),
        }),
    );
    assert!(s.plan.is_none());
    assert_eq!(historical_plans(&s).len(), 1);
}

/// D4 / D5: every turn terminal archives its exact plan. Whether work may be
/// resumed later does not make the old turn active now.
#[test]
fn incomplete_and_cancelled_turns_archive_their_plan() {
    for event in [
        RuntimeEvent::TurnIncomplete {
            reason: "预算用尽".into(),
        },
        RuntimeEvent::TurnCancelled,
    ] {
        let mut s = busy_state();
        stale_open_plan(&mut s);
        reduce(&mut s, Action::Runtime(event.clone()));
        assert!(s.plan.is_none(), "{event:?} is terminal for this turn");
        assert_eq!(historical_plans(&s).len(), 1);
    }
}

/// D6: the terminal marker of a finished turn must not carry a stale plan
/// fraction. "任务已完成 · 计划 6/9" uses an old plan to argue against the
/// outcome the runtime just reported — two authorities, one line.
#[test]
fn a_completed_turn_marker_does_not_deny_itself_with_a_stale_plan() {
    let mut s = busy_state();
    stale_open_plan(&mut s);
    answer(&mut s, "m-final", "做完了。");
    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCompleted));
    let text = format!("{:?}", s.transcript.items());
    let zh = leveler_tui::Locale::Zh.text();
    assert!(
        !text.contains(
            &zh.summary_plan
                .replacen("{}", "5", 1)
                .replacen("{}", "9", 1)
        ),
        "{text}"
    );
}

/// D7: an incomplete turn keeps the plan fraction — there it is true progress
/// information, not a contradiction.
#[test]
fn an_incomplete_turn_marker_keeps_real_plan_progress() {
    let mut s = busy_state();
    stale_open_plan(&mut s);
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::TurnIncomplete {
            reason: "预算用尽".into(),
        }),
    );
    let text = format!("{:?}", s.transcript.items());
    assert!(text.contains("计划 5/9"), "{text}");
}

/// D8: retiring the presentation must never fabricate plan state. Nothing
/// here may mark the four open steps Done — the runtime cannot prove they ran.
#[test]
fn retiring_the_plan_never_fabricates_completed_steps() {
    let mut s = busy_state();
    stale_open_plan(&mut s);
    answer(&mut s, "m-final", "做完了。");
    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCompleted));
    let text = format!("{:?}", s.transcript.items());
    assert!(!text.contains("9/9"), "no invented completion: {text}");
    assert!(!text.contains("计划"), "no plan claim at all: {text}");
}

/// R004 F1 adjacent: a KNOWN slash command with a small multiline paste (below
/// the chip threshold) must still dispatch as that command with the full
/// multiline argument — not silently degrade to a chat message.
#[test]
fn slash_goal_with_small_multiline_paste_still_runs_a_goal() {
    let mut s = opened();
    typed(&mut s, "/goal ");
    reduce(
        &mut s,
        Action::Paste("修 README\n第二行要求\n第三行要求".into()),
    );
    assert!(
        !s.composer.text().contains("[Pasted:"),
        "precondition: below chip threshold: {}",
        s.composer.text()
    );
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Submit { command: ClientCommand::RunGoal { content, .. }, .. }]
                if content == "修 README\n第二行要求\n第三行要求"
        ),
        "multiline /goal must stay a goal: {effects:?}"
    );
}

/// The unknown-command guard keeps its old scope: multiline text starting with
/// an unknown /token is an ordinary message, never swallowed.
#[test]
fn multiline_unknown_slash_prefix_stays_a_message() {
    let mut s = opened();
    typed(&mut s, "/usr/local/bin notes");
    reduce(&mut s, Action::Paste("\nline2\nline3".into()));
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Submit { command: ClientCommand::SubmitMessage { content, .. }, .. }] if content.contains("line3")
        ),
        "{effects:?}"
    );
}

// ── Long-goal P3: durable goal recaps ────────────────────────────────────

fn goal_recap(id: &str, ordinal: Option<u64>) -> leveler_client_protocol::UiGoalRecap {
    leveler_client_protocol::UiGoalRecap {
        checkpoint_id: id.to_string(),
        goal_id: "g1".to_string(),
        reason: "manual".to_string(),
        created_at: "2026-08-27T00:00:00Z".to_string(),
        transcript_ordinal: ordinal,
        display_summary: "架构审查已完成".to_string(),
        phase: Some("Beta Capability Closure".to_string()),
        next_action: Some("检查 API 边界".to_string()),
        plan_completed: Some(3),
        plan_total: Some(5),
        completed_milestones: vec!["ownership 清理".to_string()],
        findings_total: None,
        known_limitations: Vec::new(),
        unresolved_work: Vec::new(),
    }
}

#[test]
fn recap_slash_sends_the_recap_command() {
    let mut s = opened();
    s.composer.replace("/recap");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(
        matches!(
            &effects[..],
            [Effect::Send(ClientCommand::Recap { session_id })]
                if session_id.as_str() == "s1"
        ),
        "effects: {effects:?}"
    );
}

/// A GoalRecapCreated event lands in HISTORY as a GoalRecap item, and
/// at-least-once delivery of the same checkpoint never duplicates it.
#[test]
fn goal_recap_event_is_history_and_idempotent() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::GoalRecapCreated {
            recap: goal_recap("cp-1", Some(2)),
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::GoalRecapCreated {
            recap: goal_recap("cp-1", Some(2)),
        }),
    );
    let recaps: Vec<_> = s
        .transcript
        .items()
        .iter()
        .filter(|i| matches!(i, TranscriptItem::GoalRecap(_)))
        .collect();
    assert_eq!(recaps.len(), 1, "one checkpoint = one history item");
    assert!(matches!(
        s.transcript.items().last(),
        Some(TranscriptItem::GoalRecap(block))
            if block.recap.checkpoint_id == "cp-1" && !block.expanded
    ));
}

/// A reopened session interleaves persisted recaps at the transcript
/// position their checkpoint represents (messages [0..ordinal) precede it).
#[test]
fn session_snapshot_interleaves_goal_recaps_by_ordinal() {
    let mut s = opened();
    let mut snap = snapshot();
    snap.messages = vec![
        UiMessage {
            id: MessageId::new("m0"),
            role: UiRole::User,
            text: "第一问".to_string(),
            ordinal: Some(0),
            kind: None,
            images: 0,
        },
        UiMessage {
            id: MessageId::new("m1"),
            role: UiRole::Assistant,
            text: "第一答".to_string(),
            ordinal: Some(1),
            kind: None,
            images: 0,
        },
        UiMessage {
            id: MessageId::new("m2"),
            role: UiRole::User,
            text: "第二问".to_string(),
            ordinal: Some(2),
            kind: None,
            images: 0,
        },
    ];
    snap.recaps = vec![goal_recap("cp-mid", Some(2))];
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened { session: snap }),
    );

    let kinds: Vec<&'static str> = s
        .transcript
        .items()
        .iter()
        .map(|i| match i {
            TranscriptItem::User(_) => "user",
            TranscriptItem::Assistant(_) => "assistant",
            TranscriptItem::GoalRecap(_) => "recap",
            _ => "other",
        })
        .collect();
    assert_eq!(
        kinds,
        vec!["user", "assistant", "recap", "user"],
        "the recap sits after the messages it represents, before the delta"
    );
}

// --- Beta Product Closure, Phase B: the turn-end summary and the command
// --- heartbeat are user-facing prose and must come from the locale table.

fn opened_in(locale: leveler_tui::Locale) -> AppState {
    let mut s = AppState::new(
        Theme::no_color(),
        Boot {
            session_id: SessionId::new("s1"),
            user: "麻凡".to_string(),
            version: "0.1.0".to_string(),
            show_welcome: false,
            draft_path: None,
            history_path: None,
            context_window: 0,
            locale,
            untrusted_config: Vec::new(),
            reasoning_effort: None,
        },
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot(),
        }),
    );
    s
}

fn finished_turn_summary(locale: leveler_tui::Locale) -> String {
    let mut s = opened_in(locale);
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::DiffUpdated {
            diff: leveler_client_protocol::UiDiff {
                files: vec![leveler_client_protocol::UiDiffFile {
                    path: "src/lib.rs".into(),
                    added: 4,
                    removed: 1,
                    patch: None,
                }],
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
    end.summary.clone().unwrap_or_default()
}

fn has_han(s: &str) -> bool {
    s.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
}

#[test]
fn the_turn_end_summary_speaks_one_language_in_each_locale() {
    let zh = finished_turn_summary(leveler_tui::Locale::Zh);
    let en = finished_turn_summary(leveler_tui::Locale::En);
    assert!(has_han(&zh), "the zh summary must be Chinese: {zh}");
    assert!(
        !zh.contains("files") && !zh.contains("verify"),
        "the zh summary must not carry English words: {zh}"
    );
    assert!(!has_han(&en), "the en summary must not carry Chinese: {en}");
}

#[test]
fn one_changed_file_is_not_reported_in_the_plural() {
    let en = finished_turn_summary(leveler_tui::Locale::En);
    assert!(!en.contains("1 files"), "one file is not \"1 files\": {en}");
}

#[test]
fn the_running_command_heartbeat_follows_the_locale() {
    let mut s = opened_in(leveler_tui::Locale::En);
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::CommandProgress {
            label: "cargo test".into(),
            elapsed_ms: 137_000,
        }),
    );
    let activity = s.activity.clone().unwrap_or_default();
    assert!(
        !has_han(&activity),
        "an English session must not be told 运行: {activity}"
    );
    assert!(activity.contains("cargo test"), "{activity}");
}

// ---- Assistant prose presentation, driven by real event order ----

/// Every visual row the assistant block at `index` currently occupies, as
/// plain text.
fn assistant_rows(s: &AppState, index: usize, width: usize) -> Vec<String> {
    let block = match &s.transcript.items()[index] {
        TranscriptItem::Assistant(b) => b,
        other => panic!("item {index} is not an assistant block: {other:?}"),
    };
    leveler_tui::render::assistant_render(block, &s.theme, width)
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect()
}

fn assistant_indexes(s: &AppState) -> Vec<usize> {
    s.transcript
        .items()
        .iter()
        .enumerate()
        .filter_map(|(i, item)| matches!(item, TranscriptItem::Assistant(_)).then_some(i))
        .collect()
}

fn stream(s: &mut AppState, id: &str, chunks: &[&str], complete: bool) {
    let message_id = MessageId::new(id);
    reduce(
        s,
        Action::Runtime(RuntimeEvent::AssistantMessageStarted {
            message_id: message_id.clone(),
        }),
    );
    for chunk in chunks {
        reduce(
            s,
            Action::Runtime(RuntimeEvent::AssistantTextDelta {
                message_id: message_id.clone(),
                delta: (*chunk).to_string(),
            }),
        );
    }
    if complete {
        reduce(
            s,
            Action::Runtime(RuntimeEvent::AssistantMessageCompleted { message_id }),
        );
    }
}

fn tool(s: &mut AppState, id: &str) {
    reduce(
        s,
        Action::Runtime(RuntimeEvent::ToolCallStarted {
            id: ToolCallId::new(id),
            name: "read_file".into(),
            arguments: r#"{"path":"a.rs"}"#.into(),
            parallel: false,
        }),
    );
    reduce(
        s,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            exit_code: None,
            stop: None,
            id: ToolCallId::new(id),
            ok: true,
            preview: "ok".into(),
            duration_ms: 900,
            applied_diff: None,
        }),
    );
}

const LONG_PROSE: &[&str] = &[
    "关键判断：用户说的优化是视觉层面的\n\n",
    "方案：把状态色收敛到现有 semantic token\n\n",
    "其实更稳妥：先查同域页面有没有现成实现\n\n",
    "先查一下 PaymentRecords 的当前写法\n\n",
    "再决定 scope",
];

fn shows_all_of_long_prose(rows: &[String]) -> bool {
    rows.iter().any(|row| row.contains("关键判断"))
        && rows.iter().any(|row| row.contains("再决定 scope"))
        && !rows
            .iter()
            .any(|row| row.contains('▸') || row.contains('▾'))
}

/// Streaming prose grows in place, and the tool call that classifies it as
/// progress does not hide any of it.
#[test]
fn streaming_prose_stays_whole_and_the_tool_call_does_not_fold_it() {
    let mut s = state();
    stream(&mut s, "m1", LONG_PROSE, false);
    let at = assistant_indexes(&s)[0];
    let live = assistant_rows(&s, at, 60);
    assert!(shows_all_of_long_prose(&live), "{live:?}");

    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AssistantMessageCompleted {
            message_id: MessageId::new("m1"),
        }),
    );
    tool(&mut s, "t1");
    let progress = assistant_rows(&s, at, 60);
    assert!(shows_all_of_long_prose(&progress), "{progress:?}");
}

/// The whole-turn shape: prose / tools / prose / tools / answer. Every message
/// the agent wrote to the user is on screen in full — the dogfood regression
/// was interim explanations hidden behind `▸ 展开过程说明 · N 行`.
#[test]
fn a_long_turn_shows_every_message_in_full() {
    let mut s = state();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::UserMessageAdded {
            message: UiMessage {
                id: MessageId::new("u1"),
                role: UiRole::User,
                text: "优化一下 PaymentRecords".into(),
                ordinal: None,
                kind: None,
                images: 0,
            },
        }),
    );
    stream(&mut s, "m1", LONG_PROSE, true);
    tool(&mut s, "t1");
    stream(&mut s, "m2", LONG_PROSE, true);
    tool(&mut s, "t2");
    stream(
        &mut s,
        "m3",
        &[
            "PaymentRecords 已统一到现有 semantic token。\n\n",
            "- 状态色跟随明暗主题\n",
            "- 搜索支持当前字段过滤\n\n",
            "pnpm lint / pnpm build 通过。",
        ],
        true,
    );
    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCompleted));

    let blocks = assistant_indexes(&s);
    assert_eq!(blocks.len(), 3);
    for at in &blocks[..2] {
        let rows = assistant_rows(&s, *at, 80);
        assert!(shows_all_of_long_prose(&rows), "{rows:?}");
    }
    let answer = assistant_rows(&s, blocks[2], 80);
    assert!(
        answer.iter().any(|row| row.contains("pnpm lint")),
        "{answer:?}"
    );
}

/// A reconnect replays messages with no tool ordering at all; restored
/// interim prose and answers both render whole.
#[test]
fn a_replayed_session_shows_every_message_in_full() {
    let mut s = state();
    let mut snap = snapshot();
    let answer = "改完了。\n\n状态色统一。\n\n搜索已修复。\n\npnpm lint 通过。";
    snap.messages = vec![
        UiMessage {
            id: MessageId::new("u1"),
            role: UiRole::User,
            text: "优化一下".into(),
            ordinal: Some(1),
            kind: None,
            images: 0,
        },
        UiMessage {
            id: MessageId::new("m1"),
            role: UiRole::Assistant,
            text: LONG_PROSE.concat(),
            ordinal: Some(2),
            kind: None,
            images: 0,
        },
        UiMessage {
            id: MessageId::new("m2"),
            role: UiRole::Assistant,
            text: answer.into(),
            ordinal: Some(3),
            kind: None,
            images: 0,
        },
    ];
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened { session: snap }),
    );
    let blocks = assistant_indexes(&s);
    assert_eq!(blocks.len(), 2);
    let interim = assistant_rows(&s, blocks[0], 80);
    assert!(shows_all_of_long_prose(&interim), "{interim:?}");
    let restored = assistant_rows(&s, blocks[1], 80);
    assert!(
        restored.iter().any(|row| row.contains("pnpm lint")),
        "{restored:?}"
    );
}

/// Reasoning is status scratch, not conversation content — the bound must not
/// have turned it into a foldable transcript block.
#[test]
fn raw_reasoning_still_never_reaches_the_conversation() {
    let mut s = state();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ReasoningDelta {
            delta: "The user wants me to look at PaymentRecords first".into(),
        }),
    );
    stream(&mut s, "m1", LONG_PROSE, true);
    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCompleted));
    let text = rendered(&mut s, 100, 30);
    assert!(
        !text.contains("wants me"),
        "raw reasoning leaked into the conversation: {text}"
    );
    assert_eq!(assistant_indexes(&s).len(), 1, "one assistant block only");
}

fn snapshot_child(
    id: &str,
    state: leveler_client_protocol::UiChildState,
    stop: Option<leveler_client_protocol::ChildStop>,
) -> leveler_client_protocol::UiChildAgent {
    leveler_client_protocol::UiChildAgent {
        id: id.into(),
        nickname: format!("nick-{id}"),
        role: "explorer".into(),
        profile_id: None,
        read_only: true,
        title: None,
        purpose: format!("purpose of {id}"),
        agent: None,
        state,
        ok: false,
        background: true,
        scope: Vec::new(),
        resumes: 0,
        outcome: None,
        stop,
        limit: None,
        summary: None,
        input_tokens: 0,
        output_tokens: 0,
        cost_usd_micros: None,
    }
}

/// U5: a reopened or reconnected session shows its children from the
/// snapshot — the runtime's record — not from live events it never saw.
#[test]
fn a_session_snapshot_restores_its_children_with_their_recorded_state() {
    use leveler_client_protocol::{ChildStop, UiChildState};
    use leveler_tui::multi_agent::ChildStatus;
    let mut s = state();
    let mut snap = snapshot();
    snap.children = vec![
        snapshot_child("c1", UiChildState::Settled, Some(ChildStop::Budget)),
        snapshot_child("c2", UiChildState::Interrupted, None),
        snapshot_child("c3", UiChildState::Running, None),
    ];
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened { session: snap }),
    );
    let statuses: Vec<_> = s
        .team
        .children
        .iter()
        .map(|c| (c.id.as_str(), c.status, c.stop))
        .collect();
    // Settled history (c1) is the transcript's, not the live team's.
    assert_eq!(
        statuses,
        vec![
            ("c2", ChildStatus::Interrupted, None),
            ("c3", ChildStatus::Waiting, None),
        ]
    );
    assert_eq!(s.team.children[0].purpose, "purpose of c2");
}

/// U3: a runtime notice in history is not a user turn.
#[test]
fn a_runtime_notice_in_history_is_not_rendered_as_user_input() {
    let mut s = state();
    let mut snap = snapshot();
    snap.messages = vec![leveler_client_protocol::UiMessage {
        id: leveler_client_protocol::MessageId::new("m1"),
        role: leveler_client_protocol::UiRole::User,
        text: "## Background sub-agent settled\nEuclid finished.".into(),
        ordinal: Some(0),
        kind: Some(leveler_client_protocol::UiMessageKind::RuntimeNotice),
        images: 0,
    }];
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened { session: snap }),
    );
    let items = s.transcript.items();
    assert!(
        !items
            .iter()
            .any(|item| matches!(item, leveler_tui::transcript::TranscriptItem::User(_))),
        "{items:?}"
    );
    assert!(items.iter().any(|item| matches!(
        item,
        leveler_tui::transcript::TranscriptItem::Note(text) if text.contains("Euclid finished")
    )));
}

fn child_detail_state(status: leveler_tui::multi_agent::ChildStatus) -> AppState {
    let mut s = state();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot(),
        }),
    );
    s.team
        .children
        .push(leveler_tui::multi_agent::ChildAgentView {
            id: "c1".into(),
            nickname: "Euclid".into(),
            role: "explorer".into(),
            profile_id: None,
            agent_name: None,
            read_only: true,
            title: None,
            purpose: "look".into(),
            status,
            contribution: leveler_tui::multi_agent::Contribution::Pending,
            recent_step: None,
            input_tokens: 0,
            output_tokens: 0,
            stop: None,
            limit: None,
            started_elapsed_secs: 0,
            settled_elapsed_secs: None,
            detail: None,
            activity: Vec::new(),
        });
    s.activity_open = Some(leveler_tui::activity::ActivityId::Child("c1".into()));
    s.active_screen = Screen::Activity;
    s
}

/// In a running child's detail, `x` stops exactly that child; Esc only
/// leaves. Back and stop are different actions, as in Shell Details.
#[test]
fn x_in_a_running_child_detail_cancels_that_child_only() {
    let mut s = child_detail_state(leveler_tui::multi_agent::ChildStatus::Running);
    let effects = reduce(&mut s, raw_char('x'));
    assert!(
        effects.iter().any(|e| matches!(
            e,
            Effect::Send(leveler_client_protocol::ClientCommand::CancelChild { child_id, .. })
                if child_id == "c1"
        )),
        "{effects:?}"
    );
    let mut settled = child_detail_state(leveler_tui::multi_agent::ChildStatus::Completed);
    // Wide glyphs occupy two cells; compare without the padding spaces.
    let running_frame = render_screen_text(&mut s).replace(' ', "");
    assert!(running_frame.contains("x停止"), "{running_frame}");
    assert!(
        !render_screen_text(&mut settled)
            .replace(' ', "")
            .contains("x停止")
    );
    assert!(
        reduce(&mut settled, raw_char('x')).is_empty(),
        "a settled child has nothing to stop"
    );
}

fn render_screen_text(s: &mut AppState) -> String {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal
        .draw(|f| leveler_tui::render::render(f, s))
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ── Agent Extensibility: /agents and a child's agent identity ─────────────

fn last_note(s: &AppState) -> String {
    s.transcript
        .items()
        .iter()
        .rev()
        .find_map(|i| match i {
            TranscriptItem::Note(t) => Some(t.clone()),
            _ => None,
        })
        .expect("a transcript note")
}

fn agent_entry(
    name: &str,
    source: leveler_client_protocol::UiAgentSource,
) -> leveler_client_protocol::UiAgentEntry {
    leveler_client_protocol::UiAgentEntry {
        name: name.into(),
        source,
        location: Some(format!("/repo/.leveler/agents/{name}")),
        status: leveler_client_protocol::UiAgentStatus::Available,
        reason: None,
        description: Some(format!("The {name} agent.")),
        capability: Some(leveler_client_protocol::UiAgentCapability::ReadOnly),
        structural: false,
        harness_only: false,
        model: None,
        reasoning_effort: None,
        skills: Vec::new(),
        tools: None,
        write_roots: Vec::new(),
        max_rounds: None,
        max_duration_secs: None,
        fingerprint: Some("sha256:0123456789abcdef".into()),
        shadowed: Vec::new(),
    }
}

#[test]
fn slash_agents_lists_and_slash_agents_name_inspects() {
    let mut s = opened();
    typed(&mut s, "/agents");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::Send(ClientCommand::ListAgents { .. }))),
        "effects={effects:?}"
    );
    let mut s = opened();
    typed(&mut s, "/agents security-reviewer");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(
        effects.iter().any(|e| matches!(
            e,
            Effect::Send(ClientCommand::GetAgent { name, .. }) if name == "security-reviewer"
        )),
        "effects={effects:?}"
    );
}

#[test]
fn slash_skills_reads_the_local_registry_without_a_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join(".leveler/skills/deploy");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        "---\nname: deploy\ndescription: Ship safely.\n---\n\nBODY_MARKER\n",
    )
    .unwrap();
    let mut s = opened();
    s.repository = tmp.path().display().to_string();

    typed(&mut s, "/skills");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(effects.is_empty(), "the registry is local: {effects:?}");
    let note = last_note(&s);
    assert!(note.contains("$deploy"), "{note}");
    assert!(
        !note.contains("BODY_MARKER"),
        "the listing must stay metadata-only: {note}"
    );

    typed(&mut s, "/skills deploy");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(effects.is_empty(), "{effects:?}");
    let detail = last_note(&s);
    assert!(detail.contains("Skill $deploy"), "{detail}");
    assert!(!detail.contains("BODY_MARKER"), "{detail}");
}

#[test]
fn the_agents_listing_names_source_class_and_what_is_wrong() {
    use leveler_client_protocol::{UiAgentSource, UiAgentStatus};
    let mut s = opened();
    let mut invalid = agent_entry("broken", UiAgentSource::Project);
    invalid.status = UiAgentStatus::Invalid;
    invalid.reason = Some("agent.yaml: unknown field `wirte`".into());
    invalid.description = None;
    invalid.capability = None;
    let mut unavailable = agent_entry("far-model", UiAgentSource::User);
    unavailable.status = UiAgentStatus::Unavailable;
    unavailable.reason = Some("model nowhere/big: not configured".into());
    let mut shadowing = agent_entry("code-reviewer", UiAgentSource::Project);
    shadowing.shadowed = vec![leveler_client_protocol::UiShadowedAgent {
        source: UiAgentSource::Builtin,
        location: None,
    }];
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AgentsLoaded {
            query_id: None,
            agents: vec![
                invalid,
                shadowing,
                unavailable,
                agent_entry("security-reviewer", UiAgentSource::Project),
            ],
            problems: vec![leveler_client_protocol::UiAgentProblem {
                source: UiAgentSource::Project,
                location: "/repo/.leveler/agents/worker".into(),
                error: "`worker` is a built-in agent".into(),
            }],
        }),
    );
    let note = last_note(&s);
    assert!(note.contains("security-reviewer"), "{note}");
    assert!(
        note.contains("read_only") && note.contains("project"),
        "{note}"
    );
    assert!(note.contains("unknown field `wirte`"), "{note}");
    assert!(note.contains("nowhere/big"), "{note}");
    assert!(note.contains("shadows builtin"), "{note}");
    assert!(note.contains("`worker` is a built-in agent"), "{note}");
}

#[test]
fn an_agent_detail_shows_its_bounds_and_instructions() {
    use leveler_client_protocol::UiAgentSource;
    let mut s = opened();
    let mut entry = agent_entry("frontend-worker", UiAgentSource::Project);
    entry.capability = Some(leveler_client_protocol::UiAgentCapability::ScopedWriter);
    entry.write_roots = vec!["web".into()];
    entry.tools = Some(vec!["read_file".into(), "apply_patch".into()]);
    entry.model = Some("deepseek/deepseek-v4-pro".into());
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AgentLoaded {
            query_id: None,
            name: "frontend-worker".into(),
            agent: Some(leveler_client_protocol::UiAgentDetail {
                entry,
                instructions: Some("Frontend only.\nKeep changes small.".into()),
            }),
            error: None,
        }),
    );
    let note = last_note(&s);
    for needle in [
        "frontend-worker",
        "scoped_writer",
        "web",
        "read_file, apply_patch",
        "deepseek/deepseek-v4-pro",
        "/repo/.leveler/agents/frontend-worker",
        "Frontend only.",
    ] {
        assert!(note.contains(needle), "missing {needle}: {note}");
    }
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AgentLoaded {
            query_id: None,
            name: "nope".into(),
            agent: None,
            error: Some("Agent \"nope\" not found.".into()),
        }),
    );
    assert!(last_note(&s).contains("Agent \"nope\" not found."));
}

#[test]
fn a_child_spawned_from_an_agent_is_shown_under_its_agent_name() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SubAgentUpdated {
            id: "a1".into(),
            nickname: "Curie".into(),
            role: "explorer".into(),
            title: None,
            done: false,
            ok: false,
            detail: "review the auth change".into(),
            profile_id: Some("security-reviewer".into()),
            profile_role: Some("explorer".into()),
            read_only: true,
            agent: Some(leveler_client_protocol::UiChildAgentIdentity {
                name: "security-reviewer".into(),
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
    let t = s.t();
    let rows = leveler_tui::multi_agent::roster_rows(&s.team, 0, t);
    assert!(
        rows.iter().any(|r| r.label.contains("security-reviewer")),
        "{rows:?}"
    );
    assert_eq!(
        s.team.children[0].agent_name.as_deref(),
        Some("security-reviewer")
    );
}

/// The footer advertises Ctrl+? on every idle screen. A terminal without the
/// Kitty keyboard protocol sends 0x1F for both Ctrl+? and Ctrl+/, and
/// crossterm decodes that byte as Ctrl+7 — so the advertised shortcut reached
/// nothing at all.
#[test]
fn ctrl_question_opens_help_from_a_plain_terminal() {
    for ch in ['?', '/', '7'] {
        let mut s = opened();
        reduce(
            &mut s,
            Action::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL)),
        );
        assert_eq!(
            s.active_screen,
            leveler_tui::screen::Screen::Help,
            "Ctrl+{ch} must open help"
        );
    }
}

/// Compaction changes what the model knows, so it belongs in the conversation
/// and not only in a notification that fades: scrolling back through detail
/// the model has replaced with a summary must show where that happened.
#[test]
fn compaction_leaves_a_line_in_the_conversation() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ContextCompacted { from: 42, to: 6 }),
    );
    let notes: Vec<String> = s
        .transcript
        .items()
        .iter()
        .filter_map(|i| match i {
            leveler_tui::transcript::TranscriptItem::Note(text) => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(
        notes.iter().any(|n| n.contains("42") && n.contains('6')),
        "{notes:?}"
    );
}

// --- Plan resume authority -------------------------------------------------
//
// The snapshot is the active plan's reconnect authority only while its session
// is running. Durable history rebuilds terminal plan blocks through the same
// reducer, in scratch state, and only that transcript is adopted.

use leveler_client_protocol::{PlanStepStatus, UiHistoryEntry};

fn plan_of(steps: &[(&str, PlanStepStatus)]) -> UiPlan {
    UiPlan {
        steps: steps
            .iter()
            .enumerate()
            .map(|(index, (description, status))| UiPlanStep {
                index,
                description: (*description).to_string(),
                status: *status,
            })
            .collect(),
    }
}

#[test]
fn a_completed_plan_is_not_restored_from_a_snapshot() {
    let mut s = state();
    let mut snap = snapshot();
    snap.plan = Some(plan_of(&[
        ("write the test", PlanStepStatus::Done),
        ("make it pass", PlanStepStatus::Done),
    ]));

    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened { session: snap }),
    );

    assert!(
        s.plan.is_none(),
        "a plan whose every step is done is finished work, not live state: {:?}",
        s.plan
    );
}

#[test]
fn an_idle_snapshot_never_restores_a_failed_plan_as_active() {
    let mut s = state();
    let mut snap = snapshot();
    snap.plan = Some(plan_of(&[
        ("write the test", PlanStepStatus::Done),
        ("make it pass", PlanStepStatus::Failed),
    ]));

    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened { session: snap }),
    );

    assert!(
        s.plan.is_none(),
        "an idle snapshot's failed plan is historical, not active"
    );
}

#[test]
fn history_replay_does_not_overwrite_the_plan_the_snapshot_restored() {
    let mut s = state();
    let mut snap = snapshot();
    snap.status = "running".into();
    snap.plan = Some(plan_of(&[(
        "the plan the user is resuming",
        PlanStepStatus::Running,
    )]));
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened { session: snap }),
    );
    assert_eq!(
        s.plan.as_ref().unwrap().steps.len(),
        1,
        "snapshot restored it"
    );

    // The durable history carries plan events too. Replaying them rebuilds the
    // transcript in a scratch state; the live plan must not follow.
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionHistoryLoaded {
            query_id: None,
            session_id: SessionId::new("s1"),
            omitted_turns: 0,
            entries: vec![UiHistoryEntry {
                turn_elapsed_ms: 0,
                turn_start: true,
                event: RuntimeEvent::PlanUpdated {
                    plan: plan_of(&[
                        ("a stale step from an older turn", PlanStepStatus::Pending),
                        ("another stale step", PlanStepStatus::Pending),
                    ]),
                },
            }],
        }),
    );

    let plan = s.plan.as_ref().expect("replay must not clear the plan");
    assert_eq!(
        plan.steps.len(),
        1,
        "replay is not the plan's authority; the snapshot is: {plan:?}"
    );
    assert_eq!(plan.steps[0].description, "the plan the user is resuming");
}

/// A slash command writes its result at the end of the conversation. Running
/// one while reading history left the reader where they were, so /compact
/// looked like it had only produced a notification that faded.
#[test]
fn a_slash_command_returns_the_view_to_the_live_edge() {
    let mut s = opened();
    for i in 0..40 {
        s.transcript.push_user(format!("line {i}"));
    }
    s.conv.auto_scroll = false;
    s.conv.scroll = 3;
    s.composer.replace("/compact".to_string());
    reduce(&mut s, key(KeyCode::Enter));
    assert!(s.conv.auto_scroll, "the view follows the result again");
}

/// Shift+↑ walks back through user turns one at a time. It is the only way to
/// find what was asked in a turn whose diffs fill twenty screens.
#[test]
fn shift_up_walks_back_through_every_user_turn() {
    let mut s = opened();
    for text in ["第一个问题", "第二个问题", "第三个问题"] {
        s.transcript.push_user(text.to_string());
        for i in 0..30 {
            s.transcript.push_note(format!("filler {i}"));
        }
    }
    let up = || KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT);
    reduce(&mut s, Action::Key(up()));
    assert_eq!(s.turn_nav, Some(2), "first press lands on the newest turn");
    reduce(&mut s, Action::Key(up()));
    assert_eq!(s.turn_nav, Some(1), "the second press keeps walking back");
    reduce(&mut s, Action::Key(up()));
    assert_eq!(s.turn_nav, Some(0), "and the third reaches the oldest");
}

/// The memory listing is the UI that gates what the model is allowed to
/// remember. Its chrome was English in a Chinese session ("pending (0)",
/// "hint:"), beside a Chinese sensitivity note — IA §12 Class A.
#[test]
fn the_memory_listing_speaks_the_sessions_language() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::MemoryList {
            memory_dir: "/repo/.leveler/memory".into(),
            active: Vec::new(),
            archived: Vec::new(),
            pending: Vec::new(),
        }),
    );
    let text = s
        .transcript
        .items()
        .iter()
        .filter_map(|i| match i {
            leveler_tui::transcript::TranscriptItem::Note(t) => Some(t.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!text.contains("hint:"), "zh kept English chrome:\n{text}");
    assert!(!text.contains("(none)"), "zh kept English chrome:\n{text}");
    assert!(
        text.contains("memory_dir=/repo/.leveler/memory"),
        "the path and its key are data:\n{text}"
    );
}

// ---- Pasting an image: the terminal hands us a path, the user means a picture

/// Write a real 1×1 PNG so the paste path can be judged by what is on disk,
/// not by how the name is spelled.
fn write_png(dir: &std::path::Path, name: &str) -> String {
    const PNG_1X1: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];
    let path = dir.join(name);
    std::fs::write(&path, PNG_1X1).unwrap();
    path.to_string_lossy().into_owned()
}

/// Ghostty (and other terminals) answer Cmd+V on an image by writing a temp
/// PNG and bracketed-pasting its PATH. That path is an implementation detail
/// of the terminal — the user pasted a picture, so it must become an
/// attachment, never composer text.
#[test]
fn pasting_an_image_file_path_attaches_it_instead_of_typing_the_path() {
    // The terminal writes its scratch file in the OS temp directory itself,
    // which is what earns the generated name being dropped.
    let path = write_png(
        &std::env::temp_dir(),
        "clipboard-2026-09-16-215212-CE7C9522.png",
    );
    let mut s = opened();
    let effects = reduce(&mut s, Action::Paste(path.clone()));
    assert_eq!(
        effects,
        vec![Effect::Send(ClientCommand::AddAttachment {
            session_id: SessionId::new("s1"),
            path: path.clone(),
            name: Some("clipboard.png".to_string()),
        })],
        "an image path paste must attach, not type"
    );
    assert!(
        s.composer.is_empty(),
        "the composer must not hold the path: {:?}",
        s.composer.text()
    );
    let _ = std::fs::remove_file(&path);
}

/// A pasted path keeps its own name when it is a file the user chose, not a
/// terminal's clipboard scratch file.
#[test]
fn pasting_a_real_image_keeps_its_own_name() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_png(dir.path(), "login-error.png");
    let mut s = opened();
    let effects = reduce(&mut s, Action::Paste(path.clone()));
    assert_eq!(
        effects,
        vec![Effect::Send(ClientCommand::AddAttachment {
            session_id: SessionId::new("s1"),
            path,
            name: None,
        })]
    );
}

/// Surrounding whitespace and shell quoting are the terminal's punctuation,
/// not part of the path.
#[test]
fn a_quoted_image_path_paste_still_attaches() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_png(dir.path(), "shot.png");
    let mut s = opened();
    let effects = reduce(&mut s, Action::Paste(format!("'{path}'\n")));
    assert_eq!(
        effects,
        vec![Effect::Send(ClientCommand::AddAttachment {
            session_id: SessionId::new("s1"),
            path,
            name: None,
        })]
    );
    assert!(s.composer.is_empty());
}

/// Only a path that IS an image on disk attaches. Text that merely looks like
/// one — a path to a missing file, or to a file that is not an image — is
/// ordinary prose and must survive verbatim.
#[test]
fn pasting_text_that_only_looks_like_an_image_path_stays_text() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope.png").to_string_lossy().into_owned();
    let not_image = dir.path().join("notes.png");
    std::fs::write(&not_image, b"this is not a png").unwrap();
    let not_image = not_image.to_string_lossy().into_owned();
    let dir_path = dir.path().to_string_lossy().into_owned();

    for text in [
        missing,
        not_image,
        dir_path,
        "看看 /tmp/a.png 这张图".into(),
    ] {
        let mut s = opened();
        reduce(&mut s, Action::Paste(text.clone()));
        assert_eq!(s.composer.text(), text, "must stay text: {text}");
        assert!(s.pending_attachments.is_empty());
    }
}

#[test]
fn pasted_document_paths_render_as_file_references_but_submit_real_paths() {
    let dir = tempfile::tempdir().unwrap();
    let sheet = dir.path().join("价逻辑-必填字段.xlsx");
    let prototype = dir.path().join("接单方-交互原型.html");
    std::fs::write(&sheet, b"sheet").unwrap();
    std::fs::write(&prototype, b"<html></html>").unwrap();
    let pasted = format!(
        "{} {} 阅读这两个文件。",
        sheet.to_string_lossy(),
        prototype.to_string_lossy()
    );

    let mut s = opened();
    let effects = reduce(&mut s, Action::Paste(pasted.clone()));
    assert!(effects.is_empty(), "file references are local presentation");
    assert_eq!(s.composer.text(), "[文件 #1] [文件 #2] 阅读这两个文件。");
    assert_eq!(s.composer.canonical_text(), pasted);

    let frame = rendered(&mut s, 120, 40);
    assert!(frame.contains("价逻辑-必填字段.xlsx"), "{frame}");
    assert!(frame.contains("接单方-交互原型.html"), "{frame}");
    assert!(
        !frame.contains(dir.path().to_string_lossy().as_ref()),
        "the long directory must stay hidden:\n{frame}"
    );

    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert_eq!(
        submitted(&effects).0,
        ClientCommand::SubmitMessage {
            session_id: SessionId::new("s1"),
            content: pasted.clone(),
            attachments: Vec::new(),
        }
    );

    let mut burst = opened();
    reduce(&mut burst, Action::TextInput(pasted.clone()));
    assert_eq!(
        burst.composer.text(),
        "[文件 #1] [文件 #2] 阅读这两个文件。",
        "non-bracketed terminal paste should get the same compact display"
    );
    assert_eq!(burst.composer.canonical_text(), pasted);
}

/// Two pastes stage two attachments, in order, and the composer shows them as
/// `[图片 #1] [图片 #2]` — never a filesystem path.
#[test]
fn two_pasted_images_render_as_numbered_chips_and_no_path() {
    let dir = tempfile::tempdir().unwrap();
    let a = write_png(dir.path(), "clipboard-2026-09-16-215212-CE7C9522.png");
    let b = write_png(dir.path(), "diagram.png");
    let mut s = opened();
    reduce(&mut s, Action::Paste(a.clone()));
    reduce(&mut s, Action::Paste(b.clone()));
    // The runtime answers each with the staged attachment.
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AttachmentAdded {
            attachment: attachment("clipboard.png"),
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AttachmentAdded {
            attachment: attachment("diagram.png"),
        }),
    );
    typed(&mut s, "compare");
    let frame = rendered(&mut s, 120, 40);
    assert!(
        frame.contains("[图片 #1]") && frame.contains("[图片 #2]"),
        "chips missing:\n{frame}"
    );
    assert!(
        !frame.contains("CE7C9522") && !frame.contains(dir.path().to_string_lossy().as_ref()),
        "a filesystem path reached the screen:\n{frame}"
    );
}

/// A picture IS a message. Enter with an image staged and nothing typed used
/// to do nothing at all — the image sat in the composer with no way to send it
/// except by typing something first.
#[test]
fn enter_with_only_an_image_sends_it() {
    let mut s = opened();
    s.vision = true;
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AttachmentAdded {
            attachment: attachment("clipboard.png"),
        }),
    );
    let effects = reduce(&mut s, key(KeyCode::Enter));
    let sent = effects.iter().any(|e| {
        matches!(
            e,
            Effect::Submit {
                command: ClientCommand::SubmitMessage { attachments, .. },
                ..
            } if attachments.len() == 1
        )
    });
    assert!(sent, "an image alone must be sendable: {effects:?}");
    assert!(s.pending_attachments.is_empty(), "it was sent, not kept");
}

/// An empty composer with nothing staged is still not a message.
#[test]
fn enter_on_an_empty_composer_still_does_nothing() {
    let mut s = opened();
    assert!(reduce(&mut s, key(KeyCode::Enter)).is_empty());
}

/// What the conversation shows about a sent message must say it carried a
/// picture — both as it is sent and when the session is reopened. Otherwise an
/// image-only turn reads as an empty message, or vanishes.
#[test]
fn a_sent_image_is_visible_in_the_conversation() {
    let mut s = opened();
    s.vision = true;
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AttachmentAdded {
            attachment: attachment("clipboard.png"),
        }),
    );
    typed(&mut s, "这是什么");
    reduce(&mut s, key(KeyCode::Enter));
    let frame = rendered(&mut s, 120, 40);
    assert!(
        frame.contains("[图片 #1] 这是什么"),
        "the sent message must show its picture:\n{frame}"
    );
}

/// A replayed message already names its images — they are words in the text
/// the user wrote — so nothing is added to it.
#[test]
fn a_replayed_message_keeps_the_names_the_user_wrote() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::UserMessageAdded {
            message: UiMessage {
                id: MessageId::new("m1"),
                role: UiRole::User,
                text: "对比 [图片 #1] 和 [图片 #2] 的差异".to_string(),
                ordinal: None,
                kind: None,
                images: 2,
            },
        }),
    );
    let frame = rendered(&mut s, 120, 40);
    assert!(
        frame.contains("对比 [图片 #1] 和 [图片 #2] 的差异"),
        "replay must keep the sentence as written:\n{frame}"
    );
}

/// A message that was only pictures has no words to show. Another client can
/// send one, and it must not replay as a blank line.
#[test]
fn a_message_of_pictures_alone_still_shows_them() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::UserMessageAdded {
            message: UiMessage {
                id: MessageId::new("m1"),
                role: UiRole::User,
                text: String::new(),
                ordinal: None,
                kind: None,
                images: 2,
            },
        }),
    );
    let frame = rendered(&mut s, 120, 40);
    assert!(
        frame.contains("[图片 #1] [图片 #2]"),
        "an image-only message must not read as empty:\n{frame}"
    );
}

/// The attachments strip is one unwrapped row, so at a narrow width ratatui
/// cut the second entry mid-value — `[2] clipboard.png · PNG · 7` reads as a
/// whole line and is not one. When the entries do not fit, say how many there
/// are instead of showing half of one.
#[test]
fn a_narrow_terminal_counts_the_attachments_instead_of_clipping_one() {
    let mut s = opened();
    for _ in 0..2 {
        reduce(
            &mut s,
            Action::Runtime(RuntimeEvent::AttachmentAdded {
                attachment: attachment("clipboard.png"),
            }),
        );
    }
    let wide = rendered(&mut s, 160, 30);
    assert!(
        wide.contains("[1] clipboard.png") && wide.contains("[2] clipboard.png"),
        "a wide terminal names both:\n{wide}"
    );
    let narrow = rendered(&mut s, 70, 30);
    assert!(
        narrow.contains("2 个附件"),
        "a narrow terminal counts them:\n{narrow}"
    );
    assert!(
        !narrow.contains("[2] clipboard"),
        "half an entry is worse than none:\n{narrow}"
    );
}

/// CASE C/D/E: the image goes where the user is writing, and the sentence the
/// model reads is the sentence the user sees.
#[test]
fn images_land_where_the_user_is_writing() {
    let mut s = opened();
    s.vision = true;
    typed(&mut s, "对比 ");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AttachmentAdded {
            attachment: attachment("a.png"),
        }),
    );
    typed(&mut s, "和 ");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AttachmentAdded {
            attachment: attachment("b.png"),
        }),
    );
    typed(&mut s, "的差异");
    assert_eq!(s.composer.text(), "对比 [图片 #1] 和 [图片 #2] 的差异");

    let effects = reduce(&mut s, key(KeyCode::Enter));
    let (command, _) = submitted(&effects);
    match command {
        ClientCommand::SubmitMessage {
            content,
            attachments,
            ..
        } => {
            assert_eq!(content, "对比 [图片 #1] 和 [图片 #2] 的差异");
            assert_eq!(attachments.len(), 2);
            assert_eq!(attachments[0].name, "a.png");
            assert_eq!(attachments[1].name, "b.png");
        }
        other => panic!("expected SubmitMessage, got {other:?}"),
    }
}

/// An image pasted ahead of another one IS the first image: the numbers follow
/// the sentence, and the attachments follow the numbers.
#[test]
fn an_image_pasted_earlier_becomes_the_first_image() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AttachmentAdded {
            attachment: attachment("later.png"),
        }),
    );
    reduce(&mut s, key(KeyCode::Home));
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AttachmentAdded {
            attachment: attachment("earlier.png"),
        }),
    );
    assert_eq!(s.composer.text(), "[图片 #1] [图片 #2] ");
    assert_eq!(s.pending_attachments[0].name, "earlier.png");
    assert_eq!(s.pending_attachments[1].name, "later.png");
}

/// Deleting the middle image renumbers what is left AND drops the right one —
/// the screen and the request cannot disagree about which picture is gone.
#[test]
fn deleting_the_middle_image_drops_the_middle_attachment() {
    let mut s = opened();
    s.vision = true;
    for name in ["one.png", "two.png", "three.png"] {
        reduce(
            &mut s,
            Action::Runtime(RuntimeEvent::AttachmentAdded {
                attachment: attachment(name),
            }),
        );
    }
    assert_eq!(s.composer.text(), "[图片 #1] [图片 #2] [图片 #3] ");

    // Put the caret just after the second token and take it out.
    reduce(&mut s, key(KeyCode::Home));
    for _ in 0..3 {
        reduce(&mut s, key(KeyCode::Right));
    }
    reduce(&mut s, key(KeyCode::Backspace));

    assert_eq!(s.composer.text(), "[图片 #1] [图片 #2] ");
    assert_eq!(s.pending_attachments.len(), 2);
    assert_eq!(s.pending_attachments[0].name, "one.png");
    assert_eq!(
        s.pending_attachments[1].name, "three.png",
        "the one the user deleted is the one that went"
    );

    let effects = reduce(&mut s, key(KeyCode::Enter));
    match submitted(&effects).0 {
        ClientCommand::SubmitMessage { attachments, .. } => {
            assert_eq!(attachments.len(), 2);
            assert!(attachments.iter().all(|a| a.name != "two.png"));
        }
        other => panic!("expected SubmitMessage, got {other:?}"),
    }
}

// ---- /update ---------------------------------------------------------------

fn update_step(s: &mut AppState, step: leveler_update::UpdateStep) {
    reduce(s, Action::UpdateStep(step));
}

#[test]
fn update_is_refused_while_a_turn_is_running() {
    let mut s = opened();
    s.status = RuntimeStatus::Busy;
    s.composer.replace("/update");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(
        effects.is_empty(),
        "an update under an active task must not start: {effects:?}"
    );
    assert!(s.update.is_none());
    assert!(s.notification.is_some(), "the refusal must be visible");
}

#[test]
fn update_idle_starts_the_shared_service() {
    let mut s = opened();
    s.composer.replace("/update");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(
        matches!(effects.as_slice(), [Effect::StartUpdate]),
        "{effects:?}"
    );
    assert!(s.update.is_some());
    assert!(!s.restart_requested);
}

#[test]
fn update_progress_and_install_requests_a_restart() {
    let mut s = opened();
    s.composer.replace("/update");
    reduce(&mut s, key(KeyCode::Enter));

    update_step(
        &mut s,
        leveler_update::UpdateStep::Available {
            current: leveler_update::parse_version("1.0.0").unwrap(),
            latest: leveler_update::parse_version("1.0.1").unwrap(),
        },
    );
    update_step(
        &mut s,
        leveler_update::UpdateStep::Downloading {
            asset: "a.tar.gz".into(),
            received: 50,
            total: Some(100),
        },
    );
    let view = s.update.as_ref().unwrap();
    assert_eq!(view.received, 50);

    update_step(
        &mut s,
        leveler_update::UpdateStep::Installed {
            from: leveler_update::parse_version("1.0.0").unwrap(),
            to: leveler_update::parse_version("1.0.1").unwrap(),
        },
    );
    reduce(
        &mut s,
        Action::UpdateFinished(Ok(leveler_update::UpdateOutcome::Installed {
            from: leveler_update::parse_version("1.0.0").unwrap(),
            to: leveler_update::parse_version("1.0.1").unwrap(),
        })),
    );
    assert!(s.restart_requested, "an install must ask for a restart");
    assert!(
        !s.running,
        "the loop must end so the CLI can replace the process"
    );
}

#[test]
fn update_already_current_does_not_restart() {
    let mut s = opened();
    s.composer.replace("/update");
    reduce(&mut s, key(KeyCode::Enter));
    update_step(
        &mut s,
        leveler_update::UpdateStep::UpToDate {
            current: leveler_update::parse_version("1.0.0").unwrap(),
        },
    );
    reduce(
        &mut s,
        Action::UpdateFinished(Ok(leveler_update::UpdateOutcome::UpToDate {
            current: leveler_update::parse_version("1.0.0").unwrap(),
        })),
    );
    assert!(!s.restart_requested);
    assert!(s.running, "an up-to-date check is not an exit");
}

#[test]
fn update_failure_keeps_the_session_and_shows_the_reason() {
    let mut s = opened();
    s.composer.replace("/update");
    reduce(&mut s, key(KeyCode::Enter));
    reduce(
        &mut s,
        Action::UpdateFinished(Err("checksum verification failed".into())),
    );
    assert!(!s.restart_requested);
    assert!(s.running);
    let view = s.update.as_ref().unwrap();
    assert_eq!(view.phase, leveler_tui::update::UpdatePhase::Failed);
    assert_eq!(view.error.as_deref(), Some("checksum verification failed"));
}

#[test]
fn the_upgrade_alias_reaches_the_same_command() {
    let mut s = opened();
    s.composer.replace("/upgrade");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(
        matches!(effects.as_slice(), [Effect::StartUpdate]),
        "{effects:?}"
    );
}

#[test]
fn esc_dismisses_a_finished_update_panel() {
    let mut s = opened();
    s.composer.replace("/update");
    reduce(&mut s, key(KeyCode::Enter));
    reduce(
        &mut s,
        Action::UpdateFinished(Err("checksum verification failed".into())),
    );
    assert!(s.update.is_some());
    let effects = reduce(&mut s, key(KeyCode::Esc));
    assert!(effects.is_empty(), "{effects:?}");
    assert!(s.update.is_none(), "Esc must close the finished panel");
}

#[test]
fn esc_does_not_dismiss_an_update_in_flight() {
    let mut s = opened();
    s.composer.replace("/update");
    reduce(&mut s, key(KeyCode::Enter));
    update_step(
        &mut s,
        leveler_update::UpdateStep::Downloading {
            asset: "a.tar.gz".into(),
            received: 1,
            total: Some(100),
        },
    );
    reduce(&mut s, key(KeyCode::Esc));
    assert!(
        s.update.is_some(),
        "Esc must not hide an update that is still running"
    );
}
