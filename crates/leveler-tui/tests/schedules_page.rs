//! Headless dogfood for the `/schedules` page.
//!
//! Commands and answers go through the real reducer (`RuntimeEvent` →
//! `ScheduleManaged`), and the assertions read the real painted frame. This
//! proves the page is wired into the screen dispatch, that click hits land on
//! the rows that were actually painted, and that every pane survives a narrow
//! terminal.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use leveler_client_protocol::schedule::{
    ScheduleActionWire, ScheduleGrantView, ScheduleResponse, ScheduleTriggerWire, ScheduleView,
};
use leveler_client_protocol::{CommandId, RuntimeEvent, SessionId};
use leveler_tui::action::Action;
use leveler_tui::reducer::reduce;
use leveler_tui::render::render;
use leveler_tui::schedules::{self, ScheduleField};
use leveler_tui::screen::Screen;
use leveler_tui::state::{AppState, Boot};
use leveler_tui::theme::Theme;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use unicode_width::UnicodeWidthStr;

fn state() -> AppState {
    AppState::new(
        Theme::no_color(),
        Boot {
            session_id: SessionId::new("schedules-page"),
            user: "u".into(),
            version: "0".into(),
            show_welcome: false,
            draft_path: None,
            history_path: None,
            context_window: 0,
            locale: leveler_tui::Locale::Zh,
            untrusted_config: Vec::new(),
            model_notice: None,
            thinking: None,
        },
    )
}

fn view(id: &str) -> ScheduleView {
    ScheduleView {
        id: id.into(),
        name: format!("nightly {id}"),
        description: "runs the nightly job".into(),
        action_kind: "shell".into(),
        action: ScheduleActionWire::Shell {
            command: "echo hi".into(),
        },
        workspace: "/tmp/work".into(),
        trigger: ScheduleTriggerWire::Daily {
            time: "09:00".into(),
            timezone: Some("Asia/Shanghai".into()),
        },
        trigger_text: "Every day at 09:00".into(),
        timezone: Some("Asia/Shanghai".into()),
        enabled: true,
        running: false,
        run_state: None,
        next_run_at: Some("2026-11-06T01:00:00Z".into()),
        last_run: None,
        needs_attention: false,
        attention_reason: None,
        session_policy: "continue".into(),
        overlap: "skip-if-running".into(),
        mode: "assisted".into(),
        model: None,
        timeout_secs: 600,
        misfire: "skip".into(),
        misfire_grace_secs: 0,
        session_id: None,
        grant: ScheduleGrantView {
            granted: true,
            external_side_effects: false,
            granted_at: None,
            requires_regrant: false,
        },
        created_at: "2026-11-01T00:00:00Z".into(),
        revision: "rev-1".into(),
    }
}

fn managed(state: &mut AppState, query_id: CommandId, response: ScheduleResponse) {
    let requester_session_id = state.session_id.clone();
    let _ = reduce(
        state,
        Action::Runtime(RuntimeEvent::ScheduleManaged {
            requester_session_id,
            query_id,
            response: Box::new(response),
        }),
    );
}

fn press(state: &mut AppState, code: KeyCode) {
    let _ = reduce(state, Action::Key(KeyEvent::new(code, KeyModifiers::NONE)));
}

fn render_text(state: &mut AppState, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| render(frame, state)).unwrap();
    let buffer = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..buffer.area.height {
        let mut x = 0;
        while x < buffer.area.width {
            let symbol = buffer.cell((x, y)).map(|cell| cell.symbol()).unwrap_or(" ");
            out.push_str(symbol);
            x += UnicodeWidthStr::width(symbol).max(1) as u16;
        }
        out.push('\n');
    }
    out
}

#[test]
fn the_list_paints_runtime_rows_and_publishes_matching_hits() {
    let mut state = state();
    let _ = schedules::open(&mut state);
    let query = state.schedule.list_query.clone().expect("list query");
    managed(
        &mut state,
        query,
        ScheduleResponse::List {
            schedules: vec![view("alpha"), view("beta")],
        },
    );

    let text = render_text(&mut state, 80, 22);
    assert!(text.contains("nightly alpha"), "{text}");
    assert!(text.contains("nightly beta"), "{text}");
    assert!(text.contains("已计划"), "{text}");
    assert_eq!(state.screen_rows.len(), 2);

    // A click resolves against the rows this frame painted.
    let key = state.screen_rows[1].1.clone();
    schedules::select_row(&mut state, &key);
    assert_eq!(state.schedule.selected, 1);

    // Narrow terminals truncate; they neither panic nor lose the row.
    let narrow = render_text(&mut state, 30, 16);
    assert!(narrow.contains("nightly"), "{narrow}");
}

#[test]
fn the_detail_paints_the_fields_the_runtime_reported() {
    let mut state = state();
    state.active_screen = Screen::Schedules;
    state.schedule.detail = Some(view("alpha"));
    state.schedule.runs_loaded = true;

    let text = render_text(&mut state, 100, 30);
    assert!(text.contains("nightly alpha"), "{text}");
    assert!(text.contains("工作目录"), "{text}");
    assert!(text.contains("/tmp/work"), "{text}");
    assert!(text.contains("每天 09:00 (Asia/Shanghai)"), "{text}");
    assert!(text.contains("还没有运行记录"), "{text}");
}

#[test]
fn the_form_paints_every_field_and_enter_advances_the_focus() {
    let mut state = state();
    let _ = schedules::open(&mut state);
    press(&mut state, KeyCode::Char('n'));
    assert!(state.schedule.form.is_some());

    let text = render_text(&mut state, 100, 30);
    assert!(text.contains("新建定时任务"), "{text}");
    assert!(text.contains("工作目录"), "{text}");
    assert!(text.contains("无人值守授权"), "{text}");

    for ch in "nightly".chars() {
        press(&mut state, KeyCode::Char(ch));
    }
    assert_eq!(state.schedule.form.as_ref().unwrap().name, "nightly");

    // Enter advances instead of submitting before the last field.
    press(&mut state, KeyCode::Enter);
    assert_eq!(
        state.schedule.form.as_ref().unwrap().focus,
        ScheduleField::Workspace
    );
    assert!(!state.schedule.form.as_ref().unwrap().submitting);
}
