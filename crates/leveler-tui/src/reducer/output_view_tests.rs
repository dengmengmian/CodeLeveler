//! Contracts for the full-output page: when it opens, what it keeps bounded,
//! and which capture paths it refuses.

use super::*;
use crate::state::Boot;
use crate::transcript::{TranscriptItem, UserShellStatus};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use leveler_client_protocol::SessionId;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn state(root: &std::path::Path) -> AppState {
    let mut s = AppState::new(
        crate::theme::Theme::dark(),
        Boot {
            session_id: SessionId::new("s1"),
            user: "u".into(),
            version: "0.1.0".into(),
            show_welcome: false,
            draft_path: Some(root.join("state").join("draft.txt")),
            history_path: Some(root.join("state").join("input_history.json")),
            context_window: 200_000,
            locale: crate::i18n::Locale::Zh,
            untrusted_config: Vec::new(),
            model_notice: None,
            thinking: None,
        },
    );
    s.size = (100, 40);
    s
}

fn capture(root: &std::path::Path, name: &str, text: &str) -> std::path::PathBuf {
    let directory = root
        .join("state")
        .join("shell-output")
        .join("s1")
        .join(name);
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("stdout.log"), text).unwrap();
    std::fs::write(directory.join("stderr.log"), "").unwrap();
    std::fs::write(directory.join("combined.log"), text).unwrap();
    for stream in ["stdout", "stderr", "combined"] {
        std::fs::write(
            directory.join(format!("{stream}.meta.json")),
            "{\"complete\":true}\n",
        )
        .unwrap();
    }
    directory
}

fn push_finished_shell(
    state: &mut AppState,
    output: String,
    truncated: bool,
    full_output: Option<std::path::PathBuf>,
) -> usize {
    let id = leveler_core::UserShellId::new("ush-1");
    state
        .transcript
        .push_user_shell_started(id.clone(), "seq 1 1000000".into(), "/repo".into(), 0);
    state
        .transcript
        .append_user_shell_output(&id, &output, false);
    state
        .transcript
        .set_user_shell_full_output(&id, full_output.map(|path| path.display().to_string()));
    if truncated {
        // Simulate the runtime's retained-tail cap without writing 1 MiB when
        // the test only needs the flag.
        state.transcript.mark_user_shell_output_truncated(&id);
    }
    state
        .transcript
        .complete_user_shell(&id, Some(0), 10, UserShellStatus::Success);
    state.transcript.user_shell_index(&id).expect("shell")
}

