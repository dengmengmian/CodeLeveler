//! Markdown rendering for the transcript (spec §62).
//!
//! Parsing (pulldown-cmark) and syntax highlighting (syntect) happen once, when
//! a message completes, producing a width-agnostic [`MdDoc`]. Each frame that
//! doc is laid out to the current width — cheap word-wrapping, no re-parsing —
//! honoring the "don't re-parse Markdown every frame" performance rule.

use std::sync::OnceLock;

use pulldown_cmark::{Alignment, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::theme::Theme;

/// A parsed markdown document, width-agnostic.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MdDoc {
    blocks: Vec<MdBlock>,
    /// An unmatched `**` can be closed by a later streaming delta and
    /// retroactively change every wrapped line in the final block.
    unclosed_strong: bool,
    /// The trailing Markdown block starts like a GFM table. Before its
    /// separator/rows are complete, pulldown-cmark may temporarily parse the
    /// header or current row as a paragraph; none of that block is safe to
    /// commit while streaming because the whole table can still reflow.
    streaming_table_tail: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum MdBlock {
    Heading {
        level: u8,
        spans: Vec<MdSpan>,
    },
    Paragraph(Vec<MdSpan>),
    Quote(Vec<MdSpan>),
    List {
        ordered: bool,
        items: Vec<Vec<MdSpan>>,
    },
    /// Syntax-highlighted code: per line, a run of `(rgb, text)`.
    /// `lang` is the fence language (e.g. `rust`, `diff`); `title` is an
    /// optional path header (`webhook.go:792`) from the fence info string.
    Code {
        lang: Option<String>,
        title: Option<String>,
        lines: Vec<Vec<((u8, u8, u8), String)>>,
    },
    /// A GFM table: a header row plus body rows, each cell a run of spans.
    /// `align` has one entry per column (from the separator row).
    Table {
        align: Vec<ColAlign>,
        header: Vec<Vec<MdSpan>>,
        rows: Vec<Vec<Vec<MdSpan>>>,
    },
    Rule,
}

/// Column alignment for GFM tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ColAlign {
    #[default]
    None,
    Left,
    Center,
    Right,
}

impl From<Alignment> for ColAlign {
    fn from(a: Alignment) -> Self {
        match a {
            Alignment::None => ColAlign::None,
            Alignment::Left => ColAlign::Left,
            Alignment::Center => ColAlign::Center,
            Alignment::Right => ColAlign::Right,
        }
    }
}

/// An inline run with its markdown emphasis.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MdSpan {
    text: String,
    bold: bool,
    italic: bool,
    code: bool,
    /// When set, this span is a hyperlink; layout may emit OSC 8.
    link: Option<String>,
}

impl MdDoc {
    /// Parse markdown text into a document (done once per completed message).
    pub fn parse(text: &str) -> Self {
        let mut opts = Options::empty();
        opts.insert(Options::ENABLE_STRIKETHROUGH);
        opts.insert(Options::ENABLE_TABLES);
        let parser = Parser::new_ext(text, opts);

        let mut blocks: Vec<MdBlock> = Vec::new();
        let mut spans: Vec<MdSpan> = Vec::new();
        let mut bold = 0u32;
        let mut italic = 0u32;
        // Block context.
        let mut heading: Option<u8> = None;
        let mut in_quote = false;
        let mut list_ordered: Option<bool> = None;
        let mut list_items: Vec<Vec<MdSpan>> = Vec::new();
        let mut in_item = false;
        let mut code_lang: Option<String> = None;
        let mut code_title: Option<String> = None;
        let mut code_buf = String::new();
        let mut in_code = false;
        // Table context.
        let mut in_table_head = false;
        let mut table_align: Vec<ColAlign> = Vec::new();
        let mut table_header: Vec<Vec<MdSpan>> = Vec::new();
        let mut table_rows: Vec<Vec<Vec<MdSpan>>> = Vec::new();
        let mut table_row: Vec<Vec<MdSpan>> = Vec::new();
        // Link destination stack (nested links are rare; last wins for text).
        let mut link_url: Option<String> = None;

        let push_text =
            |spans: &mut Vec<MdSpan>, text: String, bold, italic, code, link: Option<String>| {
                if !text.is_empty() {
                    spans.push(MdSpan {
                        text,
                        bold,
                        italic,
                        code,
                        link,
                    });
                }
            };

        for event in parser {
            match event {
                Event::Start(Tag::Heading { level, .. }) => heading = Some(heading_level(level)),
                Event::End(TagEnd::Heading(_)) => {
                    blocks.push(MdBlock::Heading {
                        level: heading.take().unwrap_or(1),
                        spans: std::mem::take(&mut spans),
                    });
                }
                Event::Start(Tag::Paragraph) => {}
                Event::End(TagEnd::Paragraph) => {
                    let taken = std::mem::take(&mut spans);
                    if in_item {
                        list_items.push(taken);
                    } else if in_quote {
                        blocks.push(MdBlock::Quote(taken));
                    } else if !taken.is_empty() {
                        blocks.push(MdBlock::Paragraph(taken));
                    }
                }
                Event::Start(Tag::BlockQuote(_)) => in_quote = true,
                Event::End(TagEnd::BlockQuote(_)) => in_quote = false,
                Event::Start(Tag::List(first)) => {
                    list_ordered = Some(first.is_some());
                    list_items.clear();
                }
                Event::End(TagEnd::List(_)) => {
                    blocks.push(MdBlock::List {
                        ordered: list_ordered.take().unwrap_or(false),
                        items: std::mem::take(&mut list_items),
                    });
                }
                Event::Start(Tag::Item) => in_item = true,
                Event::End(TagEnd::Item) => {
                    // Loose lists emit paragraphs; tight lists emit text directly.
                    if !spans.is_empty() {
                        list_items.push(std::mem::take(&mut spans));
                    }
                    in_item = false;
                }
                Event::Start(Tag::CodeBlock(kind)) => {
                    in_code = true;
                    code_buf.clear();
                    code_title = None;
                    match kind {
                        pulldown_cmark::CodeBlockKind::Fenced(info) if !info.is_empty() => {
                            let meta = crate::code_block::parse_fence_info(&info);
                            code_lang = meta.lang;
                            code_title = meta.title;
                        }
                        _ => {
                            code_lang = None;
                        }
                    }
                }
                Event::End(TagEnd::CodeBlock) => {
                    in_code = false;
                    let lang = code_lang.take();
                    let title = code_title.take();
                    let lines = if lang.as_deref() == Some("diff") {
                        highlight_diff(&code_buf)
                    } else {
                        highlight_code(&code_buf, lang.as_deref())
                    };
                    blocks.push(MdBlock::Code { lang, title, lines });
                }
                Event::Start(Tag::Table(alignments)) => {
                    table_header.clear();
                    table_rows.clear();
                    table_align = alignments.into_iter().map(ColAlign::from).collect();
                }
                Event::End(TagEnd::Table) => {
                    blocks.push(MdBlock::Table {
                        align: std::mem::take(&mut table_align),
                        header: std::mem::take(&mut table_header),
                        rows: std::mem::take(&mut table_rows),
                    });
                }
                Event::Start(Tag::TableHead) => in_table_head = true,
                Event::End(TagEnd::TableHead) => {
                    in_table_head = false;
                    table_header = std::mem::take(&mut table_row);
                }
                Event::Start(Tag::TableRow) => table_row.clear(),
                Event::End(TagEnd::TableRow) if !in_table_head => {
                    table_rows.push(std::mem::take(&mut table_row));
                }
                Event::Start(Tag::TableCell) => {}
                Event::End(TagEnd::TableCell) => {
                    table_row.push(std::mem::take(&mut spans));
                }
                Event::Start(Tag::Strong) => bold += 1,
                Event::End(TagEnd::Strong) => bold = bold.saturating_sub(1),
                Event::Start(Tag::Emphasis) => italic += 1,
                Event::End(TagEnd::Emphasis) => italic = italic.saturating_sub(1),
                Event::Start(Tag::Link { dest_url, .. }) => {
                    link_url = Some(dest_url.into_string());
                }
                Event::End(TagEnd::Link) => {
                    link_url = None;
                }
                Event::Text(t) => {
                    if in_code {
                        code_buf.push_str(&t);
                    } else {
                        push_text(
                            &mut spans,
                            t.into_string(),
                            bold > 0,
                            italic > 0,
                            false,
                            link_url.clone(),
                        );
                    }
                }
                Event::Code(t) => push_text(
                    &mut spans,
                    t.into_string(),
                    bold > 0,
                    italic > 0,
                    true,
                    link_url.clone(),
                ),
                Event::SoftBreak | Event::HardBreak => push_text(
                    &mut spans,
                    "\n".to_string(),
                    bold > 0,
                    italic > 0,
                    false,
                    None,
                ),
                Event::Rule => blocks.push(MdBlock::Rule),
                _ => {}
            }
        }
        // Any trailing inline content.
        if !spans.is_empty() {
            blocks.push(MdBlock::Paragraph(spans));
        }

        MdDoc {
            blocks,
            unclosed_strong: !text.match_indices("**").count().is_multiple_of(2),
            streaming_table_tail: trailing_block_may_be_table(text),
        }
    }

