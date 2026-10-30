//! `/context` under the real ratatui renderer: the Context Map and the
//! breakdown must both fit the pane at every width, and the layout must degrade
//! from side-by-side to stacked without dropping a number.

use leveler_client_protocol::SessionId;
use leveler_model::{
    CompactionRecord, ContextAccounting, ContextCategory, ContextPressure, FoldRequirement,
    ModelRef, TokenCountKind,
};
use leveler_tui::screen::Screen;
use leveler_tui::state::{AppState, Boot};
use leveler_tui::theme::Theme;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn cat(name: &str, label: &str, tokens: u64, children: Vec<ContextCategory>) -> ContextCategory {
    ContextCategory {
        name: name.to_string(),
        label: label.to_string(),
        tokens,
        calls: 0,
        children,
    }
}

fn snapshot() -> ContextAccounting {
    let tool_results = cat(
        "tool_results",
        "Tool results",
        18_600,
        vec![
            cat("read_file", "read_file", 10_700, vec![]),
            cat("run_command", "run_command", 6_200, vec![]),
            cat("other", "other", 1_700, vec![]),
        ],
    );
    let messages = cat(
        "messages",
        "Messages",
        51_800,
        vec![
            cat("user", "User", 5_800, vec![]),
            cat("assistant", "Assistant", 17_100, vec![]),
            cat("tool_calls", "Tool calls", 7_300, vec![]),
            tool_results,
            cat("compaction_summary", "Compaction summary", 3_000, vec![]),
        ],
    );
    let categories = vec![
        messages,
        cat("tool_definitions", "Tool definitions", 6_100, vec![]),
        cat("system", "System", 7_200, vec![]),
    ];
    let used: u64 = categories.iter().map(|c| c.tokens).sum();
    ContextAccounting {
        model: ModelRef::new("deepseek", "deepseek-chat"),
        context_window_tokens: Some(128_000),
        compact_at_tokens: Some(64_000),
        output_reservation_tokens: Some(32_000),
        headroom_tokens: Some(0),
        input_capacity_tokens: Some(96_000),
        fold_state: FoldRequirement::None,
        used_tokens: used,
        free_tokens: Some(128_000 - used),
        token_count_kind: TokenCountKind::Estimated,
        pressure: ContextPressure::Warning,
        categories,
        reasoning_projection: None,
        last_compaction: Some(CompactionRecord {
            before_tokens: 142_600,
            after_tokens: 53_400,
        }),
    }
}

fn opened(acc: ContextAccounting) -> AppState {
    let mut s = AppState::new(
        Theme::no_color(),
        Boot {
            session_id: SessionId::new("s1"),
            user: "tester".into(),
            version: "0.1.0".into(),
            show_welcome: false,
            draft_path: None,
            history_path: None,
            context_window: 128_000,
            locale: leveler_tui::Locale::En,
            untrusted_config: Vec::new(),
            model_notice: None,
            thinking: None,
        },
    );
    s.active_screen = Screen::Context;
    s.context.loaded = Some(acc);
    s
}

