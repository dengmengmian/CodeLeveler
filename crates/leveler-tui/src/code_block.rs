//! Conversation CodeBlock — for *reading* code, not diffs.
//!
//! Strategy:
//! - **Short** snippets (≤4 lines, no path): inline style — indent + highlight, no box/bg.
//! - **Long / file** content: light header (path:line), complete scrollable body; no solid black fill.
//!
//! Diffs never enter here — use [`crate::diff_view`].

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::render::truncate_display;
use crate::theme::Theme;

/// Lines at or below this count render as inline (no container), unless a path header exists.
pub const SHORT_CODE_MAX_LINES: usize = 4;
/// Parsed fence info string (`rust`, `go path/file.go:10`, `webhook.go:792`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FenceMeta {
    pub lang: Option<String>,
    pub title: Option<String>,
}

/// Parse a fenced code-block info string into language + optional path header.
pub fn parse_fence_info(raw: &str) -> FenceMeta {
    let raw = raw.trim();
    if raw.is_empty() {
        return FenceMeta::default();
    }
    if let Some((lang, path)) = raw.split_once(':')
        && !lang.is_empty()
        && !path.is_empty()
        && !lang.contains([' ', '/', '\\'])
        && looks_like_path(path)
    {
        return FenceMeta {
            lang: Some(lang.to_string()),
            title: Some(compact_title(path)),
        };
    }
    if let Some((first, rest)) = raw.split_once(char::is_whitespace) {
        let rest = rest.trim();
        if looks_like_lang(first) && !rest.is_empty() {
            return FenceMeta {
                lang: Some(first.to_string()),
                title: Some(compact_title(rest)),
            };
        }
    }
    if looks_like_path(raw) {
        return FenceMeta {
            lang: None,
            title: Some(compact_title(raw)),
        };
    }
    FenceMeta {
        lang: Some(raw.to_string()),
        title: None,
    }
}

fn looks_like_lang(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 16
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '#')
}

fn looks_like_path(s: &str) -> bool {
    let base = s.split(':').next().unwrap_or(s);
    base.contains('/')
        || base.contains('\\')
        || base.contains('.')
        || base.ends_with(".go")
        || base.ends_with(".rs")
        || base.ends_with(".ts")
        || base.ends_with(".py")
}

fn compact_title(path: &str) -> String {
    let path = path.trim().trim_matches('`');
    if let Some((file, line)) = path.rsplit_once(':')
        && line.chars().all(|c| c.is_ascii_digit())
        && !line.is_empty()
    {
        let name = std::path::Path::new(file)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(file);
        return format!("{name}:{line}");
    }
    std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
        .to_string()
}

/// Whether this fence should use the light container (vs inline).
pub fn needs_container(title: Option<&str>, line_count: usize) -> bool {
    title.is_some() || line_count > SHORT_CODE_MAX_LINES
}

/// Render code for Conversation: short = inline; long/file = light container.
///
/// No solid black fill. Diffs must not call this.
pub fn render_code_block<F>(
    title: Option<&str>,
    lang: Option<&str>,
    line_count: usize,
    width: usize,
    theme: &Theme,
    mut line_render: F,
    out: &mut Vec<Line<'static>>,
) where
    F: FnMut(usize) -> Vec<Vec<Span<'static>>>,
{
    let width = width.max(8);
    if !needs_container(title, line_count) {
        render_inline(line_count, theme, &mut line_render, out);
        return;
    }
    render_container(title, lang, line_count, width, theme, line_render, out);
}

/// Inline: two-space indent, syntax only, conversation background.
fn render_inline<F>(
    line_count: usize,
    theme: &Theme,
    line_render: &mut F,
    out: &mut Vec<Line<'static>>,
) where
    F: FnMut(usize) -> Vec<Vec<Span<'static>>>,
{
    for idx in 0..line_count {
        for row in line_render(idx) {
            let mut spans = vec![Span::styled(
                "  ",
                Style::default().fg(theme.text.secondary),
            )];
            spans.extend(row);
            out.push(Line::from(spans));
        }
    }
}