    /// Lay the document out to `width` columns with theme colors.
    pub fn to_lines(&self, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        self.to_lines_split(width, theme).0
    }

    /// Flatten parsed Markdown into readable plain text for compact UI chrome.
    /// Syntax markers are discarded rather than leaked as literal `**`/`##`.
    pub fn plain_text(&self) -> String {
        fn spans_text(spans: &[MdSpan]) -> String {
            spans.iter().map(|span| span.text.as_str()).collect()
        }

        let mut parts = Vec::new();
        for block in &self.blocks {
            match block {
                MdBlock::Heading { spans, .. }
                | MdBlock::Paragraph(spans)
                | MdBlock::Quote(spans) => parts.push(spans_text(spans)),
                MdBlock::List { items, .. } => {
                    parts.extend(items.iter().map(|item| spans_text(item)));
                }
                MdBlock::Code { lines, .. } => {
                    for line in lines {
                        parts.push(line.iter().map(|(_, text)| text.as_str()).collect());
                    }
                }
                MdBlock::Table { header, rows, .. } => {
                    parts.extend(header.iter().map(|cell| spans_text(cell)));
                    for row in rows {
                        parts.extend(row.iter().map(|cell| spans_text(cell)));
                    }
                }
                MdBlock::Rule => {}
            }
        }
        parts
            .join(" ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Like [`to_lines`], but also returns the index at which the LAST block's
    /// lines begin. Everything before that index belongs to fully-received
    /// (stable) blocks and is safe to commit to scrollback during streaming; the
    /// tail (the last, possibly-still-growing block) should stay in the live
    /// region. For an empty doc the split index is 0.
    pub fn to_lines_split(&self, width: usize, theme: &Theme) -> (Vec<Line<'static>>, usize) {
        let width = width.max(1);
        let mut out: Vec<Line<'static>> = Vec::new();
        let mut last_block_start = 0;
        let mut block_starts = Vec::with_capacity(self.blocks.len());
        for (i, block) in self.blocks.iter().enumerate() {
            if i > 0 {
                out.push(Line::from(""));
            }
            last_block_start = out.len();
            block_starts.push(last_block_start);
            render_block(block, width, theme, &mut out);
        }
        // Within the LAST (still-streaming) block, greedy wrapping normally
        // leaves only the final display line mutable. An unmatched strong
        // delimiter can restyle the whole block when its closer arrives, and a
        // table can reflow every column as rows arrive, so both stay fully live.
        let stable = if self.streaming_table_tail {
            let unstable_block = match self.blocks.as_slice() {
                [.., MdBlock::Table { .. }, _] => self.blocks.len() - 2,
                [] => return (out, 0),
                _ => self.blocks.len() - 1,
            };
            block_starts[unstable_block]
        } else if self.unclosed_strong {
            last_block_start
        } else {
            match self.blocks.last() {
                None => 0,
                Some(MdBlock::Table { .. }) => last_block_start,
                Some(_) => out.len().saturating_sub(1),
            }
        };
        (out, stable)
    }
}

/// Whether the final blank-line-delimited Markdown block can still become or
/// extend a GFM table. A header row followed by a partial separator is parsed
/// as an ordinary paragraph until enough `-` characters arrive, so detection
/// must use the source text rather than only the parsed block type.
fn trailing_block_may_be_table(text: &str) -> bool {
    let tail = text.rsplit("\n\n").next().unwrap_or(text);
    let mut lines = tail.lines();
    let Some(header) = lines.next() else {
        return false;
    };
    if !header.contains('|') {
        return false;
    }
    let Some(separator) = lines.next() else {
        return true;
    };
    !separator.is_empty()
        && separator
            .chars()
            .all(|ch| ch.is_whitespace() || matches!(ch, '|' | '-' | ':'))
}

/// Render a single markdown block into styled lines.
fn render_block(block: &MdBlock, width: usize, theme: &Theme, out: &mut Vec<Line<'static>>) {
    {
        match block {
            MdBlock::Heading { level, spans } => {
                // Render as a styled heading (no literal "#" markers). H1/H2
                // get a heading-colored bar so the level still reads at a glance.
                let style = Style::default()
                    .fg(theme.heading)
                    .add_modifier(Modifier::BOLD);
                let mut heading = Vec::new();
                if *level <= 2 {
                    heading.push(MdSpan {
                        text: "▎".to_string(),
                        bold: true,
                        italic: false,
                        code: false,
                        link: None,
                    });
                }
                heading.extend(spans.iter().cloned());
                out.extend(wrap_spans(&heading, width, theme, style));
            }
            MdBlock::Paragraph(spans) => {
                out.extend(wrap_spans(spans, width, theme, Style::default()));
            }
            MdBlock::Quote(spans) => {
                let inner = width.saturating_sub(2).max(1);
                for line in wrap_spans(spans, inner, theme, Style::default().fg(theme.muted)) {
                    let mut spans = vec![Span::styled("▌ ", Style::default().fg(theme.border))];
                    spans.extend(line.spans);
                    out.push(Line::from(spans));
                }
            }
            MdBlock::List { ordered, items } => {
                for (n, item) in items.iter().enumerate() {
                    let marker = if *ordered {
                        format!("{}. ", n + 1)
                    } else {
                        "• ".to_string()
                    };
                    let indent = " ".repeat(marker.width());
                    let inner = width.saturating_sub(marker.width()).max(1);
                    let wrapped = wrap_spans(item, inner, theme, Style::default());
                    for (li, line) in wrapped.into_iter().enumerate() {
                        let lead = if li == 0 {
                            marker.clone()
                        } else {
                            indent.clone()
                        };
                        let mut spans = vec![Span::styled(lead, Style::default().fg(theme.heading))];
                        spans.extend(line.spans);
                        out.push(Line::from(spans));
                    }
                }
            }
            MdBlock::Code { lang, title, lines } => {
                let avail = width.saturating_sub(2).max(1);
                // Diff is comparison UI — never reuse CodeBlock chrome.
                if lang.as_deref() == Some("diff") {
                    let src: String = lines
                        .iter()
                        .map(|segs| segs.iter().map(|(_, t)| t.as_str()).collect::<String>())
                        .collect::<Vec<_>>()
                        .join("\n");
                    crate::diff_view::render_diff_fence(&src, title.as_deref(), width, theme, out);
                } else {
                    // Short code = inline; long/file = light container (no solid fill).
                    crate::code_block::render_code_block(
                        title.as_deref(),
                        lang.as_deref(),
                        lines.len(),
                        width,
                        theme,
                        |idx| wrap_code_line(&lines[idx], avail, theme),
                        out,
                    );
                }
            }
            MdBlock::Table {
                align,
                header,
                rows,
            } => {
                out.extend(table_lines(header, rows, align, width, theme));
            }
            MdBlock::Rule => {
                out.push(Line::from(Span::styled(
                    "─".repeat(width.min(40)),
                    Style::default().fg(theme.border),
                )));
            }
        }
    }
}

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// Hard-wrap one syntax-highlighted code line (no background fill).
fn wrap_code_line(
    segments: &[((u8, u8, u8), String)],
    avail: usize,
    theme: &Theme,
) -> Vec<Vec<Span<'static>>> {
    let avail = avail.max(1);
    let mut rows: Vec<Vec<Span<'static>>> = Vec::new();
    let mut row: Vec<Span<'static>> = Vec::new();
    let mut w = 0usize;
    let mut cur_color: Option<(u8, u8, u8)> = None;
    let mut cur_text = String::new();
    let flush_span = |row: &mut Vec<Span<'static>>, color: (u8, u8, u8), text: &mut String| {
        if text.is_empty() {
            return;
        }
        row.push(Span::styled(
            std::mem::take(text),
            crate::code_block::tone_code_rgb(color, theme),
        ));
    };
    for (color, text) in segments {
        for g in text.graphemes(true) {
            let gw = grapheme_width(g).max(1);
            if w + gw > avail && w > 0 {
                if let Some(c) = cur_color {
                    flush_span(&mut row, c, &mut cur_text);
                }
                rows.push(std::mem::take(&mut row));
                w = 0;
                cur_color = None;
            }
            if cur_color != Some(*color) {
                if let Some(c) = cur_color {
                    flush_span(&mut row, c, &mut cur_text);
                }
                cur_color = Some(*color);
            }
            cur_text.push_str(g);
            w += gw;
        }
    }
    if let Some(c) = cur_color {
        flush_span(&mut row, c, &mut cur_text);
    }
    if !row.is_empty() || rows.is_empty() {
        rows.push(row);
    }
    rows
}

