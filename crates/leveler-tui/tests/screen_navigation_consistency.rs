//! Standing consistency contract for the independent pages.
//!
//! Every full-viewport page behind [`Screen`] shares one navigation, mouse,
//! keyboard and layout language, and a user must not have to remember a
//! different one per page. This file drives the real reducer and the real
//! renderer — the same projection the interactive TUI paints — so a page that
//! drifts from the shared rules turns red here rather than in a user's hands.
//!
//! The rules under test (the shell in `secondary.rs` implements them):
//!
//! 1. One level per press: Esc and the header's `←` run the same one-level
//!    back, and a page opened from another page returns to THAT page.
//! 2. Every page that draws a `←` publishes it as a hit target, and the hit is
//!    the painted glyph + title, not the whole row.
//! 3. The wheel scrolls the page under the pointer.
//! 4. Header, rules, content and footer share one safe content rect.
//!
//! Per-page contracts that belong to a single page (the Issue page's expansion
//! and acknowledgement lifecycle, a child detail returning to Team) stay in
//! `reducer.rs` and `detail_page_dogfood.rs`; this file holds the rules that
//! must hold for EVERY page.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use leveler_client_protocol::{RuntimeStatus, SessionId, UiSessionSummary};
use leveler_tui::action::Action;
use leveler_tui::reducer::reduce;
use leveler_tui::render::render;
use leveler_tui::screen::Screen;
use leveler_tui::state::{AppState, BackgroundTaskChrome, Boot};
use leveler_tui::theme::Theme;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use unicode_width::UnicodeWidthStr;

fn opened() -> AppState {
    AppState::new(
        Theme::no_color(),
        Boot {
            session_id: SessionId::new("s1"),
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

fn key(code: KeyCode) -> Action {
    Action::Key(KeyEvent::new(code, KeyModifiers::empty()))
}

fn click(x: u16, y: u16) -> Action {
    Action::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: x,
        row: y,
        modifiers: KeyModifiers::empty(),
    })
}

fn wheel(x: u16, y: u16, up: bool) -> Action {
    Action::Mouse(MouseEvent {
        kind: if up {
            MouseEventKind::ScrollUp
        } else {
            MouseEventKind::ScrollDown
        },
        column: x,
        row: y,
        modifiers: KeyModifiers::empty(),
    })
}

/// Paint the current screen and return its text, one line per terminal row.
fn render_text(state: &mut AppState, w: u16, h: u16) -> String {
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, state)).unwrap();
    let buf = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..buf.area.height {
        let mut x = 0;
        while x < buf.area.width {
            let sym = buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ");
            out.push_str(sym);
            x += UnicodeWidthStr::width(sym).max(1) as u16;
        }
        out.push('\n');
    }
    out
}

/// A rendered page: one trailing-trimmed string per terminal row.
struct Page {
    lines: Vec<String>,
}

impl Page {
    fn of(state: &mut AppState, w: u16, h: u16) -> Self {
        let lines = render_text(state, w, h)
            .lines()
            .map(|l| l.trim_end().to_string())
            .collect();
        Self { lines }
    }

    fn row(&self, i: usize) -> &str {
        self.lines.get(i).map(String::as_str).unwrap_or("")
    }

    /// The rows that actually have ink, with their index — a blank row cannot
    /// overrun anything, and counting them would only add noise.
    fn non_empty(&self) -> Vec<(usize, &str)> {
        self.lines
            .iter()
            .enumerate()
            .filter(|(_, l)| !l.is_empty())
            .map(|(i, l)| (i, l.as_str()))
            .collect()
    }
}

fn summary(id: &str, goal: &str) -> UiSessionSummary {
    UiSessionSummary {
        id: SessionId::new(id),
        task_status: None,
        task_terminal: None,
        goal: goal.into(),
        status: "idle".into(),
        declaration: None,
        model: "m".into(),
        updated_at: "0".into(),
        repository: None,
    }
}

fn failed_task(state: &mut AppState, id: &str, label: &str) {
    state.background_task_labels.insert(
        id.to_string(),
        BackgroundTaskChrome {
            ok: Some(false),
            exit_code: Some(3),
            duration_ms: Some(1_200),
            ..BackgroundTaskChrome::running(label, 0)
        },
    );
}

