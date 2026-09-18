//! 待发送: input written while a turn runs is held in the bottom control area,
//! sent one item at a time on the user's say-so, and enters the conversation
//! only once the runtime admits it. Once the running turn reaches its terminal
//! event and the runtime is ready, the FIFO head advances on its own.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use leveler_client_protocol::{ClientCommand, CommandId, RuntimeEvent, RuntimeStatus, SessionId};

use super::reduce;
use crate::action::{Action, Effect, EffectCompletion};
use crate::pending_inputs::PendingInputState;
use crate::state::{AppState, Boot, WorkbenchFocus};
use crate::transcript::TranscriptItem;

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
    s.status = RuntimeStatus::Busy;
    s
}

fn key(s: &mut AppState, code: KeyCode) -> Vec<Effect> {
    reduce(s, Action::Key(KeyEvent::new(code, KeyModifiers::empty())))
}

fn type_and_enter(s: &mut AppState, text: &str) -> Vec<Effect> {
    s.composer.replace(text.to_string());
    key(s, KeyCode::Enter)
}

fn user_messages(s: &AppState) -> Vec<String> {
    s.transcript
        .items()
        .iter()
        .filter_map(|i| match i {
            TranscriptItem::User(t) => Some(t.clone()),
            _ => None,
        })
        .collect()
}

fn texts(s: &AppState) -> Vec<String> {
    s.pending_inputs.iter().map(|p| p.text.clone()).collect()
}

fn submitted(effects: &[Effect]) -> (ClientCommand, CommandId) {
    match effects {
        [
            Effect::Submit {
                command,
                command_id,
            },
        ] => (command.clone(), command_id.clone()),
        other => panic!("expected one submission: {other:?}"),
    }
}

fn stage_three(s: &mut AppState) {
    for text in ["结果如何？", "如果失败先不要修改配置", "先别继续了"] {
        assert!(type_and_enter(s, text).is_empty(), "staging sends nothing");
    }
}

/// Send item `index` from the keyboard: focus the list, select, Enter.
fn send_from_keyboard(s: &mut AppState, index: usize) -> Vec<Effect> {
    s.workbench_focus = WorkbenchFocus::Pending;
    s.pending_selected = index;
    key(s, KeyCode::Enter)
}

#[test]
fn enter_while_a_turn_runs_holds_the_input_instead_of_sending_it() {
    let mut s = state();
    stage_three(&mut s);
    assert_eq!(
        texts(&s),
        vec!["结果如何？", "如果失败先不要修改配置", "先别继续了"]
    );
    assert!(s.composer.is_empty());
    assert!(
        user_messages(&s).is_empty(),
        "nothing unsent is conversation"
    );
    assert!(s.pending_submissions.is_empty());
}

#[test]
fn enter_while_idle_still_sends_at_once() {
    let mut s = state();
    s.status = RuntimeStatus::Idle;
    let (command, _) = submitted(&type_and_enter(&mut s, "你好"));
    assert!(matches!(command, ClientCommand::SubmitMessage { content, .. } if content == "你好"));
    assert!(s.pending_inputs.is_empty());
}

/// Any item can be sent first — the list is not a FIFO queue.
#[test]
fn any_item_can_be_sent_and_it_stays_until_the_runtime_admits_it() {
    let mut s = state();
    stage_three(&mut s);
    let (command, id) = submitted(&send_from_keyboard(&mut s, 2));
    assert_eq!(
        command,
        ClientCommand::SteerCurrentTurn {
            session_id: SessionId::new("s1"),
            content: "先别继续了".into(),
        }
    );
    assert_eq!(
        s.pending_inputs[2].state,
        PendingInputState::Submitting(id.clone())
    );
    assert!(user_messages(&s).is_empty(), "sent is not admitted");

    reduce(
        &mut s,
        Action::EffectCompleted(EffectCompletion::SubmissionDelivered {
            command_id: id,
            snapshot: None,
        }),
    );
    assert_eq!(texts(&s), vec!["结果如何？", "如果失败先不要修改配置"]);
    assert_eq!(user_messages(&s), vec!["先别继续了"]);
}

