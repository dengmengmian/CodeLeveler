//! A windowed projection of a complete user-shell capture.
//!
//! The conversation keeps only a bounded tail of a finished shell, and it must
//! never materialize a multi-megabyte result just because the reader asked to
//! see it. This module owns the other half of that contract:
//!
//! * one index per capture — logical line byte ranges plus wrapped visual-row
//!   counts at a given width;
//! * a window renderer that projects only the lines a viewport asks for;
//! * a selection projection that returns the authored text (tabs and
//!   indentation intact, soft wraps rejoined).
//!
//! The index is proportional to the number of LOGICAL lines, never to the
//! rendered rows. A 50 MiB stream of 45-byte lines costs roughly 12 bytes per
//! line; the rendered text exists only for the visible window.
//!
//! The capture is a trust boundary: it is read back on resume from a persisted
//! path. [`Capture::open`] validates that the directory is a real directory
//! under the CodeLeveler state root (when one is known) and that
//! `combined.log` is a regular file, never a symlink.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::selection::TextSelection;

/// Logical lines per prefix entry used to map a visual row to a line.
const INDEX_BLOCK_LINES: usize = 1024;
/// Terminal tab stop used by the display projection.
const TAB_STOP: usize = 8;
/// Bytes of the capture rendered provisionally while the index is built.
pub const PROVISIONAL_TAIL_BYTES: usize = 64 * 1024;

/// Where a complete output lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputSource {
    /// The runtime's exact capture (`combined.log`).
    File { path: PathBuf, bytes: u64 },
    /// A complete retained result that never had a capture. The text is the
    /// runtime's own projection, not a second copy of the capture.
    Memory(Arc<str>),
}

impl OutputSource {
    pub fn len(&self) -> u64 {
        match self {
            Self::File { bytes, .. } => *bytes,
            Self::Memory(text) => text.len() as u64,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The plain-text projection of the whole source, for "copy all".
    pub fn copy_all(&self) -> Result<String, String> {
        match self {
            Self::File { path, .. } => {
                let bytes = std::fs::read(path)
                    .map_err(|_| "完整输出不可用（捕获文件无法读取）".to_string())?;
                Ok(crate::shell_render::copy_text(&String::from_utf8_lossy(
                    &bytes,
                )))
            }
            Self::Memory(text) => Ok(crate::shell_render::copy_text(text)),
        }
    }
}

/// A validated complete capture directory.
#[derive(Debug, Clone)]
pub struct Capture {
    pub directory: PathBuf,
    pub combined: PathBuf,
    pub bytes: u64,
}

impl Capture {
    /// Validate and open the capture directory the runtime advertised.
    ///
    /// `allowed_root` is the local `shell-output` root when the UI knows it.
    /// A directory outside that root is refused; without a root the structural
    /// checks still apply (real directory, regular non-symlink combined file,
    /// every stream sidecar reports a finished copy).
    pub fn open(directory: &Path, allowed_root: Option<&Path>) -> Result<Self, String> {
        let unavailable = || "完整输出不可用（捕获目录无效）".to_string();
        let meta = std::fs::symlink_metadata(directory).map_err(|_| unavailable())?;
        if !meta.file_type().is_dir() || meta.file_type().is_symlink() {
            return Err(unavailable());
        }
        let canonical = std::fs::canonicalize(directory).map_err(|_| unavailable())?;
        if let Some(root) = allowed_root {
            let root = std::fs::canonicalize(root)
                .map_err(|_| "完整输出不可用（状态目录无效）".to_string())?;
            if !canonical.starts_with(&root) {
                return Err("完整输出不可用（捕获不在本机状态目录内）".to_string());
            }
        }
        for stream in ["stdout", "stderr", "combined"] {
            let sidecar = canonical.join(format!("{stream}.meta.json"));
            let meta = std::fs::symlink_metadata(&sidecar).map_err(|_| unavailable())?;
            if !meta.file_type().is_file() || meta.file_type().is_symlink() {
                return Err(unavailable());
            }
            let text = std::fs::read_to_string(&sidecar).map_err(|_| unavailable())?;
            if !text.contains("\"complete\":true") {
                return Err("完整输出不可用（捕获尚未完成）".to_string());
            }
        }
        let combined = canonical.join("combined.log");
        let meta = std::fs::symlink_metadata(&combined).map_err(|_| unavailable())?;
        if !meta.file_type().is_file() || meta.file_type().is_symlink() {
            return Err(unavailable());
        }
        let bytes = meta.len();
        Ok(Self {
            directory: canonical,
            combined,
            bytes,
        })
    }

    pub fn source(&self) -> OutputSource {
        OutputSource::File {
            path: self.combined.clone(),
            bytes: self.bytes,
        }
    }
}

/// Immutable row index over one output source at one display width.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputIndex {
    pub width: usize,
    pub total_rows: usize,
    pub line_count: usize,
    pub bytes: u64,
    line_starts: Vec<u64>,
    line_ends: Vec<u64>,
    line_rows: Vec<u32>,
    /// `block_rows[b]` is the number of visual rows before line
    /// `b * INDEX_BLOCK_LINES`.
    block_rows: Vec<u64>,
}

impl OutputIndex {
    /// Index a source. This is the only operation that touches every line;
    /// callers run it off the paint loop.
    pub fn build(source: &OutputSource, width: usize) -> Result<Self, String> {
        let width = width.max(1);
        let bytes = source.len();
        let mut line_starts = Vec::new();
        let mut line_ends = Vec::new();
        let mut line_rows = Vec::new();
        let mut block_rows = Vec::new();
        let mut total: u64 = 0;
        scan_lines(source, |offset, length, raw| {
            if line_starts.len() % INDEX_BLOCK_LINES == 0 {
                block_rows.push(total);
            }
            line_starts.push(offset);
            line_ends.push(offset.saturating_add(length));
            let rows = u32::try_from(row_count(raw, width)).unwrap_or(u32::MAX);
            line_rows.push(rows);
            total = total.saturating_add(u64::from(rows));
        })?;
        Ok(Self {
            width,
            total_rows: usize::try_from(total).unwrap_or(usize::MAX),
            line_count: line_starts.len(),
            bytes,
            line_starts,
            line_ends,
            line_rows,
            block_rows,
        })
    }

