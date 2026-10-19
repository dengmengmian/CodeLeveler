//! User prompt identity: one left-aligned card + one `❯` marker for real user
//! input, and nothing else.
//!
//! These paint the FULL workbench and read the cells the terminal receives.
//! The marker and card are paint-time decoration, so the assertions are made
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

/// The card width every prompt must share at this content width: 96% of the
/// reading area, rounded up, clamped to the content rect.
fn expected_card_width(state: &AppState) -> u16 {
    let (_, _, width, _) = content_of(state);
    let target = (width as usize * 96).div_ceil(100);
    let floor = 4.min(width as usize);
    target.max(floor).min(width as usize) as u16
}

/// The painted card's left/right columns on one screen row. `None` when the
/// row is canvas.
fn card_span(state: &AppState, buffer: &Buffer, y: u16) -> Option<(u16, u16)> {
    let (x, _, width, _) = content_of(state);
    let band = state.theme.surface.prompt;
    let mut first = None;
    let mut last = None;
    for col in x..x + width {
        if buffer[(col, y)].bg == band {
            first.get_or_insert(col);
            last = Some(col);
        }
    }
    first.zip(last)
}

/// Assert one prompt card row: the card starts at the content's left edge,
/// its cells are contiguous, and every column outside it stays on the canvas.
fn assert_card_at(state: &AppState, buffer: &Buffer, y: u16) -> (u16, u16) {
    let (x, _, width, _) = content_of(state);
    let band = state.theme.surface.prompt;
    let canvas = state.theme.surface.canvas;
    let span = card_span(state, buffer, y)
        .unwrap_or_else(|| panic!("row {y} carries no prompt card: {:?}", row_text(buffer, y)));
    assert_eq!(span.0, x, "the card is left-aligned at the content edge");
    for col in span.0..=span.1 {
        if covered_by_wide(buffer, col, y) {
            continue;
        }
        assert_eq!(
            buffer[(col, y)].bg,
            band,
            "row {y} column {col} must carry the card: {:?}",
            row_text(buffer, y)
        );
    }
    for col in (x..x + width).filter(|col| *col < span.0 || *col > span.1) {
        assert_eq!(
            buffer[(col, y)].bg,
            canvas,
            "row {y} column {col} outside the card must stay on the canvas: {:?}",
            row_text(buffer, y)
        );
    }
    span
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

/// The rows of the card the prompt is painted on, top to bottom.
fn card_rows(state: &AppState, buffer: &Buffer) -> Vec<u16> {
    let (_, y, _, height) = content_of(state);
    (y..y + height)
        .filter(|row| card_span(state, buffer, *row).is_some())
        .collect()
}

/// Resume (the snapshot path) paints the card on real user input only:
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

    assert_card_at(&s, &buffer, user_row);
    assert_canvas_at(&s, &buffer, summary_row);
    assert_canvas_at(&s, &buffer, answer_row);
    assert_canvas_at(&s, &buffer, notice_row);

    let marks = markers(&buffer);
    assert_eq!(marks.len(), 1, "only the real prompt is marked: {marks:?}");
    let (x, _, _, _) = content_of(&s);
    assert_eq!(
        marks[0].0,
        x + 1,
        "the marker is padded one cell inside the card edge"
    );
}

/// Two user turns keep separate cards; the agent prose between them stays on
/// the canvas. A user's prompt never absorbs the adjacent answer.
#[test]
fn two_turns_keep_separate_cards_and_no_agent_row_inherits_one() {
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
        assert_card_at(&s, &buffer, u);
        assert_canvas_at(&s, &buffer, a);
        // Every row between the prompt text and the answer text is either the
        // prompt's own padding (banded) or the inter-item gap (canvas); no
        // answer text starts on a banded row.
        assert!(a > u);
    }
    assert_eq!(markers(&buffer).len(), 2, "one marker per user turn");
}

/// The live path (a newly submitted message) paints the same card as resumed
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
    let live_span = assert_card_at(&live, &live_buffer, live_row);
    let resumed_span = assert_card_at(&resumed, &resumed_buffer, resumed_row);
    assert_eq!(live_span, resumed_span, "live and resumed cards must match");
    assert_eq!(
        row_text(&live_buffer, live_row),
        row_text(&resumed_buffer, resumed_row),
        "live and resumed rows must paint identically"
    );
}

