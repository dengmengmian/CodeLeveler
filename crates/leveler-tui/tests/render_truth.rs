//! Render-truth regression suite for the production frame.
//!
//! These tests drive the real reducer and the real `render` entry point on a
//! `TestBackend`; they compare the painted cell grid, not in-memory layout
//! values. The terminal-level counterpart (tab stops, byte stream, full-screen
//! equivalence on a real PTY) lives in `scripts/tui_render_truth.py`, because
//! a cell buffer cannot model how a terminal expands a control byte.
//!
//! Contract under test: for the same logical transcript, the screen after an
//! incremental run must equal the screen after a single full paint (dynamic
//! chrome rows normalised), and no painted cell may hold a control character.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use leveler_client_protocol::{MessageId, RuntimeEvent, SessionId, ToolCallId};
use leveler_tui::action::Action;
use leveler_tui::composer::Composer;
use leveler_tui::conversation::sync_scroll;
use leveler_tui::reducer::reduce;
use leveler_tui::render::render;
use leveler_tui::state::{AppState, Boot};
use leveler_tui::theme::Theme;
use leveler_tui::transcript::{AssistantBlock, RecapBlock, TranscriptItem};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use unicode_width::UnicodeWidthStr;

const ANSWER: &str = include_str!("../../../testdata/tui_render_truth/streaming_answer.md");
const SPINNERS: &str = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏";

fn boot(width: u16, height: u16) -> AppState {
    let mut state = AppState::new(
        Theme::no_color(),
        Boot {
            session_id: SessionId::new("render-truth"),
            user: "tester".into(),
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
    );
    state.size = (width, height);
    state.transcript.push_user("现在什么情况".into());
    state
}

fn start_answer(state: &mut AppState) {
    reduce(
        state,
        Action::Runtime(RuntimeEvent::AssistantMessageStarted {
            message_id: MessageId::new("m0"),
        }),
    );
}

fn stream(state: &mut AppState, deltas: &[String]) {
    for delta in deltas {
        reduce(
            state,
            Action::Runtime(RuntimeEvent::AssistantTextDelta {
                message_id: MessageId::new("m0"),
                delta: delta.clone(),
            }),
        );
    }
}

fn complete(state: &mut AppState) {
    reduce(
        state,
        Action::Runtime(RuntimeEvent::AssistantMessageCompleted {
            message_id: MessageId::new("m0"),
        }),
    );
}

fn chunks(text: &str, size: usize) -> Vec<String> {
    text.chars()
        .collect::<Vec<_>>()
        .chunks(size)
        .map(|c| c.iter().collect())
        .collect()
}

/// Deterministic pseudo-random chunking; the seed is recorded in the test name
/// when a case fails, so a failure is reproducible.
fn seeded_chunks(text: &str, seed: u64) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut x = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
    let mut i = 0;
    while i < chars.len() {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let n = ((x >> 33) % 9 + 1) as usize;
        let end = (i + n).min(chars.len());
        out.push(chars[i..end].iter().collect());
        i = end;
    }
    out
}

fn assistant_mut(state: &mut AppState) -> &mut AssistantBlock {
    state
        .transcript
        .items_mut()
        .iter_mut()
        .find_map(|item| match item {
            TranscriptItem::Assistant(block) => Some(block),
            _ => None,
        })
        .expect("an assistant block")
}

struct Painted {
    state: AppState,
    term: Terminal<TestBackend>,
}

impl Painted {
    fn new(state: AppState, width: u16, height: u16) -> Self {
        Self {
            state,
            term: Terminal::new(TestBackend::new(width, height)).unwrap(),
        }
    }

    fn draw(&mut self) -> Vec<String> {
        sync_scroll(&mut self.state);
        let state = &mut self.state;
        self.term.draw(|frame| render(frame, state)).unwrap();
        self.rows()
    }

    fn resize(&mut self, width: u16, height: u16) {
        reduce(&mut self.state, Action::Resize(width, height));
        self.term.backend_mut().resize(width, height);
    }

    fn rows(&self) -> Vec<String> {
        let area = self.term.backend().buffer().area;
        rect_rows(self.term.backend().buffer(), area)
    }

    fn viewport_rect(&self) -> Rect {
        let (x, y, w, h) = self
            .state
            .conv
            .rect
            .expect("the renderer published its rect");
        Rect::new(x, y, w, h)
    }