    /// Map an absolute visual row to `(logical line, row within that line)`.
    pub fn locate(&self, row: usize) -> Option<(usize, usize)> {
        if row >= self.total_rows || self.line_count == 0 {
            return None;
        }
        let block = match self
            .block_rows
            .binary_search_by(|value| (*value as usize).cmp(&row))
        {
            Ok(index) => index,
            Err(0) => 0,
            Err(index) => index - 1,
        };
        let mut line = block.saturating_mul(INDEX_BLOCK_LINES);
        let mut base = self.block_rows.get(block).copied().unwrap_or(0) as usize;
        while line < self.line_count {
            let rows = self.line_rows[line] as usize;
            if base + rows > row {
                return Some((line, row - base));
            }
            base += rows;
            line += 1;
        }
        None
    }

    /// The absolute first visual row of a logical line.
    pub fn line_start_row(&self, line: usize) -> usize {
        if line >= self.line_count {
            return self.total_rows;
        }
        let block = line / INDEX_BLOCK_LINES;
        let mut base = self.block_rows.get(block).copied().unwrap_or(0) as usize;
        for index in block * INDEX_BLOCK_LINES..line {
            base += self.line_rows[index] as usize;
        }
        base
    }

    /// Render `count` visual rows starting at `first_row`.
    pub fn rows(
        &self,
        source: &OutputSource,
        first_row: usize,
        count: usize,
    ) -> Result<Vec<RenderedRow>, String> {
        if count == 0 || first_row >= self.total_rows {
            return Ok(Vec::new());
        }
        let Some((mut line, mut skip)) = self.locate(first_row) else {
            return Ok(Vec::new());
        };
        let mut reader = SourceReader::open(source)?;
        let mut out = Vec::with_capacity(count.min(1024));
        while line < self.line_count && out.len() < count {
            let start = self.line_starts[line];
            let end = self.line_ends[line];
            let raw = reader.line(start, end)?;
            for row in project_line(&raw, self.width).into_iter().skip(skip) {
                if out.len() == count {
                    break;
                }
                out.push(RenderedRow {
                    line,
                    row_in_line: row.row_in_line,
                    display: row.display,
                    source: row.source,
                    columns: row.columns,
                    soft: row.soft,
                });
            }
            skip = 0;
            line += 1;
        }
        Ok(out)
    }

    /// Rows for a provisional tail while the index is still being built.
    pub fn tail_rows(
        source: &OutputSource,
        width: usize,
        max_bytes: usize,
    ) -> Result<Vec<RenderedRow>, String> {
        let width = width.max(1);
        let len = source.len();
        let start = len.saturating_sub(max_bytes as u64);
        let mut reader = SourceReader::open(source)?;
        let text = reader.line(start, len)?;
        let text = if start > 0 {
            match text.find('\n') {
                Some(index) => &text[index + 1..],
                None => "",
            }
        } else {
            text.as_str()
        };
        let mut out = Vec::new();
        let mut lines = text.split('\n').peekable();
        while let Some(raw) = lines.next() {
            if raw.is_empty() && lines.peek().is_none() {
                break;
            }
            for row in project_line(raw, width) {
                out.push(RenderedRow {
                    line: 0,
                    row_in_line: row.row_in_line,
                    display: row.display,
                    source: row.source,
                    columns: row.columns,
                    soft: row.soft,
                });
            }
        }
        Ok(out)
    }
}

/// One painted output row, plus the source mapping copy needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedRow {
    pub line: usize,
    pub row_in_line: usize,
    /// The terminal-visible text (tabs expanded, CR resolved, ANSI stripped).
    pub display: String,
    /// The authored text this row came from, tabs preserved.
    pub source: String,
    /// For every occupied display column, the byte range in `source` it came
    /// from. A tab maps every expanded cell to the same tab byte.
    pub columns: Vec<(usize, usize)>,
    /// True when this row continues the previous logical line.
    pub soft: bool,
}