/// A multiline prompt keeps one card through its blank line and wraps, and the
/// full text stays readable at the acceptance widths.
#[test]
fn a_multiline_prompt_keeps_one_card_at_every_width() {
    let text =
        "第一行中文 😀\n\n第三行 with English\nhttps://example.com/a/very/long/path/that/wraps";
    let mut s = opened(vec![message("u1", UiRole::User, text)]);
    for (width, height) in [(120u16, 40u16), (80, 24), (48, 20)] {
        let buffer = paint(&mut s, width, height);
        let (x, _, _, _) = content_of(&s);
        let expected = expected_card_width(&s);
        let first = row_of(&buffer, "第一行中文");
        let blank = first + 1;
        let third = row_of(&buffer, "第三行 with English");
        assert!(blank < third, "the explicit blank line is preserved");
        // The whole card, including its vertical padding rows, is banded.
        let mut span = None;
        for y in first - 1..=third {
            let row_span = assert_card_at(&s, &buffer, y);
            if let Some(expected) = span {
                assert_eq!(row_span, expected, "one card width on every row");
            } else {
                span = Some(row_span);
            }
        }
        let span = span.expect("card rows");
        assert_eq!(
            span.1 - span.0 + 1,
            expected,
            "{width}x{height}: every prompt card shares one width"
        );
        // The card is the content's left block, never the workspace gutter.
        assert_eq!(span.0, x);
        assert_ne!(buffer[(x - 1, first)].bg, s.theme.surface.prompt);
        let marks = markers(&buffer);
        assert_eq!(marks.len(), 1, "{width}x{height}");
        assert_eq!(marks[0], (x + 1, first));
        let row = row_text(&buffer, first);
        assert!(row.contains('❯'), "{row}");
        assert!(row.contains("第一行中文"), "text must stay readable: {row}");
    }
}

/// A short message uses the same card as a long one: the card is the
/// message's container, never a text-sized bubble.
#[test]
fn a_short_prompt_fills_the_same_card_as_a_long_one() {
    let mut s = opened(vec![
        message("u1", UiRole::User, "hi"),
        message("a1", UiRole::Assistant, "短答"),
        message("u2", UiRole::User, &format!("long {}", "word ".repeat(40))),
    ]);
    let buffer = paint(&mut s, 100, 40);
    let expected = expected_card_width(&s);
    let short_y = row_of(&buffer, "❯ hi");
    let long_y = row_of(&buffer, "long word");
    let (short_left, short_right) = assert_card_at(&s, &buffer, short_y);
    let (long_left, long_right) = assert_card_at(&s, &buffer, long_y);
    assert_eq!(
        short_left, long_left,
        "both cards start at the content edge"
    );
    assert_eq!(
        short_right - short_left + 1,
        expected,
        "a short prompt takes the full card"
    );
    assert_eq!(
        long_right - long_left + 1,
        expected,
        "a long prompt takes the same card"
    );
    assert_eq!(
        row_text(&buffer, short_y).trim(),
        "❯ hi",
        "no identity text and no extra decoration"
    );
}

/// A long unbroken message fills the same card and every character survives.
#[test]
fn a_long_prompt_fills_the_card_without_truncation() {
    let text = "Z".repeat(200);
    let mut s = opened(vec![message("u1", UiRole::User, &text)]);
    let buffer = paint(&mut s, 120, 40);
    let expected = expected_card_width(&s);
    let rows = card_rows(&s, &buffer);
    assert!(rows.len() >= 3, "the message must wrap: {rows:?}");
    let mut painted = 0usize;
    for &y in &rows {
        let (left, right) = card_span(&s, &buffer, y).expect("card");
        assert_eq!(
            right - left + 1,
            expected,
            "row {y}: every prompt card shares one width"
        );
        painted += row_text(&buffer, y).chars().filter(|c| *c == 'Z').count();
    }
    assert_eq!(painted, text.len(), "no character may be dropped");
    assert_eq!(
        row_text(&buffer, rows[0]).trim(),
        "",
        "the card's top padding row carries no text"
    );
    assert_eq!(
        markers(&buffer).len(),
        1,
        "one marker per prompt, on the first body row"
    );
    assert_eq!(markers(&buffer)[0].1, rows[1]);
}