/// Keep raw lines for DiffView (styling applied at render time).
fn highlight_diff(src: &str) -> Vec<Vec<((u8, u8, u8), String)>> {
    const NEU: (u8, u8, u8) = (0x9A, 0xA3, 0xA7);
    src.lines()
        .map(|line| vec![(NEU, line.to_string())])
        .collect()
}

/// Display width of `s` in terminal cells. `unicode-width` disagrees with most
/// terminal fonts on emoji — especially the colored shapes (🔴🟡🟢) and any glyph
/// forced to emoji presentation with VS16 (⚠️) — counting them as 1 while the
/// terminal draws 2. That single-cell drift accumulates and misaligns every
/// column after an emoji. Treat any emoji-presentation grapheme as 2 cells so
/// what the layout reserves matches what the terminal paints.
pub(crate) fn disp_width(s: &str) -> usize {
    s.graphemes(true).map(grapheme_width).sum()
}

pub(crate) fn grapheme_width(g: &str) -> usize {
    if g.chars().any(is_emoji_presentation) {
        return 2;
    }
    g.width()
}

/// Whether a scalar renders with emoji (double-width) presentation in terminals.
/// Covers the common pictographic blocks plus VS16, which forces emoji style on
/// an otherwise text-default symbol.
fn is_emoji_presentation(c: char) -> bool {
    c == '\u{FE0F}'
        || matches!(c as u32,
            0x1F300..=0x1FAFF   // symbols & pictographs (incl. 🔴), extended-A, emoji
            | 0x2600..=0x27BF   // misc symbols + dingbats (⚠ ✅ ❗ …)
            | 0x2B00..=0x2BFF   // misc symbols & arrows (⭐ …)
            | 0x1F000..=0x1F0FF // mahjong / dominoes / playing cards (wide)
        )
}

/// A legible minimum for the single flexible (widest) column.
const MIN_FLEX_COL: usize = 16;

