//! Memory page responsiveness at real terminal sizes.
//!
//! Pinned behaviour:
//!
//! - The footer names every consent action (move, view, select, accept,
//!   reject, back) at 120×40, 80×24, 48×20 and narrower widths. A hint is
//!   wrapped onto another row rather than ellipsized, and the supported
//!   degradation never drops an action.
//! - Long memory text is wrapped by the detail page, never silently cut.
//! - A taller footer does not break list scrolling or the back affordance.

use leveler_client_protocol::{SessionId, UiMemoryCandidate};
use leveler_tui::Locale;
use leveler_tui::memory::MemoryView;
use leveler_tui::render::render;
use leveler_tui::screen::Screen;
use leveler_tui::state::{AppState, Boot};
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
            version: "0.1.0".into(),
            show_welcome: false,
            draft_path: None,
            history_path: None,
            context_window: 200_000,
            locale: Locale::Zh,
            untrusted_config: Vec::new(),
            model_notice: None,
            thinking: None,
        },
    )
}

fn candidate(id: &str, title: &str, body: &str) -> UiMemoryCandidate {
    UiMemoryCandidate {
        id: id.into(),
        scope: leveler_client_protocol::UiMemoryScope::Project,
        title: title.into(),
        body: body.into(),
        kind: "constraint".into(),
        source: "user_explicit".into(),
    }
}

fn review_state(count: usize) -> AppState {
    let mut state = opened();
    state.active_screen = Screen::Memory;
    state.memory.view = MemoryView::Review;
    state.memory.pending = (0..count)
        .map(|index| {
            candidate(
                &format!("m{index}"),
                &format!("约定 {index}：提交前先运行测试"),
                "提交前先运行 cargo test。",
            )
        })
        .collect();
    state.memory.sync_focus();
    state
}

fn frame(state: &mut AppState, w: u16, h: u16) -> Vec<String> {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| render(f, state)).unwrap();
    let buf = term.backend().buffer().clone();
    (0..buf.area.height)
        .map(|y| {
            let mut line = String::new();
            let mut x = 0u16;
            while x < buf.area.width {
                let sym = buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ");
                line.push_str(sym);
                // A double-width grapheme owns the next cell too; skip it so
                // the scan does not inject a phantom space between CJK chars.
                x += unicode_width::UnicodeWidthStr::width(sym).max(1) as u16;
            }
            line
        })
        .collect()
}

/// The rows below the last shell rule: the interaction hint, however many
/// rows it needed.
fn footer(lines: &[String]) -> Vec<String> {
    let last_rule = lines
        .iter()
        .rposition(|line| {
            let trimmed = line.trim();
            !trimmed.is_empty() && trimmed.chars().all(|c| c == '─')
        })
        .expect("the secondary shell draws a rule above its footer");
    lines[last_rule + 1..].to_vec()
}

fn gutter(width: u16) -> usize {
    match width {
        w if w >= 80 => 2,
        w if w >= 40 => 1,
        _ => 0,
    }
}

/// The consent page is only usable if the keys that consent are readable.
#[test]
fn every_consent_action_survives_the_footer_at_every_supported_width() {
    for (w, h) in [
        (120u16, 40u16),
        (80, 24),
        (48, 20),
        (40, 20),
        (32, 20),
        (24, 20),
        (16, 20),
    ] {
        let mut state = review_state(2);
        let lines = frame(&mut state, w, h);
        assert_eq!(lines.len(), h as usize, "w={w}: frame height");
        assert!(
            lines[0].contains("← 项目记忆"),
            "w={w}: back affordance missing:\n{}",
            lines.join("\n")
        );
        let footer = footer(&lines);
        let joined = footer.join("\n");
        for action in [
            "↑↓ 移动",
            "空格 选择",
            "Enter 查看",
            "A 接受",
            "R 拒绝",
            "Esc 返回",
        ] {
            assert!(
                joined.contains(action),
                "w={w}: {action:?} is not discoverable:\n{joined}\n--- frame ---\n{}",
                lines.join("\n")
            );
        }
        assert!(
            !joined.contains('…'),
            "w={w}: the footer hint was truncated:\n{joined}"
        );
        for row in &footer {
            assert!(
                UnicodeWidthStr::width(row.trim_end()) <= w as usize - gutter(w),
                "w={w}: footer row runs past the safe margin: {row:?}"
            );
        }
    }
}

/// The other memory panes use the same adaptive rule: at 24 columns their
/// action words move to a second row instead of disappearing.
#[test]
fn narrow_memory_panes_wrap_their_own_hints() {
    for (view, actions) in [
        (MemoryView::Home, vec!["↑↓ 选择", "Enter 打开", "Esc 返回"]),
        (
            MemoryView::Active,
            vec!["↑↓ 移动", "Enter 查看", "Esc 返回"],
        ),
        (MemoryView::Detail, vec!["↑↓ 滚动", "Esc 返回"]),
    ] {
        let mut state = review_state(1);
        state.memory.view = view;
        state.memory.focus = Some("m0".into());
        let lines = frame(&mut state, 24, 20);
        let joined = footer(&lines).join("\n");
        for action in actions {
            assert!(
                joined.contains(action),
                "{view:?}: {action:?} is not discoverable:\n{joined}\n--- frame ---\n{}",
                lines.join("\n")
            );
        }
        assert!(
            !joined.contains('…'),
            "{view:?}: the footer hint was truncated:\n{joined}"
        );
    }
}

/// A long record is readable in full: the detail page wraps it, and the title
/// is not repeated as body text.
#[test]
fn a_long_memory_is_wrapped_not_cut_in_the_detail_view() {
    let title =
        "所有对外 JSON 响应字段一律使用 snake_case 命名，并且不要在任何新接口里改成 camelCase";
    let mut state = opened();
    state.active_screen = Screen::Memory;
    state.memory.view = MemoryView::Detail;
    state.memory.detail_return = MemoryView::Review;
    state.memory.pending = vec![candidate("long", title, title)];
    state.memory.focus = Some("project:long".into());

    let lines = frame(&mut state, 48, 20);
    let joined = lines.join("\n");
    assert!(!joined.contains('…'), "content was ellipsized:\n{joined}");
    let flat: String = joined.chars().filter(|c| !c.is_whitespace()).collect();
    let wanted: String = title.chars().filter(|c| !c.is_whitespace()).collect();
    assert_eq!(
        flat.matches(&wanted).count(),
        1,
        "the title must appear once and in full:\n{joined}"
    );
    // The hint keeps the two detail actions on screen.
    let footer = footer(&lines).join("\n");
    assert!(footer.contains("↑↓ 滚动"), "{footer}");
    assert!(footer.contains("Esc 返回"), "{footer}");
}

/// A wrapped footer shrinks the body by a row; the list still scrolls to the
/// focused record instead of hiding it.
#[test]
fn a_wrapped_footer_does_not_break_list_scrolling() {
    let mut state = review_state(30);
    state.memory.move_focus(29);
    assert_eq!(state.memory.focused_id(), Some("project:m29"));
    let lines = frame(&mut state, 48, 20);
    let text = lines.join("\n");
    assert!(
        text.contains("约定 29"),
        "the focused record must be visible:\n{text}"
    );
    assert!(
        !text.contains("约定 0："),
        "the list must have scrolled past the first record:\n{text}"
    );
}
