//! Independent DiffView — comparison UI, not a CodeBlock.
//!
//! Diff is for *what changed*, not full-file reading. No solid black fill:
//! conversation background stays; only +/- lines get soft red/green text.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::render::{truncate_display, wrap};
use crate::theme::Theme;

/// Max body lines shown before middle fold (header is extra).
const DIFF_MAX_BODY: usize = 10;
const DIFF_HEAD: usize = 5;
const DIFF_TAIL: usize = 3;

/// One logical line in a unified diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffLineKind {
    Meta,
    HunkHeader,
    Context,
    Add,
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub text: String,
}

/// Parsed unified-diff presentation model.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiffView {
    pub file: String,
    pub start_line: Option<u32>,
    pub added: usize,
    pub removed: usize,
    pub lines: Vec<DiffLine>,
}

impl DiffView {
    /// Parse a unified-diff body (optionally with `***` patch headers).
    pub fn parse(src: &str, title_hint: Option<&str>) -> Self {
        let mut file = title_hint.unwrap_or("diff").to_string();
        let mut start_line = None;
        let mut added = 0usize;
        let mut removed = 0usize;
        let mut lines = Vec::new();

        for raw in src.lines() {
            if let Some(path) = raw
                .strip_prefix("+++ b/")
                .or_else(|| raw.strip_prefix("+++ "))
                .or_else(|| raw.strip_prefix("*** Update File: "))
                .or_else(|| raw.strip_prefix("*** Add File: "))
            {
                let p = path.trim();
                if p != "/dev/null" && !p.is_empty() {
                    file = compact_file(p);
                }
                lines.push(DiffLine {
                    kind: DiffLineKind::Meta,
                    text: raw.to_string(),
                });
                continue;
            }
            if raw.starts_with("--- ") || raw.starts_with("diff --git") {
                lines.push(DiffLine {
                    kind: DiffLineKind::Meta,
                    text: raw.to_string(),
                });
                continue;
            }
            if let Some(rest) = raw.strip_prefix("@@") {
                // @@ -a,b +c,d @@ optional
                if let Some(plus) = rest.find('+') {
                    let after = &rest[plus + 1..];
                    let num: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
                    if let Ok(n) = num.parse::<u32>() {
                        start_line = Some(n);
                    }
                }
                lines.push(DiffLine {
                    kind: DiffLineKind::HunkHeader,
                    text: raw.to_string(),
                });
                continue;
            }
            if raw.starts_with('+') && !raw.starts_with("+++") {
                added += 1;
                lines.push(DiffLine {
                    kind: DiffLineKind::Add,
                    text: raw.to_string(),
                });
                continue;
            }
            if raw.starts_with('-') && !raw.starts_with("---") {
                removed += 1;
                lines.push(DiffLine {
                    kind: DiffLineKind::Remove,
                    text: raw.to_string(),
                });
                continue;
            }
            lines.push(DiffLine {
                kind: DiffLineKind::Context,
                text: raw.to_string(),
            });
        }

        if file == "diff" {
            // Fallback: first non-meta path-looking line.
            if let Some(p) = lines.iter().find_map(|l| {
                l.text
                    .strip_prefix("*** Update File: ")
                    .or_else(|| l.text.strip_prefix("+++ b/"))
            }) {
                file = compact_file(p.trim());
            }
        }

        // Drop pure meta noise from body when we have real +/- content.
        if added + removed > 0 {
            lines.retain(|l| !matches!(l.kind, DiffLineKind::Meta));
        }

        Self {
            file,
            start_line,
            added,
            removed,
            lines,
        }
    }

    /// Header label: `auth.go:120` or `auth.go (+3 -1)`.
    pub fn header_label(&self) -> String {
        let base = if let Some(line) = self.start_line {
            format!("{}:{line}", self.file)
        } else {
            self.file.clone()
        };
        if self.added > 0 || self.removed > 0 {
            format!("{base}  (+{} -{})", self.added, self.removed)
        } else {
            base
        }
    }
}

fn compact_file(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
        .to_string()
}

/// Render DiffView into conversation lines (no solid block background).
pub fn render_diff_view(
    view: &DiffView,
    width: usize,
    theme: &Theme,
    out: &mut Vec<Line<'static>>,
) {
    let width = width.max(12);
    let header = view.header_label();

    // Compact header — not a black box.
    out.push(Line::from(vec![
        Span::styled("▼ ", Style::default().fg(theme.accent)),
        Span::styled(
            truncate_display(&header, width.saturating_sub(2).max(4)),
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        ),
    ]));

    let body = fold_diff_lines(&view.lines);
    for part in body {
        match part {
            Folded::Line(line) => {
                for row in render_diff_line(line, width, theme) {
                    out.push(row);
                }
            }
            Folded::Omitted(n) => {
                out.push(Line::from(Span::styled(
                    format!("  ⋮ {n} lines hidden"),
                    Style::default().fg(theme.dim),
                )));
            }
        }
    }
}

enum Folded<'a> {
    Line(&'a DiffLine),
    Omitted(usize),
}