/// The turn a steer aimed at can end first; the runtime then starts a turn
/// with it and announces the message itself. Admission must not add it twice.
#[test]
fn a_steer_that_became_a_new_turn_is_shown_once() {
    let mut s = state();
    stage_three(&mut s);
    let (_, id) = submitted(&send_from_keyboard(&mut s, 0));
    reduce(
        &mut s,
        Action::Runtime(leveler_client_protocol::RuntimeEvent::UserMessageAdded {
            message: leveler_client_protocol::UiMessage {
                id: leveler_client_protocol::MessageId::new("u1"),
                role: leveler_client_protocol::UiRole::User,
                text: "结果如何？".into(),
                ordinal: None,
                kind: None,
                images: 0,
            },
        }),
    );
    reduce(
        &mut s,
        Action::EffectCompleted(EffectCompletion::SubmissionDelivered {
            command_id: id,
            snapshot: None,
        }),
    );
    assert_eq!(user_messages(&s), vec!["结果如何？"]);
}

#[test]
fn an_unanswered_send_stays_and_says_its_outcome_is_unknown() {
    let mut s = state();
    stage_three(&mut s);
    let (_, id) = submitted(&send_from_keyboard(&mut s, 0));
    reduce(
        &mut s,
        Action::EffectCompleted(EffectCompletion::SubmissionUnconfirmed {
            command_id: id.clone(),
        }),
    );
    assert_eq!(
        s.pending_inputs[0].state,
        PendingInputState::DeliveryUnknown(id.clone())
    );
    // It may already be in the runtime: it can be neither deleted nor resent.
    s.workbench_focus = WorkbenchFocus::Pending;
    s.pending_selected = 0;
    key(&mut s, KeyCode::Delete);
    assert_eq!(s.pending_inputs.len(), 3);
    assert!(key(&mut s, KeyCode::Enter).is_empty());

    reduce(
        &mut s,
        Action::EffectCompleted(EffectCompletion::SubmissionDelivered {
            command_id: id,
            snapshot: None,
        }),
    );
    assert_eq!(s.pending_inputs.len(), 2);
    assert_eq!(user_messages(&s), vec!["结果如何？"]);
}

/// An answer that is only late is not a lost connection: while the runtime has
/// not answered, and once it has, the notice claims no disconnect or reconnect
/// that nobody observed.
#[test]
fn a_late_answer_claims_no_disconnect() {
    let mut s = state();
    stage_three(&mut s);
    let (_, id) = submitted(&send_from_keyboard(&mut s, 0));
    reduce(
        &mut s,
        Action::EffectCompleted(EffectCompletion::SubmissionUnconfirmed {
            command_id: id.clone(),
        }),
    );
    let waiting = s.notification.as_ref().unwrap().message.clone();
    assert!(!waiting.contains("中断"), "{waiting}");
    reduce(
        &mut s,
        Action::EffectCompleted(EffectCompletion::SubmissionDelivered {
            command_id: id,
            snapshot: None,
        }),
    );
    let settled = s.notification.as_ref().unwrap().message.clone();
    assert!(!settled.contains("重新连接"), "{settled}");
    assert!(settled.contains("已送达"), "{settled}");
}

#[test]
fn a_refused_send_stays_in_the_list_marked_failed_and_the_composer_is_untouched() {
    let mut s = state();
    stage_three(&mut s);
    s.composer.replace("正在写的草稿".to_string());
    let (_, id) = submitted(&send_from_keyboard(&mut s, 1));
    reduce(
        &mut s,
        Action::EffectCompleted(EffectCompletion::SubmissionRejected {
            command_id: id,
            message: "session busy".into(),
            snapshot: None,
        }),
    );
    assert!(matches!(
        s.pending_inputs[1].state,
        PendingInputState::Failed(_)
    ));
    assert_eq!(s.composer.text(), "正在写的草稿");
    assert!(user_messages(&s).is_empty());
    // Still unsent: it can be sent again.
    assert!(!send_from_keyboard(&mut s, 1).is_empty());
}