// ---- 1. A child page returns to the page that opened it ---------------------

/// Chat → Sessions → Trace → Esc lands on Sessions, the page that opened the
/// observatory. Skipping it would drop the user back into the conversation
/// from a page they never left on purpose.
#[test]
fn a_trace_opened_from_sessions_returns_to_sessions() {
    let mut s = opened();
    s.sessions.push(summary("s2", "other session"));
    s.active_screen = Screen::Sessions;

    reduce(&mut s, key(KeyCode::Char('t')));
    assert_eq!(s.active_screen, Screen::Trace, "t opens the observatory");

    reduce(&mut s, key(KeyCode::Esc));
    assert_eq!(
        s.active_screen,
        Screen::Sessions,
        "Esc returns to the page that opened the trace"
    );

    reduce(&mut s, key(KeyCode::Esc));
    assert_eq!(s.active_screen, Screen::Conversation);
}

/// The same rule from the command line: `/trace` is opened from the
/// conversation, so its `←` goes back to the conversation.
#[test]
fn a_trace_opened_from_the_conversation_returns_to_the_conversation() {
    let mut s = opened();
    s.active_screen = Screen::Trace;
    reduce(&mut s, key(KeyCode::Esc));
    assert_eq!(s.active_screen, Screen::Conversation);
}

// ---- 2. Every page publishes the `←` it paints ------------------------------

/// A page that draws a `←` in its header must publish it as a hit target, and
/// clicking it must run the same one-level back as Esc.
#[test]
fn every_page_that_paints_a_back_glyph_publishes_its_hit() {
    for (screen, label) in pages_with_a_back_glyph() {
        let mut s = opened();
        seed(&mut s, screen);
        let page = Page::of(&mut s, 120, 30);
        assert!(
            page.row(0).contains('←'),
            "{label}: page paints no back glyph: {:?}",
            page.row(0)
        );
        let hit = s
            .back_hit
            .unwrap_or_else(|| panic!("{label}: ← is painted but not published as a hit"));
        assert_eq!(hit.0, 0, "{label}: the nav lives on the header row");
        let before = s.active_screen;
        reduce(&mut s, click(hit.1, hit.0));
        assert_ne!(
            s.active_screen, before,
            "{label}: clicking ← must leave the page (one level back)"
        );
    }
}

/// The published hit is the painted glyph + title, never the whole header row:
/// the right half belongs to the status, so a click there must not navigate.
#[test]
fn the_back_hit_covers_the_painted_title_only() {
    let mut s = opened();
    seed(&mut s, Screen::Plan);
    let page = Page::of(&mut s, 120, 30);
    let (row, x0, x1) = s.back_hit.expect("plan publishes its ← hit");
    assert!(
        (x1 - x0) < 120,
        "the hit must not span the whole row (got {x0}..{x1})"
    );
    // Columns, not bytes: the title is CJK, so the byte offset of its end is
    // not the column the glyph ends on.
    let header = page.row(row as usize);
    let title_end = header
        .find("计划")
        .map(|byte| UnicodeWidthStr::width(&header[..byte + "计划".len()]))
        .unwrap_or(0);
    assert!(
        x1 as usize >= title_end,
        "the hit covers the painted title (hit ends {x1}, title ends {title_end})"
    );
}

// ---- 3. The wheel scrolls the page under the pointer ------------------------

/// Every page answers the wheel when its body does not fit the viewport.
///
/// The seeded pages below are the ones whose content is longer than the 12-row
/// window used here. A page whose whole body fits is left out rather than
/// asserted to be unchanged: "the wheel did nothing" is the right answer
/// there, and asserting it would only pin the fixture's size.
#[test]
fn the_wheel_scrolls_a_page_whose_body_overflows() {
    for (screen, label) in overflowing_pages() {
        let mut s = opened();
        seed(&mut s, screen);
        let before = Page::of(&mut s, 80, 12);
        for _ in 0..6 {
            reduce(&mut s, wheel(40, 5, false));
        }
        let after = Page::of(&mut s, 80, 12);
        assert_ne!(
            before.lines, after.lines,
            "{label}: the wheel changed nothing on a page whose body overflows"
        );
    }
}

// ---- 4. Keyboard and mouse reach the same things ----------------------------

