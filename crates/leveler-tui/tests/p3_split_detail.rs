//! P3 geometry and reading contracts for List+Detail / Split pages.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use leveler_client_protocol::{MessageId, RuntimeEvent, SessionId, ToolCallId, UiDiff, UiDiffFile};
use leveler_tui::action::Action;
use leveler_tui::layout::{SplitAxis, SplitFocus, SplitSpec, split_pane};
use leveler_tui::reducer::reduce;
use leveler_tui::render::render;
use leveler_tui::screen::Screen;
use leveler_tui::state::{AppState, BackgroundTaskChrome, Boot};
use leveler_tui::theme::Theme;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn opened() -> AppState {
    AppState::new(
        Theme::dark(),
        Boot {
            session_id: SessionId::new("p3"),
            user: "u".into(),
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

fn key(code: KeyCode) -> Action {
    Action::Key(KeyEvent::new(code, KeyModifiers::empty()))
}

fn text(state: &mut AppState, w: u16, h: u16) -> String {
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, state)).unwrap();
    let buf = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..buf.area.height {
        let mut x = 0;
        while x < buf.area.width {
            let sym = buf.cell((x, y)).map(|cell| cell.symbol()).unwrap_or(" ");
            out.push_str(sym);
            x += unicode_width::UnicodeWidthStr::width(sym).max(1) as u16;
        }
        out.push('\n');
    }
    out
}

fn mouse(kind: MouseEventKind, x: u16, y: u16) -> Action {
    Action::Mouse(MouseEvent {
        kind,
        column: x,
        row: y,
        modifiers: KeyModifiers::empty(),
    })
}

fn layout_row(spec: SplitSpec, w: u16, h: u16) -> Option<leveler_tui::layout::SplitLayout> {
    split_pane(
        ratatui::layout::Rect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        },
        spec,
    )
}

fn compact(text: &str) -> String {
    text.chars().filter(|ch| !ch.is_whitespace()).collect()
}

fn created_patch(lines: usize) -> String {
    let content: String = (1..=lines)
        .map(|line| format!("    expectCookie{line}(t)\n"))
        .collect();
    let hunks = leveler_tools::tools::applied_diff::whole_file_hunks("", &content);
    leveler_tools::tools::applied_diff::unified_diff("a.rs", &hunks).expect("patch")
}

#[test]
fn diff_wide_is_dual_and_narrow_enters_and_returns_from_the_patch() {
    let mut s = opened();
    s.active_screen = Screen::Diff;
    s.diff = Some(UiDiff {
        files: vec![
            UiDiffFile {
                path: "a.rs".into(),
                added: 200,
                removed: 0,
                patch: Some(created_patch(200)),
            },
            UiDiffFile {
                path: "b.rs".into(),
                added: 3,
                removed: 0,
                patch: Some(created_patch(3)),
            },
        ],
    });
    let wide = text(&mut s, 120, 40);
    assert!(wide.contains("a.rs"), "{wide}");
    assert!(wide.contains("expectCookie1"), "wide patch visible: {wide}");
    assert_eq!(s.page_focus, SplitFocus::Primary);

    // Narrow: the list keeps the whole content until Enter opens the patch.
    let narrow = text(&mut s, 80, 24);
    assert!(narrow.contains("a.rs"), "{narrow}");
    assert!(
        !narrow.contains("expectCookie1"),
        "narrow list is single: {narrow}"
    );
    reduce(&mut s, key(KeyCode::Enter));
    assert_eq!(s.page_focus, SplitFocus::Detail);
    let detail = text(&mut s, 80, 24);
    assert!(detail.contains("expectCookie1"), "{detail}");
    reduce(&mut s, key(KeyCode::Down));
    assert_eq!(s.diff_selected, 0, "detail arrows scroll, not select");
    reduce(&mut s, key(KeyCode::Esc));
    assert_eq!(s.page_focus, SplitFocus::Primary);
    let list = text(&mut s, 80, 24);
    assert!(list.contains("b.rs"), "{list}");
}