/// A refused head pauses the FIFO queue instead of being skipped: retrying it
/// automatically would loop, and jumping over it would reorder the user's own
/// messages. Deleting it lets the next queued item advance.
#[test]
fn a_refused_head_pauses_the_queue_until_the_user_decides() {
    let mut s = state();
    type_and_enter(&mut s, "A");
    type_and_enter(&mut s, "B");
    let (_, id) = submitted(&send_from_keyboard(&mut s, 0));
    reduce(
        &mut s,
        Action::EffectCompleted(EffectCompletion::SubmissionRejected {
            command_id: id,
            message: "session busy".into(),
            snapshot: None,
        }),
    );
    assert!(matches!(
        s.pending_inputs[0].state,
        PendingInputState::Failed(_)
    ));

    // The runtime is ready and B is queued, but A is the head and is the
    // user's to decide on.
    let effects = reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCompleted));
    assert!(
        submits(&effects).is_empty(),
        "B must not jump A: {effects:?}"
    );
    assert!(matches!(
        s.pending_inputs[1].state,
        PendingInputState::Queued
    ));

    // Deleting the refused head lets B advance.
    let effects = key(&mut s, KeyCode::Delete);
    let commands = submits(&effects);
    assert_eq!(commands.len(), 1, "B advances after A is gone: {effects:?}");
    assert!(
        matches!(&commands[0], ClientCommand::SubmitMessage { content, .. } if content == "B"),
        "{commands:?}"
    );
}

#[test]
fn deleting_removes_only_that_unsent_item_and_sends_nothing() {
    let mut s = state();
    stage_three(&mut s);
    s.workbench_focus = WorkbenchFocus::Pending;
    s.pending_selected = 1;
    assert!(key(&mut s, KeyCode::Delete).is_empty());
    assert_eq!(texts(&s), vec!["结果如何？", "先别继续了"]);
}

// ---- The bottom control area ----

fn render(s: &mut AppState, width: u16, height: u16) -> Vec<String> {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| crate::render::render(frame, s))
        .unwrap();
    let buf = terminal.backend().buffer().clone();
    // A wide glyph owns two cells; its trailing cell is padding, not text.
    (0..buf.area.height)
        .map(|y| {
            let mut line = String::new();
            let mut x = 0;
            while x < buf.area.width {
                let symbol = buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ");
                line.push_str(symbol);
                x += unicode_width::UnicodeWidthStr::width(symbol).max(1) as u16;
            }
            line
        })
        .collect()
}

fn row_of(lines: &[String], needle: &str) -> Option<usize> {
    lines.iter().position(|l| l.contains(needle))
}

#[test]
fn pending_inputs_sit_above_the_composer_and_never_in_the_conversation() {
    let mut s = state();
    s.transcript.push_user("验证续期".into());
    stage_three(&mut s);
    let lines = render(&mut s, 100, 40);
    let header = row_of(&lines, "待发送 · 3").expect("the 待发送 header");
    let item = row_of(&lines, "› 结果如何？").expect("an item row");
    let composer = lines
        .iter()
        .rposition(|l| l.contains('╭'))
        .expect("the composer box");
    assert!(header < item && item < composer, "{lines:#?}");
    let width = s.conv.rect.unwrap().2 as usize;
    assert!(
        !s.conversation_lines(width)
            .iter()
            .map(crate::selection::line_to_plain)
            .any(|l| l.contains("结果如何")),
        "unsent input is not conversation content"
    );
}

#[test]
fn with_nothing_pending_the_area_is_absent() {
    let mut s = state();
    let lines = render(&mut s, 100, 40);
    assert!(row_of(&lines, "待发送").is_none(), "{lines:#?}");
}

#[test]
fn hovering_an_item_offers_send_and_delete_and_clicking_send_sends_that_item() {
    let mut s = state();
    stage_three(&mut s);
    let lines = render(&mut s, 100, 40);
    assert!(row_of(&lines, "发送 · 删除").is_none(), "clean by default");
    let row = row_of(&lines, "› 如果失败先不要修改配置").unwrap() as u16;
    reduce(
        &mut s,
        Action::Mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 10,
            row,
            modifiers: KeyModifiers::empty(),
        }),
    );
    let lines = render(&mut s, 100, 40);
    let hovered = &lines[row as usize];
    assert!(hovered.contains("发送 · 删除"), "{hovered:?}");
    let col =
        unicode_width::UnicodeWidthStr::width(&hovered[..hovered.find("发送 · 删除").unwrap()])
            as u16;
    let effects = reduce(
        &mut s,
        Action::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: col,
            row,
            modifiers: KeyModifiers::empty(),
        }),
    );
    let (command, _) = submitted(&effects);
    assert!(
        matches!(command, ClientCommand::SteerCurrentTurn { content, .. } if content == "如果失败先不要修改配置")
    );
}

