//! The one diff surface: parse, inline emphasis and paint.
//!
//! Every place the TUI shows a *structured* diff — an edit tool's applied
//! patch, a Markdown `diff` fence, the Diff page, an expanded `git_diff` — goes
//! through this module. No consumer re-implements marker parsing, line
//! numbering or colour policy, so an added line looks the same everywhere.
//!
//! Ordinary tool logs are NOT diffs. [`looks_like_diff`] is the gate a
//! free-form preview must pass before it is parsed; a log that happens to
//! contain `+`/`-` lines stays a log.

use std::ops::Range;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::render::{sanitize_terminal_line, truncate_display};
use crate::selection::PlainRow;
use crate::theme::{ColorDepth, Theme};

/// Max body lines a fenced diff shows before folding the middle (header is
/// extra). Tool diffs are not folded here — their block owns the drawer.
const DIFF_MAX_BODY: usize = 10;
const DIFF_HEAD: usize = 5;
const DIFF_TAIL: usize = 3;

/// Tabs become spaces before anything measures or highlights a row: a raw `\t`
/// has no terminal-cell width, so every column after it would drift.
const DIFF_TAB: &str = "    ";

/// Bounds for syntax-highlighting one hunk on the paint path.
///
/// A whole-file write of a generated 200k-line file is still rendered in full
/// (the diff is the result the user asked to read); only the syntect pass is
/// skipped, so direction color remains and no frame stalls on a parser.
const DIFF_HIGHLIGHT_MAX_BYTES: usize = 128 * 1024;
const DIFF_HIGHLIGHT_MAX_LINES: usize = 4_000;
/// Whole-patch bound: past it, no hunk is highlighted at all.
const DIFF_PATCH_HIGHLIGHT_MAX_BYTES: usize = 1024 * 1024;

/// Placeholder RGB for a row syntect did not color (no language, over budget,
/// or a projection mismatch). It maps to the row's direction ink.
const DIFF_PLAIN_RGB: (u8, u8, u8) = (0x9A, 0xA3, 0xA7);

/// Inline-diff work bounds. Past either bound the changed middle is marked as a
/// whole instead of running an O(n·m) table on the paint path.
const INLINE_MAX_TOKENS: usize = 512;
const INLINE_MAX_CELLS: usize = 256 * 1024;

/// Which side of the patch a code row belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiffCodeKind {
    Context,
    Add,
    Remove,
}

/// One body row of a structured diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DiffRow {
    FileHeader {
        path: String,
    },
    Meta {
        text: String,
    },
    Hunk {
        /// Human-readable locator after the closing `@@`, when present.
        label: Option<String>,
        /// The raw `@@ … @@` header. A tool cell treats it as chrome, but a
        /// pasted diff fence must be able to copy its own payload unchanged.
        header: String,
    },
    /// Unchanged lines the applied diff skipped between two of its own hunks.
    /// `None` when the gap cannot be proved from the two hunk headers.
    Separator {
        unchanged: Option<usize>,
    },
    Code {
        /// Old-file line number when known — the side a removal lives on.
        old_line: Option<u32>,
        /// New-file line number when known — the side an addition lives on.
        new_line: Option<u32>,
        kind: DiffCodeKind,
        /// Code without the leading `+`/`-`/` ` marker, tabs expanded.
        text: String,
    },
}

impl DiffRow {
    /// The one number the single-column gutter shows: a removal lives in the
    /// old file, an addition in the new one, and context is the same line in
    /// both. `None` when the patch carried no numeric hunk header — an unknown
    /// location stays unknown rather than becoming a plausible-looking guess.
    pub(crate) fn gutter_line(&self) -> Option<u32> {
        match self {
            Self::Code {
                old_line,
                new_line,
                kind,
                ..
            } => match kind {
                DiffCodeKind::Remove => *old_line,
                DiffCodeKind::Add => *new_line,
                DiffCodeKind::Context => new_line.or(*old_line),
            },
            _ => None,
        }
    }
}

fn expand_diff_tabs(text: &str) -> String {
    if text.contains('\t') {
        text.replace('\t', DIFF_TAB)
    } else {
        text.to_string()
    }
}

/// Model text must be safe to paint: tabs are expanded, and control bytes
/// (including CSI colour sequences from model-authored content) are flattened
/// so no tool output can move the cursor or leave a colour set.
fn clean(text: &str) -> String {
    sanitize_terminal_line(&expand_diff_tabs(text))
}

/// Parse apply_patch / unified-diff text into display rows with line numbers.
///
/// Both sides are tracked. A removal belongs to the old file and an addition
/// to the new one, so a single cursor made a two-line replacement print the
/// same number twice ("59 -", "59 -"). Each hunk header re-seeds BOTH cursors,
/// so a second hunk never inherits the first one's count.
///
/// A patch with no numeric hunk header leaves both cursors `None`, and every
/// row it produces is unnumbered. Nothing here invents a starting line: a line
/// number is a fact about where the runtime actually changed the file, and a
/// wrong one shown as fact is worse than none.
pub(crate) fn parse_diff_rows(patch: &str) -> Vec<DiffRow> {
    let mut rows = Vec::new();
    let mut old_ln: Option<u32> = None;
    let mut new_ln: Option<u32> = None;
    // Whether the current file section has produced a code row yet. A hunk
    // header only separates two hunks; it never precedes the first one.
    let mut section_has_code = false;
    for raw in patch.lines() {
        let trimmed = raw.trim_end();
        if trimmed.starts_with("*** Begin Patch") || trimmed.starts_with("*** End Patch") {
            continue;
        }
        if let Some(path) = trimmed
            .strip_prefix("*** Update File: ")
            .or_else(|| trimmed.strip_prefix("*** Add File: "))
            .or_else(|| trimmed.strip_prefix("*** Delete File: "))
            .or_else(|| trimmed.strip_prefix("+++ b/"))
            .or_else(|| trimmed.strip_prefix("+++ "))
        {
            let path = path.trim();
            if path.is_empty() || path == "/dev/null" {
                continue;
            }
            rows.push(DiffRow::FileHeader { path: clean(path) });
            old_ln = None;
            new_ln = None;
            section_has_code = false;
            continue;
        }
        if trimmed.starts_with("--- ")
            || trimmed.starts_with("diff --git")
            || is_git_metadata(trimmed)
        {
            continue;
        }
        if trimmed.starts_with("@@") {
            // `@@ -a,b +c,d @@` or bare `@@` / `@@ label`. A bare header
            // carries no location, so both cursors go unknown rather than
            // carrying the previous hunk's count into this one.
            let next_old = hunk_start(trimmed, '-');
            let next_new = hunk_start(trimmed, '+');
            if section_has_code {
                // Two hunks of the SAME applied diff are numbered against the
                // same file snapshot, so the lines between them can be
                // counted. The previous hunk's new-side cursor is one past the
                // last line it covered — including a deletion-only hunk, whose
                // removed lines live on the old side. A count is stated only
                // when both headers prove it.
                let previous_last = new_ln.and_then(|next| next.checked_sub(1));
                let unchanged = match (previous_last, next_new) {
                    (Some(last), Some(next)) => next
                        .checked_sub(last)
                        .and_then(|gap| gap.checked_sub(1))
                        .filter(|gap| *gap > 0)
                        .map(|gap| gap as usize),
                    _ => None,
                };
                rows.push(DiffRow::Separator { unchanged });
            }
            old_ln = next_old;
            new_ln = next_new;
            section_has_code = false;
            // The raw header stays in the model for copy fidelity; whether a
            // bare ``@@ -56,5 +56,6 @@`` is painted is the surface's choice.
            rows.push(DiffRow::Hunk {
                label: hunk_label(trimmed).map(|label| clean(&label)),
                header: clean(trimmed),
            });
            continue;
        }
        if trimmed.starts_with("*** ") {
            rows.push(DiffRow::Meta {
                text: clean(trimmed),
            });
            continue;
        }
        if let Some(rest) = trimmed
            .strip_prefix('+')
            .filter(|_| !trimmed.starts_with("+++"))
        {
            // An addition exists only in the new file: it advances that
            // cursor and leaves the old one where it was.
            let ln = new_ln;
            if let Some(n) = new_ln.as_mut() {
                *n = n.saturating_add(1);
            }
            section_has_code = true;
            rows.push(DiffRow::Code {
                old_line: None,
                new_line: ln,
                kind: DiffCodeKind::Add,
                text: clean(rest),
            });
            continue;
        }
        if let Some(rest) = trimmed
            .strip_prefix('-')
            .filter(|_| !trimmed.starts_with("---"))
        {
            // A removal exists only in the old file: it advances that cursor
            // and leaves the new one where it was.
            let ln = old_ln;
            if let Some(n) = old_ln.as_mut() {
                *n = n.saturating_add(1);
            }
            section_has_code = true;
            rows.push(DiffRow::Code {
                old_line: ln,
                new_line: None,
                kind: DiffCodeKind::Remove,
                text: clean(rest),
            });
            continue;
        }
        // Context: leading space optional. It exists on both sides, so both
        // cursors advance.
        let text = trimmed.strip_prefix(' ').unwrap_or(trimmed);
        let (old_at, new_at) = (old_ln, new_ln);
        if let Some(n) = old_ln.as_mut() {
            *n = n.saturating_add(1);
        }
        if let Some(n) = new_ln.as_mut() {
            *n = n.saturating_add(1);
        }
        section_has_code = true;
        rows.push(DiffRow::Code {
            old_line: old_at,
            new_line: new_at,
            kind: DiffCodeKind::Context,
            text: clean(text),
        });
    }
    rows
}