    fn viewport(&self) -> Vec<String> {
        rect_rows(self.term.backend().buffer(), self.viewport_rect())
    }

    fn viewport_styled(&self) -> Vec<Vec<(String, ratatui::style::Style)>> {
        rect_cells(self.term.backend().buffer(), self.viewport_rect())
    }
}

fn rect_rows(buf: &Buffer, area: Rect) -> Vec<String> {
    rect_cells(buf, area)
        .into_iter()
        .map(|row| {
            let mut s = String::new();
            for (symbol, _) in row {
                s.push_str(&symbol);
            }
            s.trim_end().to_string()
        })
        .collect()
}

fn rect_cells(buf: &Buffer, area: Rect) -> Vec<Vec<(String, ratatui::style::Style)>> {
    (area.y..area.y + area.height)
        .map(|y| {
            let mut row = Vec::new();
            let mut x = area.x;
            while x < area.x + area.width {
                let cell = buf.cell((x, y)).expect("cell inside the buffer");
                row.push((cell.symbol().to_string(), cell.style()));
                x += UnicodeWidthStr::width(cell.symbol()).max(1) as u16;
            }
            row
        })
        .collect()
}

/// Normalise only explicitly dynamic chrome rows: the waiting status (elapsed
/// seconds, live token estimate), the footer (clock, usage meter) and the
/// header (build/repo line). Everything else — conversation, tools, composer,
/// borders, blank rows — compares cell-for-cell.
fn normalize_chrome(rows: &[String]) -> Vec<String> {
    rows.iter()
        .map(|row| {
            let dynamic = row.contains("等待模型")
                || row.contains("就绪")
                || row.contains("Esc 打断")
                || row.contains("CodeLeveler v");
            if !dynamic {
                return row.clone();
            }
            row.chars()
                .map(|c| {
                    if c.is_ascii_digit() || SPINNERS.contains(c) {
                        '#'
                    } else {
                        c
                    }
                })
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

fn assert_no_control_cells(screen: &[String], context: &str) {
    for (y, row) in screen.iter().enumerate() {
        for ch in row.chars() {
            assert!(
                !ch.is_control(),
                "{context}: row {y} carries a control cell {ch:?}: {row:?}"
            );
        }
    }
}

fn painted_with_prefix(width: u16, height: u16, prefix_len: usize) -> Painted {
    let text: String = ANSWER.chars().take(prefix_len).collect();
    let mut state = boot(width, height);
    start_answer(&mut state);
    stream(&mut state, &chunks(&text, 4));
    Painted::new(state, width, height)
}

// ---------------------------------------------------------------------------
// R1: content fidelity. What is on screen is the projection of the received
// prefix and nothing else.
// ---------------------------------------------------------------------------

#[test]
fn the_reported_streaming_answer_paints_exact_import_rows_and_no_control_cells() {
    for width in [60u16, 80, 96, 116, 120] {
        let mut p = painted_with_prefix(width, 40, 1420);
        let screen = p.draw();
        assert_no_control_cells(&screen, &format!("width {width}"));
        let joined = screen.join("\n");
        // The viewport rect starts after the terminal gutter, so the painted
        // row is the code block's 2-space gutter plus the tab's expansion to
        // the next 8-column stop.
        let viewport = p.viewport();
        for (source, expected) in [
            ("\t\"fmt\"", "\"fmt\""),
            ("\t\"os\"", "\"os\""),
            (
                "\t\"golang.org/x/crypto/bcrypt\"",
                "\"golang.org/x/crypto/bcrypt\"",
            ),
        ] {
            assert!(
                ANSWER.contains(source),
                "the fixture still contains the tab-indented import {source:?}"
            );
            let expected_row = format!("          {}", expected);
            assert!(
                viewport.iter().any(|row| row.starts_with(&expected_row)),
                "width {width}: no viewport row starts with {expected_row:?}; screen:\n{joined}"
            );
        }
        // A row that mixes two source lines is the reported artifact.
        for broken in ["impo\"fmt\"", "paos\" mt\"", "func:= bcrypt"] {
            assert!(
                !joined.contains(broken),
                "width {width}: mixed row {broken:?} on screen:\n{joined}"
            );
        }
    }
}

#[test]
fn no_prefix_of_the_reported_answer_ever_produces_a_control_cell() {
    let total = ANSWER.chars().count();
    let mut offset = 0;
    while offset <= total {
        let text: String = ANSWER.chars().take(offset).collect();
        let mut state = boot(100, 36);
        start_answer(&mut state);
        stream(&mut state, &chunks(&text, 37));
        let mut p = Painted::new(state, 100, 36);
        let screen = p.draw();
        assert_no_control_cells(&screen, &format!("prefix {offset}"));
        assert!(
            !screen.join("\n").contains("▌\u{0}"),
            "prefix {offset}: live marker followed by a control byte"
        );
        offset += 37;
    }
}

// ---------------------------------------------------------------------------
// R2/R6: incremental rendering equals a full paint of the same content, and
// chunk boundaries do not change the screen.
// ---------------------------------------------------------------------------

#[test]
fn incremental_stream_matches_a_single_delta_paint_mid_stream() {
    let offsets = [320usize, 900, 1220, 1600, 2200, ANSWER.chars().count()];
    for offset in offsets {
        let text: String = ANSWER.chars().take(offset).collect();
        let mut increment = boot(120, 40);
        start_answer(&mut increment);
        stream(&mut increment, &chunks(&text, 3));
        let mut increment = Painted::new(increment, 120, 40);
        let incremental = normalize_chrome(&increment.draw());

        let mut single = boot(120, 40);
        start_answer(&mut single);
        stream(&mut single, std::slice::from_ref(&text));
        let mut single = Painted::new(single, 120, 40);
        let one_shot = normalize_chrome(&single.draw());

        assert_eq!(
            incremental, one_shot,
            "offset {offset}: incremental screen differs from a full paint"
        );
    }
}

#[test]
fn final_screen_is_chunk_invariant_across_recorded_seeds() {
    let mut reference = boot(100, 36);
    start_answer(&mut reference);
    stream(&mut reference, &[ANSWER.to_string()]);
    complete(&mut reference);
    let mut reference = Painted::new(reference, 100, 36);
    let expected = normalize_chrome(&reference.draw());

    for seed in 0..20u64 {
        let mut state = boot(100, 36);
        start_answer(&mut state);
        stream(&mut state, &seeded_chunks(ANSWER, seed));
        complete(&mut state);
        let mut p = Painted::new(state, 100, 36);
        let screen = normalize_chrome(&p.draw());
        assert_eq!(
            screen, expected,
            "seed {seed} (chunk sizes 1..=9, recorded by the deterministic PRNG) diverged"
        );
    }
}

// ---------------------------------------------------------------------------
// R3: old regions are cleared when content shrinks.
// ---------------------------------------------------------------------------

#[test]
fn a_shrunk_answer_clears_every_row_it_used() {
    let mut p = painted_with_prefix(120, 40, 1700);
    let before = p.draw().join("\n");
    assert!(
        before.contains("CompareHashAndPassword"),
        "precondition: the long streaming answer is on screen"
    );

    let block = assistant_mut(&mut p.state);
    block.text = "已核实，三种验证办法都可行。".into();
    block.done = true;
    block.rendered = None;
    let after = p.draw();
    let joined = after.join("\n");
    assert!(
        !joined.contains("CompareHashAndPassword"),
        "the shrunk answer left old rows behind:\n{joined}"
    );

    let viewport = p.viewport();
    let last = viewport
        .iter()
        .rposition(|row| !row.trim().is_empty())
        .expect("the short answer is painted");
    assert!(viewport[last].contains("已核实"));
    for row in &viewport[last + 1..] {
        assert!(
            row.trim().is_empty(),
            "stale row after the new answer: {row:?}"
        );
    }
}

#[test]
fn overwriting_a_wide_row_with_a_narrow_one_leaves_no_wide_tail() {
    let mut state = boot(80, 30);
    start_answer(&mut state);
    stream(&mut state, &["汉字宽字符占两列".into()]);
    let mut p = Painted::new(state, 80, 30);
    p.draw();

    let block = assistant_mut(&mut p.state);
    block.text = "A".into();
    block.rendered = None;
    let screen = p.draw();
    let joined = screen.join("\n");
    assert!(
        !joined.contains('汉'),
        "a wide glyph survived the overwrite:\n{joined}"
    );
    assert!(
        screen.iter().any(|row| row.trim() == "A"),
        "the narrow replacement was not painted:\n{joined}"
    );
}

// ---------------------------------------------------------------------------
// R5: repeated paints of the same state are stable; no pumping.
// ---------------------------------------------------------------------------

#[test]
fn repainting_an_unchanged_state_is_screen_stable() {
    let mut p = painted_with_prefix(120, 40, 1520);
    let first = p.draw();
    let second = p.draw();
    let third = p.draw();
    assert_eq!(first, second, "the second paint changed the screen");
    assert_eq!(second, third, "the third paint changed the screen");
}

// ---------------------------------------------------------------------------
// T04/T10: Markdown structures that change height while streaming.
// ---------------------------------------------------------------------------

#[test]
fn an_unclosed_fence_settles_to_exactly_the_full_paint() {
    // 1380 is mid-way through the Go heredoc: the fence has not closed yet, so
    // the code block is still growing. 1420 includes the closing fence and the
    // prose after it, which re-lays out the whole block.
    let mut inside = boot(110, 36);
    start_answer(&mut inside);
    let partial: String = ANSWER.chars().take(1380).collect();
    stream(&mut inside, &chunks(&partial, 7));
    let mut inside = Painted::new(inside, 110, 36);
    let live = inside.draw();
    assert_no_control_cells(&live, "unclosed fence, mid-stream");
    assert!(
        live.join("\n").contains('▌'),
        "the live marker is missing mid-stream"
    );

    // Close the fence and finish the message; the code block stops growing and
    // the layout above it settles.
    let settled_text: String = ANSWER.chars().take(1420).collect();
    let rest: String = settled_text.chars().skip(1380).collect();
    stream(&mut inside.state, &chunks(&rest, 7));
    complete(&mut inside.state);
    let settled = normalize_chrome(&inside.draw());

    let mut full = boot(110, 36);
    start_answer(&mut full);
    stream(&mut full, &[settled_text]);
    complete(&mut full);
    let mut full = Painted::new(full, 110, 36);
    let expected = normalize_chrome(&full.draw());
    assert_eq!(
        settled, expected,
        "the streamed screen after the fence closed differs from a full paint"
    );
}

#[test]
fn a_content_growth_never_paints_text_from_beyond_the_received_prefix() {
    let total = ANSWER.chars().count();
    for prefix in [300usize, 700, 1100, 1400, 2000] {
        let mut p = painted_with_prefix(100, 36, prefix);
        let screen = p.draw();
        let head: String = ANSWER.chars().take(prefix).collect();
        let beyond: String = ANSWER.chars().skip(prefix).take(80).collect();
        let painted = screen.join("\n");
        for candidate in beyond.split('\n') {
            let candidate = candidate.trim();
            if candidate.chars().count() < 16 || head.contains(candidate) {
                continue;
            }
            assert!(
                !painted.contains(candidate),
                "prefix {prefix}: text from beyond the prefix was painted: {candidate:?}"
            );
        }
        assert!(prefix <= total);
    }
}

// ---------------------------------------------------------------------------
// T11/T12: resize while streaming, including consecutive resizes.
// ---------------------------------------------------------------------------

#[test]
fn a_resize_during_streaming_equals_a_fresh_paint_at_the_new_size() {
    let text: String = ANSWER.chars().take(1500).collect();
    let mut resized = boot(120, 40);
    start_answer(&mut resized);
    stream(&mut resized, &chunks(&text, 8));
    let mut resized = Painted::new(resized, 120, 40);
    resized.draw();
    resized.resize(90, 30);
    let after = normalize_chrome(&resized.draw());

    let mut fresh = boot(90, 30);
    start_answer(&mut fresh);
    stream(&mut fresh, &[text]);
    let mut fresh = Painted::new(fresh, 90, 30);
    let expected = normalize_chrome(&fresh.draw());
    assert_eq!(
        after, expected,
        "resize reflow differs from a fresh 90x30 paint"
    );
}

#[test]
fn consecutive_resizes_return_to_the_same_screen() {
    let text: String = ANSWER.chars().take(1500).collect();
    let mut state = boot(120, 40);
    start_answer(&mut state);
    stream(&mut state, &chunks(&text, 8));
    let mut p = Painted::new(state, 120, 40);
    p.draw();
    p.resize(90, 30);
    p.draw();
    p.resize(120, 40);
    let after = normalize_chrome(&p.draw());

    let mut fresh = boot(120, 40);
    start_answer(&mut fresh);
    stream(&mut fresh, &[text]);
    let mut fresh = Painted::new(fresh, 120, 40);
    let expected = normalize_chrome(&fresh.draw());
    assert_eq!(
        after, expected,
        "120x40 -> 90x30 -> 120x40 differs from a fresh paint"
    );
}

// ---------------------------------------------------------------------------
// T13/T14: scrolling during streaming.
// ---------------------------------------------------------------------------

#[test]
fn a_scrolled_view_stays_put_while_streaming_and_returns_to_the_live_edge() {
    let mut state = boot(100, 36);
    start_answer(&mut state);
    let head: String = ANSWER.chars().take(1200).collect();
    stream(&mut state, &chunks(&head, 6));
    let mut p = Painted::new(state, 100, 36);
    p.draw();

    p.state.conv.auto_scroll = false;
    p.state.conv.scroll = 0;
    p.draw();
    let pinned = p.viewport();
    assert!(
        pinned.iter().any(|row| row.contains("现在什么情况")),
        "scroll 0 does not show the start of the conversation: {pinned:#?}"
    );

    stream(&mut p.state, &["\n\n继续补充一句。".to_string()]);
    p.draw();
    assert_eq!(
        pinned,
        p.viewport(),
        "the pinned viewport moved while new content streamed below it"
    );
    assert!(
        p.state.conv.unread > 0,
        "growth while pinned was not counted"
    );

    p.state.conv.auto_scroll = true;
    p.draw();
    let max = {
        let width = leveler_tui::conversation::geometry::content_width(&p.state);
        let height = leveler_tui::conversation::geometry::viewport_height(&p.state);
        let total = leveler_tui::conversation::build::conversation_line_count(&p.state, width);
        leveler_tui::conversation::geometry::max_scroll(total, height)
    };
    assert_eq!(
        p.state.conv.scroll, max,
        "auto-follow did not return to the live edge"
    );
    assert_eq!(
        p.state.conv.unread, 0,
        "the unread badge survived the return to the edge"
    );
}

// ---------------------------------------------------------------------------
// T15: the composer grows and shrinks; the live edge stays visible.
// ---------------------------------------------------------------------------

#[test]
fn composer_growth_shrinks_the_viewport_without_losing_the_live_edge() {
    let text: String = ANSWER.chars().take(2000).collect();
    let mut state = boot(110, 40);
    start_answer(&mut state);
    stream(&mut state, &chunks(&text, 8));
    let mut p = Painted::new(state, 110, 40);
    p.draw();
    let tall_rect = p.viewport_rect();
    let tall = p.viewport();
    let tail = tall
        .iter()
        .rposition(|row| !row.trim().is_empty())
        .expect("content is painted");
    assert!(
        tall[tail].contains('▌') || tall[tail].trim_end().ends_with(')'),
        "the live edge is not the bottom content row: {:?}",
        tall[tail]
    );

    p.state.composer = Composer::new();
    p.state
        .composer
        .insert_str("第一行\n第二行\n第三行\n第四行\n第五行\n第六行");
    let short = p.draw();
    let short_rect = p.viewport_rect();
    assert!(
        short_rect.height < tall_rect.height,
        "the composer grew but the viewport did not shrink ({} vs {})",
        short_rect.height,
        tall_rect.height
    );
    assert!(
        short.iter().any(|row| row.contains('▌')),
        "the live edge scrolled out of view when the composer grew"
    );

    p.state.composer = Composer::new();
    let restored = normalize_chrome(&p.draw());
    let mut fresh = boot(110, 40);
    start_answer(&mut fresh);
    stream(&mut fresh, &[text]);
    let mut fresh = Painted::new(fresh, 110, 40);
    let expected = normalize_chrome(&fresh.draw());
    assert_eq!(
        restored, expected,
        "clearing the composer did not restore the layout of a fresh paint"
    );
}

// ---------------------------------------------------------------------------
// T16: tool groups fold and unfold; collapsing clears the detail rows.
// ---------------------------------------------------------------------------

fn push_tool_group(state: &mut AppState) {
    reduce(
        state,
        Action::Runtime(RuntimeEvent::ToolCallStarted {
            id: ToolCallId::new("call-1"),
            name: "run_command".into(),
            arguments: "{\"program\":\"echo\",\"args\":[\"TOOL_PREVIEW_98765\"]}".into(),
            parallel: false,
            model_step: Some(1),
            answer_effect: None,
        }),
    );
    reduce(
        state,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            id: ToolCallId::new("call-1"),
            ok: true,
            preview: "TOOL_PREVIEW_98765\nsecond line of the preview".into(),
            duration_ms: 12,
            applied_diff: None,
            exit_code: Some(0),
            stop: None,
        }),
    );
}

/// Click a painted row through the production mouse path (disclosure hit test).
fn click_row(p: &mut Painted, needle: &str) {
    let rows = p.rows();
    let y = rows
        .iter()
        .position(|row| row.contains(needle))
        .unwrap_or_else(|| panic!("no painted row contains {needle:?}: {rows:#?}"))
        as u16;
    let x = p.viewport_rect().x + 2;
    reduce(
        &mut p.state,
        Action::Mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: x,
            row: y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        }),
    );
}

