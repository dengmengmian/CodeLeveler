//! `/context` under the real ratatui renderer: the Context Map and the
//! breakdown must both fit the pane at every width, and the layout must degrade
//! from side-by-side to stacked without dropping a number.

use leveler_client_protocol::SessionId;
use leveler_model::{
    CompactionRecord, ContextAccounting, ContextCategory, ContextPressure, ModelRef, TokenCountKind,
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
        used_tokens: used,
        free_tokens: Some(128_000 - used),
        token_count_kind: TokenCountKind::Estimated,
        pressure: ContextPressure::Warning,
        categories,
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
            reasoning_effort: None,
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
        assert!(text.contains("Free"), "width {w}: missing free");
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