#[test]
fn clicking_delete_removes_that_item() {
    let mut s = state();
    stage_three(&mut s);
    render(&mut s, 100, 40);
    let lines = render(&mut s, 100, 40);
    let row = row_of(&lines, "› 结果如何？").unwrap() as u16;
    reduce(
        &mut s,
        Action::Mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 10,
            row,
            modifiers: KeyModifiers::empty(),
        }),
    );
    let lines = render(&mut s, 100, 40);
    let hovered = &lines[row as usize];
    let at = hovered.find("删除").unwrap();
    let col = unicode_width::UnicodeWidthStr::width(&hovered[..at]) as u16;
    let effects = reduce(
        &mut s,
        Action::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: col,
            row,
            modifiers: KeyModifiers::empty(),
        }),
    );
    assert!(effects.is_empty());
    assert_eq!(texts(&s), vec!["如果失败先不要修改配置", "先别继续了"]);
}

#[test]
fn an_unknown_send_is_labelled_in_its_row() {
    let mut s = state();
    stage_three(&mut s);
    let (_, id) = submitted(&send_from_keyboard(&mut s, 0));
    reduce(
        &mut s,
        Action::EffectCompleted(EffectCompletion::SubmissionUnconfirmed { command_id: id }),
    );
    let lines = render(&mut s, 100, 40);
    let row = row_of(&lines, "› 结果如何？").unwrap();
    assert!(lines[row].contains("状态未知"), "{:?}", lines[row]);
}

/// Many items never push the composer off screen or eat the conversation.
#[test]
fn a_long_list_is_bounded_and_names_what_it_hides() {
    let mut s = state();
    for i in 1..=9 {
        type_and_enter(&mut s, &format!("第 {i} 条"));
    }
    let lines = render(&mut s, 100, 30);
    assert!(row_of(&lines, "待发送 · 9").is_some(), "{lines:#?}");
    let shown = lines.iter().filter(|l| l.contains("› 第 ")).count();
    assert!((1..9).contains(&shown), "{lines:#?}");
    assert!(
        row_of(&lines, &format!("还有 {} 条", 9 - shown)).is_some(),
        "{lines:#?}"
    );
    assert!(
        lines.iter().any(|l| l.contains('╭')),
        "composer stays visible"
    );
    assert!(s.conv.rect.unwrap().3 >= 3, "conversation keeps its floor");
}

// ---- Automatic continuation -------------------------------------------------

/// How many turn-input submissions an effect batch carries. The drain must add
/// at most one, and the same item at most once.
fn submits(effects: &[Effect]) -> Vec<ClientCommand> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Submit { command, .. } => Some(command.clone()),
            _ => None,
        })
        .collect()
}

fn idle_snapshot() -> leveler_client_protocol::UiSessionSnapshot {
    leveler_client_protocol::UiSessionSnapshot {
        id: SessionId::new("s1"),
        repository: "/repo".into(),
        goal: "interactive session".into(),
        model: leveler_client_protocol::ModelRef::parse("deepseek/v3"),
        mode: leveler_client_protocol::PermissionProfile::Assisted,
        branch: Some("main".into()),
        status: "idle".into(),
        finalization_stage: None,
        messages: Vec::new(),
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

fn admitted_message(text: &str) -> RuntimeEvent {
    RuntimeEvent::UserMessageAdded {
        message: leveler_client_protocol::UiMessage {
            id: leveler_client_protocol::MessageId::new("u1"),
            role: leveler_client_protocol::UiRole::User,
            text: text.into(),
            ordinal: None,
            kind: None,
            images: 0,
        },
    }
}

/// Case A — the core lifecycle: a queued input becomes the next user turn as
/// soon as the running turn reaches its terminal event, with no keystroke.
#[test]
fn a_completed_turn_advances_the_queue_as_the_next_user_turn() {
    let mut s = state();
    type_and_enter(&mut s, "完事之后提交远端。");
    assert_eq!(s.pending_inputs.len(), 1, "queued, not sent");
    assert!(s.pending_submissions.is_empty());

    let effects = reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCompleted));
    let (command, id) = submitted(&effects);
    assert!(
        matches!(&command, ClientCommand::SubmitMessage { content, .. } if content == "完事之后提交远端。"),
        "the queued item starts the next turn, not a steer: {command:?}"
    );
    assert_eq!(
        s.pending_inputs.len(),
        1,
        "shown until the runtime admits it"
    );
    assert!(matches!(
        s.pending_inputs[0].state,
        PendingInputState::Submitting(_)
    ));
    assert!(s.is_busy(), "the new turn is optimistically busy");

    reduce(
        &mut s,
        Action::EffectCompleted(EffectCompletion::SubmissionDelivered {
            command_id: id,
            snapshot: None,
        }),
    );
    assert!(s.pending_inputs.is_empty(), "accepted leaves 待发送");
    reduce(
        &mut s,
        Action::Runtime(admitted_message("完事之后提交远端。")),
    );
    assert_eq!(user_messages(&s), vec!["完事之后提交远端。"]);
}