#[test]
fn tool_group_unfold_paints_details_and_fold_clears_them() {
    let mut state = boot(110, 36);
    push_tool_group(&mut state);
    let mut p = Painted::new(state, 110, 36);
    let collapsed = p.draw().join("\n");
    assert!(
        !collapsed.contains("TOOL_PREVIEW_98765"),
        "collapsed group already painted the detail preview:\n{collapsed}"
    );

    // The receipt row is a real disclosure target: a left click on it toggles
    // the run through the production hit test, not a private setter.
    click_row(&mut p, "执行了 1 个命令");
    let expanded = p.draw().join("\n");
    assert!(
        expanded.contains("TOOL_PREVIEW_98765"),
        "expanded group did not paint the preview:\n{expanded}"
    );

    click_row(&mut p, "执行了 1 个命令");
    let folded = p.draw().join("\n");
    assert!(
        !folded.contains("TOOL_PREVIEW_98765"),
        "folding the group left detail rows behind:\n{folded}"
    );
}

// ---------------------------------------------------------------------------
// T17: the live marker disappears when the message completes.
// ---------------------------------------------------------------------------

#[test]
fn the_live_marker_disappears_and_final_state_equals_a_full_paint() {
    let mut state = boot(100, 36);
    start_answer(&mut state);
    stream(&mut state, &chunks(ANSWER, 5));
    let mut p = Painted::new(state, 100, 36);
    let streaming = p.draw().join("\n");
    assert!(streaming.contains('▌'), "no live marker while streaming");

    complete(&mut p.state);
    let settled = normalize_chrome(&p.draw());
    assert!(
        !settled.iter().any(|row| row.contains('▌')),
        "the live marker survived completion"
    );

    let mut fresh = boot(100, 36);
    start_answer(&mut fresh);
    stream(&mut fresh, &[ANSWER.to_string()]);
    complete(&mut fresh);
    let mut fresh = Painted::new(fresh, 100, 36);
    assert_eq!(settled, normalize_chrome(&fresh.draw()));
}

