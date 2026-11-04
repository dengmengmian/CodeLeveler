//! The crash-recovery gate in the TUI.
//!
//! A runtime that died mid-call refuses the next turn until the user
//! acknowledges that the interrupted calls' outcomes are unknown. The gate is
//! the runtime's; this client only shows the evidence the snapshot carries and
//! sends the explicit acknowledgement. Nothing here replays a call, and
//! cancelling leaves the gate exactly where it was.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use leveler_client_protocol::{
    ClientCommand, PermissionProfile, RuntimeEvent, SessionId, UiInterruptedCallState,
    UiInterruptedToolCall, UiSessionSnapshot,
};
use leveler_tui::Locale;
use leveler_tui::action::{Action, Effect};
use leveler_tui::overlay::Overlay;
use leveler_tui::reducer::reduce;
use leveler_tui::state::{AppState, Boot};
use leveler_tui::theme::Theme;

fn snapshot(calls: Vec<UiInterruptedToolCall>) -> UiSessionSnapshot {
    UiSessionSnapshot {
        history: None,
        id: SessionId::new("s1"),
        repository: Some("/repo".into()),
        task_status: None,
        task_terminal: None,
        goal: "g".into(),
        model: None,
        mode: PermissionProfile::Assisted,
        branch: None,
        status: "idle".into(),
        finalization_stage: None,
        messages: Vec::new(),
        pending_interactions: Vec::new(),
        available_models: Vec::new(),
        vision: false,
        last_sequence: None,
        active_tools: Vec::new(),
        interrupted_tool_calls: calls,
        active_background_tasks: Vec::new(),
        background_tasks: Vec::new(),
        plan: None,
        diff: None,
        checkpoints: Vec::new(),
        recaps: Vec::new(),
        user_shells: Vec::new(),
        completion_report: None,
        thinking: None,
        work_profile: None,
        collaboration: None,
        execution_task: None,
        children: Vec::new(),
    }
}

fn state(calls: Vec<UiInterruptedToolCall>) -> AppState {
    let mut s = AppState::new(
        Theme::no_color(),
        Boot {
            session_id: SessionId::new("s1"),
            user: "tester".into(),
            version: "1.0.22".into(),
            show_welcome: false,
            draft_path: None,
            history_path: None,
            context_window: 200_000,
            locale: Locale::Zh,
            untrusted_config: Vec::new(),
            model_notice: None,
            thinking: None,
        },
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot(calls),
        }),
    );
    s.notification = None;
    s
}

fn key(code: KeyCode) -> Action {
    Action::Key(KeyEvent::new(code, KeyModifiers::empty()))
}

fn typed(s: &mut AppState, text: &str) {
    for ch in text.chars() {
        reduce(s, key(KeyCode::Char(ch)));
    }
}

fn interrupted(call_id: &str, tool: &str) -> UiInterruptedToolCall {
    UiInterruptedToolCall {
        call_id: call_id.into(),
        tool: tool.into(),
        pending_approval: false,
        state: UiInterruptedCallState::Interrupted,
    }
}