fn screen_text(state: &mut AppState, w: u16, h: u16) -> String {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| leveler_tui::render::render(f, state))
        .unwrap();
    let buf = term.backend().buffer();
    let mut out = String::new();
    for y in 0..h {
        let mut line = String::new();
        for x in 0..w {
            line.push_str(buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

#[test]
fn the_pane_holds_at_every_width() {
    for (w, h) in [(120u16, 30u16), (80, 30), (60, 30), (40, 30)] {
        let mut s = opened(snapshot());
        let text = screen_text(&mut s, w, h);
        assert!(text.contains("Breakdown"), "width {w}: missing breakdown");
        // No `Free` detail row: remaining capacity is the map's empty cells and
        // the header's own ratio. The Usage section follows under its own rule.
        assert!(
            !text.contains("Free") && !text.contains("○ 可用"),
            "width {w}: the Free detail row is gone:\n{text}"
        );
        assert!(text.contains("Usage"), "width {w}: missing usage section");
        assert!(text.contains("●"), "width {w}: missing map");
        // No line may carry more cells than the pane (the renderer clips, so a
        // silent overflow would be invisible without counting pre-render).
        assert!(
            text.lines()
                .all(|l| unicode_width::UnicodeWidthStr::width(l) <= w as usize),
            "width {w}: a line overflowed:\n{text}"
        );
    }
}

#[test]
fn wide_panes_place_the_map_beside_the_breakdown() {
    let mut s = opened(snapshot());
    let text = screen_text(&mut s, 120, 30);
    let first_map_row = text.lines().find(|l| l.contains('●')).expect("a map row");
    assert!(
        first_map_row.contains("Breakdown"),
        "map and breakdown should share the first row on a wide pane: {first_map_row:?}"
    );
}

#[test]
fn narrow_panes_stack_the_map_over_the_breakdown() {
    let mut s = opened(snapshot());
    let text = screen_text(&mut s, 40, 30);
    let first_map_row = text.lines().position(|l| l.contains('●')).expect("map");
    let breakdown_row = text
        .lines()
        .position(|l| l.contains("Breakdown"))
        .expect("breakdown");
    assert!(breakdown_row > first_map_row, "narrow panes must stack");
}

#[test]
fn drill_down_shows_the_tool_result_leaves() {
    let mut s = opened(snapshot());
    s.context.expanded.insert("messages".to_string());
    s.context.expanded.insert("tool_results".to_string());
    let text = screen_text(&mut s, 120, 40);
    assert!(text.contains("read_file"), "{text}");
    assert!(text.contains("run_command"), "{text}");
    assert!(text.contains("Tool calls"), "{text}");
}

/// The header figure and the rows under it are ONE snapshot's arithmetic.
///
/// The report behind this test was a screen whose top figure (4.1K) was smaller
/// than a row printed below it (Tool definitions 9.3K). Two numbers can only
/// disagree like that when they come from different measurements, so the
/// invariant pinned here is stated on the RENDERED screen: the header's used
/// figure equals the sum of the top-level rows, and no row exceeds it.
#[test]
fn the_header_figure_is_the_sum_of_the_rows_it_prints() {
    // Multiples of 100 so the one-decimal rendering is exact: parsing the
    // printed figures back cannot agree by rounding accident.
    let messages = cat(
        "messages",
        "Messages",
        4_000,
        vec![
            cat("user", "User", 1_000, vec![]),
            cat("assistant", "Assistant", 3_000, vec![]),
        ],
    );
    // Deliberately larger than every other row: this is the shape of the report.
    let categories = vec![
        cat("tool_definitions", "Tool definitions", 9_300, vec![]),
        cat("system", "System", 2_000, vec![]),
        messages,
    ];
    let used: u64 = categories.iter().map(|c| c.tokens).sum();
    let acc = ContextAccounting {
        model: ModelRef::new("deepseek", "deepseek-chat"),
        context_window_tokens: Some(128_000),
        compact_at_tokens: Some(64_000),
        output_reservation_tokens: Some(32_000),
        headroom_tokens: Some(0),
        input_capacity_tokens: Some(96_000),
        fold_state: FoldRequirement::None,
        used_tokens: used,
        free_tokens: Some(128_000 - used),
        token_count_kind: TokenCountKind::Estimated,
        pressure: ContextPressure::Normal,
        categories,
        reasoning_projection: None,
        last_compaction: None,
    };
    assert_eq!(used, 15_300);

    let mut s = opened(acc);
    let text = screen_text(&mut s, 120, 40);
    let lines: Vec<&str> = text.lines().collect();
    let header = lines
        .iter()
        .find(|line| line.contains("96k"))
        .unwrap_or_else(|| panic!("the capacity header is missing:\n{text}"));
    let header_used =
        printed_figure(header).unwrap_or_else(|| panic!("the header prints no figure: {header:?}"));
    assert_eq!(
        header_used, used,
        "the header must print the snapshot's used figure: {header:?}"
    );

    // Every top-level row, found by the label it prints. On a wide pane the
    // map shares the row, so the line also carries `○` cells.
    let mut row_sum = 0u64;
    for (label, expected) in [
        ("Tool definitions", 9_300u64),
        ("System", 2_000),
        ("Messages", 4_000),
    ] {
        let row = lines
            .iter()
            .find(|line| line.contains(label))
            .unwrap_or_else(|| panic!("the {label} row is missing:\n{text}"));
        let figure = printed_figure(row).unwrap_or_else(|| panic!("row prints no figure: {row:?}"));
        assert_eq!(figure, expected, "{label}: {row:?}");
        assert!(
            figure <= header_used,
            "a row is larger than the total it belongs to ({figure} > {header_used}): {row:?}"
        );
        row_sum += figure;
    }
    assert_eq!(
        row_sum, header_used,
        "the printed rows must sum to the printed total:\n{text}"
    );
}

/// The first token figure a breakdown/header line prints, in tokens.
///
/// Percentages carry their own `%` suffix, so a ratio can never be mistaken for
/// a token count here.
fn printed_figure(line: &str) -> Option<u64> {
    line.split_whitespace().find_map(|word| {
        if word.ends_with('%') {
            return None;
        }
        let (num, scale) = match word.strip_suffix('k') {
            Some(num) => (num, 1_000.0),
            None => match word.strip_suffix('M') {
                Some(num) => (num, 1_000_000.0),
                None => (word, 1.0),
            },
        };
        num.parse::<f64>()
            .ok()
            .map(|value| (value * scale).round() as u64)
    })
}

/// Manual visual harness:
///   cargo test -p leveler-tui --test context_screen -- --ignored --nocapture
/// Manual visual harness:
///   cargo test -p leveler-tui --test context_screen -- --ignored --nocapture
#[test]
#[ignore = "manual visual harness; run with --ignored --nocapture"]
fn visual_context_screen() {
    for (w, h) in [(120u16, 26u16), (80, 26), (60, 30)] {
        let mut s = opened(snapshot());
        s.context.expanded.insert("messages".to_string());
        println!("\n══════════ /context {w}x{h} ══════════");
        print!("{}", screen_text(&mut s, w, h));
    }
}

/// Manual visual harness for the whole unified page, with a real usage report:
///   cargo test -p leveler-tui --test context_screen -- --ignored --nocapture
#[test]
#[ignore = "manual visual harness; run with --ignored --nocapture"]
fn visual_stats_page() {
    use leveler_client_protocol::{UiModelUsage, UiUsageReport, UiUsageScope};
    for (w, h) in [
        (120u16, 40u16),
        (80, 40),
        (60, 30),
        (60, 40),
        (40, 30),
        (40, 40),
    ] {
        let mut s = opened(snapshot());
        s.context.expanded.insert("messages".to_string());
        s.stats_section = leveler_tui::screen::StatsSection::Usage;
        // Exactly what `/usage` does on the next paint.
        s.stats_reveal = Some(leveler_tui::screen::StatsSection::Usage);
        s.usage.scope = UiUsageScope::Session;
        s.usage.loaded = Some(UiUsageReport {
            scope: UiUsageScope::Session,
            from: None,
            to: None,
            generated_at: "2026-03-06T01:00:00+00:00".into(),
            requests: 12,
            unmeasured_requests: 1,
            input_tokens: 431_200,
            cached_input_tokens: Some(388_800),
            output_tokens: 9_400,
            reasoning_tokens: Some(4_100),
            cost_usd_micros: Some(210_400),
            priced_requests: 11,
            models: vec![
                UiModelUsage {
                    provider: "deepseek".into(),
                    model: "deepseek-flash".into(),
                    requests: 10,
                    unmeasured_requests: 0,
                    input_tokens: 391_200,
                    cached_input_tokens: Some(388_800),
                    output_tokens: 9_000,
                    reasoning_tokens: Some(4_100),
                    cost_usd_micros: Some(210_400),
                    priced_requests: 10,
                },
                UiModelUsage {
                    provider: "bigmodel".into(),
                    model: "glm-5.3".into(),
                    requests: 2,
                    unmeasured_requests: 1,
                    input_tokens: 40_000,
                    cached_input_tokens: None,
                    output_tokens: 400,
                    reasoning_tokens: None,
                    cost_usd_micros: None,
                    priced_requests: 0,
                },
            ],
        });
        println!("\n══════════ /usage {w}x{h} ══════════");
        print!("{}", screen_text(&mut s, w, h));
    }
}

/// The screen shows the two axes with their OWN denominators, and the fold
/// state comes from the classifier — not from a rounded percentage.
#[test]
fn the_screen_separates_the_compaction_axis_from_the_model_window() {
    // The fixture in this file is a 128K window with a 96K effective input
    // capacity and a 64K soft threshold; force the soft state by filling the
    // request past the threshold.
    let mut accounting = snapshot();
    accounting.used_tokens = 70_000;
    accounting.compact_at_tokens = Some(64_000);
    accounting.input_capacity_tokens = Some(96_000);
    accounting.output_reservation_tokens = Some(32_000);
    accounting.headroom_tokens = Some(0);
    accounting.fold_state = FoldRequirement::Soft;
    accounting.pressure = ContextPressure::Critical;
    let mut state = opened(accounting.clone());
    let joined = screen_text(&mut state, 100, 40);
    assert!(
        joined.contains("70k / 96k"),
        "compaction axis first:\n{joined}"
    );
    assert!(
        joined.contains("Model window") && joined.contains("70k / 128k"),
        "the model window axis is named, not mixed in:\n{joined}"
    );
    assert!(joined.contains("Output reservation 32k"), "{joined}");
    assert!(joined.contains("Soft threshold"), "{joined}");
    assert!(joined.contains("Hard capacity 96k"), "{joined}");
    assert!(
        joined.contains("Fold state"),
        "the real state is shown:\n{joined}"
    );
    // The two percentages must not collapse into one number: 70k/96k is 72.9%
    // of the compaction axis and 54.7% of the window.
    assert!(joined.contains("72.9%"), "{joined}");
    assert!(joined.contains("54.7%"), "{joined}");
}

/// Without a resolved capacity the screen must not invent one: the model window
/// is the only total left, so it is named and the fold thresholds are simply
/// not shown.
#[test]
fn no_resolved_capacity_shows_no_compaction_denominator() {
    let mut accounting = snapshot();
    accounting.input_capacity_tokens = None;
    accounting.output_reservation_tokens = None;
    accounting.headroom_tokens = None;
    accounting.compact_at_tokens = None;
    accounting.fold_state = FoldRequirement::None;
    accounting.pressure = ContextPressure::Normal;
    let mut state = opened(accounting);
    let joined = screen_text(&mut state, 100, 40);
    assert!(!joined.contains("Hard capacity"), "{joined}");
    assert!(!joined.contains("Soft threshold"), "{joined}");
    assert!(!joined.contains("Output reservation"), "{joined}");
    // The window is a runtime fact, not a compaction axis: it stays named as
    // the only total, and no denominator is derived from it.
    assert!(joined.contains("Model window"), "{joined}");
    assert!(joined.contains("65.1k / 128k"), "{joined}");
}
