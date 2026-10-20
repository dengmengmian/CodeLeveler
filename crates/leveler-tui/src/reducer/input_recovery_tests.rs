use super::*;
use crate::state::Boot;
use leveler_client_protocol::SessionId;

fn state() -> AppState {
    AppState::new(
        crate::theme::Theme::no_color(),
        Boot {
            session_id: SessionId::new("input-recovery"),
            user: "u".into(),
            version: "test".into(),
            show_welcome: false,
            draft_path: None,
            history_path: None,
            context_window: 200_000,
            locale: crate::i18n::Locale::Zh,
            untrusted_config: Vec::new(),
            model_notice: None,
            thinking: None,
        },
    )
}

fn key(state: &mut AppState, code: KeyCode, modifiers: KeyModifiers) -> Vec<Effect> {
    reduce(state, Action::Key(KeyEvent::new(code, modifiers)))
}

#[test]
fn input_recovery_undo_redo_routes_without_sending() {
    let mut s = state();
    reduce(&mut s, Action::TextInput("中文草稿".into()));
    key(&mut s, KeyCode::Char('u'), KeyModifiers::CONTROL);
    assert!(s.composer.is_empty());
    assert!(key(&mut s, KeyCode::Char('z'), KeyModifiers::CONTROL).is_empty());
    assert_eq!(s.composer.text(), "中文草稿");
    assert!(key(&mut s, KeyCode::Char('z'), KeyModifiers::ALT).is_empty());
    assert!(s.composer.is_empty());
}

#[test]
fn input_recovery_history_search_keeps_draft_until_acceptance() {
    let mut s = state();
    s.composer
        .set_history(vec!["修复登录".into(), "增加测试".into()]);
    s.composer.replace("当前草稿");
    key(&mut s, KeyCode::Char('r'), KeyModifiers::CONTROL);
    reduce(&mut s, Action::TextInput("登录".into()));
    assert_eq!(s.composer.text(), "当前草稿");
    assert!(key(&mut s, KeyCode::Enter, KeyModifiers::NONE).is_empty());
    assert_eq!(s.composer.text(), "修复登录");
    assert!(s.transcript.is_empty());
}

#[test]
fn input_recovery_history_cancel_restores_draft_and_never_cancels_turn() {
    let mut s = state();
    s.composer.replace("保留草稿");
    s.status = RuntimeStatus::Busy;
    key(&mut s, KeyCode::Char('r'), KeyModifiers::CONTROL);
    reduce(&mut s, Action::Paste("查找内容".into()));
    assert!(key(&mut s, KeyCode::Char('c'), KeyModifiers::CONTROL).is_empty());
    assert_eq!(s.composer.text(), "保留草稿");
    assert_eq!(s.status, RuntimeStatus::Busy);
}

#[test]
fn input_recovery_undo_restores_the_original_attachment_identity() {
    use leveler_client_protocol::{AttachmentId, AttachmentKind, AttachmentRef};
    let mut s = state();
    let attachment = AttachmentRef {
        id: AttachmentId::new("image-one"),
        kind: AttachmentKind::Image,
        name: "one.png".into(),
        mime_type: "image/png".into(),
        size_bytes: 1,
        sha256: "hash".into(),
        width: Some(1),
        height: Some(1),
    };
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AttachmentAdded {
            command_id: None,
            attachment: attachment.clone(),
        }),
    );
    key(&mut s, KeyCode::Left, KeyModifiers::NONE);
    key(&mut s, KeyCode::Backspace, KeyModifiers::NONE);
    assert!(s.pending_attachments.is_empty());
    key(&mut s, KeyCode::Char('z'), KeyModifiers::CONTROL);
    assert_eq!(s.pending_attachments, vec![attachment]);
    assert_eq!(s.composer.image_token_count(), 1);
    key(&mut s, KeyCode::Char('z'), KeyModifiers::ALT);
    assert!(s.pending_attachments.is_empty());
    assert_eq!(s.composer.image_token_count(), 0);
}