impl RenderedRow {
    pub fn display_width(&self) -> usize {
        UnicodeWidthStr::width(self.display.as_str())
    }

    /// The authored slice under display columns `[from, to)`.
    pub fn source_slice(&self, from: usize, to: usize) -> String {
        if from >= to || self.columns.is_empty() {
            return String::new();
        }
        let last = self.columns.len() - 1;
        let from = from.min(last);
        let to = to.clamp(from + 1, self.columns.len());
        let start = self.columns[from].0;
        let end = self.columns[to - 1].1;
        self.source.get(start..end).unwrap_or_default().to_string()
    }
}

/// Extract the authored text for a selection over `rows`.
///
/// `rows` must start at `sel.range().0.row`. A soft-wrapped row rejoins with
/// nothing; a new logical line joins with `\n`.
pub fn selection_text(rows: &[RenderedRow], sel: &TextSelection) -> String {
    let Some((start, end)) = sel.range() else {
        return String::new();
    };
    let mut out = String::new();
    let mut wrote = false;
    for (offset, row) in rows.iter().enumerate() {
        let abs = start.row + offset;
        if abs > end.row {
            break;
        }
        let width = row.display_width();
        let from = if abs == start.row {
            start.col.min(width)
        } else {
            0
        };
        let to = if abs == end.row {
            end.col.min(width)
        } else {
            width
        };
        if wrote && !row.soft {
            out.push('\n');
        }
        if to > from {
            out.push_str(&row.source_slice(from, to));
        }
        wrote = true;
    }
    out
}

/// The selection's rows, starting at its first row.
pub fn selection_rows(
    index: &OutputIndex,
    source: &OutputSource,
    sel: &TextSelection,
) -> Result<Vec<RenderedRow>, String> {
    let Some((start, end)) = sel.range() else {
        return Ok(Vec::new());
    };
    let count = end.row.saturating_sub(start.row).saturating_add(1);
    index.rows(source, start.row, count)
}

// ── projection ──────────────────────────────────────────────────────────────

fn char_width(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(0)
}

/// Read every logical line from the source, with its byte offset and the
/// number of content bytes (the terminating newline excluded).
fn scan_lines<F>(source: &OutputSource, mut visit: F) -> Result<(), String>
where
    F: FnMut(u64, u64, &str),
{
    match source {
        OutputSource::File { path, .. } => {
            use std::io::{BufRead, BufReader};
            let file = std::fs::File::open(path)
                .map_err(|_| "完整输出不可用（捕获文件无法打开）".to_string())?;
            let mut reader = BufReader::with_capacity(256 * 1024, file);
            let mut offset = 0u64;
            let mut buffer = Vec::new();
            loop {
                buffer.clear();
                match reader.read_until(b'\n', &mut buffer) {
                    Ok(0) => break,
                    Ok(_) => {
                        let had_newline = buffer.last() == Some(&b'\n');
                        if had_newline {
                            buffer.pop();
                        }
                        visit(
                            offset,
                            buffer.len() as u64,
                            &String::from_utf8_lossy(&buffer),
                        );
                        offset = offset
                            .saturating_add(buffer.len() as u64)
                            .saturating_add(u64::from(had_newline));
                        if !had_newline {
                            break;
                        }
                    }
                    Err(_) => return Err("完整输出不可读".to_string()),
                }
            }
            Ok(())
        }
        OutputSource::Memory(text) => {
            let mut offset = 0usize;
            while offset < text.len() {
                let rest = &text[offset..];
                let (line, advance) = match rest.find('\n') {
                    Some(index) => (&rest[..index], index + 1),
                    None => (rest, rest.len()),
                };
                visit(offset as u64, line.len() as u64, line);
                offset += advance;
            }
            Ok(())
        }
    }
}

/// One logical line read from a source at a byte range.
struct SourceReader {
    file: Option<std::fs::File>,
    memory: Option<Arc<str>>,
}