// ---------------------------------------------------------------------------
// T09: syntax-highlight spans are identical after streaming and after a full
// paint (style cells, not just text).
// ---------------------------------------------------------------------------

#[test]
fn highlighted_code_cells_match_between_streaming_and_full_paint() {
    let text: String = ANSWER.chars().take(1500).collect();
    let mut increment = boot(120, 40);
    increment.theme = Theme::default();
    start_answer(&mut increment);
    stream(&mut increment, &chunks(&text, 2));
    let mut increment = Painted::new(increment, 120, 40);
    increment.draw();

    let mut full = boot(120, 40);
    full.theme = Theme::default();
    start_answer(&mut full);
    stream(&mut full, &[text]);
    let mut full = Painted::new(full, 120, 40);
    full.draw();

    assert_eq!(
        increment.viewport_styled(),
        full.viewport_styled(),
        "per-cell styles differ between incremental and full paint"
    );
}

// ---------------------------------------------------------------------------
// T18: the recap block refreshes its content without leaving stale rows.
// ---------------------------------------------------------------------------

#[test]
fn a_refreshing_recap_replaces_its_previous_text() {
    let mut state = boot(100, 36);
    state.transcript.push_recap(RecapBlock {
        summary: Some("旧回顾内容-ALPHA".into()),
        next_step: Some("旧下一步".into()),
    });
    let mut p = Painted::new(state, 100, 36);
    assert!(p.draw().join("\n").contains("ALPHA"));

    let mut updated = false;
    for item in p.state.transcript.items_mut() {
        if let TranscriptItem::Recap(recap) = item {
            recap.summary = Some("新回顾内容-BETA".into());
            updated = true;
        }
    }
    assert!(updated);
    let screen = p.draw().join("\n");
    assert!(
        screen.contains("BETA"),
        "the refreshed recap was not painted:\n{screen}"
    );
    assert!(
        !screen.contains("ALPHA"),
        "the previous recap text survived the refresh:\n{screen}"
    );
}