#[test]
fn diff_detail_can_reach_the_last_line_at_both_widths() {
    let mut s = opened();
    s.active_screen = Screen::Diff;
    s.diff = Some(UiDiff {
        files: vec![UiDiffFile {
            path: "a.rs".into(),
            added: 200,
            removed: 0,
            patch: Some(created_patch(200)),
        }],
    });
    for (w, h) in [(120u16, 40u16), (80, 24)] {
        let _ = text(&mut s, w, h);
        reduce(&mut s, key(KeyCode::Enter));
        for _ in 0..30 {
            reduce(&mut s, key(KeyCode::PageDown));
        }
        let tail = text(&mut s, w, h);
        assert!(
            tail.contains("expectCookie200"),
            "last patch line at {w}x{h}:\n{tail}"
        );
        reduce(&mut s, key(KeyCode::Esc));
    }
}

#[test]
fn tools_detail_scrolls_long_output_and_returns_to_the_list() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AssistantMessageStarted {
            message_id: MessageId::new("m0"),
        }),
    );
    let id = ToolCallId::new("call-1");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ToolCallStarted {
            id: id.clone(),
            name: "run_command".into(),
            arguments: serde_json::json!({"program":"sh","args":["-c","seq 1 100"]}).to_string(),
            parallel: false,
            model_step: Some(0),
            answer_effect: None,
        }),
    );
    let preview: String = (1..=100).map(|line| format!("line-{line}\n")).collect();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            id,
            ok: true,
            preview,
            duration_ms: 5,
            applied_diff: None,
            exit_code: Some(0),
            stop: None,
        }),
    );
    s.active_screen = Screen::Tools;
    let wide = text(&mut s, 120, 40);
    assert!(wide.contains("run_command"), "{wide}");
    assert!(wide.contains("line-1"), "detail visible: {wide}");
    reduce(&mut s, key(KeyCode::Enter));
    for _ in 0..20 {
        reduce(&mut s, key(KeyCode::PageDown));
    }
    let tail = text(&mut s, 120, 40);
    assert!(tail.contains("line-100"), "{tail}");
    reduce(&mut s, key(KeyCode::Esc));
    assert_eq!(s.page_focus, SplitFocus::Primary);

    let narrow = text(&mut s, 80, 24);
    assert!(!narrow.contains("line-100"), "list-only narrow: {narrow}");
    reduce(&mut s, key(KeyCode::Enter));
    for _ in 0..20 {
        reduce(&mut s, key(KeyCode::PageDown));
    }
    let tail = text(&mut s, 80, 24);
    assert!(tail.contains("line-100"), "{tail}");
}

#[test]
fn settings_detail_is_a_real_focusable_pane_and_escape_returns() {
    let mut s = opened();
    s.active_screen = Screen::Settings;
    s.settings.selected = Some("model.session".into());
    let list = compact(&text(&mut s, 80, 24));
    assert!(list.contains("当前模型"), "{list}");
    reduce(&mut s, key(KeyCode::Tab));
    assert_eq!(s.page_focus, SplitFocus::Detail);
    let detail = compact(&text(&mut s, 80, 24));
    assert!(detail.contains("生效时间"), "{detail}");
    reduce(&mut s, key(KeyCode::Esc));
    assert_eq!(s.page_focus, SplitFocus::Primary);
    let back = compact(&text(&mut s, 80, 24));
    assert!(back.contains("当前模型"), "{back}");
}

#[test]
fn tasks_fact_pane_scrolls_and_escape_returns_to_the_list() {
    let mut s = opened();
    s.active_screen = Screen::Tasks;
    let mut chrome = BackgroundTaskChrome::running("worker --job 1", 0);
    chrome.output = (1..=100)
        .map(|line| format!("[stdout] line-{line}\n"))
        .collect();
    s.background_task_labels.insert("bg-1".into(), chrome);
    let list = text(&mut s, 120, 40);
    assert!(list.contains("worker --job 1"), "{list}");
    s.tasks.detail_open = true;
    s.page_focus = SplitFocus::Detail;
    for _ in 0..20 {
        reduce(&mut s, key(KeyCode::PageDown));
    }
    let detail = text(&mut s, 120, 40);
    assert!(detail.contains("详情 · 后台"), "{detail}");
    reduce(&mut s, key(KeyCode::Esc));
    assert_eq!(s.page_focus, SplitFocus::Primary);
    assert!(!s.tasks.detail_open);
}

