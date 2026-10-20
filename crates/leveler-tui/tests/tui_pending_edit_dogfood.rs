//! Standing dogfood acceptance for taking an unsent message back to the composer.
//! Coordinates come from painted cells, independently of the production hit map.
//! This drives the public renderer and reducer; no runtime/provider is simulated.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use leveler_client_protocol::{
    AttachmentId, AttachmentKind, AttachmentRef, CommandId, RuntimeStatus, SessionId,
};
use leveler_tui::action::{Action, Effect};
use leveler_tui::pending_inputs::{PendingInput, PendingInputState};
use leveler_tui::reducer::reduce;
use leveler_tui::state::{AppState, Boot, WorkbenchFocus};
use leveler_tui::{Locale, Theme};
use ratatui::{Terminal, backend::TestBackend};

fn state(locale: Locale) -> AppState {
    let mut s = AppState::new(
        Theme::no_color(),
        Boot {
            session_id: SessionId::new("pending-edit-dogfood"),
            user: "dogfood".into(),
            version: "test".into(),
            show_welcome: false,
            draft_path: None,
            history_path: None,
            context_window: 200_000,
            locale,
            untrusted_config: Vec::new(),
            model_notice: None,
            thinking: None,
        },
    );
    s.status = RuntimeStatus::Busy;
    s.workbench_focus = WorkbenchFocus::Pending;
    s
}

fn key(s: &mut AppState, code: KeyCode) -> Vec<Effect> {
    reduce(s, Action::Key(KeyEvent::new(code, KeyModifiers::NONE)))
}

fn stage(s: &mut AppState, text: &str) {
    s.pending_inputs
        .push(PendingInput::queued(text, s.session_id.clone()));
}

/// Each string has one character per terminal column, including wide-cell padding.
fn draw(s: &mut AppState, width: u16) -> Vec<String> {
    s.size = (width, 24);
    let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
    terminal
        .draw(|frame| leveler_tui::render::render(frame, s))
        .unwrap();
    let buffer = terminal.backend().buffer();
    (0..24)
        .map(|y| {
            (0..width)
                .map(|x| {
                    buffer
                        .cell((x, y))
                        .unwrap()
                        .symbol()
                        .chars()
                        .next()
                        .unwrap_or(' ')
                })
                .collect()
        })
        .collect()
}

fn find(rows: &[String], label: &str) -> Option<(u16, u16)> {
    rows.iter().enumerate().find_map(|(y, row)| {
        row.find(label)
            .map(|byte| (row[..byte].chars().count() as u16, y as u16))
    })
}

fn edit_label(locale: Locale) -> &'static str {
    match locale {
        Locale::Zh => "编 辑",
        _ => "edit",
    }
}

fn click_edit(s: &mut AppState, width: u16, locale: Locale) {
    let rows = draw(s, width);
    let (column, row) = find(&rows, edit_label(locale))
        .unwrap_or_else(|| panic!("edit action missing from screen:\n{}", rows.join("\n")));
    assert!(
        reduce(
            s,
            Action::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row,
                modifiers: KeyModifiers::NONE,
            })
        )
        .is_empty(),
        "editing must not produce an external effect"
    );
}

#[test]
fn dogfood_pending_edit_mouse_restores_full_text_and_requeues_after_edit() {
    let original = format!("中文 {}\n第二行不能丢失", "long-text-".repeat(24));
    for locale in [Locale::Zh, Locale::En] {
        for width in [48, 80, 120] {
            let mut s = state(locale);
            stage(&mut s, "first queued item");
            stage(&mut s, &original);
            s.pending_selected = 1;
            click_edit(&mut s, width, locale);
            assert_eq!(s.composer.canonical_text(), original);
            assert_eq!(s.workbench_focus, WorkbenchFocus::Input);
            assert_eq!(s.pending_inputs.len(), 1);
            assert_eq!(s.pending_inputs[0].text, "first queued item");
            assert!(s.pending_submissions.is_empty());
            let rows = draw(&mut s, width);
            assert!(
                find(&rows, "第 二 行 不 能 丢 失").is_some(),
                "{}",
                rows.join("\n")
            );
            assert!(reduce(&mut s, Action::TextInput(" EDITED".into())).is_empty());
            assert!(key(&mut s, KeyCode::Enter).is_empty());
            assert!(s.composer.is_empty());
            assert_eq!(s.pending_inputs.len(), 2);
            assert_eq!(s.pending_inputs[1].text, format!("{original} EDITED"));
            assert!(s.pending_inputs[1].is_queued());
            assert!(s.pending_submissions.is_empty());
            let rows = draw(&mut s, width);
            assert!(find(&rows, "long-text-").is_some(), "{}", rows.join("\n"));
        }
    }
}