// ---------------------------------------------------------------------------
// T22: concurrent background output and a tool call together do not corrupt
// the conversation cells.
// ---------------------------------------------------------------------------

#[test]
fn background_activity_chunks_between_stream_deltas_do_not_corrupt_cells() {
    let mut state = boot(110, 36);
    start_answer(&mut state);
    let text: String = ANSWER.chars().take(1600).collect();
    let parts = chunks(&text, 11);
    for (index, part) in parts.iter().enumerate() {
        stream(&mut state, std::slice::from_ref(part));
        if index % 7 == 0 {
            reduce(
                &mut state,
                Action::Runtime(RuntimeEvent::ToolCallStarted {
                    id: ToolCallId::new(format!("bg-{index}")),
                    name: "run_command".into(),
                    arguments: "{\"command\":\"sleep 1\"}".into(),
                    parallel: true,
                    model_step: Some(1),
                    answer_effect: None,
                }),
            );
            reduce(
                &mut state,
                Action::Runtime(RuntimeEvent::ToolCallOutput {
                    id: ToolCallId::new(format!("bg-{index}")),
                    stream: "stdout".into(),
                    chunk: "background line with 汉字 and \"quotes\"".into(),
                }),
            );
            reduce(
                &mut state,
                Action::Runtime(RuntimeEvent::ToolCallCompleted {
                    id: ToolCallId::new(format!("bg-{index}")),
                    ok: true,
                    preview: "done".into(),
                    duration_ms: 1000,
                    applied_diff: None,
                    exit_code: Some(0),
                    stop: None,
                }),
            );
        }
    }
    let mut p = Painted::new(state, 110, 36);
    let screen = p.draw();
    assert_no_control_cells(&screen, "background interleaving");
}