/// Git's own patch metadata (`index …`, `new file mode …`) is not code and has
/// no marker, so it would otherwise be mistaken for a context row.
fn is_git_metadata(line: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "index ",
        "new file mode ",
        "deleted file mode ",
        "old mode ",
        "new mode ",
        "similarity index ",
        "dissimilarity index ",
        "rename from ",
        "rename to ",
        "copy from ",
        "copy to ",
        "Binary files ",
        "GIT binary patch",
        "\\ No newline at end of file",
    ];
    PREFIXES.iter().any(|prefix| line.starts_with(prefix))
}

/// Whether free-form tool output really is a structured patch.
///
/// The bar is deliberately high: a log line that starts with `+` or `-` is
/// ordinary text. A hunk header alone is not enough either — it must be
/// followed by at least one marked change, or a patch header must name a file.
pub(crate) fn looks_like_diff(text: &str) -> bool {
    let mut has_hunk = false;
    for line in text.lines().take(4_000) {
        if line.starts_with("diff --git ")
            || line.starts_with("*** Begin Patch")
            || line.starts_with("*** Update File: ")
        {
            return true;
        }
        if line.starts_with("@@") {
            has_hunk = true;
            continue;
        }
        if has_hunk
            && (line.starts_with('+') && !line.starts_with("+++")
                || line.starts_with('-') && !line.starts_with("---"))
        {
            return true;
        }
    }
    false
}

/// The human-readable part of a hunk header, if any: the text after the
/// closing `@@` of a unified header, or the whole locator of an apply_patch
/// `@@ <label>` line. Pure numeric ranges return `None`.
fn hunk_label(header: &str) -> Option<String> {
    let rest = header.strip_prefix("@@")?;
    let label = match rest.find("@@") {
        Some(i) => &rest[i + 2..],
        None => rest,
    };
    let label = label.trim();
    (!label.is_empty()).then(|| label.to_string())
}

/// The start line on one side of a unified hunk header (`@@ -56,5 +56,6 @@`).
/// `None` for a bare `@@` or any header without a number on that side.
fn hunk_start(header: &str, side: char) -> Option<u32> {
    let at = header.find(side)?;
    let digits: String = header[at + side.len_utf8()..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// Syntect colors for every code row, or `None` for a row that renders plain.
///
/// Each hunk is highlighted as its OWN code excerpt, with the old and new
/// side parsed separately: a unified diff interleaves two file versions, and a
/// single highlighter would leak an unterminated comment or string across
/// them. A row only accepts its highlighted line when the runs concatenate
/// back to its exact text, so a projection mismatch degrades to direction
/// color instead of showing different characters.
pub(crate) fn highlight_diff_rows(
    rows: &[DiffRow],
    language_hint: Option<&str>,
) -> Vec<Option<crate::markdown::HighlightedLine>> {
    let mut styles: Vec<Option<crate::markdown::HighlightedLine>> = vec![None; rows.len()];
    let total: usize = rows
        .iter()
        .map(|row| match row {
            DiffRow::Code { text, .. } => text.len(),
            _ => 0,
        })
        .sum();
    if total > DIFF_PATCH_HIGHLIGHT_MAX_BYTES {
        return styles;
    }
    let mut language: Option<String> = language_hint
        .map(str::trim)
        .filter(|hint| !hint.is_empty())
        .and_then(|hint| {
            crate::markdown::syntax_name_for_path(hint).or_else(|| Some(hint.to_string()))
        });
    let mut i = 0;
    while i < rows.len() {
        match &rows[i] {
            DiffRow::FileHeader { path } => {
                language = crate::markdown::syntax_name_for_path(path);
                i += 1;
            }
            DiffRow::Code { .. } => {
                let start = i;
                while i < rows.len() && matches!(rows[i], DiffRow::Code { .. }) {
                    i += 1;
                }
                // Diff rows are code by definition; when neither a header nor
                // a caller hint identified the language, Bash is the generic
                // syntax fallback rather than flat text.
                let language = language.as_deref().unwrap_or("Bash");
                let run = rows.get(start..i).unwrap_or_default();
                if run.is_empty() {
                    continue;
                }
                let mut old_text = String::new();
                let mut new_text = String::new();
                for row in run {
                    let DiffRow::Code { kind, text, .. } = row else {
                        continue;
                    };
                    if *kind != DiffCodeKind::Add {
                        old_text.push_str(text);
                        old_text.push('\n');
                    }
                    if *kind != DiffCodeKind::Remove {
                        new_text.push_str(text);
                        new_text.push('\n');
                    }
                }
                if run.len() > DIFF_HIGHLIGHT_MAX_LINES
                    || old_text.len() + new_text.len() > DIFF_HIGHLIGHT_MAX_BYTES
                {
                    continue;
                }
                let old_hl = crate::markdown::highlight_code_cached(&old_text, Some(language));
                let new_hl = crate::markdown::highlight_code_cached(&new_text, Some(language));
                // Each side's line sequence is built from THIS run, so the
                // lookup index is run-local — never the section-wide one, or a
                // second hunk would read past its own highlighted text.
                let mut local_old = 0usize;
                let mut local_new = 0usize;
                for (offset, row) in run.iter().enumerate() {
                    let DiffRow::Code { kind, text, .. } = row else {
                        continue;
                    };
                    let highlighted = match kind {
                        DiffCodeKind::Remove => {
                            let line = old_hl.get(local_old);
                            local_old += 1;
                            line
                        }
                        DiffCodeKind::Add => {
                            let line = new_hl.get(local_new);
                            local_new += 1;
                            line
                        }
                        DiffCodeKind::Context => {
                            let line = new_hl.get(local_new);
                            local_old += 1;
                            local_new += 1;
                            line
                        }
                    };
                    let Some(highlighted) = highlighted else {
                        continue;
                    };
                    let joined: String = highlighted
                        .as_ref()
                        .iter()
                        .map(|(_, segment)| segment.as_str())
                        .collect();
                    if joined != *text {
                        continue;
                    }
                    if let Some(slot) = styles.get_mut(start + offset) {
                        *slot = Some(highlighted.clone());
                    }
                }
            }
            _ => i += 1,
        }
    }
    styles
}

/// How a renderer expresses add/remove direction with the palette it has.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BandMode {
    /// A row-wide tint under the body. TrueColor uses the directional theme
    /// bands; 256-colour uses a neutral row band because the xterm cube has no
    /// soft dark green/red (its nearest hue step is a saturated slab).
    Row,
    /// 16-colour / monochrome: no background; direction ink carries meaning
    /// on the marker and gutter, while code keeps its syntax ink.
    None,
}

fn band_mode(theme: &Theme) -> BandMode {
    match theme.color_depth() {
        ColorDepth::TrueColor | ColorDepth::Ansi256 => BandMode::Row,
        ColorDepth::Ansi16 | ColorDepth::None => BandMode::None,
    }
}

/// One content run's ink. The row band says "added" or "removed"; the code
/// body always keeps the semantic syntax ink. Only the marker, gutter and
/// gutter carry direction.
fn diff_content_style(rgb: (u8, u8, u8), theme: &Theme) -> Style {
    // Our own sentinel means "syntect did not colour this row"; it is not a
    // syntax colour and must not be classified into one.
    if rgb == DIFF_PLAIN_RGB {
        return Style::default().fg(theme.syntax.plain);
    }
    crate::code_block::code_style(rgb, theme)
}

// --- inline diff ------------------------------------------------------------

/// Byte ranges of one row that changed relative to its paired row.
type InlineRanges = Vec<Range<usize>>;

/// Pair every contiguous remove-block with the add-block that follows it.
///
/// Pairing is positional and only when both sides have the same line count:
/// a one-for-one replacement has an obvious correspondence, while unequal
/// blocks have none this renderer can prove. Unpaired rows keep their row
/// band and get no inline emphasis — no invented correspondence.
fn inline_diff(rows: &[DiffRow]) -> Vec<Option<InlineRanges>> {
    let mut out = vec![None; rows.len()];
    let mut i = 0;
    while i < rows.len() {
        let is_remove = matches!(
            rows[i],
            DiffRow::Code {
                kind: DiffCodeKind::Remove,
                ..
            }
        );
        if !is_remove {
            i += 1;
            continue;
        }
        let rem_start = i;
        while i < rows.len()
            && matches!(
                rows[i],
                DiffRow::Code {
                    kind: DiffCodeKind::Remove,
                    ..
                }
            )
        {
            i += 1;
        }
        let add_start = i;
        while i < rows.len()
            && matches!(
                rows[i],
                DiffRow::Code {
                    kind: DiffCodeKind::Add,
                    ..
                }
            )
        {
            i += 1;
        }
        let removed = &rows[rem_start..add_start];
        let added = &rows[add_start..i];
        if removed.is_empty() || added.is_empty() || removed.len() != added.len() {
            continue;
        }
        for (offset, (removed_row, added_row)) in removed.iter().zip(added).enumerate() {
            let (DiffRow::Code { text: old, .. }, DiffRow::Code { text: new, .. }) =
                (removed_row, added_row)
            else {
                continue;
            };
            let (old_ranges, new_ranges) = changed_ranges(old, new);
            out[rem_start + offset] = (!old_ranges.is_empty()).then_some(old_ranges);
            out[add_start + offset] = (!new_ranges.is_empty()).then_some(new_ranges);
        }
    }
    out
}

/// Tokens: an ASCII identifier run, a whitespace run, or one grapheme.
///
/// Keeping CJK and emoji as single-grapheme tokens makes a one-character
/// Chinese or emoji change highlight that character, not the whole sentence.
/// ASCII identifiers stay whole so a renamed variable reads as one change.
fn tokens(text: &str) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    let mut last_class = 0u8;
    for (start, grapheme) in text.grapheme_indices(true) {
        let end = start + grapheme.len();
        let first = grapheme.chars().next().unwrap_or(' ');
        let class = if first.is_ascii_alphanumeric() || first == '_' {
            1
        } else if grapheme.chars().all(char::is_whitespace) {
            2
        } else {
            // A CJK character, an emoji sequence, punctuation, a combining
            // mark: each cluster is its own token.
            3
        };
        if class != 3 && class == last_class {
            if let Some(last) = out.last_mut() {
                last.1 = end;
            }
        } else {
            out.push((start, end));
        }
        last_class = class;
    }
    out
}