/// Light container: path header + complete body. Gutter is border only (no fill).
fn render_container<F>(
    title: Option<&str>,
    lang: Option<&str>,
    line_count: usize,
    width: usize,
    theme: &Theme,
    mut line_render: F,
    out: &mut Vec<Line<'static>>,
) where
    F: FnMut(usize) -> Vec<Vec<Span<'static>>>,
{
    let header = title
        .map(str::to_string)
        .or_else(|| lang.map(|l| l.to_string()))
        .unwrap_or_else(|| "code".to_string());
    out.push(Line::from(vec![
        Span::styled("· ", Style::default().fg(theme.border.normal)),
        Span::styled(
            truncate_display(&header, width.saturating_sub(2).max(4)),
            Style::default()
                .fg(theme.text.secondary)
                .add_modifier(Modifier::BOLD),
        ),
    ]));

    // The conversation viewport owns scrolling. A separate source-line fold or
    // screen-row cap here makes wrapped code unreachable at narrow widths.
    for idx in 0..line_count {
        for row in line_render(idx) {
            let mut spans = vec![Span::styled("  ", Style::default().fg(theme.border.normal))];
            spans.extend(row);
            out.push(Line::from(spans));
        }
    }
}

/// Map syntect RGB into a low-noise palette — **no background fill**.
pub fn tone_code_rgb(rgb: (u8, u8, u8), theme: &Theme) -> Style {
    let (r, g, b) = rgb;
    let max = r.max(g).max(b) as i16;
    let min = r.min(g).min(b) as i16;
    let chroma = max - min;
    let lum = (r as u16 + g as u16 + b as u16) / 3;
    let fg = if chroma > 40 || lum > 160 {
        theme.text.primary
    } else {
        theme.text.secondary
    };
    Style::default().fg(fg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapped_code_keeps_every_screen_row() {
        let theme = Theme::no_color();
        for source_lines in [1, 4, 6, 40] {
            let mut out = Vec::new();
            render_code_block(
                None,
                Some("rust"),
                source_lines,
                48,
                &theme,
                |i| {
                    (0..5)
                        .map(|j| vec![Span::raw(format!("source-{i}-wrapped-{j}"))])
                        .collect()
                },
                &mut out,
            );
            let text: Vec<String> = out
                .iter()
                .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
                .collect();
            for i in 0..source_lines {
                for j in 0..5 {
                    assert!(
                        text.iter()
                            .any(|line| line.contains(&format!("source-{i}-wrapped-{j}"))),
                        "missing source {i} screen row {j} at 48 columns"
                    );
                }
            }
        }
    }

    #[test]
    fn parse_lang_and_path_with_line() {
        let m = parse_fence_info("go internal/admin/webhook.go:792");
        assert_eq!(m.lang.as_deref(), Some("go"));
        assert_eq!(m.title.as_deref(), Some("webhook.go:792"));
    }

    #[test]
    fn short_code_is_inline_without_box() {
        let theme = Theme::no_color();
        let mut out = Vec::new();
        render_code_block(
            None,
            Some("rust"),
            2,
            40,
            &theme,
            |i| vec![vec![Span::raw(format!("let x = {i};"))]],
            &mut out,
        );
        let text: Vec<String> = out
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(text.len(), 2, "{text:?}");
        assert!(
            !text
                .iter()
                .any(|l| l.contains('·') || l.contains('┌') || l.contains('│')),
            "short code must be inline: {text:?}"
        );
        assert!(text[0].starts_with("  "), "{text:?}");
    }

    #[test]
    fn file_code_gets_light_header_not_black_box() {
        let theme = Theme::no_color();
        let mut out = Vec::new();
        render_code_block(
            Some("webhook.go:792"),
            Some("go"),
            30,
            60,
            &theme,
            |i| vec![vec![Span::raw(format!("line {i}"))]],
            &mut out,
        );
        let text: Vec<String> = out
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(
            text.iter().any(|l| l.contains("webhook.go:792")),
            "{text:?}"
        );
        assert_eq!(text.len(), 31, "header plus all source lines");
        assert!(
            text.iter().any(|l| l.contains("line 15")),
            "middle must remain readable"
        );
        assert!(
            !text
                .iter()
                .any(|l| l.starts_with('┌') || l.starts_with('┃')),
            "no heavy black box: {text:?}"
        );
    }
}

#[cfg(test)]
pub(crate) mod visibility_fixtures {
    pub struct Case {
        pub name: &'static str,
        pub markdown: String,
        pub bodies: Vec<String>,
    }

    fn rows(n: usize, payload: &str) -> String {
        (0..n)
            .map(|i| format!("ROW-{i:04} {payload} END-{i:04}\n"))
            .collect()
    }

    fn fence(name: &'static str, body: String, closed: bool) -> Case {
        Case {
            name,
            markdown: format!("```rust\n{body}{}", if closed { "```\n" } else { "" }),
            bodies: vec![body],
        }
    }

    pub fn cases() -> Vec<Case> {
        let six: String = (0..6)
            .map(|i| {
                let start = format!("ROW-{i:04} ");
                let end = if i == 5 {
                    " CRITICAL-INNER-END".to_owned()
                } else {
                    format!(" END-{i:04}")
                };
                format!(
                    "{start}{}{end}\n",
                    "x".repeat(180 - start.len() - end.len())
                )
            })
            .collect();
        let mut cases = vec![
            fence("A-six-long-lines", six, true),
            fence("B-twenty", rows(20, "let value = 42;"), true),
            fence("C-hundred", rows(100, "let value = 42;"), true),
            fence("D-thousand", rows(1000, "let value = 42;"), true),
            fence("E-hundred-kib", rows(1024, &"x".repeat(81)), true),
            fence("F-single-long-line", rows(1, &"x".repeat(10_000)), true),
            fence(
                "G-cjk",
                rows(20, "// 中文注释：完整展示每一行，不省略正文。"),
                true,
            ),
            fence(
                "H-graphemes",
                rows(20, "// 🦀 👩‍💻 e\u{301} o\u{308} 中文"),
                true,
            ),
            fence("I-unclosed", rows(100, "let value = 42;"), false),
        ];
        let first = rows(20, "first fence");
        let second = rows(20, "second fence");
        cases.push(Case {
            name: "J-multiple-fences",
            markdown: format!("```rust\n{first}```\n\n```text\n{second}```\n"),
            bodies: vec![first, second],
        });
        let body = rows(20, "ordinary code");
        cases.push(Case {
            name: "K-surrounding-markdown",
            markdown: format!(
                "BEFORE-CODE-PARAGRAPH\n\n```rust\n{body}```\n\nAFTER-CODE-PARAGRAPH\n"
            ),
            bodies: vec![body],
        });
        cases
    }

    /// Check the actual viewport cells reached by the real paging reducer.
    /// Overlapping pages are compared with the authoritative projection once,
    /// so an unreachable, reordered, or duplicated screen row cannot pass.
    pub fn assert_paging(state: &mut crate::state::AppState, label: &str) {
        use crate::action::Action;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use ratatui::{Terminal, backend::TestBackend, layout::Rect};
        use unicode_width::UnicodeWidthStr;

        for (cols, rows) in [(48, 20), (80, 24), (120, 40), (48, 20)] {
            crate::reducer::reduce(state, Action::Resize(cols, rows));
            let area = Rect::new(0, 0, cols, rows);
            let content = crate::conversation::geometry::content_rect(area);
            state.conv.rect = Some((content.x, content.y, content.width, content.height));
            state.conv.auto_scroll = false;
            state.conv.scroll = 0;
            let projection = state.conversation_lines(content.width as usize);
            let expected: Vec<String> = projection
                .iter()
                .map(|line| crate::selection::line_to_plain(line).trim_end().to_owned())
                .collect();
            let mut terminal = Terminal::new(TestBackend::new(cols, rows)).unwrap();
            let mut visited = Vec::new();
            loop {
                terminal
                    .draw(|frame| crate::conversation::viewport::render(frame, area, state))
                    .unwrap();
                assert!(
                    std::rc::Rc::ptr_eq(
                        &projection,
                        &state.conversation_lines(content.width as usize)
                    ),
                    "{label} scroll/repaint must reuse the same full projection"
                );
                let buffer = terminal.backend().buffer();
                let top_pad = (content.height as usize).saturating_sub(expected.len());
                for (row, expected_row) in expected
                    .iter()
                    .enumerate()
                    .take((state.conv.scroll + content.height as usize).min(expected.len()))
                    .skip(visited.len().max(state.conv.scroll))
                {
                    let y = content.y + (row - state.conv.scroll + top_pad) as u16;
                    let mut text = String::new();
                    let mut x = content.x;
                    while x < content.right() {
                        let symbol = buffer[(x, y)].symbol();
                        text.push_str(symbol);
                        x += symbol.width().max(1) as u16;
                    }
                    assert_eq!(
                        text.trim_end(),
                        expected_row,
                        "{label} {cols}x{rows} row {row}"
                    );
                    visited.push(text.trim_end().to_owned());
                }
                let before = state.conv.scroll;
                crate::reducer::reduce(
                    state,
                    Action::Key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE)),
                );
                if state.conv.scroll == before {
                    break;
                }
            }
            assert_eq!(
                visited, expected,
                "{label} every visual row must be pageable at {cols}x{rows}"
            );
            while state.conv.scroll > 0 {
                let before = state.conv.scroll;
                crate::reducer::reduce(
                    state,
                    Action::Key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)),
                );
                assert!(
                    state.conv.scroll < before,
                    "{label} PageUp must reach the beginning"
                );
            }
            if expected.len() > content.height as usize {
                assert!(!state.conv.auto_scroll, "reading stays pinned");
            }
        }
    }
}
