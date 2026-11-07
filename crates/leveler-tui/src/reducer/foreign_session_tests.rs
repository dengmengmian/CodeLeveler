//! Contracts for `/resume-claude` / `/resume-codex`: exact-id import, the
//! source picker, and the source identity a continuation keeps visible.
//!
//! The runtime owns discovery and import; these tests pin which command the
//! page sends, what it does with each runtime answer, and what the person sees
//! before and after the new native session opens.

use super::*;
use crate::state::Boot;
use leveler_client_protocol::{
    ModelRef, PermissionProfile, SessionId, UiForeignOrigin, UiForeignSession, UiForeignTool,
};
use unicode_width::UnicodeWidthStr;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn state(locale: crate::i18n::Locale) -> AppState {
    let mut s = AppState::new(
        crate::theme::Theme::no_color(),
        Boot {
            session_id: SessionId::new("requester"),
            user: "u".into(),
            version: "0.1.0".into(),
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
    s.size = (120, 40);
    s
}

fn source(id: &str, title: &str) -> UiForeignSession {
    UiForeignSession {
        tool: UiForeignTool::ClaudeCode,
        native_id: id.into(),
        title: title.into(),
        cwd: Some("/repo".into()),
        branch: Some("main".into()),
        updated_at: "2026-11-05T10:00:00Z".into(),
        size_bytes: 4096,
    }
}

fn commands(effects: &[Effect]) -> Vec<&ClientCommand> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Send(command) => Some(command),
            _ => None,
        })
        .collect()
}

fn slash(state: &mut AppState, command: &str) -> Vec<Effect> {
    super::submit::handle_slash(state, command)
}

fn open_picker(state: &mut AppState, tool: UiForeignTool) {
    let command = tool.slash_command();
    let effects = slash(state, command);
    assert!(
        commands(&effects).iter().any(|command| matches!(
            command,
            ClientCommand::ListForeignSessionsFor { tool: listed, .. } if *listed == tool
        )),
        "the picker asks the runtime for this source's sessions"
    );
}

fn snapshot(origin: Option<UiForeignOrigin>) -> leveler_client_protocol::UiSessionSnapshot {
    leveler_client_protocol::UiSessionSnapshot {
        history: None,
        id: SessionId::new("requester"),
        repository: Some("/repo".into()),
        task_status: None,
        task_terminal: None,
        goal: "g".into(),
        model: ModelRef::parse("test/model"),
        mode: PermissionProfile::Assisted,
        branch: Some("main".into()),
        status: "idle".into(),
        finalization_stage: None,
        messages: Vec::new(),
        pending_interactions: Vec::new(),
        available_models: Vec::new(),
        vision: false,
        last_sequence: None,
        latest_turn_ordinal: None,
        active_tools: Vec::new(),
        interrupted_tool_calls: Vec::new(),
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
        foreign_origin: origin,
    }
}

fn origin(native_id: &str) -> UiForeignOrigin {
    UiForeignOrigin {
        tool: UiForeignTool::ClaudeCode,
        native_id: native_id.into(),
        source_path: "/home/u/.claude/projects/p/session.jsonl".into(),
        source_cwd: Some("/repo".into()),
        imported_at: "2026-11-05T11:00:00Z".into(),
    }
}

fn render_text(state: &mut AppState, width: u16, height: u16) -> String {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| crate::render::render(frame, state))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..buffer.area.height {
        let mut x = 0;
        while x < buffer.area.width {
            let symbol = buffer.cell((x, y)).map(|cell| cell.symbol()).unwrap_or(" ");
            out.push_str(symbol);
            x += symbol.width().max(1) as u16;
        }
        out.push('\n');
    }
    out
}

#[test]
fn an_exact_id_imports_and_an_omitted_id_opens_that_sources_picker() {
    let mut s = state(crate::i18n::Locale::Zh);
    let id = "b8603c8f-6095-4bb5-8b37-12e4fafbfa5b";

    let effects = slash(&mut s, &format!("/resume-claude {id}"));
    let sent = commands(&effects);
    assert!(
        matches!(
            sent.as_slice(),
            [ClientCommand::ImportForeignSession {
                requester_session_id,
                tool: UiForeignTool::ClaudeCode,
                native_id,
            }] if requester_session_id == &SessionId::new("requester") && native_id == id
        ),
        "an exact id goes straight to the runtime: {sent:?}"
    );
    assert_eq!(s.active_screen, Screen::Conversation);
    let notice = s.notification.as_ref().expect("a reading notice");
    assert_eq!(notice.level, NotificationLevel::Info);
    assert!(
        notice.message.contains("Claude Code") && notice.message.contains(id),
        "the notice names what is being read: {}",
        notice.message
    );

    let effects = slash(&mut s, "/resume-codex");
    assert_eq!(s.active_screen, Screen::ForeignSessions);
    assert_eq!(s.foreign_sessions.tool, Some(UiForeignTool::Codex));
    assert!(s.foreign_sessions.loading);
    let sent = commands(&effects);
    assert!(
        matches!(
            sent.as_slice(),
            [ClientCommand::ListForeignSessionsFor {
                requester_session_id,
                tool: UiForeignTool::Codex,
            }] if requester_session_id == &SessionId::new("requester")
        ),
        "an omitted id opens that source's picker: {sent:?}"
    );
}