/// Changed byte ranges for a paired (removed, added) line.
fn changed_ranges(old: &str, new: &str) -> (InlineRanges, InlineRanges) {
    let old_tokens = tokens(old);
    let new_tokens = tokens(new);
    let mut prefix = 0usize;
    while prefix < old_tokens.len().min(new_tokens.len())
        && old[old_tokens[prefix].0..old_tokens[prefix].1]
            == new[new_tokens[prefix].0..new_tokens[prefix].1]
    {
        prefix += 1;
    }
    let mut suffix = 0usize;
    while suffix
        < old_tokens
            .len()
            .min(new_tokens.len())
            .saturating_sub(prefix)
        && old[old_tokens[old_tokens.len() - 1 - suffix].0
            ..old_tokens[old_tokens.len() - 1 - suffix].1]
            == new[new_tokens[new_tokens.len() - 1 - suffix].0
                ..new_tokens[new_tokens.len() - 1 - suffix].1]
    {
        suffix += 1;
    }
    let old_mid = &old_tokens[prefix..old_tokens.len() - suffix];
    let new_mid = &new_tokens[prefix..new_tokens.len() - suffix];
    if old_mid.is_empty() && new_mid.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let changed_old = if old_mid.is_empty() {
        Vec::new()
    } else if old_mid.len() > INLINE_MAX_TOKENS
        || new_mid.len() > INLINE_MAX_TOKENS
        || old_mid.len() * new_mid.len() > INLINE_MAX_CELLS
    {
        whole_token_span(old_mid)
    } else {
        unmatched_ranges(old, old_mid, new, new_mid)
    };
    let changed_new = if new_mid.is_empty() {
        Vec::new()
    } else if old_mid.len() > INLINE_MAX_TOKENS
        || new_mid.len() > INLINE_MAX_TOKENS
        || old_mid.len() * new_mid.len() > INLINE_MAX_CELLS
    {
        whole_token_span(new_mid)
    } else {
        unmatched_ranges(new, new_mid, old, old_mid)
    };
    (changed_old, changed_new)
}

/// One byte span from the first token start through the last token end.
///
/// This is the LCS fallback: a single [`Range`] over the whole changed run,
/// including any token the table would have matched in the middle. It is not
/// one element per index.
fn whole_token_span(tokens: &[(usize, usize)]) -> InlineRanges {
    std::iter::once(tokens[0].0..tokens[tokens.len() - 1].1).collect()
}