/// The Issue page's expanded row carries a detail entry. A mouse can click it,
/// so the keyboard must reach it too — otherwise the task's own page is
/// unreachable without a pointer.
#[test]
fn the_issue_page_opens_its_detail_entry_from_the_keyboard() {
    let mut s = opened();
    failed_task(&mut s, "bg-1", "sh -c 'exit 3'");
    s.active_screen = Screen::Issue;
    s.issue_selected = Some("bg-1".into());

    // Enter expands the row (the same interaction a row click runs)...
    reduce(&mut s, key(KeyCode::Enter));
    assert!(
        s.issue_expanded.contains("bg-1"),
        "Enter expands the selected row"
    );

    // ...and a second, distinct binding enters the detail the expanded row
    // advertises. Enter must NOT do this: one action never means both.
    reduce(&mut s, key(KeyCode::Right));
    assert_eq!(
        s.active_screen,
        Screen::Activity,
        "→ opens the selected expanded row's detail entry"
    );
    assert_eq!(s.detail_return, Some(Screen::Issue), "and it returns here");

    reduce(&mut s, key(KeyCode::Esc));
    assert_eq!(s.active_screen, Screen::Issue);
    assert!(
        s.issue_expanded.contains("bg-1"),
        "the expansion survives the round trip"
    );
}

/// With the row collapsed there is no detail entry on screen, so `→` must not
/// invent one.
#[test]
fn the_issue_page_does_not_open_a_detail_that_is_not_on_screen() {
    let mut s = opened();
    failed_task(&mut s, "bg-1", "sh -c 'exit 3'");
    s.active_screen = Screen::Issue;
    s.issue_selected = Some("bg-1".into());

    reduce(&mut s, key(KeyCode::Right));
    assert_eq!(
        s.active_screen,
        Screen::Issue,
        "a collapsed row advertises no detail entry"
    );
}

// ---- 5. One safe content rect for header, rules, body and footer ------------

/// The shell's safe horizontal margin, as the product specifies it: two
/// columns from 80 up, one in the 40–79 band, none below. Spelled out here on
/// purpose — it is the user-facing contract, not a call into the code under
/// test, so changing it has to be argued for rather than absorbed.
fn gutter(width: u16) -> usize {
    match width {
        w if w >= 80 => 2,
        w if w >= 40 => 1,
        _ => 0,
    }
}

/// Header, both rules and the body all start on the same column, and the
/// page's own rows never run past the right margin. Without this the chrome
/// drifts from the content it frames — the header inset, the rule full-bleed.
#[test]
fn every_page_keeps_its_chrome_on_one_content_rect() {
    for (screen, label) in pages_with_a_back_glyph() {
        let mut s = opened();
        seed(&mut s, screen);
        let page = Page::of(&mut s, 120, 30);
        let left = gutter(120);
        let right = 120 - left as u16;

        let nav = page.row(0);
        assert_eq!(
            nav.find('←'),
            Some(left),
            "{label}: the nav is not on the safe margin: {nav:?}"
        );
        let rule = page.lines.get(1).map(String::as_str).unwrap_or("");
        assert!(
            rule.starts_with(&" ".repeat(left)) && rule[left..].starts_with('─'),
            "{label}: the top rule is not on the safe margin: {rule:?}"
        );
        assert!(
            !rule.starts_with('─'),
            "{label}: the top rule runs out to the terminal edge: {rule:?}"
        );

        for (index, line) in page.non_empty() {
            let width = UnicodeWidthStr::width(line);
            assert!(
                width <= right as usize,
                "{label}: row {index} overran the safe margin ({width} > {right}): {line:?}"
            );
        }
    }
}

/// The margin follows the terminal, so a narrow window keeps its columns
/// instead of paying for a gutter it cannot afford.
#[test]
fn the_safe_margin_follows_the_terminal_width() {
    for (width, expected) in [(120u16, 2usize), (80, 2), (60, 1), (40, 1), (24, 0)] {
        let mut s = opened();
        seed(&mut s, Screen::Plan);
        let page = Page::of(&mut s, width, 20);
        assert_eq!(
            page.row(0).find('←'),
            Some(expected),
            "w={width}: nav margin: {:?}",
            page.row(0)
        );
        let rule = page.row(1);
        assert!(
            rule.starts_with(&" ".repeat(expected)) && rule[expected..].starts_with('─'),
            "w={width}: rule margin: {rule:?}"
        );
    }
}

