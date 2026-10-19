//! User prompt identity: one continuous band + one `❯` marker for real user
//! input, and nothing else.
//!
//! These paint the FULL workbench and read the cells the terminal receives.
//! The marker and band are paint-time decoration, so the assertions are made
//! against the painted buffer, never against the projection text.

use leveler_client_protocol::{
    COMPACTION_SUMMARY_PREFIX, MessageId, ModelRef, PermissionProfile, RuntimeEvent, SessionId,
    UiMessage, UiMessageKind, UiRole, UiSessionSnapshot,
};
use leveler_tui::action::Action;
use leveler_tui::reducer::reduce;
use leveler_tui::render::render;
use leveler_tui::state::{AppState, Boot};
use leveler_tui::theme::Theme;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;

fn state() -> AppState {
    AppState::new(
        Theme::dark(),
        Boot {
            session_id: SessionId::new("s1"),
            user: "麻凡".into(),
            version: "0.1.0".into(),
            show_welcome: false,
            draft_path: None,
            history_path: None,
            context_window: 200_000,
            locale: leveler_tui::Locale::Zh,
            untrusted_config: Vec::new(),
            model_notice: None,
            thinking: None,
        },
    )
}

fn message(id: &str, role: UiRole, text: &str) -> UiMessage {
    UiMessage {
        id: MessageId::new(id),
        role,
        text: text.into(),
        ordinal: None,
        kind: None,
        images: 0,
    }
}

fn snapshot(messages: Vec<UiMessage>) -> UiSessionSnapshot {
    UiSessionSnapshot {
        id: SessionId::new("s1"),
        repository: Some("~/repo".into()),
        task_status: None,
        task_terminal: None,
        goal: "interactive session".into(),
        model: ModelRef::parse("deepseek/v3"),
        mode: PermissionProfile::Assisted,
        branch: Some("main".into()),
        status: "idle".into(),
        finalization_stage: None,
        messages,
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
    }
}

fn opened(messages: Vec<UiMessage>) -> AppState {
    let mut s = state();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot(messages),
        }),
    );
    s
}

fn paint(state: &mut AppState, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| render(frame, state)).unwrap();
    terminal.backend().buffer().clone()
}

fn row_text(buffer: &Buffer, y: u16) -> String {
    let mut out = String::new();
    let mut x = 0u16;
    while x < buffer.area.width {
        let symbol = buffer[(x, y)].symbol();
        out.push_str(symbol);
        x += unicode_width::UnicodeWidthStr::width(symbol).max(1) as u16;
    }
    out.trim_end().to_string()
}

fn row_of(buffer: &Buffer, needle: &str) -> u16 {
    for y in 0..buffer.area.height {
        if row_text(buffer, y).contains(needle) {
            return y;
        }
    }
    let all: Vec<String> = (0..buffer.area.height)
        .map(|y| row_text(buffer, y))
        .collect();
    panic!("no screen row contains {needle:?}:\n{}", all.join("\n"));
}

/// The second cell of a wide glyph is covered by that glyph; the terminal
/// paints both columns with the glyph's style, so its stored style is never
/// visible.
fn covered_by_wide(buffer: &Buffer, x: u16, y: u16) -> bool {
    x > 0 && unicode_width::UnicodeWidthStr::width(buffer[(x - 1, y)].symbol()) > 1
}

fn content_of(state: &AppState) -> (u16, u16, u16, u16) {
    state.conv.rect.expect("conversation rect published")
}

fn assert_band_at(state: &AppState, buffer: &Buffer, y: u16) {
    let (x, _, width, _) = content_of(state);
    let band = state.theme.surface.elevated;
    for col in x..x + width {
        if covered_by_wide(buffer, col, y) {
            continue;
        }
        assert_eq!(
            buffer[(col, y)].bg,
            band,
            "row {y} column {col} must carry the prompt band: {:?}",
            row_text(buffer, y)
        );
    }
}

fn assert_canvas_at(state: &AppState, buffer: &Buffer, y: u16) {
    let (x, _, width, _) = content_of(state);
    let canvas = state.theme.surface.canvas;
    for col in x..x + width {
        if covered_by_wide(buffer, col, y) {
            continue;
        }
        assert_eq!(
            buffer[(col, y)].bg,
            canvas,
            "row {y} column {col} must stay on the canvas: {:?}",
            row_text(buffer, y)
        );
    }
}

fn markers(buffer: &Buffer) -> Vec<(u16, u16)> {
    (0..buffer.area.height)
        .flat_map(|y| (0..buffer.area.width).map(move |x| (x, y)))
        .filter(|(x, y)| buffer[(*x, *y)].symbol() == "❯")
        .collect()
}