// ---------------------------------------------------------------------------
// T06/T08: CJK, emoji, full-width and combining characters keep the same
// grid whether they arrive in chunks or in one delta, and a later narrow line
// clears every cell the wide glyphs used.
// ---------------------------------------------------------------------------

#[test]
fn cjk_emoji_and_combining_rows_survive_width_transitions() {
    let long = "中文混排 abc 汉字 👩‍💻 e\u{301} ＡＢＣ 🚀🧪 iiii".to_string();
    let mut state = boot(64, 30);
    start_answer(&mut state);
    stream(&mut state, &chunks(&long, 3));
    let mut p = Painted::new(state, 64, 30);
    let screen = normalize_chrome(&p.draw());
    assert_no_control_cells(&screen, "wide/emoji/combining");
    let joined = screen.join("\n");
    assert!(joined.contains("中文混排"), "CJK line missing: {joined}");
    assert!(
        joined.contains('👩') || joined.contains("👩‍💻"),
        "emoji missing: {joined}"
    );

    let mut fresh_state = boot(64, 30);
    start_answer(&mut fresh_state);
    stream(&mut fresh_state, std::slice::from_ref(&long));
    let mut fresh = Painted::new(fresh_state, 64, 30);
    assert_eq!(
        screen,
        normalize_chrome(&fresh.draw()),
        "chunked wide/emoji paint differs from one delta"
    );

    let block = assistant_mut(&mut p.state);
    block.text = "done".into();
    block.rendered = None;
    let screen = p.draw();
    let joined = screen.join("\n");
    for glyph in ['中', '👩', '🚀', 'Ａ'] {
        assert!(
            !joined.contains(glyph),
            "wide glyph {glyph} survived the narrow overwrite: {joined}"
        );
    }
    assert!(screen.iter().any(|row| row.trim() == "done"));
    assert_no_control_cells(&screen, "wide overwrite");
}