impl SourceReader {
    fn open(source: &OutputSource) -> Result<Self, String> {
        match source {
            OutputSource::File { path, .. } => {
                let file = std::fs::File::open(path)
                    .map_err(|_| "完整输出不可用（捕获文件无法打开）".to_string())?;
                Ok(Self {
                    file: Some(file),
                    memory: None,
                })
            }
            OutputSource::Memory(text) => Ok(Self {
                file: None,
                memory: Some(Arc::clone(text)),
            }),
        }
    }

    fn line(&mut self, start: u64, end: u64) -> Result<String, String> {
        if end < start {
            return Err("输出索引损坏".to_string());
        }
        let len = (end - start) as usize;
        if let Some(file) = self.file.as_mut() {
            use std::io::{Read, Seek, SeekFrom};
            file.seek(SeekFrom::Start(start))
                .map_err(|_| "完整输出不可读".to_string())?;
            let mut bytes = vec![0u8; len];
            file.read_exact(&mut bytes)
                .map_err(|_| "完整输出不可读".to_string())?;
            return Ok(String::from_utf8_lossy(&bytes).into_owned());
        }
        let text = self.memory.as_deref().unwrap_or_default();
        text.get(start as usize..end as usize)
            .map(str::to_string)
            .ok_or_else(|| "输出索引损坏".to_string())
    }
}

/// Project one raw logical line to its painted rows.
///
/// The order matches the inline renderer exactly: strip terminal escapes
/// (preserving layout controls), resolve carriage-return overwrites, expand
/// tabs at absolute tab stops, then hard-wrap by display width.
fn project_line(raw: &str, width: usize) -> Vec<Row> {
    let width = width.max(1);
    let clean = leveler_core::sanitize_terminal_output_preserving_layout(raw);
    let resolved = resolve_carriage_returns(&clean);
    let expanded = expand_tabs(&resolved);
    wrap_rows(&resolved, &expanded, width)
}

/// The visual row count of one raw logical line, without building its text.
fn row_count(raw: &str, width: usize) -> usize {
    let width = width.max(1);
    let clean = leveler_core::sanitize_terminal_output_preserving_layout(raw);
    let resolved = resolve_carriage_returns(&clean);
    let mut line_col = 0usize;
    let mut row_col = 0usize;
    let mut rows = 1usize;
    let take = |w: usize, row_col: &mut usize, rows: &mut usize| {
        if *row_col > 0 && *row_col + w > width {
            *rows += 1;
            *row_col = 0;
        }
        *row_col += w;
    };
    for ch in resolved.chars() {
        if ch == '\t' {
            let spaces = TAB_STOP - (line_col % TAB_STOP);
            line_col += spaces;
            for _ in 0..spaces {
                take(1, &mut row_col, &mut rows);
            }
        } else {
            let w = char_width(ch);
            line_col += w;
            take(w, &mut row_col, &mut rows);
        }
    }
    rows
}

/// One projected row before it is attached to a logical line index.
struct Row {
    row_in_line: usize,
    display: String,
    source: String,
    columns: Vec<(usize, usize)>,
    soft: bool,
}

