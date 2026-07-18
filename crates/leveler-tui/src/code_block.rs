//! Conversation CodeBlock — for *reading* code, not diffs.
//!
//! Strategy:
//! - **Short** snippets (≤4 lines, no path): inline style — indent + highlight, no box/bg.
//! - **Long / file** content: light header (path:line) + fold; no solid black fill.
//!
//! Diffs never enter here — use [`crate::diff_view`].

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::render::truncate_display;
use crate::theme::Theme;

/// Lines at or below this count render as inline (no container), unless a path header exists.
pub const SHORT_CODE_MAX_LINES: usize = 4;
/// Visible body lines before folding kicks in (header is extra).
pub const CODE_BLOCK_MAX_BODY: usize = 14;
const FOLD_HEAD: usize = 6;
const FOLD_TAIL: usize = 4;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FoldPart {
    Range { start: usize, end: usize },
    Omitted { count: usize },
}

/// Plan a height-capped, middle-folded view of a code fence body.
pub fn fold_plan(line_count: usize) -> Vec<FoldPart> {
    if line_count == 0 {
        return Vec::new();
    }
    if line_count <= CODE_BLOCK_MAX_BODY {
        return vec![FoldPart::Range {
            start: 0,
            end: line_count,
        }];
    }
    let head = FOLD_HEAD.min(line_count);
    let tail = FOLD_TAIL.min(line_count.saturating_sub(head));
    let omitted = line_count.saturating_sub(head + tail);
    if omitted == 0 {
        return vec![FoldPart::Range {
            start: 0,
            end: line_count,
        }];
    }
    vec![
        FoldPart::Range {
            start: 0,
            end: head,
        },
        FoldPart::Omitted { count: omitted },
        FoldPart::Range {
            start: line_count - tail,
            end: line_count,
        },
    ]
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
            let mut spans = vec![Span::styled("  ", Style::default().fg(theme.muted))];
            spans.extend(row);
            out.push(Line::from(spans));
        }
    }
}

/// Light container: path header + optional fold. Gutter is border only (no fill).
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
        Span::styled("· ", Style::default().fg(theme.border)),
        Span::styled(
            truncate_display(&header, width.saturating_sub(2).max(4)),
            Style::default()
                .fg(theme.muted)
                .add_modifier(Modifier::BOLD),
        ),
    ]));

    let plan = fold_plan(line_count);
    let mut body_rows = 0usize;
    for part in plan {
        match part {
            FoldPart::Range { start, end } => {
                for idx in start..end {
                    if body_rows >= CODE_BLOCK_MAX_BODY {
                        break;
                    }
                    for row in line_render(idx) {
                        if body_rows >= CODE_BLOCK_MAX_BODY {
                            break;
                        }
                        let mut spans = vec![Span::styled("  ", Style::default().fg(theme.border))];
                        spans.extend(row);
                        out.push(Line::from(spans));
                        body_rows += 1;
                    }
                }
            }
            FoldPart::Omitted { count } => {
                out.push(Line::from(vec![
                    Span::styled("  ", Style::default().fg(theme.border)),
                    Span::styled(
                        format!("⋮ {count} lines omitted"),
                        Style::default().fg(theme.muted),
                    ),
                ]));
            }
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
        theme.text
    } else {
        theme.muted
    };
    Style::default().fg(fg)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(text.iter().any(|l| l.contains("lines omitted")), "{text:?}");
        assert!(
            !text
                .iter()
                .any(|l| l.starts_with('┌') || l.starts_with('┃')),
            "no heavy black box: {text:?}"
        );
    }

    #[test]
    fn fold_long_block_omits_middle() {
        let p = fold_plan(40);
        assert!(p.iter().any(|x| matches!(x, FoldPart::Omitted { .. })));
    }
}