#[test]
fn dogfood_pending_edit_keyboard_restores_failed_input() {
    for pending_state in [
        PendingInputState::Queued,
        PendingInputState::Failed("refused".into()),
    ] {
        let mut s = state(Locale::En);
        stage(&mut s, "retry this\ncomplete text");
        s.pending_inputs[0].state = pending_state;
        assert!(find(&draw(&mut s, 80), "edit").is_some());
        assert!(key(&mut s, KeyCode::Char('e')).is_empty());
        assert_eq!(s.composer.canonical_text(), "retry this\ncomplete text");
        assert!(s.pending_inputs.is_empty());
        assert!(s.pending_submissions.is_empty());
        let rows = draw(&mut s, 80);
        assert!(find(&rows, "complete text").is_some());
        assert!(find(&rows, "edit").is_none());
    }
}

#[test]
fn dogfood_pending_edit_preserves_text_and_attachment_drafts() {
    for attachment_only in [false, true] {
        let mut s = state(Locale::En);
        if attachment_only {
            s.composer.insert_image_token();
            s.pending_attachments.push(AttachmentRef {
                id: AttachmentId::new("draft-image"),
                kind: AttachmentKind::Image,
                name: "draft.png".into(),
                mime_type: "image/png".into(),
                size_bytes: 1,
                sha256: "test-hash".into(),
                width: Some(1),
                height: Some(1),
            });
        } else {
            s.composer.replace("existing draft");
        }
        stage(&mut s, "keep queued");
        let text = s.composer.canonical_text();
        let attachments = s.pending_attachments.clone();
        let queue = s.pending_inputs.clone();
        click_edit(&mut s, 80, Locale::En);
        assert_eq!(s.composer.canonical_text(), text);
        assert_eq!(s.pending_attachments, attachments);
        assert_eq!(s.pending_inputs, queue);
        assert!(s.pending_submissions.is_empty());
        let warning = s
            .notification
            .as_ref()
            .expect("visible conflict warning")
            .message
            .clone();
        assert!(find(&draw(&mut s, 120), &warning).is_some());
    }
}

#[test]
fn dogfood_pending_edit_refuses_inflight_unknown_and_other_session() {
    for pending_state in [
        PendingInputState::Submitting(CommandId::new("sent")),
        PendingInputState::DeliveryUnknown(CommandId::new("sent")),
        PendingInputState::Queued,
    ] {
        let mut s = state(Locale::En);
        stage(&mut s, "protected input");
        let other_session = matches!(pending_state, PendingInputState::Queued);
        s.pending_inputs[0].state = pending_state;
        if other_session {
            s.pending_inputs[0].session_id = SessionId::new("other");
        }
        let queue = s.pending_inputs.clone();
        let rows = draw(&mut s, 80);
        if !other_session {
            let row = rows
                .iter()
                .find(|row| row.contains("protected input"))
                .unwrap();
            assert!(!row.contains("edit"), "{row}");
        }
        assert!(key(&mut s, KeyCode::Char('e')).is_empty());
        assert!(s.composer.is_empty());
        assert_eq!(s.pending_inputs, queue);
        assert!(s.pending_submissions.is_empty());
        assert!(find(&draw(&mut s, 80), "protected input").is_some());
    }
}