/// Resume (the snapshot path) paints the band on real user input only:
/// a compaction summary and a runtime notice ride `UiRole::User` but are not
/// user input, and neither is assistant prose.
#[test]
fn resumed_history_bands_only_real_user_input() {
    let mut notice = message("n1", UiRole::User, "Runtime notice body");
    notice.kind = Some(UiMessageKind::RuntimeNotice);
    let mut s = opened(vec![
        message("u1", UiRole::User, "真实用户输入"),
        message(
            "c1",
            UiRole::User,
            &format!("{COMPACTION_SUMMARY_PREFIX}\n摘要正文"),
        ),
        notice,
        message("a1", UiRole::Assistant, "Agent 回答正文"),
    ]);
    let buffer = paint(&mut s, 100, 40);

    let user_row = row_of(&buffer, "真实用户输入");
    let summary_row = row_of(&buffer, "摘要正文");
    let answer_row = row_of(&buffer, "Agent 回答正文");
    let notice_row = row_of(&buffer, "Runtime notice body");

    assert_band_at(&s, &buffer, user_row);
    assert_canvas_at(&s, &buffer, summary_row);
    assert_canvas_at(&s, &buffer, answer_row);
    assert_canvas_at(&s, &buffer, notice_row);

    let marks = markers(&buffer);
    assert_eq!(marks.len(), 1, "only the real prompt is marked: {marks:?}");
    let (x, _, _, _) = content_of(&s);
    assert_eq!(marks[0].0, x, "the marker sits in the prompt gutter");
}

/// Two user turns keep separate bands; the agent prose between them stays on
/// the canvas. A user's prompt never absorbs the adjacent answer.
#[test]
fn two_turns_keep_separate_bands_and_no_agent_row_inherits_one() {
    let mut s = opened(vec![
        message("u1", UiRole::User, "第一轮用户输入"),
        message("a1", UiRole::Assistant, "第一轮回答"),
        message("u2", UiRole::User, "第二轮用户输入"),
        message("a2", UiRole::Assistant, "第二轮回答"),
    ]);
    let buffer = paint(&mut s, 100, 40);
    for (user, answer) in [
        ("第一轮用户输入", "第一轮回答"),
        ("第二轮用户输入", "第二轮回答"),
    ] {
        let u = row_of(&buffer, user);
        let a = row_of(&buffer, answer);
        assert_band_at(&s, &buffer, u);
        assert_canvas_at(&s, &buffer, a);
        // Every row between the prompt text and the answer text is either the
        // prompt's own padding (banded) or the inter-item gap (canvas); no
        // answer text starts on a banded row.
        assert!(a > u);
    }
    assert_eq!(markers(&buffer).len(), 2, "one marker per user turn");
}

/// The live path (a newly submitted message) paints the same band as resumed
/// history, so the style cannot be lost across a reconnect.
#[test]
fn a_live_prompt_matches_resumed_history() {
    let mut live = state();
    reduce(
        &mut live,
        Action::Runtime(RuntimeEvent::UserMessageAdded {
            message: message("u1", UiRole::User, "同一句话"),
        }),
    );
    let mut resumed = opened(vec![message("u1", UiRole::User, "同一句话")]);
    let live_buffer = paint(&mut live, 100, 40);
    let resumed_buffer = paint(&mut resumed, 100, 40);
    let live_row = row_of(&live_buffer, "同一句话");
    let resumed_row = row_of(&resumed_buffer, "同一句话");
    assert_band_at(&live, &live_buffer, live_row);
    assert_band_at(&resumed, &resumed_buffer, resumed_row);
    assert_eq!(
        row_text(&live_buffer, live_row),
        row_text(&resumed_buffer, resumed_row),
        "live and resumed rows must paint identically"
    );
}

/// A multiline prompt keeps one band through its blank line and wraps, and the
/// full text stays readable at the acceptance widths.
#[test]
fn a_multiline_prompt_keeps_one_band_at_every_width() {
    let text =
        "第一行中文 😀\n\n第三行 with English\nhttps://example.com/a/very/long/path/that/wraps";
    let mut s = opened(vec![message("u1", UiRole::User, text)]);
    for (width, height) in [(120u16, 40u16), (80, 24), (48, 20)] {
        let buffer = paint(&mut s, width, height);
        let (x, _, w, _) = content_of(&s);
        let first = row_of(&buffer, "第一行中文");
        let blank = first + 1;
        let third = row_of(&buffer, "第三行 with English");
        assert!(blank < third, "the explicit blank line is preserved");
        for y in first..=third {
            assert_band_at(&s, &buffer, y);
        }
        // The band is the content rect, never the workspace gutter.
        assert_ne!(buffer[(x - 1, first)].bg, s.theme.surface.elevated);
        assert_ne!(
            buffer[(x + w - 1, first)].bg,
            Theme::dark().surface.canvas,
            "the band reaches the content's right edge"
        );
        let marks = markers(&buffer);
        assert_eq!(marks.len(), 1, "{width}x{height}");
        assert_eq!(marks[0], (x, first));
        let row = row_text(&buffer, first);
        assert!(row.contains('❯'), "{row}");
        assert!(row.contains("第一行中文"), "text must stay readable: {row}");
    }
}

/// Resizing cannot leave paint from the previous width behind: each size
/// repaints a band that exactly matches its own content rect.
#[test]
fn resize_repaints_the_band_to_the_new_content_rect() {
    let mut s = opened(vec![message("u1", UiRole::User, "宽度变化")]);
    for (width, height) in [(120u16, 40u16), (80, 24), (48, 20), (120, 40)] {
        let buffer = paint(&mut s, width, height);
        let (x, _, w, _) = content_of(&s);
        let gutter = leveler_tui::conversation::geometry::GUTTER_X;
        assert_eq!(x, gutter, "left edge follows the workspace gutter");
        assert_eq!(x + w, width - gutter, "and so does the right edge");
        let row = row_of(&buffer, "宽度变化");
        assert_band_at(&s, &buffer, row);
        assert_eq!(markers(&buffer), vec![(x, row)]);
    }
}