/// At the narrow acceptance width, mixed-width text stays complete and the
/// card keeps its single fixed width.
#[test]
fn a_narrow_terminal_keeps_mixed_width_text_complete() {
    let tail = "Z".repeat(90);
    let text = format!("中文 English 😀 {tail}");
    let mut s = opened(vec![message("u1", UiRole::User, &text)]);
    let buffer = paint(&mut s, 48, 20);
    let expected = expected_card_width(&s);
    let rows = card_rows(&s, &buffer);
    assert!(!rows.is_empty(), "the prompt must be visible");
    let mut painted = String::new();
    for &y in &rows {
        let (left, right) = card_span(&s, &buffer, y).expect("card");
        assert_eq!(
            right - left + 1,
            expected,
            "row {y}: every prompt card shares one width"
        );
        painted.push_str(&row_text(&buffer, y));
    }
    assert_eq!(
        painted.matches('Z').count(),
        tail.len(),
        "the long unspaced run must not be truncated"
    );
    for needle in ["中文", "English", "😀"] {
        assert!(painted.contains(needle), "missing {needle:?}: {painted:?}");
    }
}

/// The `❯` is the prompt's only decoration: soft accent ink, padded inside
/// the card, with no identity label, border or divider.
#[test]
fn the_marker_is_soft_blue_and_carries_no_identity_text() {
    let mut s = opened(vec![message("u1", UiRole::User, "hello there")]);
    let buffer = paint(&mut s, 100, 40);
    let (x, _, _, _) = content_of(&s);
    let y = row_of(&buffer, "❯");
    let (left, right) = assert_card_at(&s, &buffer, y);
    assert_eq!(left, x);
    let marker = &buffer[(left + 1, y)];
    assert_eq!(marker.symbol(), "❯");
    assert_eq!(
        marker.fg, s.theme.accent.secondary,
        "the marker wears the soft accent ink"
    );
    assert_eq!(marker.bg, s.theme.surface.prompt, "inside the card");
    assert_eq!(
        buffer[(left + 2, y)].symbol(),
        " ",
        "one blank column between the marker and the body"
    );
    assert_eq!(buffer[(left + 3, y)].symbol(), "h");
    for (screen_row, text) in [(y - 1, ""), (y, "❯ hello there")] {
        assert_eq!(row_text(&buffer, screen_row).trim(), text);
    }
    let card_text = format!("{}{}", row_text(&buffer, y - 1), row_text(&buffer, y));
    for label in ["You", "User", "你", "用户"] {
        assert!(
            !card_text.contains(label),
            "identity label {label:?} leaked"
        );
    }
    for border in ["─", "━", "│", "┃", "┌", "┐", "└", "┘"] {
        assert!(
            !card_text.contains(border),
            "card must not draw a border/divider: {card_text:?}"
        );
    }
    assert_eq!(
        right - left + 1,
        expected_card_width(&s),
        "the card keeps its fixed width for a short message"
    );
}

/// Resizing cannot leave paint from the previous width behind: each size
/// repaints a card that fits its own content rect.
#[test]
fn resize_repaints_the_card_to_the_new_content_rect() {
    let mut s = opened(vec![message("u1", UiRole::User, "宽度变化")]);
    for (width, height) in [(120u16, 40u16), (80, 24), (48, 20), (120, 40)] {
        let buffer = paint(&mut s, width, height);
        let (x, _, w, _) = content_of(&s);
        let gutter = leveler_tui::conversation::geometry::GUTTER_X;
        assert_eq!(x, gutter, "left edge follows the workspace gutter");
        assert_eq!(x + w, width - gutter, "and so does the right edge");
        let row = row_of(&buffer, "宽度变化");
        let (left, right) = assert_card_at(&s, &buffer, row);
        assert_eq!(left, x);
        assert_eq!(
            right - left + 1,
            expected_card_width(&s),
            "the card is 95-100% of this content width"
        );
        assert!(
            (right - left + 1) as usize <= w as usize,
            "the card never overflows the content"
        );
        assert_eq!(markers(&buffer), vec![(x + 1, row)]);
    }
}