/// Case B — a running turn is never interrupted by the drain: the item waits,
/// and no second turn input is sent while the first is active.
#[test]
fn a_queued_input_is_not_sent_while_a_turn_is_still_running() {
    let mut s = state();
    type_and_enter(&mut s, "别急");
    assert!(reduce(&mut s, Action::Resize(120, 40)).is_empty());
    assert!(matches!(
        s.pending_inputs[0].state,
        PendingInputState::Queued
    ));
    assert!(s.pending_inputs[0].is_queued());
    assert!(s.pending_submissions.is_empty());
}

/// Case C — the turn is terminal, but an earlier turn input is still in flight,
/// so the runtime is not ready: the queue must not advance. Once that input
/// settles and the head is still queued, it advances.
#[test]
fn a_terminal_turn_does_not_advance_the_queue_while_input_is_in_flight() {
    let mut s = state();
    type_and_enter(&mut s, "先改模块");
    // A steer is in flight and unanswered. It becomes item 0 (`Submitting`).
    let (_, steer_id) = submitted(&send_from_keyboard(&mut s, 0));
    // Back to the composer to write the follow-up.
    s.workbench_focus = WorkbenchFocus::Input;
    type_and_enter(&mut s, "完事之后提交远端。");
    assert_eq!(texts(&s), vec!["先改模块", "完事之后提交远端。"]);

    // Terminal event, but the in-flight submission keeps the runtime busy.
    let effects = reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCompleted));
    assert!(
        submits(&effects).is_empty(),
        "nothing new may be sent: {effects:?}"
    );
    assert!(matches!(
        s.pending_inputs[1].state,
        PendingInputState::Queued
    ));

    // The first attempt got no answer: still not ready, still no advance.
    reduce(
        &mut s,
        Action::EffectCompleted(EffectCompletion::SubmissionUnconfirmed {
            command_id: steer_id.clone(),
        }),
    );
    assert!(submits(&reduce(&mut s, Action::Resize(120, 40))).is_empty());
    assert!(matches!(
        s.pending_inputs[1].state,
        PendingInputState::Queued
    ));

    // Answered: the in-flight input settles, the runtime is ready, and the
    // still-queued head advances.
    let effects = reduce(
        &mut s,
        Action::EffectCompleted(EffectCompletion::SubmissionDelivered {
            command_id: steer_id,
            snapshot: None,
        }),
    );
    let commands = submits(&effects);
    assert_eq!(commands.len(), 1, "exactly the queued head: {effects:?}");
    assert!(
        matches!(&commands[0], ClientCommand::SubmitMessage { content, .. } if content == "完事之后提交远端。"),
        "{commands:?}"
    );
}

/// Case D — completion, a duplicate idle event, a refresh and a reconnect all
/// observe the same ready state; the item is still submitted exactly once.
#[test]
fn the_same_ready_window_submits_a_queued_input_only_once() {
    let mut s = state();
    type_and_enter(&mut s, "A");
    let mut submitted_ids = Vec::new();

    submitted_ids.extend(submits(&reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::TurnCompleted),
    )));
    // Duplicate terminal / idle transitions.
    submitted_ids.extend(submits(&reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::TurnCompleted),
    )));
    submitted_ids.extend(submits(&reduce(&mut s, Action::Resize(120, 40))));
    // A reconnect snapshot reporting the runtime idle.
    submitted_ids.extend(submits(&reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: idle_snapshot(),
        }),
    )));

    assert_eq!(
        submitted_ids.len(),
        1,
        "one queued input, one submission: {submitted_ids:?}"
    );
}