/// Every page renders at the widths the product supports, with empty and
/// pathological data, and paints nothing past the terminal.
#[test]
fn every_page_renders_at_every_supported_width() {
    for (screen, label) in pages_with_a_back_glyph() {
        for width in [120u16, 80, 60, 40] {
            for seed_data in [true, false] {
                let mut s = opened();
                if seed_data {
                    seed(&mut s, screen);
                } else {
                    s.active_screen = screen;
                }
                let page = Page::of(&mut s, width, 14);
                let nav = page.row(0);
                assert!(
                    nav.contains('←'),
                    "{label} @{width} (seeded={seed_data}): no way back: {nav:?}"
                );
                for (index, line) in page.non_empty() {
                    assert!(
                        UnicodeWidthStr::width(line) <= width as usize,
                        "{label} @{width} (seeded={seed_data}): row {index} overran \
                         the terminal: {line:?}"
                    );
                }
            }
        }
    }
}

// ---- 6. A click selects the row under the pointer ---------------------------

/// Clicking a painted row moves the page's cursor onto exactly that row, and
/// clicking it again moves nothing — the same idempotence a keyboard selection
/// has. This is the whole rule, checked black-box: the page is only ever read
/// through what it paints.
#[test]
fn a_row_click_selects_the_row_under_the_pointer() {
    for (screen, label) in pages_with_clickable_rows() {
        let mut s = opened();
        seed(&mut s, screen);
        let _ = Page::of(&mut s, 100, 20);
        let target = 2usize;
        let hits = s.screen_rows.clone();
        assert!(
            hits.len() > target,
            "{label}: the fixture painted {} rows, need > {target}",
            hits.len()
        );
        let (rect, _) = hits[target];

        let before = Page::of(&mut s, 100, 20);
        reduce(&mut s, click(rect.x + 1, rect.y));
        let after = Page::of(&mut s, 100, 20);
        assert_ne!(
            before.lines, after.lines,
            "{label}: clicking row {target} moved nothing"
        );

        // Clicking the same row again is a no-op: the cursor is already there.
        reduce(&mut s, click(rect.x + 1, rect.y));
        let again = Page::of(&mut s, 100, 20);
        assert_eq!(
            after.lines, again.lines,
            "{label}: a second click on the same row moved the cursor"
        );
    }
}

/// A wheel notch over a page moves the same cursor the arrow keys move: one
/// row for a list, so the wheel and the keyboard cannot disagree.
#[test]
fn a_wheel_notch_moves_the_same_cursor_the_keys_move() {
    for (screen, label) in pages_with_clickable_rows() {
        let mut keyboard = opened();
        seed(&mut keyboard, screen);
        let _ = Page::of(&mut keyboard, 100, 20);
        reduce(&mut keyboard, key(KeyCode::Down));
        let by_key = Page::of(&mut keyboard, 100, 20).lines;

        let mut wheeled = opened();
        seed(&mut wheeled, screen);
        let _ = Page::of(&mut wheeled, 100, 20);
        reduce(&mut wheeled, wheel(50, 10, false));
        let by_wheel = Page::of(&mut wheeled, 100, 20).lines;

        assert_eq!(
            by_key, by_wheel,
            "{label}: the wheel and ↓ landed on different rows"
        );
    }
}

/// A selection the user cannot see is a selection the user does not have: the
/// Issue page's cursor drags the window with it when it moves, exactly as the
/// other list pages' does.
#[test]
fn the_issue_page_keeps_its_cursor_on_screen() {
    let mut s = opened();
    seed(&mut s, Screen::Issue);
    let rows = s.background_task_labels.len();
    assert!(
        rows >= 8,
        "the fixture needs a list taller than the viewport"
    );
    // The page's own opener picks the first issue; stand in for it here.
    s.issue_selected = Some("bg-0".into());
    // A window too short for the whole list, so the cursor has to move it.
    let cursor_row = |page: &Page| {
        page.lines
            .iter()
            .position(|l| l.trim_start().starts_with("→"))
    };
    assert!(cursor_row(&Page::of(&mut s, 80, 10)).is_some(), "no cursor");

    for step in 0..rows {
        let page = Page::of(&mut s, 80, 10);
        assert!(
            cursor_row(&page).is_some(),
            "step {step}: the cursor left the screen entirely:\n{}",
            page.lines.join("\n")
        );
        reduce(&mut s, key(KeyCode::Down));
    }
}