fn diff_three_files() -> UiDiff {
    UiDiff {
        files: vec![
            UiDiffFile {
                path: "a.rs".into(),
                added: 200,
                removed: 0,
                patch: Some(created_patch(200)),
            },
            UiDiffFile {
                path: "b.rs".into(),
                added: 3,
                removed: 0,
                patch: Some(created_patch(3)),
            },
            UiDiffFile {
                path: "c.rs".into(),
                added: 2,
                removed: 0,
                patch: Some(created_patch(2)),
            },
        ],
    }
}

#[test]
fn split_geometry_holds_across_the_named_terminal_sizes() {
    let specs = [
        (
            "diff",
            SplitSpec {
                axis: SplitAxis::Horizontal,
                primary_pct: 32,
                primary_min: 22,
                detail_min: 40,
                min_dual_width: 78,
                min_dual_height: 8,
            },
            vec![
                (120u16, 40u16, true),
                (100, 30, true),
                (80, 24, false),
                (60, 20, false),
                (40, 16, false),
            ],
        ),
        (
            "tools",
            SplitSpec {
                axis: SplitAxis::Horizontal,
                primary_pct: 40,
                primary_min: 24,
                detail_min: 30,
                min_dual_width: 76,
                min_dual_height: 8,
            },
            vec![
                (120u16, 40u16, true),
                (100, 30, true),
                (80, 24, true),
                (60, 20, false),
                (40, 16, false),
            ],
        ),
        (
            "settings",
            SplitSpec {
                axis: SplitAxis::Horizontal,
                primary_pct: 44,
                primary_min: 24,
                detail_min: 34,
                min_dual_width: 84,
                min_dual_height: 8,
            },
            vec![
                (120u16, 40u16, true),
                (100, 30, true),
                (80, 24, false),
                (60, 20, false),
                (40, 16, false),
            ],
        ),
        (
            "team",
            SplitSpec {
                axis: SplitAxis::Horizontal,
                primary_pct: 38,
                primary_min: 18,
                detail_min: 28,
                min_dual_width: 72,
                min_dual_height: 8,
            },
            vec![
                (120u16, 40u16, true),
                (100, 30, true),
                (80, 24, true),
                (60, 20, false),
                (40, 16, false),
            ],
        ),
        (
            "background",
            SplitSpec {
                axis: SplitAxis::Vertical,
                primary_pct: 33,
                primary_min: 1,
                detail_min: 2,
                min_dual_width: 1,
                min_dual_height: 4,
            },
            vec![
                (120u16, 40u16, true),
                (100, 30, true),
                (80, 24, true),
                (60, 20, true),
                (40, 16, true),
            ],
        ),
    ];
    for (name, spec, cases) in specs {
        for (w, h, dual) in cases {
            // The page content starts one gutter inside the terminal and
            // loses two header and two footer rows; use a conservative probe
            // here and exact geometry in the page tests.
            let content_w = w.saturating_sub(4);
            let content_h = h.saturating_sub(4);
            let got = layout_row(spec, content_w, content_h);
            assert_eq!(
                got.is_some(),
                dual,
                "{name} at {w}x{h} (content {content_w}x{content_h})"
            );
            if let Some(split) = got {
                match spec.axis {
                    SplitAxis::Horizontal => {
                        assert_eq!(
                            split.primary.x + split.primary.width,
                            split.divider.x,
                            "{name}"
                        );
                        assert_eq!(split.divider.x + 1, split.detail.x, "{name}");
                        assert_eq!(split.detail.x + split.detail.width, content_w, "{name}");
                    }
                    SplitAxis::Vertical => {
                        assert_eq!(
                            split.primary.y + split.primary.height,
                            split.divider.y,
                            "{name}"
                        );
                        assert_eq!(split.divider.y + 1, split.detail.y, "{name}");
                        assert_eq!(split.detail.y + split.detail.height, content_h, "{name}");
                    }
                }
                assert!(
                    split.primary.width >= spec.primary_min.min(content_w),
                    "{name}"
                );
                assert!(
                    split.detail.width >= spec.detail_min.min(content_w),
                    "{name}"
                );
            }
        }
    }
}

