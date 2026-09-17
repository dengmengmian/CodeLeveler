//! 待发送: input written while a turn runs is held in the bottom control area,
//! sent one item at a time on the user's say-so, and enters the conversation
//! only once the runtime admits it.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use leveler_client_protocol::{ClientCommand, CommandId, RuntimeStatus, SessionId};

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
        PendingInputState::Sending(id.clone())
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
        PendingInputState::Unconfirmed(id.clone())
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