#[test]
fn input_recovery_doctor_is_local_and_reachable_while_busy() {
    let mut s = state();
    s.status = RuntimeStatus::Busy;
    s.composer.replace("/doctor");
    assert!(key(&mut s, KeyCode::Enter, KeyModifiers::NONE).is_empty());
    assert_eq!(s.active_screen, Screen::Doctor);
    assert_eq!(s.status, RuntimeStatus::Busy);
    key(&mut s, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(s.active_screen, Screen::Conversation);
}

#[test]
fn input_recovery_clipboard_notice_never_calls_sent_bytes_verified() {
    use crate::clipboard::{ClipboardRoute, CopyReceipt, Delivery};
    let receipt = CopyReceipt {
        delivery: Delivery::SentUnverified,
        routes: vec![ClipboardRoute::Tmux, ClipboardRoute::Osc52],
        failures: Vec::new(),
    };
    let message = super::clipboard_notice(&receipt, 3, crate::i18n::Locale::Zh);
    assert!(message.contains("已发送"));
    assert!(message.contains("未验证"));
    assert!(!message.contains("已复制"));
}

#[test]
fn input_recovery_search_accepts_raw_and_uppercase_cancel() {
    let mut s = state();
    s.status = RuntimeStatus::Busy;
    for (code, modifiers) in [
        (KeyCode::Char('\u{3}'), KeyModifiers::NONE),
        (KeyCode::Char('C'), KeyModifiers::CONTROL),
    ] {
        key(&mut s, KeyCode::Char('r'), KeyModifiers::CONTROL);
        assert!(key(&mut s, code, modifiers).is_empty());
        assert!(s.history_search.is_none());
    }
}

#[test]
fn input_recovery_search_mouse_cannot_send_covered_pending_input() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    let mut s = state();
    s.status = RuntimeStatus::Busy;
    reduce(&mut s, Action::TextInput("待发送".into()));
    key(&mut s, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(s.pending_inputs.len(), 1);
    s.pending_hits.push(crate::pending_inputs::PendingInputHit {
        row: 20,
        index: Some(0),
        send: Some((0, 10)),
        edit: None,
        delete: None,
    });
    key(&mut s, KeyCode::Char('r'), KeyModifiers::CONTROL);
    let effects = reduce(
        &mut s,
        Action::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 20,
            modifiers: KeyModifiers::NONE,
        }),
    );
    assert!(effects.is_empty());
    assert_eq!(s.pending_inputs.len(), 1);
    assert_eq!(s.active_screen, Screen::Conversation);
}

#[test]
fn input_recovery_session_switch_from_btw_clears_main_undo() {
    use leveler_client_protocol::{ModelRef, UiSessionSnapshot};
    let mut s = state();
    reduce(&mut s, Action::TextInput("旧会话草稿".into()));
    key(&mut s, KeyCode::Char('u'), KeyModifiers::CONTROL);
    assert!(s.composer.can_undo());
    enter_btw(&mut s);
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: UiSessionSnapshot {
                id: SessionId::new("new-session"),
                repository: None,
                task_status: None,
                task_terminal: None,
                goal: String::new(),
                model: ModelRef::parse("p/m"),
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
                active_background_tasks: Vec::new(),
                plan: None,
                diff: None,
                checkpoints: Vec::new(),
                recaps: Vec::new(),
                user_shells: Vec::new(),
                completion_report: None,
                thinking: None,
                work_profile: None,
                collaboration: None,
                children: Vec::new(),
            },
        }),
    );
    assert_eq!(s.surface, SurfaceFocus::Main);
    assert!(!s.composer.can_undo());
    assert!(s.pending_attachments.is_empty());
}

#[test]
fn input_recovery_mixed_file_paste_is_one_undo_unit() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let mut s = state();
    s.composer.replace("原稿 ");
    let pasted = format!("前缀 {} 后缀", file.path().display());
    reduce(&mut s, Action::Paste(pasted.clone()));
    assert_eq!(s.composer.canonical_text(), format!("原稿 {pasted}"));
    key(&mut s, KeyCode::Char('z'), KeyModifiers::CONTROL);
    assert_eq!(s.composer.canonical_text(), "原稿 ");
    key(&mut s, KeyCode::Char('z'), KeyModifiers::ALT);
    assert_eq!(s.composer.canonical_text(), format!("原稿 {pasted}"));
}

#[test]
fn input_recovery_update_panel_cannot_open_an_invisible_search() {
    let mut s = state();
    s.update = Some(crate::update::UpdateView::new());
    key(&mut s, KeyCode::Char('r'), KeyModifiers::CONTROL);
    assert!(s.history_search.is_none());
}

#[test]
fn pending_edit_returns_unsent_text_to_composer_without_sending() {
    for pending_state in [
        crate::pending_inputs::PendingInputState::Queued,
        crate::pending_inputs::PendingInputState::Failed("refused".into()),
    ] {
        let mut s = state();
        s.status = RuntimeStatus::Busy;
        let mut item =
            crate::pending_inputs::PendingInput::queued("中文\n继续编辑", s.session_id.clone());
        item.state = pending_state;
        s.pending_inputs.push(item);
        s.workbench_focus = WorkbenchFocus::Pending;
        assert!(key(&mut s, KeyCode::Char('e'), KeyModifiers::NONE).is_empty());
        assert_eq!(s.composer.canonical_text(), "中文\n继续编辑");
        assert!(s.pending_inputs.is_empty());
        assert!(s.pending_submissions.is_empty());
        assert_eq!(s.workbench_focus, WorkbenchFocus::Input);
        reduce(&mut s, Action::TextInput("！".into()));
        assert!(key(&mut s, KeyCode::Enter, KeyModifiers::NONE).is_empty());
        assert_eq!(s.pending_inputs[0].text, "中文\n继续编辑！");
        assert!(s.pending_inputs[0].is_queued());
    }
}