#[test]
fn discovery_fills_only_the_open_pickers_list_and_failure_is_not_empty() {
    let mut s = state(crate::i18n::Locale::Zh);
    open_picker(&mut s, UiForeignTool::ClaudeCode);
    assert!(s.foreign_sessions.loading, "loading before the answer");

    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ForeignSessionList {
            tool: UiForeignTool::ClaudeCode,
            sessions: vec![
                source("claude-1", "Fix the build"),
                source("claude-2", "Add tests"),
            ],
            error: None,
        }),
    );
    assert!(!s.foreign_sessions.loading);
    assert_eq!(s.foreign_sessions.sessions.len(), 2);
    assert_eq!(s.foreign_sessions.selected, 0);

    // A late answer for another source must not replace what is on screen.
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ForeignSessionList {
            tool: UiForeignTool::Codex,
            sessions: vec![source("codex-1", "Other work")],
            error: None,
        }),
    );
    assert_eq!(s.foreign_sessions.sessions[0].native_id, "claude-1");

    // A failed scan is an error, not a real empty result.
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ForeignSessionList {
            tool: UiForeignTool::ClaudeCode,
            sessions: Vec::new(),
            error: Some("无法读取会话目录".into()),
        }),
    );
    assert!(!s.foreign_sessions.loading);
    assert!(s.foreign_sessions.sessions.is_empty());
    assert_eq!(
        s.foreign_sessions.error.as_deref(),
        Some("无法读取会话目录"),
        "a discovery failure has its own state"
    );
}

#[test]
fn enter_imports_the_selected_source_once_and_escape_leaves() {
    let mut s = state(crate::i18n::Locale::Zh);
    open_picker(&mut s, UiForeignTool::ClaudeCode);

    // Nothing is selected before the list arrives: Enter has no subject.
    let effects = reduce(&mut s, Action::Key(key(KeyCode::Enter)));
    assert!(commands(&effects).is_empty());

    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ForeignSessionList {
            tool: UiForeignTool::ClaudeCode,
            sessions: vec![source("claude-1", "First"), source("claude-2", "Second")],
            error: None,
        }),
    );
    reduce(&mut s, Action::Key(key(KeyCode::Down)));
    assert_eq!(s.foreign_sessions.selected, 1);
    reduce(&mut s, Action::Key(key(KeyCode::Char('k'))));
    assert_eq!(s.foreign_sessions.selected, 0);
    reduce(&mut s, Action::Key(key(KeyCode::Char('j'))));

    let effects = reduce(&mut s, Action::Key(key(KeyCode::Enter)));
    let sent = commands(&effects);
    assert!(
        matches!(
            sent.as_slice(),
            [ClientCommand::ImportForeignSession {
                tool: UiForeignTool::ClaudeCode,
                native_id,
                ..
            }] if native_id == "claude-2"
        ),
        "Enter imports the row the cursor paints: {sent:?}"
    );
    assert!(s.foreign_sessions.importing);

    // A second Enter while the import is in flight is not a second import.
    let effects = reduce(&mut s, Action::Key(key(KeyCode::Enter)));
    assert!(
        commands(&effects).is_empty(),
        "one source session must not create two native sessions"
    );

    reduce(&mut s, Action::Key(key(KeyCode::Esc)));
    assert_eq!(s.active_screen, Screen::Conversation);
}

#[test]
fn a_successful_import_keeps_the_source_identity_and_warns_on_a_mismatch() {
    let mut s = state(crate::i18n::Locale::Zh);
    open_picker(&mut s, UiForeignTool::ClaudeCode);
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ForeignSessionList {
            tool: UiForeignTool::ClaudeCode,
            sessions: vec![source("claude-2", "Second")],
            error: None,
        }),
    );
    reduce(&mut s, Action::Key(key(KeyCode::Enter)));

    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ForeignSessionImported {
            tool: UiForeignTool::ClaudeCode,
            native_id: "claude-2".into(),
            session_id: SessionId::new("new-native"),
            warning: Some("来源会话的工作目录是 /elsewhere".into()),
        }),
    );
    assert_eq!(
        s.active_screen,
        Screen::Conversation,
        "the new native session owns the view"
    );
    assert!(!s.foreign_sessions.importing);
    let notice = s.notification.as_ref().expect("an imported notice");
    assert_eq!(notice.level, NotificationLevel::Warning);
    assert!(
        notice.message.contains("Claude Code") && notice.message.contains("claude-2"),
        "the notice names the source: {}",
        notice.message
    );
    assert!(
        notice.message.contains("工作目录"),
        "a source/workspace mismatch is not silent: {}",
        notice.message
    );
}