/// Lay out a GFM table. Columns are sized to their widest cell (emoji-aware). If
/// they overflow, every descriptive column shares the available width instead
/// of sacrificing all but the single longest one. If even that can't fit a
/// legible grid, the table degrades to stacked "label: value" records.
fn table_lines(
    header: &[Vec<MdSpan>],
    rows: &[Vec<Vec<MdSpan>>],
    align: &[ColAlign],
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let n_cols = header
        .len()
        .max(rows.iter().map(|r| r.len()).max().unwrap_or(0));
    if n_cols == 0 {
        return vec![Line::from("")];
    }

    // Natural column widths: the widest cell in each column (terminal-truthful).
    let mut colw = vec![1usize; n_cols];
    let mut consider = |row: &[Vec<MdSpan>]| {
        for (c, cell) in row.iter().enumerate() {
            let w: usize = cell.iter().map(|s| disp_width(s.text.as_str())).sum();
            colw[c] = colw[c].max(w);
        }
    };
    consider(header);
    for r in rows {
        consider(r);
    }

    // Fit to the available width. A full-grid row is `│ c │ c │`: n+1 vertical
    // bars plus one padding space on each side of every cell = 3*n + 1 columns
    // of chrome, leaving the rest for cell content.
    let sep_total = 3 * n_cols + 1;
    let budget = width.saturating_sub(sep_total).max(n_cols);
    let natural: usize = colw.iter().sum();
    if natural > budget {
        const LABEL_CAP: usize = 12;
        let long_cols = colw.iter().filter(|&&w| w > LABEL_CAP).count();
        // Tables with multiple long columns become visually shattered well
        // before they technically overflow: one column flexes, and the others
        // wrap into skinny fragments. Stacked records are easier to scan there.
        if ((n_cols >= 4) || long_cols >= 2) && width < 100 {
            return table_lines_stacked(header, rows, width, theme);
        }

        // A grid is only useful if every column can keep a legible minimum.
        // Short label columns keep their natural width; descriptive columns
        // need enough room to avoid one-word-per-line fragments.
        let legible: usize = colw.iter().map(|&w| w.min(MIN_FLEX_COL)).sum();
        if legible > budget {
            return table_lines_stacked(header, rows, width, theme);
        }

        // Fair width allocation. Every column starts at a legible floor so no
        // descriptive column collapses into one-grapheme-per-line fragments.
        // Columns that fit within an equal share of the surplus are then
        // satisfied outright — this protects short label/path columns. Whatever
        // is left is split among the still-hungry prose columns in proportion to
        // their unmet demand, so the column that is long on *every* row gets the
        // most room, instead of an equal split that a single outlier cell (one
        // very long entry in an otherwise short column) would skew.
        let natural = colw.clone();
        for (c, w) in colw.iter_mut().enumerate() {
            *w = natural[c].min(MIN_FLEX_COL);
        }
        let mut remaining = budget - colw.iter().sum::<usize>();
        loop {
            let hungry: Vec<usize> = (0..n_cols).filter(|&c| colw[c] < natural[c]).collect();
            if hungry.is_empty() || remaining == 0 {
                break;
            }
            let share = remaining / hungry.len();
            let fits: Vec<usize> = hungry
                .iter()
                .copied()
                .filter(|&c| natural[c] - colw[c] <= share)
                .collect();
            if fits.is_empty() {
                // Only oversized prose columns remain: split the rest by demand.
                let want: usize = hungry.iter().map(|&c| natural[c] - colw[c]).sum();
                for &c in &hungry {
                    let give = remaining * (natural[c] - colw[c]) / want;
                    colw[c] += give;
                    remaining -= give;
                }
                // Hand the rounding remainder to the hungriest columns.
                let mut order = hungry.clone();
                order.sort_by_key(|&c| std::cmp::Reverse(natural[c] - colw[c]));
                for &c in order.iter().cycle() {
                    if remaining == 0 {
                        break;
                    }
                    if colw[c] < natural[c] {
                        colw[c] += 1;
                        remaining -= 1;
                    }
                }
                break;
            }
            for c in fits {
                remaining -= natural[c] - colw[c];
                colw[c] = natural[c];
            }
        }
    }

    let border = Style::default().fg(theme.border);
    let head_style = Style::default()
        .fg(theme.heading)
        .add_modifier(Modifier::BOLD);

    // Full-grid layout: an outer box with a rule between every row.
    let mut out = vec![table_border_line(&colw, "╭", "┬", "╮", border)];
    out.extend(render_table_row(
        header, &colw, align, head_style, theme, border,
    ));
    out.push(table_border_line(&colw, "├", "┼", "┤", border));
    for (i, r) in rows.iter().enumerate() {
        out.extend(render_table_row(
            r,
            &colw,
            align,
            Style::default(),
            theme,
            border,
        ));
        if i + 1 < rows.len() {
            out.push(table_border_line(&colw, "├", "┼", "┤", border));
        }
    }
    out.push(table_border_line(&colw, "╰", "┴", "╯", border));
    out
}

/// A horizontal grid rule spanning every column: `<left>──┬──<right>` with the
/// junction char between columns. Each column's dashes cover its content width
/// plus the one-space padding on each side (`w + 2`).
fn table_border_line(
    colw: &[usize],
    left: &str,
    mid: &str,
    right: &str,
    border: Style,
) -> Line<'static> {
    let mut s = String::from(left);
    for (c, &w) in colw.iter().enumerate() {
        if c > 0 {
            s.push_str(mid);
        }
        s.push_str(&"─".repeat(w + 2));
    }
    s.push_str(right);
    Line::from(Span::styled(s, border))
}

/// Fallback layout for a table too wide for a legible grid in the current
/// terminal: render each row as compact "label: value" fields (no borders),
/// with wrapped values aligned under the first value character.
fn table_lines_stacked(
    header: &[Vec<MdSpan>],
    rows: &[Vec<Vec<MdSpan>>],
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let width = width.max(1);
    let label_style = Style::default()
        .fg(theme.heading)
        .add_modifier(Modifier::BOLD);

    let mut out: Vec<Line<'static>> = Vec::new();
    for (ri, row) in rows.iter().enumerate() {
        if ri > 0 {
            out.push(Line::from("")); // blank line between records
        }
        for (c, label_cell) in header.iter().enumerate() {
            let label: String = label_cell.iter().map(|s| s.text.as_str()).collect();
            let empty: Vec<MdSpan> = Vec::new();
            let value = row.get(c).unwrap_or(&empty);

            let prefix = format!("{label}: ");
            let prefix_width = disp_width(&prefix);
            if prefix_width >= width {
                out.push(Line::from(Span::styled(
                    prefix.trim_end().to_string(),
                    label_style,
                )));
                for mut vline in wrap_spans(
                    value,
                    width.saturating_sub(2).max(1),
                    theme,
                    Style::default(),
                ) {
                    let mut spans = vec![Span::raw("  ")];
                    spans.append(&mut vline.spans);
                    out.push(Line::from(spans));
                }
                continue;
            }

            let wrapped = wrap_spans(value, width - prefix_width, theme, Style::default());
            for (line_index, mut vline) in wrapped.into_iter().enumerate() {
                let mut spans = if line_index == 0 {
                    vec![Span::styled(prefix.clone(), label_style)]
                } else {
                    vec![Span::raw(" ".repeat(prefix_width))]
                };
                spans.append(&mut vline.spans);
                out.push(Line::from(spans));
            }
        }
    }
    out
}

/// Render one table row: wrap each cell to its column width, then stack the
/// wrapped cell lines side by side, padding each to the column width with
/// GFM alignment (left / center / right).
fn render_table_row(
    cells: &[Vec<MdSpan>],
    colw: &[usize],
    align: &[ColAlign],
    base: Style,
    theme: &Theme,
    border: Style,
) -> Vec<Line<'static>> {
    let empty: Vec<MdSpan> = Vec::new();
    let wrapped: Vec<Vec<Line<'static>>> = (0..colw.len())
        .map(|c| wrap_spans(cells.get(c).unwrap_or(&empty), colw[c].max(1), theme, base))
        .collect();
    let height = wrapped.iter().map(|w| w.len()).max().unwrap_or(1).max(1);

    let mut lines: Vec<Line<'static>> = Vec::new();
    for li in 0..height {
        // Full-grid row: `│ cell │ cell │`, one padding space inside each border.
        let mut spans: Vec<Span<'static>> = vec![Span::styled("│ ", border)];
        for (c, &w) in colw.iter().enumerate() {
            if c > 0 {
                spans.push(Span::styled(" │ ", border));
            }
            let used = if let Some(line) = wrapped[c].get(li) {
                let u: usize = line
                    .spans
                    .iter()
                    .map(|s| disp_width(s.content.as_ref()))
                    .sum();
                let pad = w.saturating_sub(u);
                let col_align = align.get(c).copied().unwrap_or(ColAlign::None);
                let (left, right) = match col_align {
                    ColAlign::Right => (pad, 0),
                    ColAlign::Center => (pad / 2, pad - pad / 2),
                    ColAlign::Left | ColAlign::None => (0, pad),
                };
                if left > 0 {
                    spans.push(Span::raw(" ".repeat(left)));
                }
                spans.extend(line.spans.iter().cloned());
                if right > 0 {
                    spans.push(Span::raw(" ".repeat(right)));
                }
                u
            } else {
                spans.push(Span::raw(" ".repeat(w)));
                w
            };
            let _ = used;
        }
        spans.push(Span::styled(" │", border));
        lines.push(Line::from(spans));
    }
    lines
}