/// Mirror `shell_render::render_output` for one line: `\r` rewinds the cursor
/// and later characters overwrite what was there.
fn resolve_carriage_returns(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains('\r') {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut chars: Vec<char> = Vec::new();
    let mut col = 0usize;
    for ch in text.chars() {
        match ch {
            '\r' => col = 0,
            other => {
                if col < chars.len() {
                    chars[col] = other;
                } else {
                    while chars.len() < col {
                        chars.push(' ');
                    }
                    chars.push(other);
                }
                col += 1;
            }
        }
    }
    std::borrow::Cow::Owned(chars.into_iter().collect())
}

/// Expanded display cells plus the byte range in `resolved` each cell came
/// from. A tab expands to its whole source range; the cells after a wrap that
/// belong to the same tab map to an empty range so a selection cannot copy the
/// tab twice.
struct ExpandedChar {
    ch: char,
    width: usize,
    range: (usize, usize),
    /// The tab this cell expands, if any. Only the first cell of a tab on a
    /// given row owns the tab's source range; a tab split by a row break must
    /// not be copied twice.
    tab: Option<usize>,
}

fn expand_tabs(text: &str) -> Vec<ExpandedChar> {
    let mut out = Vec::with_capacity(text.len());
    let mut col = 0usize;
    for (offset, ch) in text.char_indices() {
        let next = offset + ch.len_utf8();
        if ch == '\t' {
            let spaces = TAB_STOP - (col % TAB_STOP);
            for _ in 0..spaces {
                out.push(ExpandedChar {
                    ch: ' ',
                    width: 1,
                    range: (offset, next),
                    tab: Some(offset),
                });
            }
            col += spaces;
        } else {
            out.push(ExpandedChar {
                ch,
                width: char_width(ch),
                range: (offset, next),
                tab: None,
            });
            col += char_width(ch);
        }
    }
    out
}

/// Hard-wrap the expanded line, mirroring `take_display_prefix`.
fn wrap_rows(resolved: &str, expanded: &[ExpandedChar], width: usize) -> Vec<Row> {
    let width = width.max(1);
    let mut rows: Vec<Row> = Vec::new();
    let mut display = String::new();
    let mut columns: Vec<(usize, usize)> = Vec::new();
    let mut first: Option<(usize, usize)> = None;
    let mut last: Option<(usize, usize)> = None;
    let mut soft = false;
    let mut row_col = 0usize;

    let push = |rows: &mut Vec<Row>,
                display: &mut String,
                columns: &mut Vec<(usize, usize)>,
                first: &mut Option<(usize, usize)>,
                last: &mut Option<(usize, usize)>,
                soft: bool,
                resolved: &str| {
        let start = first.map(|range| range.0);
        let end = last.map(|range| range.1);
        let source = match (start, end) {
            (Some(start), Some(end)) if start <= end && end <= resolved.len() => {
                resolved[start..end].to_string()
            }
            _ => String::new(),
        };
        let base = start.unwrap_or(0);
        if base > 0 {
            for range in columns.iter_mut() {
                range.0 = range.0.saturating_sub(base);
                range.1 = range.1.saturating_sub(base);
            }
        }
        rows.push(Row {
            row_in_line: rows.len(),
            display: std::mem::take(display),
            source,
            columns: std::mem::take(columns),
            soft,
        });
        *first = None;
        *last = None;
    };

    if expanded.is_empty() {
        return vec![Row {
            row_in_line: 0,
            display: String::new(),
            source: String::new(),
            columns: Vec::new(),
            soft: false,
        }];
    }

    let mut last_emitted_tab: Option<usize> = None;
    for cell in expanded {
        let w = cell.width;
        if row_col > 0 && row_col + w > width {
            push(
                &mut rows,
                &mut display,
                &mut columns,
                &mut first,
                &mut last,
                soft,
                resolved,
            );
            row_col = 0;
            soft = true;
        }
        display.push(cell.ch);
        if cell.width == 0 {
            // A combining mark belongs to the column it modifies, including
            // the row's authored source slice.
            if let Some(range) = columns.last_mut() {
                range.1 = cell.range.1;
            }
            if let Some(range) = last.as_mut() {
                range.1 = cell.range.1;
            }
        } else {
            let already_emitted = cell.tab.is_some_and(|tab| last_emitted_tab == Some(tab));
            if let Some(tab) = cell.tab {
                last_emitted_tab = Some(tab);
            }
            let range = if already_emitted {
                (cell.range.1, cell.range.1)
            } else {
                cell.range
            };
            for _ in 0..cell.width {
                columns.push(range);
            }
            if !already_emitted {
                first.get_or_insert(cell.range);
                last = Some(cell.range);
            } else if first.is_none() {
                first = Some((cell.range.1, cell.range.1));
            }
        }
        row_col += w;
    }
    push(
        &mut rows,
        &mut display,
        &mut columns,
        &mut first,
        &mut last,
        soft,
        resolved,
    );
    rows
}

/// The live state of the full-output page.
///
/// `scroll` is a visual-row offset. The page owns no rendered text of its own:
/// `index` is the only whole-file structure, and `visible` projects just the
/// rows the viewport asks for. `tail_rows` is the provisional window painted
/// while the index is built; it is bounded by [`PROVISIONAL_TAIL_BYTES`].
#[derive(Debug)]
pub struct OutputView {
    pub source: OutputSource,
    pub title: String,
    pub index: Option<Arc<OutputIndex>>,
    /// Identity of the outstanding index request. A completion for an older
    /// generation is discarded.
    pub generation: u64,
    pub loading: bool,
    pub error: Option<String>,
    pub scroll: usize,
    pub follow: bool,
    pub viewport_height: usize,
    pub selection: TextSelection,
    pub dragging: bool,
    /// Selection edge auto-scroll: -1 up, 0 none, 1 down.
    pub edge_dir: i8,
    pub edge_streak: u32,
    /// Last pointer cell while dragging, for edge auto-scroll.
    pub last_mouse: Option<(u16, u16)>,
    pub tail_rows: Vec<RenderedRow>,
    /// `(line, row_in_line)` at the viewport top before a reflow.
    pub anchor: Option<(usize, usize)>,
    /// Last painted body rect (x, y, w, h).
    pub rect: Option<(u16, u16, u16, u16)>,
    pub bytes: u64,
}

impl OutputView {
    pub fn new(source: OutputSource, title: String, width: usize) -> Self {
        let bytes = source.len();
        let (tail_rows, error) =
            match OutputIndex::tail_rows(&source, width, PROVISIONAL_TAIL_BYTES) {
                Ok(rows) => (rows, None),
                Err(error) => (Vec::new(), Some(error)),
            };
        Self {
            source,
            title,
            index: None,
            generation: 0,
            loading: true,
            error,
            scroll: 0,
            follow: true,
            viewport_height: 0,
            selection: TextSelection::default(),
            dragging: false,
            edge_dir: 0,
            edge_streak: 0,
            last_mouse: None,
            tail_rows,
            anchor: None,
            rect: None,
            bytes,
        }
    }

    pub fn total_rows(&self) -> Option<usize> {
        self.index.as_ref().map(|index| index.total_rows)
    }

    pub fn max_scroll(&self, height: usize) -> usize {
        self.total_rows()
            .unwrap_or(self.tail_rows.len())
            .saturating_sub(height.max(1))
    }

    /// Publish this frame's geometry and keep the follow contract.
    pub fn sync_geometry(&mut self, height: usize) -> usize {
        self.viewport_height = height;
        let max = self.max_scroll(height);
        if self.follow {
            self.scroll = max;
        } else {
            self.scroll = self.scroll.min(max);
        }
        max
    }

    /// The rows this viewport paints. Never more than `count`.
    pub fn visible(&self, count: usize) -> Result<Vec<RenderedRow>, String> {
        if count == 0 {
            return Ok(Vec::new());
        }
        match &self.index {
            Some(index) => {
                let start = self
                    .scroll
                    .min(index.total_rows.saturating_sub(count.min(index.total_rows)));
                index.rows(&self.source, start, count)
            }
            None => {
                let start = if self.follow {
                    self.tail_rows.len().saturating_sub(count)
                } else {
                    self.scroll.min(self.tail_rows.len().saturating_sub(count))
                };
                Ok(self
                    .tail_rows
                    .iter()
                    .skip(start)
                    .take(count)
                    .cloned()
                    .collect())
            }
        }
    }

    pub fn scroll_by(&mut self, delta: isize) {
        let max = self.max_scroll(self.viewport_height);
        if delta < 0 {
            self.follow = false;
            self.scroll = self.scroll.saturating_sub(delta.unsigned_abs()).min(max);
        } else {
            self.scroll = (self.scroll.saturating_add(delta as usize)).min(max);
            if self.scroll >= max {
                self.follow = true;
            }
        }
    }

    pub fn scroll_page(&mut self, direction: isize) {
        let page = self.viewport_height.saturating_sub(1).max(1);
        self.scroll_by(direction * page as isize);
    }

    pub fn jump_to_top(&mut self) {
        self.follow = false;
        self.scroll = 0;
    }

    pub fn jump_to_bottom(&mut self) {
        self.follow = true;
        self.scroll = self.max_scroll(self.viewport_height);
    }

    /// Remember the reader's line before a width change rebuilds the index.
    pub fn capture_anchor(&mut self) {
        self.anchor = if self.follow {
            None
        } else {
            self.index
                .as_ref()
                .and_then(|index| index.locate(self.scroll))
        };
    }

    /// Install a freshly built index, preserving the reader's line on a
    /// reflow and staying at the live edge otherwise.
    pub fn apply_index(&mut self, index: Arc<OutputIndex>) {
        if let Some((line, row_in_line)) = self.anchor.take() {
            self.follow = false;
            self.scroll = index
                .line_start_row(line)
                .saturating_add(row_in_line)
                .min(index.total_rows.saturating_sub(1));
        }
        self.index = Some(index);
        self.loading = false;
        self.error = None;
        self.scroll = self.scroll.min(self.max_scroll(self.viewport_height));
    }

    /// The authored text for the current selection, if any.
    pub fn selected_text(&self) -> Result<String, String> {
        if self.selection.is_empty() {
            return Ok(String::new());
        }
        match &self.index {
            Some(index) => {
                let rows = selection_rows(index, &self.source, &self.selection)?;
                Ok(selection_text(&rows, &self.selection))
            }
            None => Ok(selection_text(&self.tail_rows, &self.selection)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selection::TextPos;

    fn memory(text: &str) -> OutputSource {
        OutputSource::Memory(Arc::from(text))
    }

    fn select(rows: &[RenderedRow], start: (usize, usize), end: (usize, usize)) -> String {
        let mut sel = TextSelection::default();
        sel.begin(TextPos {
            row: start.0,
            col: start.1,
        });
        sel.extend(TextPos {
            row: end.0,
            col: end.1,
        });
        selection_text(rows, &sel)
    }

    #[test]
    fn index_maps_lines_and_rows() {
        let index = OutputIndex::build(&memory("a\nbb\n\nccc\n"), 10).unwrap();
        assert_eq!(index.line_count, 4);
        assert_eq!(index.total_rows, 4);
        assert_eq!(index.locate(0), Some((0, 0)));
        assert_eq!(index.locate(1), Some((1, 0)));
        assert_eq!(index.locate(2), Some((2, 0)));
        assert_eq!(index.locate(3), Some((3, 0)));
        assert_eq!(index.locate(4), None);
    }

    #[test]
    fn wrapping_counts_wide_and_narrow_glyphs() {
        let source = memory("字字字\nabcdef\n");
        let index = OutputIndex::build(&source, 4).unwrap();
        assert_eq!(index.line_rows[0], 2, "two 2-cell glyphs per 4-column row");
        assert_eq!(index.line_rows[1], 2);
        let rows = index.rows(&source, 0, 4).unwrap();
        assert_eq!(rows[0].display, "字字");
        assert_eq!(rows[1].display, "字");
        assert_eq!(rows[2].display, "abcd");
        assert_eq!(rows[3].display, "ef");
        assert!(!rows[0].soft && rows[1].soft);
        assert!(!rows[2].soft && rows[3].soft);
    }

    #[test]
    fn carriage_return_overwrites_before_indexing() {
        let source = memory("progress 10%\rprogress 100%\ndone\n");
        let index = OutputIndex::build(&source, 80).unwrap();
        let rows = index.rows(&source, 0, 10).unwrap();
        assert_eq!(rows[0].display, "progress 100%");
        assert_eq!(rows[1].display, "done");
    }

    #[test]
    fn tab_expands_for_display_and_survives_copy() {
        let source = memory("\tSELECT s.id,\n");
        let index = OutputIndex::build(&source, 80).unwrap();
        let rows = index.rows(&source, 0, 1).unwrap();
        assert_eq!(rows[0].display, "        SELECT s.id,");
        assert_eq!(rows[0].source, "\tSELECT s.id,");
        assert_eq!(select(&rows, (0, 0), (0, 8)), "\t");
    }

    #[test]
    fn source_copy_rejoins_soft_wraps_without_newlines() {
        let source = memory("abcdefghij\n");
        let index = OutputIndex::build(&source, 4).unwrap();
        let rows = index.rows(&source, 0, 10).unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows[1].soft && rows[2].soft);
        assert_eq!(select(&rows, (0, 0), (2, 2)), "abcdefghij");
    }

    #[test]
    fn selection_preserves_tabs_inside_a_soft_wrap() {
        let source = memory("\tabcdef\n");
        let index = OutputIndex::build(&source, 4).unwrap();
        let rows = index.rows(&source, 0, 10).unwrap();
        // The tab is eight cells: two rows of four, then "abcd" and "ef".
        assert_eq!(rows.len(), 4);
        assert_eq!(select(&rows, (0, 0), (3, 2)), "\tabcdef");
    }

    #[test]
    fn selection_keeps_source_blank_lines() {
        let source = memory("a\n\nb\n");
        let index = OutputIndex::build(&source, 10).unwrap();
        let rows = index.rows(&source, 0, 10).unwrap();
        assert_eq!(select(&rows, (0, 0), (2, 1)), "a\n\nb");
    }

    #[test]
    fn indexing_never_builds_rows_for_the_whole_source() {
        let source = memory(&"x".repeat(1024 * 1024));
        let index = OutputIndex::build(&source, 80).unwrap();
        assert_eq!(index.line_count, 1);
        assert_eq!(index.line_rows.len(), 1);
        assert_eq!(index.total_rows, (1024 * 1024usize).div_ceil(80));
        let rows = index.rows(&source, 100, 3).unwrap();
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn emoji_cjk_and_combining_marks_survive_display_and_copy() {
        let source = memory("🐼 中文 e\u{301}\n");
        let index = OutputIndex::build(&source, 80).unwrap();
        let rows = index.rows(&source, 0, 1).unwrap();
        assert_eq!(rows[0].display, "🐼 中文 e\u{301}");
        assert_eq!(select(&rows, (0, 0), (0, 80)), "🐼 中文 e\u{301}");
    }

    #[test]
    fn empty_source_has_no_rows() {
        let source = memory("");
        let index = OutputIndex::build(&source, 80).unwrap();
        assert_eq!(index.total_rows, 0);
        assert!(index.rows(&source, 0, 10).unwrap().is_empty());
    }

    #[test]
    fn a_trailing_newline_does_not_add_an_empty_row() {
        let source = memory("one\n");
        let index = OutputIndex::build(&source, 80).unwrap();
        assert_eq!(index.line_count, 1);
        assert_eq!(index.total_rows, 1);
    }

    #[test]
    fn invalid_utf8_is_replaced_not_split() {
        let dir = std::env::temp_dir().join(format!("leveler-output-view-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("combined.log");
        std::fs::write(&path, b"ok\n\xff\xfe\nend\n").unwrap();
        let source = OutputSource::File {
            path: path.clone(),
            bytes: std::fs::metadata(&path).unwrap().len(),
        };
        let index = OutputIndex::build(&source, 80).unwrap();
        let rows = index.rows(&source, 0, 3).unwrap();
        assert_eq!(rows[0].display, "ok");
        assert!(rows[1].display.contains('\u{FFFD}'));
        assert_eq!(rows[2].display, "end");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn row_counts_match_the_rows_actually_painted() {
        let lines = [
            "",
            "abc",
            "\tabc",
            "a\tb\tc",
            "字字字字字",
            "e\u{301}e\u{301}e\u{301}",
            "progress 10%\rprogress 100%",
            "abcdefghij",
            "\t\t\t\t",
        ];
        for width in [1usize, 2, 3, 4, 5, 7, 8, 16, 80] {
            for line in lines {
                let projected = project_line(line, width);
                assert_eq!(
                    row_count(line, width),
                    projected.len(),
                    "line {line:?} at width {width}"
                );
            }
        }
    }

    #[test]
    fn the_view_follows_the_end_until_the_reader_scrolls_back() {
        let source = memory(
            &(0..200)
                .map(|n| format!("line-{n:03}\n"))
                .collect::<String>(),
        );
        let index = Arc::new(OutputIndex::build(&source, 80).unwrap());
        let mut view = OutputView::new(source, "seq".into(), 80);
        view.apply_index(index);
        view.sync_geometry(10);
        assert_eq!(view.scroll, 190);
        let rows = view.visible(10).unwrap();
        assert_eq!(rows.last().unwrap().display, "line-199");
        view.scroll_by(-5);
        assert!(!view.follow);
        assert_eq!(view.scroll, 185);
        view.jump_to_bottom();
        assert!(view.follow);
        assert_eq!(view.scroll, 190);
    }

    #[test]
    fn a_reflow_keeps_the_reader_on_the_same_logical_line() {
        let source = memory(
            &(0..300)
                .map(|n| format!("line-{n:03}\n"))
                .collect::<String>(),
        );
        let wide = Arc::new(OutputIndex::build(&source, 80).unwrap());
        let mut view = OutputView::new(source.clone(), "seq".into(), 80);
        view.apply_index(wide);
        view.sync_geometry(10);
        view.scroll_by(-40);
        let (line, _) = view.index.as_ref().unwrap().locate(view.scroll).unwrap();
        view.capture_anchor();
        let narrow = Arc::new(OutputIndex::build(&source, 40).unwrap());
        view.apply_index(narrow);
        assert!(!view.follow);
        let (new_line, _) = view.index.as_ref().unwrap().locate(view.scroll).unwrap();
        assert_eq!(line, new_line);
    }

    #[test]
    fn visible_never_returns_more_than_the_viewport_asked_for() {
        let source = memory(
            &(0..10_000)
                .map(|n| format!("line-{n:05}\n"))
                .collect::<String>(),
        );
        let index = Arc::new(OutputIndex::build(&source, 80).unwrap());
        let mut view = OutputView::new(source, "seq".into(), 80);
        view.apply_index(index);
        view.sync_geometry(40);
        assert_eq!(view.visible(40).unwrap().len(), 40);
        assert_eq!(view.visible(1).unwrap().len(), 1);
    }

    #[test]
    fn capture_rejects_a_foreign_root() {
        let root =
            std::env::temp_dir().join(format!("leveler-output-capture-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let allowed = root.join("allowed");
        let captured = root.join("elsewhere");
        std::fs::create_dir_all(&captured).unwrap();
        std::fs::create_dir_all(&allowed).unwrap();
        write_complete_capture(&captured, "hi\n");
        assert!(Capture::open(&captured, Some(&allowed)).is_err());
        assert!(Capture::open(&captured, None).is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn capture_rejects_a_symlinked_combined_file() {
        use std::os::unix::fs::symlink;
        let root =
            std::env::temp_dir().join(format!("leveler-output-symlink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let captured = root.join("capture");
        std::fs::create_dir_all(&captured).unwrap();
        write_complete_capture(&captured, "hi\n");
        let outside = root.join("outside.log");
        std::fs::write(&outside, "secret\n").unwrap();
        std::fs::remove_file(captured.join("combined.log")).unwrap();
        symlink(&outside, captured.join("combined.log")).unwrap();
        assert!(Capture::open(&captured, None).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    fn write_complete_capture(directory: &Path, text: &str) {
        std::fs::write(directory.join("stdout.log"), text).unwrap();
        std::fs::write(directory.join("stderr.log"), "").unwrap();
        std::fs::write(directory.join("combined.log"), text).unwrap();
        for stream in ["stdout", "stderr", "combined"] {
            std::fs::write(
                directory.join(format!("{stream}.meta.json")),
                "{\"complete\":true}\n",
            )
            .unwrap();
        }
    }
}