#[test]
fn pending_edit_preserves_existing_draft_and_queue() {
    let mut s = state();
    s.status = RuntimeStatus::Busy;
    s.composer.replace("已有草稿");
    s.pending_inputs
        .push(crate::pending_inputs::PendingInput::queued(
            "队列内容",
            s.session_id.clone(),
        ));
    s.workbench_focus = WorkbenchFocus::Pending;
    assert!(key(&mut s, KeyCode::Char('e'), KeyModifiers::NONE).is_empty());
    assert_eq!(s.composer.text(), "已有草稿");
    assert_eq!(s.pending_inputs[0].text, "队列内容");
    assert!(s.notification.is_some());
}

#[test]
fn pending_edit_cannot_take_inflight_or_other_session_input() {
    use crate::pending_inputs::{PendingInput, PendingInputState};
    for pending_state in [
        PendingInputState::Submitting(CommandId::new("sent")),
        PendingInputState::DeliveryUnknown(CommandId::new("sent")),
    ] {
        let mut s = state();
        s.status = RuntimeStatus::Busy;
        let mut item = PendingInput::queued("已发送", s.session_id.clone());
        item.state = pending_state;
        s.pending_inputs.push(item.clone());
        s.workbench_focus = WorkbenchFocus::Pending;
        assert!(key(&mut s, KeyCode::Char('e'), KeyModifiers::NONE).is_empty());
        assert!(s.composer.is_empty());
        assert_eq!(s.pending_inputs, vec![item]);
    }
    let mut s = state();
    s.status = RuntimeStatus::Busy;
    s.pending_inputs
        .push(PendingInput::queued("其他会话", SessionId::new("other")));
    s.workbench_focus = WorkbenchFocus::Pending;
    assert!(key(&mut s, KeyCode::Char('e'), KeyModifiers::NONE).is_empty());
    assert!(s.composer.is_empty());
    assert_eq!(s.pending_inputs.len(), 1);
}

#[test]
fn pending_edit_resets_shell_mode_and_preserves_other_queue_entries() {
    let mut s = state();
    s.status = RuntimeStatus::Busy;
    s.input_mode = crate::shell_mode::InputMode::Shell;
    s.pending_inputs
        .push(crate::pending_inputs::PendingInput::queued(
            "首条",
            s.session_id.clone(),
        ));
    s.pending_inputs
        .push(crate::pending_inputs::PendingInput::queued(
            "编辑这条",
            s.session_id.clone(),
        ));
    s.pending_selected = 1;
    s.pending_hover = Some(1);
    s.workbench_focus = WorkbenchFocus::Pending;
    key(&mut s, KeyCode::Char('e'), KeyModifiers::NONE);
    assert_eq!(s.input_mode, crate::shell_mode::InputMode::Normal);
    assert_eq!(s.composer.text(), "编辑这条");
    assert_eq!(s.pending_inputs[0].text, "首条");
    assert_eq!(s.pending_selected, 0);
    assert_eq!(s.pending_hover, None);
}

#[test]
fn pending_edit_mouse_returns_text_and_clears_stale_hits() {
    let mut s = state();
    s.status = RuntimeStatus::Busy;
    s.pending_inputs
        .push(crate::pending_inputs::PendingInput::queued(
            "鼠标取回",
            s.session_id.clone(),
        ));
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
    s.workbench_focus = WorkbenchFocus::Pending;
    terminal
        .draw(|frame| {
            let area = frame.area();
            crate::pending_inputs::render(frame, area, &mut s, 24);
        })
        .unwrap();
    let hit = s.pending_hits[0];
    let (column, _) = hit.edit.expect("selected item shows edit action");
    assert!(
        reduce(
            &mut s,
            Action::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row: hit.row,
                modifiers: KeyModifiers::NONE,
            })
        )
        .is_empty()
    );
    assert_eq!(s.composer.text(), "鼠标取回");
    assert!(s.pending_inputs.is_empty());
    assert!(s.pending_hits.is_empty());
    assert_eq!(s.workbench_focus, WorkbenchFocus::Input);
}

#[test]
fn pending_edit_preserves_attachment_only_draft() {
    use leveler_client_protocol::{AttachmentId, AttachmentKind, AttachmentRef};
    let mut s = state();
    s.status = RuntimeStatus::Busy;
    let attachment = AttachmentRef {
        id: AttachmentId::new("draft-image"),
        kind: AttachmentKind::Image,
        name: "draft.png".into(),
        mime_type: "image/png".into(),
        size_bytes: 1,
        sha256: "hash".into(),
        width: Some(1),
        height: Some(1),
    };
    s.pending_attachments.push(attachment.clone());
    s.pending_inputs
        .push(crate::pending_inputs::PendingInput::queued(
            "待编辑",
            s.session_id.clone(),
        ));
    s.workbench_focus = WorkbenchFocus::Pending;
    super::submit::edit_pending_input(&mut s, 0);
    assert!(s.composer.is_empty());
    assert_eq!(s.pending_attachments, vec![attachment]);
    assert_eq!(s.pending_inputs.len(), 1);
    assert!(s.notification.is_some());
}