#[test]
fn a_large_finished_shell_opens_the_windowed_page() {
    let root = std::env::temp_dir().join(format!("leveler-output-open-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("state")).unwrap();
    let output = "x".repeat(300 * 1024);
    let directory = capture(&root, "ush-1", &output);
    let mut s = state(&root);
    let item = push_finished_shell(&mut s, output, true, Some(directory));

    let effects = open_output_view(&mut s, item).expect("large output opens the page");
    assert_eq!(s.active_screen, Screen::Output);
    assert!(s.output_view.is_some());
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::LoadOutputIndex { .. })),
        "the page asks the edge to index its source"
    );
    let shell = match &s.transcript.items()[item] {
        TranscriptItem::UserShell(shell) => shell,
        _ => panic!("shell"),
    };
    assert!(
        !shell.expanded,
        "the conversation keeps the bounded record, not the full result"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_small_finished_shell_stays_inline() {
    let root = std::env::temp_dir().join(format!("leveler-output-small-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("state")).unwrap();
    let mut s = state(&root);
    let item = push_finished_shell(&mut s, "one\ntwo\n".into(), false, None);
    assert!(open_output_view(&mut s, item).is_none());
    assert_eq!(s.active_screen, Screen::Conversation);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_missing_capture_falls_back_only_when_the_retained_text_is_complete() {
    let root = std::env::temp_dir().join(format!("leveler-output-missing-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("state")).unwrap();
    let mut s = state(&root);
    let output = "x".repeat(300 * 1024);
    let item = push_finished_shell(
        &mut s,
        output.clone(),
        false,
        Some(root.join("does-not-exist")),
    );
    let effects = open_output_view(&mut s, item).expect("complete retained text is pageable");
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::LoadOutputIndex {
            source: crate::output_view::OutputSource::Memory(_),
            ..
        }
    )));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_truncated_result_without_a_trusted_capture_is_refused() {
    let root = std::env::temp_dir().join(format!("leveler-output-refuse-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("state")).unwrap();
    let outside =
        std::env::temp_dir().join(format!("leveler-output-outside-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&outside);
    let directory = capture(&outside, "ush-1", "tail\n");
    let mut s = state(&root);
    let item = push_finished_shell(&mut s, "tail\n".into(), true, Some(directory));
    assert!(open_output_view(&mut s, item).is_none());
    assert_eq!(s.active_screen, Screen::Conversation);
    assert!(
        s.notification
            .as_ref()
            .is_some_and(|n| n.message.contains("完整输出不可用")),
        "the refusal is explicit"
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&outside);
}

fn open_with_lines(state: &mut AppState, lines: usize) {
    let text: String = (0..lines).map(|n| format!("line-{n:05}\n")).collect();
    let source = crate::output_view::OutputSource::Memory(text.into());
    let index = std::sync::Arc::new(crate::output_view::OutputIndex::build(&source, 20).unwrap());
    let mut view = crate::output_view::OutputView::new(source, "seq".into(), 20);
    view.apply_index(index);
    view.viewport_height = 10;
    view.rect = Some((2, 2, 20, 10));
    state.output_view = Some(view);
    state.active_screen = Screen::Output;
}

fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

#[test]
fn a_drag_past_the_right_edge_still_copies_the_selected_rows() {
    let root = std::env::temp_dir().join(format!("leveler-output-drag-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("state")).unwrap();
    let mut s = state(&root);
    open_with_lines(&mut s, 100);
    output_mouse(&mut s, mouse(MouseEventKind::Down(MouseButton::Left), 3, 4));
    output_mouse(
        &mut s,
        mouse(MouseEventKind::Drag(MouseButton::Left), 200, 6),
    );
    let effects = output_mouse(&mut s, mouse(MouseEventKind::Up(MouseButton::Left), 200, 6));
    assert!(
        matches!(effects.as_slice(), [Effect::CopyOutputSelection { .. }]),
        "a release after dragging past the edge copies: {effects:?}"
    );
    let check = output_position(s.output_view.as_ref().unwrap(), 200, 99).unwrap();
    assert_eq!(check.col, 19, "columns clamp to the body's last cell");
    assert_eq!(check.row, 9, "rows clamp to the body's last row");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_output_page_keys_scroll_and_expose_both_copy_paths() {
    let root = std::env::temp_dir().join(format!("leveler-output-keys-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("state")).unwrap();
    let mut s = state(&root);
    let text: String = (0..500).map(|n| format!("line-{n:04}\n")).collect();
    let source = crate::output_view::OutputSource::Memory(text.into());
    let index = std::sync::Arc::new(crate::output_view::OutputIndex::build(&source, 80).unwrap());
    let mut view = crate::output_view::OutputView::new(source, "seq".into(), 80);
    view.apply_index(index);
    view.viewport_height = 10;
    s.output_view = Some(view);
    s.active_screen = Screen::Output;

    output_screen_key(&mut s, &key(KeyCode::End));
    assert!(s.output_view.as_ref().unwrap().follow);
    output_screen_key(&mut s, &key(KeyCode::Home));
    assert!(!s.output_view.as_ref().unwrap().follow);
    assert_eq!(s.output_view.as_ref().unwrap().scroll, 0);
    let effects = output_screen_key(&mut s, &key(KeyCode::Char('a')));
    assert!(matches!(effects.as_slice(), [Effect::CopyOutputAll { .. }]));
    output_screen_key(&mut s, &key(KeyCode::Esc));
    assert_eq!(s.active_screen, Screen::Conversation);
    assert!(s.output_view.is_none());
    let _ = std::fs::remove_dir_all(&root);
}