fn rendered(state: &mut AppState) -> String {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut term = Terminal::new(TestBackend::new(100, 40)).unwrap();
    term.draw(|f| leveler_tui::render::render(f, state))
        .unwrap();
    let buf = term.backend().buffer();
    let mut out = String::new();
    for y in 0..40 {
        for x in 0..100 {
            out.push_str(buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
        }
        out.push('\n');
    }
    out
}

fn open_recovery(s: &mut AppState) {
    typed(s, "/recover");
    reduce(s, key(KeyCode::Enter));
}

#[test]
fn recover_lists_every_interrupted_call_without_replaying_any() {
    let mut s = state(vec![
        interrupted("call-a", "read_file"),
        UiInterruptedToolCall {
            call_id: "call-b".into(),
            tool: "run_command".into(),
            pending_approval: true,
            state: UiInterruptedCallState::Interrupted,
        },
        UiInterruptedToolCall {
            call_id: "call-c".into(),
            tool: "write_file".into(),
            pending_approval: false,
            state: UiInterruptedCallState::Unknown,
        },
    ]);
    assert_eq!(s.interrupted_tool_calls.len(), 3);

    open_recovery(&mut s);
    assert!(matches!(s.overlay, Some(Overlay::ConfirmRecovery(_))));
    let painted = rendered(&mut s);
    // Wide glyphs occupy two cells; the second is blank, so compare without
    // whitespace to make the assertion about content rather than cell padding.
    let compact: String = painted.chars().filter(|c| !c.is_whitespace()).collect();
    for needle in [
        "read_file",
        "call-a",
        "run_command",
        "call-b",
        "write_file",
        "call-c",
        "已中断（结果未知）",
        "审批中被打断，操作未执行",
        "状态未知",
        "确认恢复",
        "不会重放",
    ] {
        let needle: String = needle.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(compact.contains(&needle), "missing {needle:?}:\n{painted}");
    }
}

#[test]
fn confirming_sends_only_the_acknowledgement() {
    let mut s = state(vec![interrupted("call-a", "read_file")]);
    open_recovery(&mut s);

    // Evidence rows cannot be selected; the cursor starts on cancel, so one
    // step down is the acknowledgement.
    reduce(&mut s, key(KeyCode::Down));
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert_eq!(
        effects,
        vec![Effect::Send(ClientCommand::AcknowledgeCrashWindow {
            session_id: SessionId::new("s1"),
        })]
    );
    assert!(
        !effects.iter().any(|effect| matches!(
            effect,
            Effect::Send(ClientCommand::SubmitMessage { .. }) | Effect::SendInteraction { .. }
        )),
        "acknowledging must never replay a call or start a turn: {effects:?}"
    );
    assert!(s.overlay.is_none());
}

#[test]
fn cancelling_keeps_the_gate_and_the_evidence() {
    let mut s = state(vec![interrupted("call-a", "read_file")]);
    s.notification = None;
    let composer_before = s.composer.text().to_string();

    open_recovery(&mut s);
    let effects = reduce(&mut s, key(KeyCode::Esc));
    assert!(
        effects.is_empty(),
        "cancel performs no command: {effects:?}"
    );
    assert!(s.overlay.is_none());
    assert_eq!(s.interrupted_tool_calls.len(), 1, "evidence is not erased");
    assert_eq!(
        s.notification.as_ref().map(|n| n.message.as_str()),
        Some("已取消，恢复阻断保持不变。")
    );
    assert_eq!(
        s.composer.text(),
        composer_before,
        "the command is consumed, not turned into a message"
    );

    // The cancel row means the same thing as Esc.
    s.notification = None;
    open_recovery(&mut s);
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(effects.is_empty(), "the cancel row performs no command");
    assert_eq!(s.interrupted_tool_calls.len(), 1);
}

#[test]
fn recover_without_evidence_says_there_is_nothing_to_acknowledge() {
    let mut s = state(Vec::new());
    open_recovery(&mut s);
    assert!(s.overlay.is_none());
    assert_eq!(
        s.notification.as_ref().map(|n| n.message.as_str()),
        Some("没有需要确认的中断调用。")
    );
}

#[test]
fn a_snapshot_is_the_authority_for_the_evidence() {
    let mut s = state(vec![interrupted("call-a", "read_file")]);
    assert_eq!(s.interrupted_tool_calls.len(), 1);
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot(Vec::new()),
        }),
    );
    assert!(
        s.interrupted_tool_calls.is_empty(),
        "an acknowledged crash window must not linger in the client"
    );
}

#[test]
fn the_acknowledgement_notice_names_the_runtime_count() {
    let mut s = state(vec![
        interrupted("call-a", "read_file"),
        interrupted("call-b", "run_command"),
    ]);
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::CrashWindowAcknowledged {
            session_id: SessionId::new("s1"),
            closed: 2,
        }),
    );
    assert_eq!(
        s.notification.as_ref().map(|n| n.message.as_str()),
        Some("已确认恢复：2 个中断调用已标记为结果未知。")
    );
}