#[test]
fn a_failed_import_keeps_the_picker_and_shows_the_runtime_reason() {
    let mut s = state(crate::i18n::Locale::Zh);
    open_picker(&mut s, UiForeignTool::ClaudeCode);
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ForeignSessionList {
            tool: UiForeignTool::ClaudeCode,
            sessions: vec![source("claude-2", "Second")],
            error: None,
        }),
    );
    reduce(&mut s, Action::Key(key(KeyCode::Enter)));

    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ForeignSessionImportFailed {
            tool: UiForeignTool::ClaudeCode,
            native_id: "claude-2".into(),
            message: "会话文件已损坏".into(),
        }),
    );
    assert_eq!(
        s.active_screen,
        Screen::ForeignSessions,
        "a failed import leaves the picker so another row can be tried"
    );
    assert!(!s.foreign_sessions.importing);
    let notice = s.notification.as_ref().expect("a failure notice");
    assert_eq!(notice.level, NotificationLevel::Error);
    assert!(
        notice.message.contains("Claude Code")
            && notice.message.contains("claude-2")
            && notice.message.contains("会话文件已损坏"),
        "the failure names the source and the runtime's reason: {}",
        notice.message
    );

    // The picker is still actionable: the same row can be retried.
    let effects = reduce(&mut s, Action::Key(key(KeyCode::Enter)));
    assert!(
        commands(&effects).iter().any(|command| matches!(
            command,
            ClientCommand::ImportForeignSession { native_id, .. } if native_id == "claude-2"
        )),
        "a failed import is retryable"
    );
}

#[test]
fn the_header_keeps_the_source_identity_across_snapshots() {
    let mut s = state(crate::i18n::Locale::Zh);
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot(Some(origin("b8603c8f-6095-4bb5-8b37-12e4fafbfa5b"))),
        }),
    );
    assert_eq!(
        s.foreign_origin.as_ref().map(|o| o.native_id.as_str()),
        Some("b8603c8f-6095-4bb5-8b37-12e4fafbfa5b")
    );
    let text = render_text(&mut s, 120, 30);
    assert!(
        text.contains("⟲ Claude Code b8603c8f"),
        "the header keeps where this session came from:\n{text}"
    );

    // Opening a session with no origin drops the identity: it belongs to one
    // session, not to the client.
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot(None),
        }),
    );
    assert!(s.foreign_origin.is_none());
    let text = render_text(&mut s, 120, 30);
    assert!(
        !text.contains("⟲"),
        "a native session must not keep another session's origin:\n{text}"
    );
}

#[test]
fn the_picker_paints_loading_sources_and_the_source_identity() {
    let mut s = state(crate::i18n::Locale::En);
    open_picker(&mut s, UiForeignTool::ClaudeCode);

    let text = render_text(&mut s, 110, 30);
    assert!(text.contains("Resume Claude Code session"), "{text}");
    assert!(text.contains("Reading local sessions"), "{text}");
    assert!(
        !text.contains("No resumable sessions"),
        "loading is not an empty result:\n{text}"
    );

    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ForeignSessionList {
            tool: UiForeignTool::ClaudeCode,
            sessions: vec![source(
                "b8603c8f-6095-4bb5-8b37-12e4fafbfa5b",
                "Fix the build",
            )],
            error: None,
        }),
    );
    let text = render_text(&mut s, 110, 30);
    assert!(text.contains("Fix the build"), "{text}");
    assert!(
        text.contains("b8603c8f-6095-4bb5-8b37-12e4fafbfa5b"),
        "{text}"
    );
    assert!(text.contains("/repo"), "{text}");
    assert!(text.contains("2026-11-05"), "{text}");
    assert!(text.contains("Enter resume"), "{text}");

    // A discovery failure replaces the list with the reason, never with the
    // "nothing found" copy.
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ForeignSessionList {
            tool: UiForeignTool::ClaudeCode,
            sessions: Vec::new(),
            error: Some("permission denied".into()),
        }),
    );
    let text = render_text(&mut s, 110, 30);
    assert!(text.contains("permission denied"), "{text}");
    assert!(!text.contains("No resumable sessions"), "{text}");
}