/// Case E — the queue drains in FIFO order, one turn at a time: A waits for its
/// own turn to end before B, and B before C.
#[test]
fn the_queue_advances_strictly_fifo() {
    let mut s = state();
    for text in ["A", "B", "C"] {
        type_and_enter(&mut s, text);
    }
    for text in ["A", "B", "C"] {
        let effects = reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCompleted));
        let commands = submits(&effects);
        assert_eq!(commands.len(), 1, "one turn at a time: {effects:?}");
        assert!(
            matches!(&commands[0], ClientCommand::SubmitMessage { content, .. } if content == text),
            "expected {text}, got {commands:?}"
        );
        // After A is admitted, B must wait for A's own turn to end.
        assert!(submits(&reduce(&mut s, Action::Resize(120, 40))).is_empty());
        let id = match effects.as_slice() {
            [Effect::Submit { command_id, .. }] => command_id.clone(),
            other => panic!("expected one submission: {other:?}"),
        };
        reduce(
            &mut s,
            Action::EffectCompleted(EffectCompletion::SubmissionDelivered {
                command_id: id,
                snapshot: None,
            }),
        );
    }
    assert!(s.pending_inputs.is_empty());
}

/// Case F — a queued item is a known state, not an unknown one.
#[test]
fn a_queued_item_is_not_labelled_unknown() {
    let mut s = state();
    type_and_enter(&mut s, "完事之后提交远端。");
    assert!(s.pending_inputs[0].is_queued());
    let lines = render(&mut s, 100, 40);
    let row = row_of(&lines, "› 完事之后提交远端。").expect("the queued row");
    assert!(!lines[row].contains("状态未知"), "{:?}", lines[row]);
    assert!(!lines[row].contains("发送中"), "{:?}", lines[row]);
}

/// Case G — `状态未知` is reserved for a delivery that was actually attempted
/// and could not be resolved. A queue that never sent is not that case.
#[test]
fn only_an_attempted_delivery_reads_as_unknown() {
    let mut s = state();
    type_and_enter(&mut s, "A");
    let queued = render(&mut s, 100, 40);
    let row = row_of(&queued, "› A").unwrap();
    assert!(!queued[row].contains("状态未知"));

    let (_, id) = submitted(&send_from_keyboard(&mut s, 0));
    reduce(
        &mut s,
        Action::EffectCompleted(EffectCompletion::SubmissionUnconfirmed {
            command_id: id.clone(),
        }),
    );
    assert_eq!(
        s.pending_inputs[0].state,
        PendingInputState::DeliveryUnknown(id)
    );
    let unknown = render(&mut s, 100, 40);
    let row = row_of(&unknown, "› A").unwrap();
    assert!(unknown[row].contains("状态未知"), "{:?}", unknown[row]);
}

/// A queued item belongs to the session it was written for: switching the
/// view to another idle session must not auto-send it there.
#[test]
fn a_queued_input_is_not_sent_into_another_session() {
    let mut s = state();
    type_and_enter(&mut s, "A");
    assert_eq!(s.pending_inputs[0].session_id, SessionId::new("s1"));

    // The user opens another idle session. The queue is shared bottom-control
    // state, so the item is still listed — but it is not that session's to run.
    s.session_id = SessionId::new("s2");
    s.status = RuntimeStatus::Idle;
    assert!(reduce(&mut s, Action::Resize(120, 40)).is_empty());
    assert!(matches!(
        s.pending_inputs[0].state,
        PendingInputState::Queued
    ));
    // Nor may it be sent by hand into the wrong session.
    s.workbench_focus = WorkbenchFocus::Pending;
    s.pending_selected = 0;
    assert!(key(&mut s, KeyCode::Enter).is_empty());

    // Back on its own session, it advances as usual.
    s.session_id = SessionId::new("s1");
    let effects = reduce(&mut s, Action::Resize(120, 40));
    let commands = submits(&effects);
    assert_eq!(commands.len(), 1, "{effects:?}");
    assert!(
        matches!(&commands[0], ClientCommand::SubmitMessage { content, .. } if content == "A"),
        "{commands:?}"
    );
}

/// Case H — a reconnect does not resend what the runtime already accepted.
#[test]
fn a_reconnect_does_not_resend_an_admitted_input() {
    let mut s = state();
    type_and_enter(&mut s, "A");
    let (_, id) = submitted(&reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::TurnCompleted),
    ));
    reduce(
        &mut s,
        Action::EffectCompleted(EffectCompletion::SubmissionDelivered {
            command_id: id,
            snapshot: None,
        }),
    );
    assert!(s.pending_inputs.is_empty(), "admitted, out of the queue");

    let effects = reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: idle_snapshot(),
        }),
    );
    assert!(submits(&effects).is_empty(), "must not resend: {effects:?}");
    assert!(s.pending_inputs.is_empty(), "replay must not resurrect it");
}