// ---------------------------------------------------------------------------
// T24: randomized UI action sequences. Each seed is reproducible; the final
// screen after the sequence must equal a fresh full paint of the same logical
// end state, with no control cells at any sampled frame.
// ---------------------------------------------------------------------------

fn key(code: KeyCode) -> Action {
    Action::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

#[test]
fn random_ui_sequences_converge_to_the_full_paint() {
    const SIZES: [(u16, u16); 3] = [(120, 40), (100, 36), (90, 30)];
    let chars: Vec<char> = ANSWER.chars().take(320).collect();
    for seed in 0..100u64 {
        let mut x = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let mut next = |bound: u64| {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (x >> 33) % bound
        };
        let mut state = boot(120, 40);
        start_answer(&mut state);
        let mut p = Painted::new(state, 120, 40);
        let mut i = 0usize;
        while i < chars.len() {
            let n = (next(7) + 1) as usize;
            let end = (i + n).min(chars.len());
            let delta: String = chars[i..end].iter().collect();
            reduce(
                &mut p.state,
                Action::Runtime(RuntimeEvent::AssistantTextDelta {
                    message_id: MessageId::new("m0"),
                    delta,
                }),
            );
            i = end;
            match next(8) {
                0 => {
                    let (w, h) = SIZES[next(3) as usize];
                    p.resize(w, h);
                }
                1 | 2 => {
                    reduce(&mut p.state, key(KeyCode::PageUp));
                }
                3 => {
                    reduce(&mut p.state, key(KeyCode::PageDown));
                }
                4 => {
                    reduce(&mut p.state, key(KeyCode::Char('a')));
                    reduce(&mut p.state, key(KeyCode::Char('b')));
                }
                5 => {
                    reduce(&mut p.state, key(KeyCode::Backspace));
                }
                _ => {}
            }
            let screen = p.draw();
            assert_no_control_cells(&screen, &format!("seed {seed} step {i}"));
        }
        p.state.composer.replace("");
        complete(&mut p.state);
        p.state.conv.auto_scroll = true;
        let final_rows = normalize_chrome(&p.draw());

        let (w, h) = p.state.size;
        let mut fresh_state = boot(w, h);
        start_answer(&mut fresh_state);
        stream(&mut fresh_state, &[chars.iter().collect::<String>()]);
        complete(&mut fresh_state);
        let mut fresh = Painted::new(fresh_state, w, h);
        let expected = normalize_chrome(&fresh.draw());
        assert_eq!(
            final_rows, expected,
            "seed {seed}: randomized UI sequence did not converge to a full paint"
        );
    }
}

// ---------------------------------------------------------------------------
// T21: entering and leaving Shell Mode swaps only the mode chrome; the text
// field and the rest of the frame stay cell-consistent.
// ---------------------------------------------------------------------------

#[test]
fn shell_mode_enter_and_exit_clear_the_mode_chrome() {
    let state = boot(100, 36);
    let mut p = Painted::new(state, 100, 36);
    let normal = p.draw();
    assert!(!normal.join("\n").contains("Tab 补全"));

    reduce(&mut p.state, key(KeyCode::Char('!')));
    let shell = p.draw();
    assert!(
        shell.join("\n").contains("Tab 补全"),
        "the shell hint was not painted:\n{}",
        shell.join("\n")
    );

    for ch in "echo hi".chars() {
        reduce(&mut p.state, key(KeyCode::Char(ch)));
    }
    let typed = p.draw();
    assert!(typed.iter().any(|row| row.contains("echo hi")));
    assert_no_control_cells(&typed, "shell mode typing");

    for _ in 0.."echo hi".chars().count() {
        reduce(&mut p.state, key(KeyCode::Backspace));
    }
    reduce(&mut p.state, key(KeyCode::Esc));
    let back = p.draw();
    assert!(
        !back.join("\n").contains("Tab 补全"),
        "the shell mode chrome survived exit:\n{}",
        back.join("\n")
    );
    assert_no_control_cells(&back, "shell mode exit");
}