/// Word-wrap styled spans to `width` columns, preserving each span's style and
/// breaking overlong tokens (e.g. CJK runs) by grapheme.
fn wrap_spans(spans: &[MdSpan], width: usize, theme: &Theme, base: Style) -> Vec<Line<'static>> {
    let style_of = |s: &MdSpan| {
        let mut style = base;
        if s.bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        if s.italic {
            style = style.add_modifier(Modifier::ITALIC);
        }
        if s.code {
            style = Style::default().fg(theme.code);
        }
        if s.link.is_some() {
            style = style.fg(theme.accent).add_modifier(Modifier::UNDERLINED);
        }
        style
    };

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut col = 0usize;

    let flush = |cur: &mut Vec<Span<'static>>, col: &mut usize, lines: &mut Vec<Line<'static>>| {
        lines.push(Line::from(std::mem::take(cur)));
        *col = 0;
    };

    for span in spans {
        let style = style_of(span);
        // http(s) links keep accent+underline styling only. Embedding OSC 8 in
        // Span content is unsafe for the inline paint path (`truncate_to`
        // turns ESC into spaces and counts the URL as display width). Real
        // hyperlinks need a write_line-level zero-width channel — until then,
        // underline is the supported fallback.
        // Split into word / whitespace tokens.
        for token in split_tokens(&span.text) {
            if token == "\n" {
                flush(&mut cur, &mut col, &mut lines);
                continue;
            }
            let is_space = token.chars().all(|c| c == ' ');
            let tw = disp_width(&token);
            if is_space {
                if col == 0 {
                    continue; // drop leading spaces on a fresh line
                }
                if col + tw > width {
                    flush(&mut cur, &mut col, &mut lines);
                } else {
                    cur.push(Span::styled(token.clone(), style));
                    col += tw;
                }
                continue;
            }
            if tw > width {
                // Break the overlong token by grapheme.
                for g in token.graphemes(true) {
                    let gw = grapheme_width(g).max(1);
                    if col + gw > width && col > 0 {
                        flush(&mut cur, &mut col, &mut lines);
                    }
                    cur.push(Span::styled(g.to_string(), style));
                    col += gw;
                }
            } else {
                if col + tw > width && col > 0 {
                    flush(&mut cur, &mut col, &mut lines);
                }
                cur.push(Span::styled(token.clone(), style));
                col += tw;
            }
        }
    }
    if !cur.is_empty() {
        lines.push(Line::from(cur));
    }
    if lines.is_empty() {
        lines.push(Line::from(""));
    }
    lines
}