#[test]
fn diff_pointer_targets_the_pane_under_it_and_the_divider_is_inert() {
    let mut s = opened();
    s.active_screen = Screen::Diff;
    s.diff = Some(diff_three_files());
    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, &mut s)).unwrap();
    let first_row = s.screen_rows[0].0;
    let second_row = s.screen_rows[1].0;

    // The left pane owns row clicks and keeps the row hit inside itself.
    reduce(
        &mut s,
        mouse(
            MouseEventKind::Down(MouseButton::Left),
            second_row.x + 2,
            second_row.y,
        ),
    );
    assert_eq!(s.diff_selected, 1);
    assert_eq!(s.page_focus, SplitFocus::Primary);
    let selected = s.diff_selected;

    // The detail pane owns wheel events and never changes list selection.
    let buf = terminal.backend().buffer().clone();
    let divider_x = (10..110u16)
        .find(|x| {
            buf.cell((*x, first_row.y))
                .is_some_and(|cell| cell.symbol() == "│")
        })
        .expect("the split paints its divider");
    // A divider click while the list owns focus must not hand it to the detail.
    reduce(
        &mut s,
        mouse(
            MouseEventKind::Down(MouseButton::Left),
            divider_x,
            first_row.y,
        ),
    );
    assert_eq!(s.diff_selected, selected, "divider is not a row");
    assert_eq!(
        s.page_focus,
        SplitFocus::Primary,
        "divider click changes neither pane's focus"
    );

    // Hover follows the pointer without stealing keyboard focus.
    reduce(
        &mut s,
        mouse(MouseEventKind::Moved, second_row.x + 2, second_row.y),
    );
    assert_eq!(s.page_focus, SplitFocus::Primary);
    reduce(&mut s, mouse(MouseEventKind::Moved, 100, first_row.y));
    assert_eq!(
        s.page_focus,
        SplitFocus::Primary,
        "detail hover took the keys"
    );

    reduce(
        &mut s,
        mouse(MouseEventKind::Down(MouseButton::Left), 100, first_row.y),
    );
    assert_eq!(s.page_focus, SplitFocus::Detail);
    reduce(
        &mut s,
        mouse(MouseEventKind::Moved, second_row.x + 2, second_row.y),
    );
    assert_eq!(
        s.page_focus,
        SplitFocus::Detail,
        "list hover must not take focus from the detail"
    );
    reduce(&mut s, mouse(MouseEventKind::Moved, 100, first_row.y));

    let before = s.page_scroll;
    reduce(&mut s, mouse(MouseEventKind::ScrollDown, 100, first_row.y));
    assert_eq!(
        s.diff_selected, selected,
        "detail wheel must not move the list"
    );
    assert_eq!(s.page_focus, SplitFocus::Detail);
    assert!(s.page_scroll > before, "detail wheel scrolls the patch");

    reduce(
        &mut s,
        mouse(
            MouseEventKind::Down(MouseButton::Left),
            divider_x,
            first_row.y,
        ),
    );
    assert_eq!(s.diff_selected, selected, "divider is not a row");
    assert_eq!(s.page_focus, SplitFocus::Detail, "divider is inert");

    // Resize and back: the detail offset is clamped and the object survives.
    let _ = text(&mut s, 80, 24);
    let _ = text(&mut s, 120, 40);
    assert_eq!(s.diff_selected, selected);
}