fn fold_diff_lines(lines: &[DiffLine]) -> Vec<Folded<'_>> {
    // Prefer keeping change lines; drop pure context from the long middle.
    let meaningful: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| {
            matches!(
                l.kind,
                DiffLineKind::Add | DiffLineKind::Remove | DiffLineKind::HunkHeader
            )
        })
        .map(|(i, _)| i)
        .collect();

    if lines.len() <= DIFF_MAX_BODY {
        return lines.iter().map(Folded::Line).collect();
    }

    // Window around first change cluster with limited context.
    let focus = meaningful.first().copied().unwrap_or(0);
    let start = focus.saturating_sub(2);
    let mut end = (focus + DIFF_MAX_BODY).min(lines.len());
    // Extend to include nearby changes.
    for &i in &meaningful {
        if i < start + DIFF_MAX_BODY {
            end = end.max((i + 3).min(lines.len()));
        }
    }
    end = end.min(start + DIFF_MAX_BODY).max(start + 1);

    let mut out = Vec::new();
    if start > 0 {
        // Keep a tiny head if it has hunk header.
        if matches!(lines[0].kind, DiffLineKind::HunkHeader) && start > 0 {
            out.push(Folded::Line(&lines[0]));
            if start > 1 {
                out.push(Folded::Omitted(start - 1));
            }
        } else {
            out.push(Folded::Omitted(start));
        }
    }
    for line in &lines[start..end] {
        out.push(Folded::Line(line));
    }
    if end < lines.len() {
        // Prefer showing last few change lines.
        let tail_start = lines.len().saturating_sub(DIFF_TAIL);
        if tail_start > end {
            out.push(Folded::Omitted(tail_start - end));
            for line in &lines[tail_start..] {
                out.push(Folded::Line(line));
            }
        } else if end < lines.len() {
            out.push(Folded::Omitted(lines.len() - end));
        }
    }
    // Hard cap rendered line count.
    let mut shown = 0usize;
    out.retain(|p| match p {
        Folded::Line(_) => {
            shown += 1;
            shown <= DIFF_HEAD + DIFF_TAIL + 4
        }
        Folded::Omitted(_) => true,
    });
    out
}

fn render_diff_line(line: &DiffLine, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let (prefix, style) = match line.kind {
        DiffLineKind::Add => ("  ", Style::default().fg(theme.diff_add)),
        DiffLineKind::Remove => ("  ", Style::default().fg(theme.diff_remove)),
        DiffLineKind::HunkHeader => (
            "  ",
            Style::default().fg(theme.muted).add_modifier(Modifier::DIM),
        ),
        DiffLineKind::Meta => ("  ", Style::default().fg(theme.border)),
        DiffLineKind::Context => ("  ", Style::default().fg(theme.muted)),
    };
    let inner = width.saturating_sub(prefix.len()).max(1);
    let text = if line.kind == DiffLineKind::HunkHeader {
        // Soften @@ headers.
        line.text.clone()
    } else {
        line.text.clone()
    };
    wrap(&text, inner)
        .into_iter()
        .enumerate()
        .map(|(i, row)| {
            let lead = if i == 0 { prefix } else { "  " };
            Line::from(vec![
                Span::styled(lead.to_string(), Style::default().fg(theme.border)),
                Span::styled(row, style),
            ])
        })
        .collect()
}

/// Convenience: parse + render a markdown `diff` fence body.
pub fn render_diff_fence(
    src: &str,
    title_hint: Option<&str>,
    width: usize,
    theme: &Theme,
    out: &mut Vec<Line<'static>>,
) {
    let view = DiffView::parse(src, title_hint);
    render_diff_view(&view, width, theme, out);
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
--- a/auth.go
+++ b/auth.go
@@ -120,4 +120,5 @@
 func login() {
-    token := oldToken
+    token := validateToken()
+    checkQuota()
 }
";

    #[test]
    fn parse_extracts_file_stats_and_lines() {
        let v = DiffView::parse(SAMPLE, None);
        assert_eq!(v.file, "auth.go");
        assert_eq!(v.start_line, Some(120));
        assert_eq!(v.added, 2);
        assert_eq!(v.removed, 1);
        assert!(v.lines.iter().any(|l| l.kind == DiffLineKind::Add));
        assert!(v.lines.iter().any(|l| l.kind == DiffLineKind::Remove));
    }

    #[test]
    fn render_has_header_and_no_code_bg_box() {
        let theme = Theme::no_color();
        let mut out = Vec::new();
        render_diff_fence(SAMPLE, Some("auth.go:120"), 60, &theme, &mut out);
        let text: Vec<String> = out
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(
            text.iter().any(|l| l.contains("auth.go")),
            "header missing: {text:?}"
        );
        assert!(
            text.iter()
                .any(|l| l.contains("oldToken") || l.contains('-')),
            "delete line missing: {text:?}"
        );
        assert!(
            !text
                .iter()
                .any(|l| l.starts_with('┌') || l.starts_with('└')),
            "diff must not use CodeBlock box: {text:?}"
        );
        // No full-bleed block marker.
        assert!(
            !text.iter().any(|l| l.starts_with("┃")),
            "diff must not use heavy code gutter: {text:?}"
        );
    }

    #[test]
    fn long_diff_folds() {
        let mut body = String::from("+++ b/big.go\n@@ -1,40 +1,40 @@\n");
        for i in 0..40 {
            if i == 10 {
                body.push_str("- old\n+ new\n");
            } else {
                body.push_str(&format!(" context {i}\n"));
            }
        }
        let v = DiffView::parse(&body, None);
        let mut out = Vec::new();
        render_diff_view(&v, 60, &Theme::no_color(), &mut out);
        let text: Vec<String> = out
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(
            text.iter().any(|l| l.contains("hidden") || l.contains("⋮")),
            "long diff should fold: {text:?}"
        );
        assert!(text.len() < 30, "must not dump full diff: {}", text.len());
    }
}