/// Split text into maximal word and whitespace runs.
fn split_tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_space = false;
    for ch in text.chars() {
        if ch == '\n' {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            out.push("\n".to_string());
            cur_space = false;
            continue;
        }
        let is_space = ch == ' ';
        if cur.is_empty() {
            cur.push(ch);
            cur_space = is_space;
        } else if is_space == cur_space {
            cur.push(ch);
        } else {
            out.push(std::mem::take(&mut cur));
            cur.push(ch);
            cur_space = is_space;
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn syntax_set() -> &'static SyntaxSet {
    static SS: OnceLock<SyntaxSet> = OnceLock::new();
    SS.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn theme_set() -> &'static ThemeSet {
    static TS: OnceLock<ThemeSet> = OnceLock::new();
    TS.get_or_init(ThemeSet::load_defaults)
}

/// Syntax-highlight a code block into per-line colored runs.
fn highlight_code(code: &str, lang: Option<&str>) -> Vec<Vec<((u8, u8, u8), String)>> {
    let ss = syntax_set();
    let syntax = lang
        .and_then(|l| ss.find_syntax_by_token(l))
        .unwrap_or_else(|| ss.find_syntax_plain_text());
    let theme = &theme_set().themes["base16-ocean.dark"];
    let mut h = HighlightLines::new(syntax, theme);

    let mut out = Vec::new();
    for line in LinesWithEndings::from(code) {
        let ranges = h.highlight_line(line, ss).unwrap_or_default();
        let rendered: Vec<((u8, u8, u8), String)> = ranges
            .into_iter()
            .map(|(style, text)| {
                let c = style.foreground;
                ((c.r, c.g, c.b), text.trim_end_matches('\n').to_string())
            })
            .filter(|(_, t)| !t.is_empty())
            .collect();
        out.push(rendered);
    }
    // Drop a trailing empty line from the final newline.
    if out.last().map(|l| l.is_empty()).unwrap_or(false) {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_headings_bold_and_code() {
        let doc = MdDoc::parse("# Title\n\nsome **bold** and `code`.\n\n```rust\nfn a() {}\n```");
        // Heading + paragraph + code block.
        assert!(matches!(doc.blocks[0], MdBlock::Heading { level: 1, .. }));
        assert!(matches!(doc.blocks[1], MdBlock::Paragraph(_)));
        assert!(matches!(doc.blocks.last(), Some(MdBlock::Code { .. })));
    }

    #[test]
    fn split_marks_all_but_last_block_stable() {
        let theme = Theme::no_color();
        // Three blocks: heading, paragraph, paragraph.
        let doc = MdDoc::parse("# Title\n\nfirst para\n\nsecond para");
        let (lines, stable) = doc.to_lines_split(80, &theme);
        // Split points at the LAST block ("second para"), so its line is live.
        assert!(stable > 0 && stable < lines.len());
        assert_eq!(
            lines[stable]
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>(),
            "second para"
        );
        // Everything up to `stable` is the settled heading + first paragraph.
        assert_eq!(doc.to_lines(80, &theme), lines);
    }

    #[test]
    fn split_of_single_one_line_block_is_all_live() {
        let doc = MdDoc::parse("just one paragraph");
        let (_lines, stable) = doc.to_lines_split(80, &Theme::no_color());
        assert_eq!(stable, 0, "a lone one-line block is entirely live");
    }

    #[test]
    fn split_commits_all_but_last_line_of_a_streaming_paragraph() {
        // A single long paragraph that wraps to several lines: while it streams,
        // every line except the final (still-growing) one is stable and commits
        // to scrollback — so streamed text flows top-down instead of piling up in
        // the bottom-pinned live region and growing upward.
        let text = "一".repeat(120); // 240 cols → several lines at width 20
        let doc = MdDoc::parse(&text);
        let (lines, stable) = doc.to_lines_split(20, &Theme::no_color());
        assert!(lines.len() > 2, "should wrap to multiple lines");
        assert_eq!(
            stable,
            lines.len() - 1,
            "only the growing last line stays live"
        );
    }

    #[test]
    fn unclosed_streaming_strong_keeps_the_whole_block_live() {
        let text = format!("> **{}", "CodeLeveler 是一个跨平台编程 Agent。".repeat(8));
        let doc = MdDoc::parse(&text);
        let (lines, stable) = doc.to_lines_split(20, &Theme::no_color());

        assert!(lines.len() > 2, "fixture must wrap across several lines");
        assert_eq!(
            stable, 0,
            "an emphasis opener can be closed by a later delta, so no line in that block is stable"
        );
    }

    #[test]
    fn wraps_long_cjk_paragraph() {
        let text = "一".repeat(100);
        let doc = MdDoc::parse(&text);
        let lines = doc.to_lines(20, &Theme::no_color());
        // 100 full-width chars = 200 cols, must wrap into multiple lines <= 20.
        assert!(lines.len() > 1);
        for line in &lines {
            let w: usize = line.spans.iter().map(|s| s.content.width()).sum();
            assert!(w <= 20, "line exceeds width: {w}");
        }
    }

    #[test]
    fn preserves_manual_line_breaks_for_structured_output() {
        let doc = MdDoc::parse("文件 │ 变更 │ 说明\n────┼──────┼────\na.go │ +1 │ ok");
        let lines = doc.to_lines(80, &Theme::no_color());
        let rendered: Vec<String> = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();

        assert!(rendered.iter().any(|line| line.contains("文件 │ 变更")));
        assert!(rendered.iter().any(|line| line.contains("────┼──────")));
        assert!(rendered.iter().any(|line| line.contains("a.go │ +1")));
    }

    #[test]
    fn renders_bold_span_with_modifier() {
        let doc = MdDoc::parse("**hi**");
        let lines = doc.to_lines(40, &Theme::dark());
        let bold = lines[0]
            .spans
            .iter()
            .any(|s| s.style.add_modifier.contains(Modifier::BOLD));
        assert!(bold);
    }

    #[test]
    fn parses_and_renders_gfm_table() {
        let doc =
            MdDoc::parse("| 目录 | 用途 |\n|------|------|\n| core | 核心库 |\n| cli | 命令行 |");
        // One table block with a 2-cell header and two body rows.
        match doc.blocks.first() {
            Some(MdBlock::Table { header, rows, .. }) => {
                assert_eq!(header.len(), 2);
                assert_eq!(rows.len(), 2);
            }
            other => panic!("expected a table block, got {other:?}"),
        }
        // Rendered output must not leak the raw pipe/dash syntax.
        let lines = doc.to_lines(60, &Theme::no_color());
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("目录") && text.contains("核心库"));
        assert!(
            !text.contains("|------|"),
            "raw table separator leaked: {text}"
        );
    }

    #[test]
    fn full_grid_table_has_outer_box_and_row_lines() {
        let md = "| 项目 | 结果 |\n|---|---|\n| 重建 | 装好 |\n| 表格 | 生效 |";
        let lines: Vec<String> = MdDoc::parse(md)
            .to_lines(40, &Theme::no_color())
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        let joined = lines.join("\n");
        assert!(
            joined.contains('╭') && joined.contains('┬') && joined.contains('╮'),
            "top box: {joined}"
        );
        assert!(
            joined.contains('├') && joined.contains('┼') && joined.contains('┤'),
            "row/header separators: {joined}"
        );
        assert!(
            joined.contains('╰') && joined.contains('┴') && joined.contains('╯'),
            "bottom box: {joined}"
        );
        assert!(joined.contains('│'), "vertical column borders: {joined}");
        // A full grid puts a ├─┼─┤ line between the two body rows: at least two
        // such separators (header rule + one inter-row).
        let seps = lines.iter().filter(|l| l.starts_with('├')).count();
        assert!(
            seps >= 2,
            "expected header + inter-row separators, got {seps}: {joined}"
        );
        // Still no line exceeds the pane width.
        for l in &lines {
            assert!(
                disp_width(l) <= 40,
                "grid line exceeds width: {} ({l})",
                disp_width(l)
            );
        }
    }

    #[test]
    fn wide_table_never_exceeds_pane_width() {
        // Reproduces the TUI overflow: a table whose "改动" column holds a long
        // code-ish line must wrap, never run off the right edge.
        let md = "| # | 文件 | 改动 |\n|---|------|------|\n| 1 | internal/worker/executor.go:135 | runIssuePreprocess 里先 e.gitManager.GetOrClone(ctx, task.Repository) 拿 repoPath, 再传给 issuepre.Run(ctx, task, repoPath, deps) |";
        let doc = MdDoc::parse(md);
        for w in [60usize, 80, 100, 120] {
            let lines = doc.to_lines(w, &Theme::no_color());
            for line in &lines {
                let width: usize = line.spans.iter().map(|s| disp_width(&s.content)).sum();
                assert!(width <= w, "table line exceeds pane width {w}: got {width}");
            }
        }
    }

    #[test]
    fn highlights_code_block() {
        let doc = MdDoc::parse("```rust\nlet x = 1;\n```");
        let lines = doc.to_lines(80, &Theme::dark());
        assert!(!lines.is_empty());
    }

    #[test]
    fn long_code_line_wraps_instead_of_truncating() {
        let long = "x".repeat(120);
        let doc = MdDoc::parse(&format!("```\n{long}\n```"));
        let lines = doc.to_lines(30, &Theme::no_color());
        // 120 cols at width 30 (28 avail after indent) must wrap to several rows,
        // none exceeding the pane width.
        assert!(lines.len() >= 4, "wide code must wrap: {}", lines.len());
        for line in &lines {
            let w: usize = line.spans.iter().map(|s| disp_width(&s.content)).sum();
            assert!(w <= 30, "wrapped code line exceeds width: {w}");
        }
    }

    // --- Table layout robustness (emoji width, flex column, narrow fallback) ---

    fn line_text(l: &Line) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// Display widths of each column region in a rendered grid row, split on
    /// the vertical border. The full-grid layout has outer bars (`│ c │ c │`),
    /// so the empty regions outside them are dropped. Measured with the
    /// terminal-truthful [`disp_width`].
    fn col_region_widths(l: &Line) -> Vec<usize> {
        let text = line_text(l);
        let trimmed = text.trim().trim_start_matches('│').trim_end_matches('│');
        trimmed.split('│').map(disp_width).collect()
    }

    #[test]
    fn emoji_cell_keeps_columns_aligned() {
        // `unicode-width` counts an emoji-presentation glyph (⚠️ = base + VS16)
        // as 1 cell, but terminals draw 2. If the layout reserves by that count,
        // every column after the emoji drifts. Columns must stay aligned when a
        // cell holds an emoji.
        let doc = MdDoc::parse(
            "| 名称 | 级别 | 说明 |\n|---|---|---|\n\
             | 沙箱 | ⚠️ | 双层隔离 |\n\
             | 记忆 | ok | 跨会话 |",
        );
        let lines = doc.to_lines(60, &Theme::no_color());
        let rows: Vec<Vec<usize>> = lines
            .iter()
            .filter(|l| line_text(l).contains('│'))
            .map(col_region_widths)
            .collect();
        assert!(rows.len() >= 3, "header + two body rows");
        let first = &rows[0];
        for (i, r) in rows.iter().enumerate() {
            assert_eq!(
                r, first,
                "row {i} column widths drifted: {r:?} vs header {first:?}"
            );
        }
    }

    #[test]
    fn wide_text_column_flexes_instead_of_crushing_short_columns() {
        // A tiny label column beside a long prose column. When the natural table
        // overflows, the short column must keep (roughly) its natural width while
        // the prose column absorbs the slack — not shrink both proportionally.
        let long = "关于这条目的详细说明反复强调很多遍以便超过可用宽度触发压缩逻辑";
        let md = format!("| # | 说明 |\n|---|---|\n| 1 | {long} |\n| 2 | {long} |");
        let doc = MdDoc::parse(&md);
        let lines = doc.to_lines(40, &Theme::no_color());
        let header = lines
            .iter()
            .find(|l| line_text(l).contains('│'))
            .map(col_region_widths)
            .expect("a grid header row");
        // "#" column natural width is 1–2; proportional shrink would crush it to 1
        // while still overflowing the prose column. Flex keeps it at natural.
        assert!(
            header[0] >= 1 && header[0] <= 4,
            "short column should stay near natural, got {}",
            header[0]
        );
        // The prose column got the remaining budget (clearly wider than the label).
        assert!(
            header[1] > header[0] * 3,
            "prose column should flex to absorb the width: {header:?}"
        );
    }

    #[test]
    fn narrow_terminal_falls_back_to_stacked_records() {
        // Three columns cannot form a legible grid in a very narrow terminal;
        // the table degrades to stacked "label: value" records (no vertical
        // borders) so it stays readable.
        let doc = MdDoc::parse(
            "| 名称 | 级别 | 说明 |\n|---|---|---|\n| 沙箱隔离 | 严重 | 双层隔离机制 |",
        );
        let lines = doc.to_lines(18, &Theme::no_color());
        let text: String = lines.iter().map(|l| line_text(l) + "\n").collect();
        assert!(
            !text.contains('│'),
            "narrow render must not draw grid borders: {text}"
        );
        // Every header label is still present as a field key.
        for label in ["名称", "级别", "说明"] {
            assert!(text.contains(label), "missing field {label} in: {text}");
        }
        // The values survive too.
        assert!(text.contains("双层隔离机制"), "value missing: {text}");
    }

    #[test]
    fn stacked_records_keep_labels_inline_and_hang_wrapped_values() {
        let doc = MdDoc::parse("| 问题 | 风险 |\n|---|---|\n| 修复 | abcdefghijklmnop |");
        let lines = doc.to_lines(18, &Theme::no_color());
        let text: Vec<String> = lines.iter().map(line_text).collect();

        assert_eq!(text[0], "问题: 修复");
        assert_eq!(text[1], "风险: abcdefghijkl");
        assert_eq!(text[2], "      mnop");
    }

    #[test]
    fn multicolumn_table_stacks_before_columns_shatter() {
        // Four-column progress tables with Chinese headers and long prose are
        // common in model answers. At moderate TUI widths a grid fits on paper,
        // but each column becomes a stack of fragments; prefer records instead.
        let doc = MdDoc::parse(
            "| 诊断项 | 之前 | 之后 | 理由 |\n\
             |---|---|---|---|\n\
             | 默认 provider | 只看原始环境变量 | 合并 .env 后再判断 | 保证 doctor 和真实启动路径一致 |",
        );
        let lines = doc.to_lines(78, &Theme::no_color());
        let text: String = lines.iter().map(|l| line_text(l) + "\n").collect();
        assert!(
            !text.contains('│'),
            "moderately narrow multicolumn tables should not draw a shattered grid: {text}"
        );
        for label in ["诊断项", "之前", "之后", "理由"] {
            assert!(text.contains(label), "missing field {label} in: {text}");
        }
        assert!(text.contains("真实启动路径一致"), "value missing: {text}");
    }

    #[test]
    fn two_long_columns_stack_instead_of_crushing_the_non_flex_column() {
        // Final summaries often use two-column tables where both columns are
        // prose-like. Flexing only the widest column leaves the other column at
        // a tiny label cap, which renders as broken vertical fragments.
        let doc = MdDoc::parse(
            "| 测试 | 覆盖场景 |\n\
             |---|---|\n\
             | duplicate provider ids generate warning | 重复 id 产生 warning，只保留第一个 provider |\n\
             | JSON output includes generic diagnostics | JSON 格式包含 provider 信息和 masked key |",
        );
        let lines = doc.to_lines(78, &Theme::no_color());
        let text: String = lines.iter().map(|l| line_text(l) + "\n").collect();
        assert!(
            !text.contains('│'),
            "two long columns should fall back to stacked records: {text}"
        );
        for label in ["测试", "覆盖场景"] {
            assert!(text.contains(label), "missing field {label} in: {text}");
        }
        assert!(text.contains("只保留第一个"), "value missing: {text}");
    }

    #[test]
    fn wide_three_column_table_keeps_all_descriptive_columns_readable() {
        // Review tables often have three descriptive columns. Even on a wide
        // terminal, assigning all overflow to the longest column must not crush
        // the other two to label-sized vertical fragments.
        let doc = MdDoc::parse(
            "| 问题 | 位置 | 风险 |\n\
             |---|---|---|\n\
             | assignByRules 没有 milestone 适配器 | run.go:258 cleanIssueContent | 目前只做 TrimSpace 和空串兜底，模板占位符与日志冗余都没有去除，会导致模型收到大量噪音并影响分类准确率 |\n\
             | CreateRecord 的 IssueURL 未填写 | issuepre_adapter.go:122-126 | RecordsSeed 里有 IssueURL 字段，但执行任务和 webhook 入队都没有填充，审计日志会丢失关联信息 |",
        );
        let lines = doc.to_lines(160, &Theme::no_color());
        let header = lines
            .iter()
            .find(|l| line_text(l).contains('│'))
            .map(col_region_widths)
            .expect("a grid header row");

        assert_eq!(header.len(), 3);
        assert!(
            header[0] >= 20,
            "issue column was crushed into vertical fragments: {header:?}"
        );
        assert!(
            header[1] >= 24,
            "location column was crushed into vertical fragments: {header:?}"
        );
    }

    #[test]
    fn overflow_surplus_favors_the_widest_prose_column() {
        // When a table overflows, the leftover width is split by demand, not
        // equally: the column that is long on every row (风险) must end up wider
        // than an equal share — otherwise it wraps into many cramped lines while
        // a mostly-short column keeps a wasteful gutter (the old equal split).
        let doc = MdDoc::parse(
            "| 问题 | 位置 | 风险 |\n\
             |---|---|---|\n\
             | 清洗逻辑过于简陋 | run.go:258 cleanIssueContent | 目前只做了 TrimSpace 和空串兜底，模板占位符与日志冗余的去除逻辑尚未实现，会导致模型收到大量模板噪音并影响分类准确率 |\n\
             | IssueURL 未填写 | issuepre_adapter.go:122-126 | RecordSeed 里有 IssueURL 字段，但执行任务和 webhook 入队都没有填充，审计日志落库会是空串并丢失关联信息 |",
        );
        let width = 100;
        let header = doc
            .to_lines(width, &Theme::no_color())
            .iter()
            .find(|l| line_text(l).contains('│'))
            .map(col_region_widths)
            .expect("a grid header row");
        assert_eq!(header.len(), 3);
        // The prose column outscores an equal three-way split of the budget.
        assert!(
            header[2] > width / 3,
            "prose column did not win the surplus: {header:?}"
        );
        // …and beats the mostly-short problem column.
        assert!(
            header[2] > header[0],
            "prose column should be wider than the short label column: {header:?}"
        );
    }

    #[test]
    fn disp_width_counts_emoji_as_two() {
        assert_eq!(
            disp_width("⚠\u{FE0F}"),
            2,
            "emoji-presentation glyph is 2 cells"
        );
        assert_eq!(disp_width("🔴"), 2);
        assert_eq!(disp_width("🟡"), 2);
        assert_eq!(disp_width("ab"), 2, "ascii unchanged");
        assert_eq!(disp_width("中"), 2, "CJK unchanged");
    }

    #[test]
    fn short_code_is_inline_without_black_box() {
        let doc = MdDoc::parse("```rust\nlet x = 1;\n```");
        let lines = doc.to_lines(40, &Theme::no_color());
        let text: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(
            !text
                .iter()
                .any(|l| l.contains('┌') || l.contains('┃') || l.starts_with('·')),
            "short code must not use container: {text:?}"
        );
    }

    #[test]
    fn file_code_uses_light_header() {
        let doc = MdDoc::parse(
            "```go webhook.go:792\nfn main() {}\nfn other() {}\nfn third() {}\nfn fourth() {}\nfn fifth() {}\n```",
        );
        let theme = Theme::dark();
        let lines = doc.to_lines(40, &theme);
        let text: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(
            text.iter().any(|l| l.contains("webhook.go:792")),
            "path header missing: {text:?}"
        );
        assert!(
            !text
                .iter()
                .any(|l| l.starts_with('┃') || l.starts_with('┌')),
            "no heavy black box: {text:?}"
        );
    }

    #[test]
    fn long_code_fence_folds_middle() {
        let body: String = (0..40).map(|i| format!("line {i}\n")).collect();
        let doc = MdDoc::parse(&format!("```rust\n{body}```"));
        let lines = doc.to_lines(60, &Theme::no_color());
        let text: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(
            text.iter().any(|l| l.contains("lines omitted")),
            "long fence must fold: {text:?}"
        );
        assert!(
            text.len() < 40,
            "folded code must not flood conversation: {} lines",
            text.len()
        );
    }

    #[test]
    fn diff_fence_uses_diff_view_not_code_block() {
        let md = "```diff\n--- a/auth.go\n+++ b/auth.go\n@@ -120,2 +120,2 @@\n- old\n+ new\n```";
        let doc = MdDoc::parse(md);
        let lines = doc.to_lines(60, &Theme::no_color());
        let text: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(
            text.iter()
                .any(|l| l.contains('▼') || l.contains("auth.go")),
            "diff header missing: {text:?}"
        );
        assert!(
            !text
                .iter()
                .any(|l| l.starts_with('┌') || l.starts_with('┃')),
            "diff must not use CodeBlock chrome: {text:?}"
        );
    }

    #[test]
    fn table_alignment_pads_right_column() {
        // Col0 left, col1 right-aligned numbers.
        let doc = MdDoc::parse("| name | n |\n|:-----|----:|\n| a | 1 |\n| bb | 22 |");
        match doc.blocks.first() {
            Some(MdBlock::Table { align, .. }) => {
                assert_eq!(align.len(), 2);
                assert_eq!(align[0], ColAlign::Left);
                assert_eq!(align[1], ColAlign::Right);
            }
            other => panic!("expected table: {other:?}"),
        }
        let lines = doc.to_lines(40, &Theme::no_color());
        let body: Vec<String> = lines
            .iter()
            .map(line_text)
            .filter(|t| t.contains('│') && (t.contains('a') || t.contains("bb")))
            .collect();
        assert!(
            !body.is_empty(),
            "expected body rows, got {:?}",
            lines.iter().map(line_text).collect::<Vec<_>>()
        );
        // Right-aligned "1" should not sit flush left of the second cell region.
        // With right align, the digit is closer to the right border than left.
        for row in &body {
            if row.contains('1') && !row.contains("22") {
                let after_last_bar = row.rsplit('│').nth(1).unwrap_or("");
                assert!(
                    after_last_bar.trim_start().starts_with('1') || after_last_bar.contains(" 1"),
                    "right-aligned cell should pad on the left: {row:?}"
                );
            }
        }
    }

    #[test]
    fn diff_fence_uses_diff_theme_roles() {
        let doc = MdDoc::parse("```diff\n+added line\n-removed line\n context\n```");
        let theme = Theme::dark();
        let lines = doc.to_lines(60, &theme);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert!(
            texts.iter().any(|t| t.contains("+added")),
            "diff add line missing: {texts:?}"
        );
        assert!(
            texts.iter().any(|t| t.contains("-removed")),
            "diff del line missing: {texts:?}"
        );
        let mut saw_add = false;
        let mut saw_del = false;
        for line in &lines {
            for span in &line.spans {
                if span.content.contains("+added") {
                    assert_eq!(span.style.fg, Some(theme.diff_add));
                    saw_add = true;
                }
                if span.content.contains("-removed") {
                    assert_eq!(span.style.fg, Some(theme.diff_remove));
                    saw_del = true;
                }
            }
        }
        assert!(saw_add && saw_del, "diff roles not applied: {texts:?}");
    }

    #[test]
    fn https_link_is_underlined_without_osc_in_span_text() {
        let doc = MdDoc::parse("see [docs](https://example.com/path) please");
        let lines = doc.to_lines(80, &Theme::dark());
        let joined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(
            joined.contains("docs"),
            "link text must remain visible: {joined}"
        );
        // OSC 8 must NOT live in span content — inline write_line/truncate_to
        // replaces ESC with spaces and would paint the URL as garbage.
        assert!(
            !joined.contains('\u{1b}') && !joined.contains("]8;;"),
            "OSC sequences must not be embedded in Span text: {joined}"
        );
        let has_underline = lines.iter().any(|l| {
            l.spans.iter().any(|s| {
                s.content.contains("docs") && s.style.add_modifier.contains(Modifier::UNDERLINED)
            })
        });
        assert!(has_underline, "https links must be underlined: {joined}");
        assert!(
            joined.contains("see") && joined.contains("please"),
            "{joined}"
        );
    }
}