/// Ranges of `side` tokens that are not part of an LCS with the other side.
fn unmatched_ranges(
    side: &str,
    side_tokens: &[(usize, usize)],
    other: &str,
    other_tokens: &[(usize, usize)],
) -> InlineRanges {
    let n = side_tokens.len();
    let m = other_tokens.len();
    let mut table = vec![vec![0u16; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            table[i][j] = if side[side_tokens[i].0..side_tokens[i].1]
                == other[other_tokens[j].0..other_tokens[j].1]
            {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }
    let mut matched = vec![false; n];
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if side[side_tokens[i].0..side_tokens[i].1] == other[other_tokens[j].0..other_tokens[j].1] {
            matched[i] = true;
            i += 1;
            j += 1;
        } else if table[i + 1][j] >= table[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    let mut ranges: InlineRanges = Vec::new();
    for (index, token) in side_tokens.iter().enumerate() {
        if matched[index] {
            continue;
        }
        match ranges.last_mut() {
            Some(last) if last.end == token.0 => last.end = token.1,
            _ => ranges.push(token.0..token.1),
        }
    }
    ranges
}

// --- painting ---------------------------------------------------------------

/// Layout policy of one diff body. The data model and paint loop are shared;
/// only the frame around the rows differs between a tool cell and a fence.
pub(crate) struct DiffPaintOptions<'a> {
    /// First-row lead (`  └ ` under a tool head, two spaces in a fence).
    pub first_lead: &'a str,
    /// Continuation lead of the same display width.
    pub rest_lead: &'a str,
    pub line_numbers: bool,
    /// Banded rows fill `width` so the band reaches the pane edge.
    pub fill_width: bool,
    /// Paint the raw `@@ … @@` header even when it has no human label. A tool
    /// cell's gutter already says the location; a fence or the Diff page is
    /// showing a patch as a document and keeps its structure visible.
    pub hunk_headers: bool,
    /// Copy the patch's own syntax: `@@ … @@` lines and the `+`/`-`/space
    /// marker. Tool-cell copy yields source code, not patch syntax.
    pub patch_copy: bool,
    /// File path or language token for a caller that already dropped the
    /// patch's `+++` header. A `DiffRow::FileHeader` still wins when present.
    pub language: Option<&'a str>,
}

impl Default for DiffPaintOptions<'_> {
    fn default() -> Self {
        Self {
            first_lead: "",
            rest_lead: "",
            line_numbers: true,
            fill_width: true,
            hunk_headers: false,
            patch_copy: false,
            language: None,
        }
    }
}

/// One painted line plus what copy may take from it. `None` is display-only
/// chrome (a header, a hunk label, a gap marker) and breaks the copied line in
/// the tool adapter.
pub(crate) struct PaintedDiffRow {
    pub line: Line<'static>,
    pub plain: Option<PlainRow>,
}

/// Paint every row of a parsed diff.
///
/// `separator_label` localizes the `⋯ N unchanged lines` marker so this module
/// owns no user-facing copy.
pub(crate) fn paint_diff_rows(
    rows: &[DiffRow],
    theme: &Theme,
    width: usize,
    opts: &DiffPaintOptions<'_>,
    separator_label: &dyn Fn(Option<usize>) -> String,
) -> Vec<PaintedDiffRow> {
    if rows.is_empty() {
        return Vec::new();
    }
    let styles = highlight_diff_rows(rows, opts.language);
    let inline = inline_diff(rows);
    // The gutter is sized from the largest real line number in the patch, so
    // the code column never shifts sideways partway down. A file with 100k
    // lines gets six digits; a short one gets two.
    let widest = opts
        .line_numbers
        .then(|| rows.iter().filter_map(DiffRow::gutter_line).max())
        .flatten();
    let digits = widest.map(|n| n.to_string().len()).unwrap_or(0);
    // "<digits> │ " when numbered, a plain two-space indent when not.
    let gutter_w = if !opts.line_numbers {
        0
    } else if digits > 0 {
        digits + 3
    } else {
        2
    };
    let mark_w = 2; // "+ " / "- " / "  "
    let lead_w = UnicodeWidthStr::width(opts.first_lead);
    let code_indent = lead_w + gutter_w + mark_w;
    let inner = width.saturating_sub(code_indent).max(1);
    // Continuation rows hold the gutter and marker columns open so wrapped
    // code stays in one column.
    let continuation = format!("{}{}", " ".repeat(gutter_w), " ".repeat(mark_w));
    let mode = band_mode(theme);

    let mut out = Vec::with_capacity(rows.len());
    for (position, row) in rows.iter().enumerate() {
        let lead = if position == 0 {
            opts.first_lead
        } else {
            opts.rest_lead
        };
        match row {
            DiffRow::FileHeader { path } => {
                out.push(PaintedDiffRow {
                    line: Line::from(vec![
                        Span::styled(lead.to_string(), Style::default().fg(theme.text.muted)),
                        Span::styled(
                            truncate_display(path, width.saturating_sub(lead_w).max(8)),
                            Style::default()
                                .fg(theme.text.primary)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ]),
                    plain: None,
                });
            }
            DiffRow::Meta { text } => {
                out.push(PaintedDiffRow {
                    line: Line::from(vec![
                        Span::styled(lead.to_string(), Style::default().fg(theme.text.muted)),
                        Span::styled(
                            truncate_display(text, width.saturating_sub(lead_w).max(8)),
                            Style::default().fg(theme.text.muted),
                        ),
                    ]),
                    plain: None,
                });
            }
            DiffRow::Hunk { label, header } => {
                // A tool cell paints only a human locator (the gutter carries
                // the machine location); a fence/whole-patch view keeps the
                // raw header as visible structure and copyable payload.
                let text = if opts.hunk_headers {
                    Some(header.as_str())
                } else {
                    label.as_deref()
                };
                let Some(text) = text else { continue };
                out.push(PaintedDiffRow {
                    line: Line::from(vec![
                        Span::styled(lead.to_string(), Style::default().fg(theme.text.muted)),
                        Span::styled(
                            truncate_display(text, width.saturating_sub(lead_w).max(8)),
                            Style::default().fg(theme.text.muted),
                        ),
                    ]),
                    plain: (opts.patch_copy && opts.hunk_headers).then(|| PlainRow {
                        indent: lead_w.min(u16::MAX as usize) as u16,
                        source: Some(Box::from(header.as_str())),
                        ..Default::default()
                    }),
                });
            }
            DiffRow::Separator { unchanged } => {
                out.push(PaintedDiffRow {
                    line: Line::from(vec![
                        Span::styled(lead.to_string(), Style::default().fg(theme.text.muted)),
                        Span::styled(
                            separator_label(*unchanged),
                            Style::default().fg(theme.text.muted),
                        ),
                    ]),
                    plain: None,
                });
            }
            DiffRow::Code { kind, text, .. } => {
                let band = match (mode, kind) {
                    (BandMode::None, _) | (_, DiffCodeKind::Context) => None,
                    (_, DiffCodeKind::Add) => Some(theme.diff.added_bg),
                    (_, DiffCodeKind::Remove) => Some(theme.diff.removed_bg),
                };
                let content_band = band;
                let (mark, mark_style) = match kind {
                    DiffCodeKind::Add => (
                        "+ ",
                        Style::default()
                            .fg(theme.diff.added)
                            .add_modifier(Modifier::BOLD),
                    ),
                    DiffCodeKind::Remove => ("- ", Style::default().fg(theme.diff.removed)),
                    DiffCodeKind::Context => ("  ", Style::default().fg(theme.diff.context)),
                };
                // The number itself stays quiet; on a band it steps up to
                // secondary because muted misses 4.5:1 on the tinted surface.
                let gutter_ink = if band.is_some() {
                    theme.text.secondary
                } else {
                    theme.text.muted
                };
                let gutter = if opts.line_numbers {
                    Some(if digits > 0 {
                        match row.gutter_line() {
                            Some(n) => format!("{n:>digits$} │ "),
                            None => format!("{:>digits$} │ ", ""),
                        }
                    } else {
                        "  ".to_string()
                    })
                } else {
                    None
                };
                let runs = styled_runs(
                    text,
                    styles[position].as_ref().map(|runs| runs.as_slice()),
                    *kind,
                    inline[position].as_deref(),
                    theme,
                );
                let wrapped = crate::markdown::wrap_styled_runs(&runs, inner);
                for (row_index, content) in wrapped.into_iter().enumerate() {
                    let mut spans = Vec::with_capacity(content.len() + 4);
                    spans.push(Span::styled(
                        lead.to_string(),
                        banded(
                            Style::default().fg(if band.is_some() {
                                theme.text.secondary
                            } else {
                                theme.text.muted
                            }),
                            band,
                        ),
                    ));
                    if row_index == 0 {
                        if let Some(gutter) = gutter.as_ref() {
                            spans.push(Span::styled(
                                gutter.clone(),
                                banded(Style::default().fg(gutter_ink), band),
                            ));
                        }
                        spans.push(Span::styled(mark.to_string(), banded(mark_style, band)));
                    } else {
                        spans.push(Span::styled(
                            continuation.clone(),
                            banded(Style::default(), band),
                        ));
                    }
                    for span in content {
                        spans.push(Span::styled(span.content, banded(span.style, content_band)));
                    }
                    let content_raw: usize = spans
                        .iter()
                        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                        .sum();
                    let content_disp: usize = spans
                        .iter()
                        .map(|span| crate::markdown::disp_width(span.content.as_ref()))
                        .sum();
                    let mut suffix = 0usize;
                    if opts.fill_width
                        && let Some(bg) = content_band
                    {
                        let pad = width.saturating_sub(content_disp);
                        if pad > 0 {
                            spans.push(Span::styled(" ".repeat(pad), Style::default().bg(bg)));
                        }
                        // Everything after the source — our band padding and
                        // any emoji-width difference the pane will add later —
                        // is decoration and stays out of the clipboard.
                        suffix = width.saturating_sub(content_raw).min(u16::MAX as usize);
                    }
                    let plain = PlainRow {
                        indent: code_indent.min(u16::MAX as usize) as u16,
                        // A tool cell copies source code, not patch syntax, so
                        // its marker stays on screen. A fence is a pasted
                        // patch: its `+`/`-`/space is the payload and rides
                        // with the first wrapped row only.
                        prefix: (opts.patch_copy && row_index == 0).then(|| {
                            Box::from(match kind {
                                DiffCodeKind::Add => "+",
                                DiffCodeKind::Remove => "-",
                                DiffCodeKind::Context => " ",
                            })
                        }),
                        joiner: (row_index > 0).then(|| Box::from("")),
                        suffix: suffix as u16,
                        ..Default::default()
                    };
                    out.push(PaintedDiffRow {
                        line: Line::from(spans),
                        plain: Some(plain),
                    });
                }
            }
        }
    }
    out
}

fn banded(style: Style, band: Option<Color>) -> Style {
    // An explicit background (the inline token band) outranks the row band;
    // the row band only fills what the content did not already own.
    if style.bg.is_some() {
        return style;
    }
    match band {
        Some(bg) => style.bg(bg),
        None => style,
    }
}

/// Convert one row's syntax runs into styled runs, splitting at inline ranges.
fn styled_runs(
    text: &str,
    highlighted: Option<&[((u8, u8, u8), String)]>,
    kind: DiffCodeKind,
    inline: Option<&[Range<usize>]>,
    theme: &Theme,
) -> Vec<(Style, String)> {
    let base: Vec<((u8, u8, u8), String)> = match highlighted {
        Some(highlighted) => highlighted.to_vec(),
        None => vec![(DIFF_PLAIN_RGB, text.to_string())],
    };
    let Some(ranges) = inline.filter(|ranges| !ranges.is_empty()) else {
        return base
            .into_iter()
            .map(|(rgb, segment)| (diff_content_style(rgb, theme), segment))
            .collect();
    };
    let mut out = Vec::new();
    let mut cursor = 0usize;
    for (rgb, segment) in base {
        let style = diff_content_style(rgb, theme);
        let run_start = cursor;
        let run_end = cursor + segment.len();
        let mut at = run_start;
        for range in ranges {
            let start = range.start.max(run_start);
            let end = range.end.min(run_end);
            if start >= end {
                continue;
            }
            if at < start {
                out.push((style, text[at..start].to_string()));
            }
            out.push((
                inline_emphasis(style, kind, theme),
                text[start..end].to_string(),
            ));
            at = end;
        }
        if at < run_end {
            out.push((style, text[at..run_end].to_string()));
        }
        cursor = run_end;
    }
    out
}

/// Inline changed characters carry a stronger background when the terminal can
/// paint one. The token's own syntax ink is preserved; only a token that would
/// sink below 4.5:1 on the stronger band is stepped toward white/black. On
/// 16-colour and monochrome terminals the emphasis moves to underline, which
/// every terminal understands.
fn inline_emphasis(base: Style, kind: DiffCodeKind, theme: &Theme) -> Style {
    match theme.color_depth() {
        ColorDepth::TrueColor | ColorDepth::Ansi256 => {
            let bg = match kind {
                DiffCodeKind::Add => theme.diff.added_inline_bg,
                DiffCodeKind::Remove => theme.diff.removed_inline_bg,
                DiffCodeKind::Context => theme.diff.added_inline_bg,
            };
            let fg = base.fg.map(|fg| theme.readable_on(fg, bg, 4.5));
            base.fg(fg.unwrap_or(theme.text.primary)).bg(bg)
        }
        ColorDepth::Ansi16 | ColorDepth::None => base.add_modifier(Modifier::UNDERLINED),
    }
}

// --- fenced diffs -----------------------------------------------------------

/// Parsed unified-diff presentation model for a Markdown `diff` fence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiffView {
    pub file: String,
    pub start_line: Option<u32>,
    pub added: usize,
    pub removed: usize,
    rows: Vec<DiffRow>,
}

impl DiffView {
    /// Parse a unified-diff body (optionally with `***` patch headers).
    pub(crate) fn parse(src: &str, title_hint: Option<&str>) -> Self {
        let rows = parse_diff_rows(src);
        let file = rows
            .iter()
            .find_map(|row| match row {
                DiffRow::FileHeader { path } => Some(compact_file(path)),
                _ => None,
            })
            .or_else(|| title_hint.map(str::to_string))
            .unwrap_or_else(|| "diff".to_string());
        let start_line = rows.iter().find_map(|row| match row {
            DiffRow::Code { new_line, .. } => *new_line,
            DiffRow::FileHeader { .. } => None,
            _ => None,
        });
        let added = rows
            .iter()
            .filter(|row| {
                matches!(
                    row,
                    DiffRow::Code {
                        kind: DiffCodeKind::Add,
                        ..
                    }
                )
            })
            .count();
        let removed = rows
            .iter()
            .filter(|row| {
                matches!(
                    row,
                    DiffRow::Code {
                        kind: DiffCodeKind::Remove,
                        ..
                    }
                )
            })
            .count();
        Self {
            file,
            start_line,
            added,
            removed,
            rows,
        }
    }

    /// Header label: `auth.go:120` or `auth.go (+3 -1)`.
    pub(crate) fn header_label(&self) -> String {
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

/// Render a diff plus its semantic copy policy: the `▼` header and the
/// "lines hidden" marker are display chrome and do not copy. Body rows are
/// the patch's own code; the gutter and `+`/`-` marker are visual, and a
/// wrapped source line rejoins with no injected newline.
pub(crate) fn render_diff_view_with_copy(
    view: &DiffView,
    width: usize,
    theme: &Theme,
) -> (Vec<Line<'static>>, Vec<PlainRow>) {
    let width = width.max(12);
    let header = view.header_label();
    let mut out = Vec::new();
    let mut copy = Vec::new();

    // Compact header — not a black box.
    out.push(Line::from(vec![
        Span::styled("▼ ", Style::default().fg(theme.accent.primary)),
        Span::styled(
            truncate_display(&header, width.saturating_sub(2).max(4)),
            Style::default()
                .fg(theme.text.primary)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    copy.push(PlainRow {
        skip: true,
        ..Default::default()
    });

    let body = fold_diff_rows(&view.rows);
    // The fence's own `▼ file` line already names the file; a per-file header
    // inside the body would repeat it and break the continuous body. The
    // header is still read for its path: it is the language fact for the rows
    // that follow, and a multi-file patch must switch grammar at each header.
    let mut language: Option<String> = Some(view.file.clone());
    let mut chunk: Vec<DiffRow> = Vec::new();
    for folded in body {
        match folded {
            Folded::Row(row) => {
                if let DiffRow::FileHeader { path } = &row {
                    paint_chunk(
                        &chunk,
                        width,
                        theme,
                        language.as_deref(),
                        &mut out,
                        &mut copy,
                    );
                    chunk.clear();
                    language = crate::markdown::syntax_name_for_path(path);
                    continue;
                }
                chunk.push(row);
            }
            Folded::Omitted(n) => {
                paint_chunk(
                    &chunk,
                    width,
                    theme,
                    language.as_deref(),
                    &mut out,
                    &mut copy,
                );
                chunk.clear();
                out.push(Line::from(Span::styled(
                    format!("  ⋮ {n} lines hidden"),
                    Style::default().fg(theme.text.muted),
                )));
                copy.push(PlainRow {
                    skip: true,
                    ..Default::default()
                });
            }
        }
    }
    paint_chunk(
        &chunk,
        width,
        theme,
        language.as_deref(),
        &mut out,
        &mut copy,
    );
    (out, copy)
}

fn paint_chunk(
    rows: &[DiffRow],
    width: usize,
    theme: &Theme,
    language: Option<&str>,
    out: &mut Vec<Line<'static>>,
    copy: &mut Vec<PlainRow>,
) {
    if rows.is_empty() {
        return;
    }
    let painted = paint_diff_rows(
        rows,
        theme,
        width,
        &DiffPaintOptions {
            first_lead: "  ",
            rest_lead: "  ",
            line_numbers: true,
            fill_width: true,
            hunk_headers: true,
            patch_copy: true,
            language,
        },
        &|unchanged| match unchanged {
            Some(gap) => format!("⋯ {gap} unchanged lines"),
            None => "⋯".to_string(),
        },
    );
    for row in painted {
        out.push(row.line);
        copy.push(row.plain.unwrap_or(PlainRow {
            skip: true,
            ..Default::default()
        }));
    }
}

enum Folded {
    Row(DiffRow),
    Omitted(usize),
}

fn fold_diff_rows(rows: &[DiffRow]) -> Vec<Folded> {
    // Prefer keeping change lines; drop pure context from the long middle.
    let meaningful: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| {
            matches!(
                row,
                DiffRow::Code {
                    kind: DiffCodeKind::Add | DiffCodeKind::Remove,
                    ..
                } | DiffRow::Hunk { .. }
            )
        })
        .map(|(i, _)| i)
        .collect();

    if rows.len() <= DIFF_MAX_BODY {
        return rows.iter().cloned().map(Folded::Row).collect();
    }

    // Window around first change cluster with limited context.
    let focus = meaningful.first().copied().unwrap_or(0);
    let start = focus.saturating_sub(2);
    let mut end = (focus + DIFF_MAX_BODY).min(rows.len());
    // Extend to include nearby changes.
    for &i in &meaningful {
        if i < start + DIFF_MAX_BODY {
            end = end.max((i + 3).min(rows.len()));
        }
    }
    end = end.min(start + DIFF_MAX_BODY).max(start + 1);

    let mut out = Vec::new();
    if start > 0 {
        // Keep a tiny head if it has hunk header.
        if matches!(rows[0], DiffRow::Hunk { .. }) && start > 0 {
            out.push(Folded::Row(rows[0].clone()));
            if start > 1 {
                out.push(Folded::Omitted(start - 1));
            }
        } else {
            out.push(Folded::Omitted(start));
        }
    }
    for row in &rows[start..end] {
        out.push(Folded::Row(row.clone()));
    }
    if end < rows.len() {
        // Prefer showing last few change lines.
        let tail_start = rows.len().saturating_sub(DIFF_TAIL);
        if tail_start > end {
            out.push(Folded::Omitted(tail_start - end));
            for row in &rows[tail_start..] {
                out.push(Folded::Row(row.clone()));
            }
        } else if end < rows.len() {
            out.push(Folded::Omitted(rows.len() - end));
        }
    }
    // Hard cap rendered line count.
    let mut shown = 0usize;
    out.retain(|part| match part {
        Folded::Row(_) => {
            shown += 1;
            shown <= DIFF_HEAD + DIFF_TAIL + 4
        }
        Folded::Omitted(_) => true,
    });
    out
}

/// Parse and render a Markdown `diff` fence body, with the semantic copy
/// policy for each painted row.
pub(crate) fn render_diff_fence_with_copy(
    src: &str,
    title_hint: Option<&str>,
    width: usize,
    theme: &Theme,
) -> (Vec<Line<'static>>, Vec<PlainRow>) {
    let view = DiffView::parse(src, title_hint);
    render_diff_view_with_copy(&view, width, theme)
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

    fn text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn painted(patch: &str, width: usize, theme: &Theme) -> Vec<PaintedDiffRow> {
        let rows = parse_diff_rows(patch);
        paint_diff_rows(
            &rows,
            theme,
            width,
            &DiffPaintOptions {
                first_lead: "  └ ",
                rest_lead: "    ",
                line_numbers: true,
                fill_width: true,
                hunk_headers: false,
                patch_copy: false,
                language: None,
            },
            &|gap| gap.map_or_else(|| "⋯".to_string(), |n| format!("⋯ {n} unchanged lines")),
        )
    }

    fn span_with<'a>(
        row: &'a PaintedDiffRow,
        needle: &str,
    ) -> Option<&'a ratatui::text::Span<'static>> {
        row.line
            .spans
            .iter()
            .find(|span| span.content.contains(needle))
    }

    #[test]
    fn parse_extracts_file_stats_and_lines() {
        let view = DiffView::parse(SAMPLE, None);
        assert_eq!(view.file, "auth.go");
        assert_eq!(view.start_line, Some(120));
        assert_eq!(view.added, 2);
        assert_eq!(view.removed, 1);
        assert!(view.rows.iter().any(|row| matches!(
            row,
            DiffRow::Code {
                kind: DiffCodeKind::Add,
                ..
            }
        )));
        assert!(view.rows.iter().any(|row| matches!(
            row,
            DiffRow::Code {
                kind: DiffCodeKind::Remove,
                ..
            }
        )));
    }

    #[test]
    fn render_has_header_and_no_code_bg_box() {
        let theme = Theme::no_color();
        let (out, _) = render_diff_fence_with_copy(SAMPLE, Some("auth.go:120"), 60, &theme);
        let lines: Vec<String> = out.iter().map(text).collect();
        assert!(
            lines.iter().any(|line| line.contains("auth.go")),
            "header missing: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("oldToken") || line.contains('-')),
            "delete line missing: {lines:?}"
        );
        assert!(
            !lines
                .iter()
                .any(|line| line.starts_with('┌') || line.starts_with('└')),
            "diff must not use CodeBlock box: {lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line.starts_with("┃")),
            "diff must not use heavy code gutter: {lines:?}"
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
        let view = DiffView::parse(&body, None);
        let (out, _) = render_diff_view_with_copy(&view, 60, &Theme::no_color());
        let lines: Vec<String> = out.iter().map(text).collect();
        assert!(
            lines
                .iter()
                .any(|line| line.contains("hidden") || line.contains("⋮")),
            "long diff should fold: {lines:?}"
        );
        assert!(lines.len() < 30, "must not dump full diff: {}", lines.len());
    }

    #[test]
    fn added_and_removed_rows_paint_a_full_width_band() {
        let theme = Theme::dark();
        let width = 60;
        let rows = painted(SAMPLE, width, &theme);
        let add = rows
            .iter()
            .find(|row| text(&row.line).contains("validateToken"))
            .expect("added row");
        for span in &add.line.spans {
            assert_eq!(
                span.style.bg,
                Some(theme.diff.added_bg),
                "every add-row cell carries the band: {:?}",
                add.line.spans
            );
        }
        assert_eq!(crate::markdown::disp_width(&text(&add.line)), width);

        let remove = rows
            .iter()
            .find(|row| text(&row.line).contains("oldToken"))
            .expect("removed row");
        for span in &remove.line.spans {
            assert_eq!(span.style.bg, Some(theme.diff.removed_bg));
        }

        let context = rows
            .iter()
            .find(|row| text(&row.line).contains("func login"))
            .expect("context row");
        assert!(
            context
                .line
                .spans
                .iter()
                .all(|span| span.style.bg.is_none()),
            "context stays on the canvas: {:?}",
            context.line.spans
        );
    }

    #[test]
    fn inline_changed_token_gets_the_stronger_band() {
        let theme = Theme::dark();
        let patch = "\
--- a/src/total.ts
+++ b/src/total.ts
@@ -1,3 +1,3 @@
 export function total(price: number, quantity: number): number {
-  return price * quantity;
+  return price * count;
 }
";
        let rows = painted(patch, 80, &theme);
        let old_line = rows
            .iter()
            .find(|row| text(&row.line).contains("return price * quantity;"))
            .expect("removed line");
        let changed = span_with(old_line, "quantity").expect("inline span");
        assert_eq!(changed.style.bg, Some(theme.diff.removed_inline_bg));
        assert_eq!(changed.style.fg, Some(theme.syntax.variable));
        let unchanged = span_with(old_line, "price").expect("unchanged span");
        assert_eq!(unchanged.style.bg, Some(theme.diff.removed_bg));

        let new_line = rows
            .iter()
            .find(|row| text(&row.line).contains("return price * count;"))
            .expect("added line");
        let changed = span_with(new_line, "count").expect("inline span");
        assert_eq!(changed.style.bg, Some(theme.diff.added_inline_bg));
        assert_eq!(
            span_with(new_line, "price")
                .expect("unchanged span")
                .style
                .bg,
            Some(theme.diff.added_bg)
        );
    }

    #[test]
    fn banded_rows_paint_code_in_readable_ink_not_direction_green() {
        let theme = Theme::dark();
        let patch = "\
--- /dev/null
+++ b/src/money.ts
@@ -0,0 +1,2 @@
+export interface Money {
+  amount: number;
+}
";
        let rows = painted(patch, 70, &theme);
        let add = rows
            .iter()
            .find(|row| text(&row.line).contains("export interface"))
            .expect("added row");
        let body = span_with(add, "export").expect("code span");
        assert_eq!(
            body.style.fg,
            Some(theme.syntax.keyword),
            "banded code keeps its syntax ink: {:?}",
            body.style
        );
        assert_eq!(body.style.bg, Some(theme.diff.added_bg));
        let marker = span_with(add, "+ ").expect("direction marker");
        assert_eq!(marker.style.fg, Some(theme.diff.added));
        assert_eq!(marker.style.bg, Some(theme.diff.added_bg));

        let remove_theme_patch = "\
--- a/src/money.ts
+++ b/src/money.ts
@@ -1,1 +1,1 @@
-export const amount = oldValue;
+export const amount = newValue;
";
        let removed = painted(remove_theme_patch, 70, &theme);
        let old = removed
            .iter()
            .find(|row| text(&row.line).contains("oldValue"))
            .expect("removed row");
        let body = span_with(old, "export").expect("code span");
        assert_eq!(body.style.fg, Some(theme.syntax.keyword));
        assert_eq!(body.style.bg, Some(theme.diff.removed_bg));
        assert_eq!(
            span_with(old, "- ").expect("marker").style.fg,
            Some(theme.diff.removed)
        );
    }

    #[test]
    fn banded_rows_keep_the_comment_and_keyword_syntax_inks() {
        let theme = Theme::dark();
        let patch = "\
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,2 +1,4 @@
 fn main() {
-    old();
+    // explain the change
+    let value = 1;
 }
";
        let rows = painted(patch, 80, &theme);
        let add = rows
            .iter()
            .find(|row| text(&row.line).contains("explain the change"))
            .expect("added comment row");
        let comment = span_with(add, "// explain").expect("comment span");
        assert_eq!(
            comment.style.fg,
            Some(theme.syntax.comment),
            "comments keep their quiet syntax ink: {:?}",
            comment.style
        );
        let code = rows
            .iter()
            .find(|row| text(&row.line).contains("let value"))
            .expect("added code row");
        let keyword = span_with(code, "let").expect("keyword span");
        assert_eq!(
            keyword.style.fg,
            Some(theme.syntax.keyword),
            "keywords never collapse into the direction colour: {:?}",
            keyword.style
        );
    }

    /// Code-row inks, excluding the lead, gutter and direction marker. Those
    /// carry diff semantics; only the code body carries syntax.
    fn code_inks(row: &PaintedDiffRow) -> std::collections::HashSet<Color> {
        row.line
            .spans
            .iter()
            .filter(|span| !span.content.contains('│'))
            .filter(|span| span.content.chars().any(|c| c.is_alphanumeric()))
            .filter_map(|span| span.style.fg)
            .collect()
    }

    #[test]
    fn a_go_diff_shows_distinct_syntax_inks() {
        let theme = Theme::dark();
        let patch = "\
--- a/internal/cookie/cookie.go
+++ b/internal/cookie/cookie.go
@@ -1,0 +1,6 @@
+func TestTotal(t *testing.T) {
+    // sum the values
+    total := \"cookie\" + 42
+    return total
+}
+";
        let rows = painted(patch, 90, &theme);
        let mut inks = std::collections::HashSet::new();
        for row in &rows {
            if row
                .line
                .spans
                .iter()
                .any(|span| span.style.bg == Some(theme.diff.added_bg))
            {
                inks.extend(code_inks(row));
            }
        }
        assert!(
            inks.len() >= 5,
            "Go tokens collapse into too few inks ({inks:?})"
        );
    }

    #[test]
    fn a_typescript_diff_shows_distinct_syntax_inks() {
        let theme = Theme::dark();
        let patch = "\
--- a/src/ledger.ts
+++ b/src/ledger.ts
@@ -1,0 +1,4 @@
+export interface Entry {
+  readonly amount: number;
+  label: \"cookie\";
+}
+";
        let rows = painted(patch, 90, &theme);
        let mut inks = std::collections::HashSet::new();
        for row in &rows {
            if row
                .line
                .spans
                .iter()
                .any(|span| span.style.bg == Some(theme.diff.added_bg))
            {
                inks.extend(code_inks(row));
            }
        }
        assert!(
            inks.len() >= 3,
            "TypeScript tokens collapse into too few inks ({inks:?})"
        );
    }

    #[test]
    fn a_multiline_comment_keeps_its_ink_on_every_row() {
        let theme = Theme::dark();
        let patch = "\
--- a/internal/ledger/ledger.go
+++ b/internal/ledger/ledger.go
@@ -1,0 +1,4 @@
+/*
+ * Postings stay balanced.
+ */
+func total() int { return 42 }
+";
        let rows = painted(patch, 90, &theme);
        let middle = rows
            .iter()
            .find(|row| text(&row.line).contains("Postings stay balanced"))
            .expect("comment body row");
        assert_eq!(
            span_with(middle, "Postings")
                .expect("comment span")
                .style
                .fg,
            Some(theme.syntax.comment),
            "the grammar state must carry across diff rows: {:?}",
            middle.line.spans
        );
        let end = rows
            .iter()
            .find(|row| text(&row.line).contains("func total"))
            .expect("code row");
        assert_eq!(
            span_with(end, "func").expect("keyword span").style.fg,
            Some(theme.syntax.keyword),
            "a closed comment must not swallow the next line: {:?}",
            end.line.spans
        );
    }

    #[test]
    fn removed_and_added_streams_keep_independent_syntax_state() {
        let theme = Theme::dark();
        let patch = "\
--- a/internal/ledger/ledger.go
+++ b/internal/ledger/ledger.go
@@ -1,3 +1,2 @@
 func total() int {
-/* open comment
- * the comment continues here
+func helper() int { return 1 }
";
        let rows = painted(patch, 90, &theme);
        let removed = rows
            .iter()
            .find(|row| text(&row.line).contains("open comment"))
            .expect("removed row");
        assert_eq!(
            span_with(removed, "open comment")
                .expect("comment span")
                .style
                .fg,
            Some(theme.syntax.comment)
        );
        let added = rows
            .iter()
            .find(|row| text(&row.line).contains("func helper"))
            .expect("added row");
        assert_eq!(
            span_with(added, "func").expect("keyword span").style.fg,
            Some(theme.syntax.keyword),
            "the old side's open comment must not leak into the new side: {:?}",
            added.line.spans
        );
    }

    #[test]
    fn a_diff_with_an_unknown_language_still_has_code_inks() {
        let theme = Theme::dark();
        let patch = "\
--- a/scripts/deploy.xyz
+++ b/scripts/deploy.xyz
@@ -1,0 +1,3 @@
+if [ -f \"$HOME/token\" ]; then
+  echo 'ready'  # probe
+fi
+";
        let rows = painted(patch, 90, &theme);
        let inks: std::collections::HashSet<_> = rows.iter().flat_map(code_inks).collect();
        assert!(
            inks.len() >= 3,
            "an unknown code language must fall back to Bash inks ({inks:?})"
        );
    }

    #[test]
    fn inline_emphasis_keeps_the_token_ink() {
        let theme = Theme::dark();
        let patch = "\
--- a/src/label.ts
+++ b/src/label.ts
@@ -1,1 +1,1 @@
-const label = \"draft\";
+const label = \"stable\";
";
        let rows = painted(patch, 80, &theme);
        let add = rows
            .iter()
            .find(|row| text(&row.line).contains("stable"))
            .expect("added row");
        let changed = span_with(add, "\"stable\"")
            .or_else(|| span_with(add, "stable"))
            .expect("inline span");
        let keyword = span_with(add, "const").expect("keyword span");
        assert_eq!(changed.style.bg, Some(theme.diff.added_inline_bg));
        assert_ne!(
            changed.style.fg, keyword.style.fg,
            "inline emphasis erased the token colour: {:?}",
            add.line.spans
        );
    }

    #[test]
    fn ansi256_code_body_carries_a_readable_row_band() {
        let theme = crate::theme::indexed_256_theme(Theme::dark());
        assert_eq!(theme.color_depth(), ColorDepth::Ansi256);
        let patch = "\
--- a/src/total.ts
+++ b/src/total.ts
@@ -1,2 +1,2 @@
 export function total(price: number, quantity: number): number {
-  return price * quantity;
+  return price * count;
 }
";
        let rows = painted(patch, 80, &theme);
        let add = rows
            .iter()
            .find(|row| text(&row.line).contains("price * count"))
            .expect("added row");
        let body = span_with(add, "price * ").expect("unchanged span");
        let bg = body
            .style
            .bg
            .unwrap_or_else(|| panic!("256-colour code body has no row band: {body:?}"));
        let fg = body.style.fg.expect("body ink");
        let ratio = crate::theme::contrast_ratio(fg, bg)
            .unwrap_or_else(|| panic!("contrast is undefined for {fg:?} on {bg:?}"));
        assert!(
            ratio >= 4.5,
            "256-colour code body contrast {ratio:.2} is below 4.5:1"
        );
    }

    #[test]
    fn ansi256_paints_a_neutral_full_row_band_and_keeps_syntax_ink() {
        let theme = crate::theme::indexed_256_theme(Theme::dark());
        assert_eq!(theme.color_depth(), ColorDepth::Ansi256);
        let patch = "\
--- a/src/total.ts
+++ b/src/total.ts
@@ -1,2 +1,2 @@
 export function total(price: number, quantity: number): number {
-  return price * quantity;
+  return price * count;
 }
";
        let rows = painted(patch, 80, &theme);
        let add = rows
            .iter()
            .find(|row| text(&row.line).contains("price * count"))
            .expect("added row");
        let marker = span_with(add, "+ ").expect("marker");
        assert_eq!(marker.style.bg, Some(theme.diff.added_bg));
        assert_eq!(
            marker.style.fg,
            Some(theme.diff.added),
            "256 colour names the direction on the marker"
        );
        let body = span_with(add, "price").expect("unchanged span");
        assert_eq!(
            body.style.bg,
            Some(theme.diff.added_bg),
            "the neutral row band covers the code body: {:?}",
            body.style
        );
        let changed = span_with(add, "count").expect("inline span");
        assert_eq!(changed.style.bg, Some(theme.diff.added_inline_bg));
        let fg = changed.style.fg.expect("inline ink");
        assert!(
            crate::theme::contrast_ratio(fg, theme.diff.added_inline_bg).unwrap() >= 4.5,
            "inline ink must stay readable on the stronger band: {fg:?}"
        );

        let remove = rows
            .iter()
            .find(|row| text(&row.line).contains("price * quantity"))
            .expect("removed row");
        let remove_marker = span_with(remove, "- ").expect("marker");
        assert_eq!(remove_marker.style.bg, Some(theme.diff.removed_bg));
        assert_eq!(remove_marker.style.fg, Some(theme.diff.removed));
        assert_eq!(
            span_with(remove, "price").expect("unchanged span").style.bg,
            Some(theme.diff.removed_bg)
        );

        let context = rows
            .iter()
            .find(|row| text(&row.line).contains("export function"))
            .expect("context row");
        assert!(
            context
                .line
                .spans
                .iter()
                .all(|span| span.style.bg.is_none()),
            "context rows paint no band: {:?}",
            context.line.spans
        );
    }

    #[test]
    fn inline_diff_handles_cjk_and_emoji_by_grapheme() {
        let patch = "\
--- a/zh.txt
+++ b/zh.txt
@@ -1,1 +1,1 @@
-你好世界 🔴 状态
+你好地球 🔵 状态
";
        let rows = painted(patch, 80, &Theme::dark());
        let old_line = rows
            .iter()
            .find(|row| text(&row.line).contains("你好世界"))
            .expect("removed line");
        for needle in ["世界", "🔴"] {
            assert_eq!(
                span_with(old_line, needle)
                    .unwrap_or_else(|| panic!("{needle} span"))
                    .style
                    .bg,
                Some(Theme::dark().diff.removed_inline_bg),
                "{needle} is inline-changed"
            );
        }
        assert_eq!(
            span_with(old_line, "你好").expect("prefix span").style.bg,
            Some(Theme::dark().diff.removed_bg),
            "the common prefix keeps the row band"
        );
    }

    fn span_bounds(ranges: &[std::ops::Range<usize>]) -> Vec<(usize, usize)> {
        ranges
            .iter()
            .map(|range| (range.start, range.end))
            .collect()
    }

    #[test]
    fn inline_span_small_change_keeps_only_the_changed_token() {
        let (old_ranges, new_ranges) = changed_ranges("aa X bb", "aa Y bb");
        assert_eq!(span_bounds(&old_ranges), [(3, 4)]);
        assert_eq!(span_bounds(&new_ranges), [(3, 4)]);
    }

    #[test]
    fn inline_span_below_the_token_cap_leaves_a_shared_middle_token_unhighlighted() {
        let old = format!("{}A{}", ".".repeat(200), ".".repeat(200));
        let new = format!("{}A{}", ",".repeat(200), ",".repeat(200));
        let (old_ranges, new_ranges) = changed_ranges(&old, &new);
        assert_eq!(span_bounds(&old_ranges), [(0, 200), (201, 401)]);
        assert_eq!(span_bounds(&new_ranges), [(0, 200), (201, 401)]);
    }

    #[test]
    fn inline_span_coarse_fallback_is_one_span_over_the_whole_changed_run() {
        let old = format!("{}A{}", ".".repeat(300), ".".repeat(300));
        let new = format!("{}A{}", ",".repeat(300), ",".repeat(300));
        let (old_ranges, new_ranges) = changed_ranges(&old, &new);
        assert_eq!(span_bounds(&old_ranges), [(0, old.len())]);
        assert_eq!(span_bounds(&new_ranges), [(0, new.len())]);
        assert_eq!(old.len(), 601);
    }

    #[test]
    fn sixteen_colour_and_monochrome_degrade_to_underline() {
        for theme in [
            crate::theme::Theme::no_color(),
            crate::theme::ansi16_theme(Theme::dark()),
        ] {
            let patch = "\
--- a/a.rs
+++ b/a.rs
@@ -1,1 +1,1 @@
-old_name();
+new_name();
";
            let rows = painted(patch, 60, &theme);
            for row in &rows {
                for span in &row.line.spans {
                    assert!(
                        span.style.bg.is_none(),
                        "{:?} must not paint a background",
                        theme.color_depth()
                    );
                }
            }
            let add = rows
                .iter()
                .find(|row| text(&row.line).contains("new_name"))
                .expect("added row");
            let changed = span_with(add, "new_name").expect("inline span");
            assert!(
                changed.style.add_modifier.contains(Modifier::UNDERLINED),
                "inline emphasis becomes underline: {:?}",
                changed.style
            );
        }
    }

    #[test]
    fn control_characters_and_colour_sequences_never_reach_the_paint() {
        let patch = "--- a/a.rs\n+++ b/a.rs\n@@ -1,1 +1,1 @@\n-old\n+\u{1b}[31mred\u{7}\n";
        let rows = painted(patch, 60, &Theme::dark());
        for row in &rows {
            let line = text(&row.line);
            assert!(
                !line.chars().any(char::is_control),
                "no control byte survives: {line:?}"
            );
        }
    }

    #[test]
    fn looks_like_diff_rejects_ordinary_logs() {
        assert!(!looks_like_diff("test result: ok. 3 passed"));
        assert!(!looks_like_diff("+1 passed; -1 failed"));
        assert!(!looks_like_diff("error: build failed\n  + note: see log"));
        assert!(!looks_like_diff("@@ not a real patch @@\njust prose"));
        assert!(looks_like_diff("diff --git a/x b/x\n@@ -1 +1 @@\n-a\n+b"));
        assert!(looks_like_diff(
            "*** Begin Patch\n*** Update File: a.rs\n@@\n-x\n+y\n"
        ));
        assert!(looks_like_diff("--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b"));
    }

    #[test]
    fn wrapped_rows_keep_the_band_and_trim_padding_from_copy() {
        let theme = Theme::dark();
        let long = "let value = some_function_with_a_long_name(argument_one, argument_two, argument_three);";
        let patch = format!("--- a/a.rs\n+++ b/a.rs\n@@ -1,1 +1,1 @@\n+{long}\n");
        let rows = painted(&patch, 30, &theme);
        let code: Vec<&PaintedDiffRow> = rows.iter().filter(|row| row.plain.is_some()).collect();
        assert!(code.len() > 1, "the line wraps: {:?}", code.len());
        for row in &code {
            assert!(
                crate::markdown::disp_width(&text(&row.line)) <= 30,
                "row overflows: {:?}",
                text(&row.line)
            );
            assert!(
                row.line
                    .spans
                    .iter()
                    .all(|span| span.style.bg == Some(theme.diff.added_bg))
            );
        }
        // The final row is padded, and the padding is declared decoration.
        let last = code.last().unwrap();
        assert!(last.plain.as_ref().unwrap().suffix > 0);
        assert!(code[1].plain.as_ref().unwrap().joiner.is_some());
    }

    #[test]
    fn line_numbers_stay_per_hunk_and_per_side() {
        let patch = "\
--- a/src/a.rs
+++ b/src/a.rs
@@ -10,2 +10,3 @@
 keep
-old
+new
+extra
@@ -100,2 +101,1 @@
 keep
-gone
";
        let numbers: Vec<Option<u32>> = parse_diff_rows(patch)
            .iter()
            .filter(|row| matches!(row, DiffRow::Code { .. }))
            .map(DiffRow::gutter_line)
            .collect();
        assert_eq!(
            numbers,
            vec![Some(10), Some(11), Some(11), Some(12), Some(101), Some(101)]
        );
    }

    #[test]
    fn deletion_only_hunk_still_counts_the_gap() {
        let patch = "\
+++ b/src/a.rs
@@ -10,3 +10,0 @@
-gone one
-gone two
-gone three
@@ -30,1 +20,1 @@
-old
+new
";
        let rows = painted(patch, 100, &Theme::no_color());
        let joined: String = rows
            .iter()
            .map(|row| text(&row.line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("10 unchanged lines"),
            "the removed block occupies old lines 10-12; the new side continues at 10: {joined}"
        );
    }
}