// ---- helpers ---------------------------------------------------------------

/// Every page that paints the shell's `← <title>` header — which is now every
/// independent page: the last four to keep the pre-shell app frame (Tools,
/// Diff, Remote, Trace) draw the same header as the rest.
fn pages_with_a_back_glyph() -> Vec<(Screen, &'static str)> {
    vec![
        (Screen::Plan, "Plan"),
        (Screen::Goal, "Goal"),
        (Screen::Issue, "Issue"),
        (Screen::Team, "Team"),
        (Screen::ActivityList, "Background"),
        (Screen::Sessions, "Sessions"),
        (Screen::Help, "Help"),
        (Screen::Doctor, "Doctor"),
        (Screen::Context, "Context"),
        (Screen::Clean, "Clean"),
        (Screen::Tools, "Tools"),
        (Screen::Diff, "Diff"),
        (Screen::Remote, "Remote"),
        (Screen::Trace, "Trace"),
    ]
}

/// Pages whose body is a list of rows a user picks from.
///
/// Trace and Context are absent: driving them needs a loaded observability /
/// accounting snapshot, and a fixture that fabricates one would be asserting
/// against invented data. Their rows are wired through the same
/// `render_list_rows` call as these, so the shared arithmetic is covered here.
fn pages_with_clickable_rows() -> Vec<(Screen, &'static str)> {
    vec![
        (Screen::Sessions, "Sessions"),
        (Screen::Diff, "Diff"),
        (Screen::Tools, "Tools"),
        (Screen::Help, "Help"),
    ]
}

/// Pages whose seeded body is longer than a 12-row viewport.
fn overflowing_pages() -> Vec<(Screen, &'static str)> {
    vec![
        (Screen::Plan, "Plan"),
        (Screen::Help, "Help"),
        (Screen::Issue, "Issue"),
        (Screen::Sessions, "Sessions"),
    ]
}

/// Put the page under test into a state where it has something to show.
fn seed(state: &mut AppState, screen: Screen) {
    state.status = RuntimeStatus::Idle;
    match screen {
        Screen::Plan => {
            state.plan = Some(leveler_client_protocol::UiPlan {
                steps: (0..30)
                    .map(|i| leveler_client_protocol::UiPlanStep {
                        index: i,
                        description: format!("step {i} of the active plan, long enough to wrap"),
                        status: leveler_client_protocol::PlanStepStatus::Pending,
                    })
                    .collect(),
            });
        }
        Screen::Goal => {
            state.goal = "a bounded objective, long enough to need more than one line".into();
        }
        Screen::Issue => {
            for i in 0..10 {
                failed_task(state, &format!("bg-{i}"), &format!("sh -c 'exit {i}'"));
            }
        }
        Screen::ActivityList => {
            for i in 0..8 {
                state.background_task_labels.insert(
                    format!("bg-{i}"),
                    BackgroundTaskChrome::running(format!("serve --port {i}"), i as u64),
                );
            }
        }
        Screen::Sessions => {
            for i in 0..14 {
                state
                    .sessions
                    .push(summary(&format!("s{i}"), "a session goal"));
            }
        }
        Screen::Diff => {
            state.diff = Some(leveler_client_protocol::UiDiff {
                files: (0..6)
                    .map(|i| leveler_client_protocol::UiDiffFile {
                        path: format!("src/module_{i}.rs"),
                        added: i as u32,
                        removed: 1,
                        patch: None,
                    })
                    .collect(),
            });
        }
        Screen::Tools => {
            for i in 0..6 {
                let call = leveler_client_protocol::ToolCallId::new(format!("c{i}"));
                state.transcript.push_tool_started(
                    call.clone(),
                    "run_command".into(),
                    r#"{"program":"cargo","args":["test"]}"#.into(),
                    false,
                    0,
                    None,
                    None,
                );
                state
                    .transcript
                    .complete_tool(&call, true, "ok".into(), 3, None);
            }
        }
        _ => {}
    }
    state.active_screen = screen;
}
