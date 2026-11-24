//! Image-chip preview.
//!
//! The chip in the sentence stays `[图片 #N]` / `[image #N]`, in the composer
//! and in a sent row. A path appears only in this overlay, and only when the
//! user attached a file. Kitty-class terminals draw the processed PNG; every
//! other terminal gets a text card. Windows and tmux never emit graphics
//! escapes. A sent row keeps its records only until the transcript is rebuilt.

use std::ops::Range;
use std::path::{Path, PathBuf};

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::screen::Screen;
use crate::state::{AppState, WorkbenchFocus};

const MIN_BOX_WIDTH: u16 = 28;
const MIN_PIXEL_HEIGHT: u16 = 8;
const MIN_TEXT_HEIGHT: u16 = 6;
const TEXT_WIDTH_RATIO: f32 = 0.75;
const DEFAULT_CELL_ASPECT: f64 = 0.5;

/// One hoverable run of an image chip. `x1` is exclusive, so the space after
/// `]` is not part of the chip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChipHit {
    pub index: usize,
    pub row: u16,
    pub x0: u16,
    pub x1: u16,
}

/// One image a sent user row still names. The transcript item is the row;
/// the text is the sentence that was sent. A later edit of that slot drops
/// the record instead of hanging these bytes on a different sentence.
#[derive(Debug, Clone)]
pub(crate) struct SentImageTurn {
    pub item: usize,
    pub text: String,
    pub attachments: Vec<leveler_client_protocol::AttachmentRef>,
}

/// One hoverable run of a sent image chip. `item` is the transcript row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SentChipHit {
    pub item: usize,
    pub index: usize,
    pub row: u16,
    pub x0: u16,
    pub x1: u16,
}

/// A wrapped composer row reduced to the bytes it paints.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ChipSlice {
    pub start_byte: usize,
    pub end_byte: usize,
    pub prompt_cols: usize,
    pub row: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PreviewProtocol {
    None,
    Kitty,
    ITerm2,
}

impl PreviewProtocol {
    pub(crate) fn supports_images(self) -> bool {
        !matches!(self, Self::None)
    }

    fn picker_type(self) -> Option<ratatui_image::picker::ProtocolType> {
        match self {
            Self::Kitty => Some(ratatui_image::picker::ProtocolType::Kitty),
            Self::ITerm2 => Some(ratatui_image::picker::ProtocolType::Iterm2),
            Self::None => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PreviewBrand {
    Kitty,
    Ghostty,
    WezTerm,
    Warp,
    ITerm2,
    Other,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ProtocolFacts<'a> {
    pub windows: bool,
    pub tmux: bool,
    pub brand: PreviewBrand,
    pub term_features: Option<&'a str>,
    pub ssh: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct TerminalSignals<'a> {
    pub term_program: Option<&'a str>,
    pub term: Option<&'a str>,
    pub wezterm_version: Option<&'a str>,
    pub iterm_session: bool,
    pub lc_terminal: Option<&'a str>,
    pub kitty_window: bool,
}

/// Labels the card prints. The product passes its locale; tests pass a fixed set.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PreviewCopy<'a> {
    pub kind: &'a str,
    pub format: &'a str,
    pub dimensions: &'a str,
    pub size: &'a str,
    pub path: &'a str,
    pub unavailable: &'a str,
    pub pending: &'a str,
    pub loading: &'a str,
}

/// Cells the protocol must fill. The card can be larger when the minimum
/// chrome exceeds the fitted image; the image stays centered in that leftover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ImagePlacement {
    pub cols: u16,
    pub rows: u16,
    pub x: u16,
    pub y: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OverlayPlan {
    pub rect: Rect,
    pub show_pixels: bool,
    pub placement: Option<ImagePlacement>,
    pub title: String,
    pub meta: String,
    pub border_title: String,
    pub body: Vec<String>,
    pub path_footer: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct PreviewFacts<'a> {
    pub protocol: PreviewProtocol,
    pub pixels_ready: bool,
    pub decode_failed: bool,
    pub pixels_pending: bool,
    pub source_path: Option<&'a str>,
    pub mime: &'a str,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub size_bytes: u64,
    pub filename: &'a str,
    pub number: usize,
    pub cell_aspect: f64,
    pub copy: PreviewCopy<'a>,
}

/// Which staged image the overlay names.
///
/// Cursor on the chip, or exactly on its right edge, wins. Then the pointer.
/// Then a just-inserted chip whose cursor has not moved. The space after the
/// chip is not the right edge.
pub(crate) fn preview_index(
    tokens: &[(Range<usize>, usize)],
    cursor_byte: usize,
    cursor_grapheme: usize,
    hover: Option<usize>,
    post_insert: Option<(usize, usize)>,
    attachment_len: usize,
) -> Option<usize> {
    let containing = tokens
        .iter()
        .position(|(range, _)| range.start <= cursor_byte && cursor_byte < range.end);
    let edge = tokens
        .iter()
        .position(|(range, _)| range.end == cursor_byte);
    let index = containing.or(edge).or(hover).or_else(|| {
        let (index, at) = post_insert?;
        (cursor_grapheme == at).then_some(index)
    })?;
    (index < attachment_len && index < tokens.len()).then_some(index)
}

pub(crate) fn selected_image(state: &AppState) -> Option<usize> {
    preview_index(
        &state.composer.image_token_spans(),
        state.composer.cursor_byte(),
        state.composer.cursor(),
        state.image_hover,
        state.image_post_insert,
        state.pending_attachments.len(),
    )
}

pub(crate) fn chip_hits(
    buffer: &str,
    tokens: &[Range<usize>],
    slices: &[ChipSlice],
    origin_x: u16,
) -> Vec<ChipHit> {
    let mut hits = Vec::new();
    for slice in slices {
        if slice.start_byte > slice.end_byte || slice.end_byte > buffer.len() {
            continue;
        }
        for (index, range) in tokens.iter().enumerate() {
            let start = range.start.max(slice.start_byte);
            let end = range.end.min(slice.end_byte);
            if start >= end {
                continue;
            }
            let prefix = UnicodeWidthStr::width(&buffer[slice.start_byte..start]);
            let span = UnicodeWidthStr::width(&buffer[start..end]);
            if span == 0 {
                continue;
            }
            let x0 = origin_x.saturating_add((slice.prompt_cols + prefix) as u16);
            hits.push(ChipHit {
                index,
                row: slice.row,
                x0,
                x1: x0.saturating_add(span as u16),
            });
        }
    }
    hits
}

/// Keep the attachments of a user row that was just sent. The conversation
/// shows only the chip. Hover reads this list for the rest of the session.
pub(crate) fn remember_sent_images(
    state: &mut AppState,
    item: usize,
    text: String,
    attachments: Vec<leveler_client_protocol::AttachmentRef>,
) {
    if attachments.is_empty() {
        return;
    }
    if let Some(existing) = state.sent_images.iter_mut().find(|sent| sent.item == item) {
        existing.text = text;
        existing.attachments = attachments;
        return;
    }
    state.sent_images.push(SentImageTurn {
        item,
        text,
        attachments,
    });
}

/// Drop every sent preview. A rebuilt transcript does not carry the records.
pub(crate) fn forget_sent_images(state: &mut AppState) {
    state.sent_images.clear();
    state.sent_image_hover = None;
    state.sent_image_hits.clear();
}

/// Screen cells of sent image chips in the conversation viewport that was
/// just measured. A chip that scrolled out is not hoverable. The cell after
/// `]` is the trailing space, not part of the chip.
pub(crate) fn publish_sent_chip_hits(
    state: &mut AppState,
    item_spans: &[(usize, usize)],
    scroll: usize,
    content: Rect,
    total: usize,
    height: usize,
) {
    let kept: Vec<bool> = state
        .sent_images
        .iter()
        .map(|sent| {
            matches!(
                state.transcript.items().get(sent.item),
                Some(crate::transcript::TranscriptItem::User(text)) if text == &sent.text
            )
        })
        .collect();
    let mut keep_at = 0;
    state.sent_images.retain(|_| {
        let keep = kept[keep_at];
        keep_at += 1;
        keep
    });

    let template = state.image_token_template();
    let above = crate::conversation::geometry::painted_top_padding(state, total, height);
    let origin_x = content
        .x
        .saturating_add(crate::render::transcript_lines::USER_PROMPT_GUTTER as u16);
    let mut hits = Vec::new();
    for sent in &state.sent_images {
        let Some(&(start, end)) = item_spans.get(sent.item) else {
            continue;
        };
        let (source, spans) =
            crate::render::transcript_lines::user_body_spans(&sent.text, content.width as usize);
        let tokens: Vec<Range<usize>> = crate::composer::scan_image_tokens(&source, &template)
            .into_iter()
            .take(sent.attachments.len())
            .map(|(range, _)| range)
            .collect();
        let mut slices = Vec::new();
        let row_count = end.saturating_sub(start);
        for (row_index, span) in spans.iter().take(row_count).enumerate() {
            let absolute = start + row_index;
            if absolute < scroll || absolute >= scroll.saturating_add(height) {
                continue;
            }
            let y = content
                .y
                .saturating_add(above as u16)
                .saturating_add((absolute - scroll) as u16);
            if y >= content.y.saturating_add(content.height) {
                continue;
            }
            slices.push(ChipSlice {
                start_byte: span.start,
                end_byte: span.end,
                prompt_cols: 0,
                row: y,
            });
        }
        for hit in chip_hits(&source, &tokens, &slices, origin_x) {
            hits.push(SentChipHit {
                item: sent.item,
                index: hit.index,
                row: hit.row,
                x0: hit.x0,
                x1: hit.x1,
            });
        }
    }
    if state.sent_image_hover.is_some_and(|(item, index)| {
        !hits
            .iter()
            .any(|hit| hit.item == item && hit.index == index)
    }) {
        state.sent_image_hover = None;
    }
    state.sent_image_hits = hits;
}

pub(crate) fn session_is_tmux(tmux_var: bool, term: &str) -> bool {
    tmux_var || term.starts_with("tmux")
}

pub(crate) fn brand_from(signals: TerminalSignals<'_>) -> PreviewBrand {
    if let Some(program) = signals.term_program
        && let Some(brand) = brand_from_program(program)
    {
        return brand;
    }
    if signals.wezterm_version.is_some() {
        return PreviewBrand::WezTerm;
    }
    if signals.iterm_session
        || signals
            .lc_terminal
            .is_some_and(|value| value.eq_ignore_ascii_case("iterm2"))
    {
        return PreviewBrand::ITerm2;
    }
    if signals.kitty_window || signals.term.is_some_and(|term| term.contains("kitty")) {
        return PreviewBrand::Kitty;
    }
    if signals
        .term
        .is_some_and(|term| term.to_ascii_lowercase().contains("ghostty"))
    {
        return PreviewBrand::Ghostty;
    }
    PreviewBrand::Other
}

pub(crate) fn protocol_for(facts: ProtocolFacts<'_>) -> PreviewProtocol {
    if facts.tmux || facts.windows {
        return PreviewProtocol::None;
    }
    if facts.brand == PreviewBrand::ITerm2 {
        let allowed = match facts.term_features {
            Some(features) => features.contains('F'),
            None => facts.ssh,
        };
        return if allowed {
            PreviewProtocol::ITerm2
        } else {
            PreviewProtocol::None
        };
    }
    match facts.brand {
        PreviewBrand::Kitty
        | PreviewBrand::Ghostty
        | PreviewBrand::WezTerm
        | PreviewBrand::Warp => PreviewProtocol::Kitty,
        PreviewBrand::ITerm2 | PreviewBrand::Other => PreviewProtocol::None,
    }
}

pub(crate) fn plan_overlay(area: Rect, facts: &PreviewFacts<'_>) -> Option<OverlayPlan> {
    let show_pixels =
        facts.protocol.supports_images() && facts.pixels_ready && !facts.decode_failed;
    let dims = match (facts.width, facts.height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => (w, h),
        _ => (640, 480),
    };
    let (rect, placement) = overlay_rect(
        area,
        show_pixels,
        facts.source_path.is_some(),
        dims,
        facts.cell_aspect,
    )?;
    let inner_w = rect.width.saturating_sub(2) as usize;
    let inner_h = rect.height.saturating_sub(2);
    let path_footer = facts
        .source_path
        .filter(|_| inner_h >= 2)
        .map(|path| path_line(facts.copy.path, path, inner_w));
    let mut body = Vec::new();
    if !show_pixels {
        body.push(format!(
            "{}: {}",
            facts.copy.format,
            format_mime(facts.mime)
        ));
        if let (Some(w), Some(h)) = (facts.width, facts.height) {
            body.push(format!("{}: {w}×{h}", facts.copy.dimensions));
        }
        if facts.decode_failed {
            body.push(facts.copy.unavailable.to_string());
        } else if facts.pixels_pending && facts.protocol.supports_images() && !facts.pixels_ready {
            body.push(facts.copy.pending.to_string());
        } else {
            body.push(format!(
                "{}: {}",
                facts.copy.size,
                format_bytes(facts.size_bytes)
            ));
        }
        if path_footer.is_none()
            && let Some(path) = facts.source_path
        {
            body.push(path_line(facts.copy.path, path, inner_w.max(1)));
        }
    }
    let title = format!(" {} #{} ", facts.copy.kind, facts.number);
    let meta = meta_line(
        facts.mime,
        facts.width,
        facts.height,
        facts.size_bytes,
        facts.filename,
    );
    let border_title = border_title(&title, &meta, rect.width as usize);
    Some(OverlayPlan {
        rect,
        show_pixels,
        placement,
        title,
        meta,
        border_title,
        body,
        path_footer,
    })
}

pub(crate) fn path_line(label: &str, path: &str, width: usize) -> String {
    let prefix = format!("{label}: ");
    let budget = width.saturating_sub(UnicodeWidthStr::width(prefix.as_str()));
    let shown = truncate_middle(path, budget);
    truncate_width(format!("{prefix}{shown}"), width)
}

pub(crate) fn truncate_middle(path: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let chars: Vec<char> = path.chars().collect();
    if chars.len() <= max_chars {
        return path.to_string();
    }
    if max_chars <= 3 {
        return chars.into_iter().take(max_chars).collect();
    }
    let keep = max_chars.saturating_sub(3) / 2;
    let end_keep = max_chars.saturating_sub(3) - keep;
    let head: String = chars.iter().take(keep).collect();
    let tail: String = chars
        .iter()
        .skip(chars.len().saturating_sub(end_keep))
        .collect();
    format!("{head}...{tail}")
}

/// A processed PNG the terminal may open. The name must be `{sha256}.png` and
/// the file must sit in this session's media directory. Anything else is not
/// a preview, so the card stays text and the file is not read.
pub(crate) fn preview_file(media_dir: &Path, sha256: &str, preview_path: &str) -> Option<PathBuf> {
    let path = Path::new(preview_path);
    let name = format!("{sha256}.png");
    if path.file_name()?.to_str()? != name {
        return None;
    }
    path.starts_with(media_dir).then(|| path.to_path_buf())
}

pub(crate) fn paint(frame: &mut Frame, state: &mut AppState) {
    paint_with(frame, state, current_protocol(), None);
}

pub(crate) fn paint_with(
    frame: &mut Frame,
    state: &mut AppState,
    protocol: PreviewProtocol,
    cell_aspect: Option<f64>,
) {
    if !preview_open(state) {
        hide_pixels(state);
        return;
    }
    let Some(area) = conversation_area(frame.area(), state) else {
        hide_pixels(state);
        return;
    };
    let Some(shown) = shown_image(state) else {
        hide_pixels(state);
        return;
    };
    let (ready, failed) = load_outcome(state, &shown, protocol);
    let aspect = if protocol.supports_images() && ready && !failed {
        cell_aspect.unwrap_or_else(current_cell_aspect)
    } else {
        DEFAULT_CELL_ASPECT
    };
    let copy = copy_of(state);
    let filename = filename_of(shown.source_path.as_deref(), &shown.name);
    let facts = PreviewFacts {
        protocol,
        pixels_ready: ready,
        decode_failed: failed,
        pixels_pending: false,
        source_path: shown.source_path.as_deref(),
        mime: &shown.mime,
        width: shown.width,
        height: shown.height,
        size_bytes: shown.size_bytes,
        filename: &filename,
        number: shown.number,
        cell_aspect: aspect,
        copy,
    };
    let Some(plan) = plan_overlay(area, &facts) else {
        hide_pixels(state);
        return;
    };
    let canvas = state.theme.surface.canvas;
    let bg = state.theme.surface.elevated;
    let border = state.theme.border.focus;
    let text_fg = state.theme.text.primary;
    let path_fg = state.theme.text.secondary;
    {
        let buf = frame.buffer_mut();
        dim_area(buf, area);
    }
    frame.render_widget(Clear, plan.rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border).bg(bg))
        .style(Style::default().fg(text_fg).bg(bg));
    let inner = block.inner(plan.rect);
    frame.render_widget(block, plan.rect);
    {
        let buf = frame.buffer_mut();
        let room = plan.rect.width.saturating_sub(2) as usize;
        let title_w = UnicodeWidthStr::width(plan.border_title.as_str()).min(room);
        let title_x = plan.rect.x.saturating_add(1).saturating_add(
            (plan
                .rect
                .width
                .saturating_sub(2)
                .saturating_sub(title_w as u16))
                / 2,
        );
        buf.set_stringn(
            title_x,
            plan.rect.y,
            &plan.border_title,
            room,
            Style::default()
                .fg(text_fg)
                .bg(canvas)
                .add_modifier(Modifier::BOLD),
        );
    }
    if inner.width == 0 || inner.height == 0 {
        hide_pixels(state);
        return;
    }
    let image_area = if plan.path_footer.is_some() && inner.height >= 2 {
        Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: inner.height.saturating_sub(1),
        }
    } else {
        inner
    };
    if let Some(path) = &plan.path_footer {
        let y = inner.y.saturating_add(inner.height.saturating_sub(1));
        let buf = frame.buffer_mut();
        buf.set_stringn(
            inner.x,
            y,
            path,
            inner.width as usize,
            Style::default().fg(path_fg).bg(bg),
        );
    }
    if plan.show_pixels && image_area.width > 0 && image_area.height > 0 {
        ensure_placement(state, protocol, rgba(bg));
        let draw = plan
            .placement
            .map(|place| Rect {
                x: place.x,
                y: place.y,
                width: place.cols,
                height: place.rows,
            })
            .unwrap_or(image_area);
        let failed = render_pixels(frame, state, image_area, draw, text_fg, bg, copy.loading);
        if !failed {
            return;
        }
    } else {
        hide_pixels(state);
    }
    if image_area.width == 0 || image_area.height == 0 {
        return;
    }
    let lines: Vec<Line<'static>> = plan
        .body
        .iter()
        .cloned()
        .map(|line| Line::from(Span::styled(line, Style::default().fg(text_fg).bg(bg))))
        .collect();
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().fg(text_fg).bg(bg))
            .wrap(Wrap { trim: false }),
        image_area,
    );
}

pub(crate) fn take_cleanup(state: &mut AppState) -> Vec<String> {
    state
        .image_preview_cache
        .as_mut()
        .map(|cache| std::mem::take(&mut cache.pending_cleanup))
        .unwrap_or_default()
}

pub(crate) fn retire(state: &mut AppState) {
    hide_pixels(state);
}

fn preview_open(state: &AppState) -> bool {
    // Conversation focus is the transcript. A wheel lands there, and a sent
    // chip is in that transcript, so the card stays available while reading.
    state.surface == crate::btw::SurfaceFocus::Main
        && state.active_screen == Screen::Conversation
        && state.overlay.is_none()
        && state.update.is_none()
        && state.history_search.is_none()
        && matches!(
            state.workbench_focus,
            WorkbenchFocus::Input | WorkbenchFocus::Conversation
        )
        && !state.completion.open
}

fn conversation_area(frame: Rect, state: &AppState) -> Option<Rect> {
    let (x, y, width, height) = state.conv.rect?;
    let area = Rect {
        x,
        y,
        width,
        height,
    };
    let x0 = area.x.max(frame.x);
    let y0 = area.y.max(frame.y);
    let x1 = area
        .x
        .saturating_add(area.width)
        .min(frame.x.saturating_add(frame.width));
    let y1 = area
        .y
        .saturating_add(area.height)
        .min(frame.y.saturating_add(frame.height));
    let clipped = Rect {
        x: x0,
        y: y0,
        width: x1.saturating_sub(x0),
        height: y1.saturating_sub(y0),
    };
    (clipped.width > 0 && clipped.height > 0).then_some(clipped)
}

struct ShownImage {
    number: usize,
    sha256: String,
    preview_path: Option<String>,
    source_path: Option<String>,
    mime: String,
    name: String,
    width: Option<u32>,
    height: Option<u32>,
    size_bytes: u64,
}

fn shown_from(attachment: &leveler_client_protocol::AttachmentRef, number: usize) -> ShownImage {
    ShownImage {
        number,
        sha256: attachment.sha256.clone(),
        preview_path: attachment.preview_path.clone(),
        source_path: attachment.source_path.clone(),
        mime: attachment.mime_type.clone(),
        name: attachment.name.clone(),
        width: attachment.width,
        height: attachment.height,
        size_bytes: attachment.size_bytes,
    }
}

fn shown_image(state: &AppState) -> Option<ShownImage> {
    // The composer chip, including one the cursor is on, wins over a sent row.
    if let Some(index) = selected_image(state) {
        let attachment = state.pending_attachments.get(index)?;
        let number = state
            .composer
            .image_token_spans()
            .get(index)
            .map(|(_, number)| *number)
            .unwrap_or(index + 1);
        return Some(shown_from(attachment, number));
    }
    let (item, index) = state.sent_image_hover?;
    let sent = state.sent_images.iter().find(|sent| sent.item == item)?;
    let text = match state.transcript.items().get(sent.item) {
        Some(crate::transcript::TranscriptItem::User(text)) if text == &sent.text => text,
        _ => return None,
    };
    let attachment = sent.attachments.get(index)?;
    let number = crate::composer::scan_image_tokens(text, &state.image_token_template())
        .get(index)
        .map(|(_, number)| *number)
        .unwrap_or(index + 1);
    Some(shown_from(attachment, number))
}

fn copy_of(state: &AppState) -> PreviewCopy<'static> {
    let t = state.t();
    PreviewCopy {
        kind: t.image_preview_title,
        format: t.image_preview_format,
        dimensions: t.image_preview_dimensions,
        size: t.image_preview_size,
        path: t.image_preview_path,
        unavailable: t.image_preview_unavailable,
        pending: t.image_preview_pending,
        loading: t.image_preview_loading,
    }
}

enum Outcome {
    Ready(image::DynamicImage),
    Failed,
    Absent,
}

struct Placed {
    protocol: ratatui_image::protocol::StatefulProtocol,
    uploaded: bool,
    kind: PreviewProtocol,
    background: [u8; 4],
}

pub(crate) struct PreviewCache {
    sha256: String,
    outcome: Outcome,
    placed: Option<Placed>,
    pending_cleanup: Vec<String>,
}

impl std::fmt::Debug for PreviewCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreviewCache")
            .field("sha256", &self.sha256)
            .field("uploaded", &self.placed.as_ref().map(|p| p.uploaded))
            .finish_non_exhaustive()
    }
}

fn load_outcome(
    state: &mut AppState,
    shown: &ShownImage,
    protocol: PreviewProtocol,
) -> (bool, bool) {
    if !protocol.supports_images() {
        hide_pixels(state);
        return (false, false);
    }
    if let Some(cache) = &state.image_preview_cache
        && cache.sha256 == shown.sha256
    {
        return match &cache.outcome {
            Outcome::Ready(_) => (true, false),
            Outcome::Failed => (false, true),
            Outcome::Absent => (false, false),
        };
    }
    let outcome = match (state.media_dir(), shown.preview_path.as_deref()) {
        (Some(dir), Some(path)) => match preview_file(&dir, &shown.sha256, path) {
            Some(file) => match image::open(file) {
                Ok(image) => Outcome::Ready(image),
                Err(_) => Outcome::Failed,
            },
            None => Outcome::Absent,
        },
        _ => Outcome::Absent,
    };
    let flags = match &outcome {
        Outcome::Ready(_) => (true, false),
        Outcome::Failed => (false, true),
        Outcome::Absent => (false, false),
    };
    // A new image replaces the cache. The delete for whatever the terminal
    // already holds has to survive that replacement; the flush after this
    // frame is what sends it.
    let mut pending = Vec::new();
    if let Some(cache) = &mut state.image_preview_cache {
        pending.append(&mut cache.pending_cleanup);
        if let Some(placed) = cache.placed.take()
            && let Some(seq) = kitty_delete(&placed)
        {
            pending.push(seq);
        }
    }
    state.image_preview_cache = Some(PreviewCache {
        sha256: shown.sha256.clone(),
        outcome,
        placed: None,
        pending_cleanup: pending,
    });
    flags
}

fn hide_pixels(state: &mut AppState) {
    let Some(cache) = &mut state.image_preview_cache else {
        return;
    };
    if let Some(placed) = cache.placed.take()
        && let Some(seq) = kitty_delete(&placed)
    {
        cache.pending_cleanup.push(seq);
    }
}

fn ensure_placement(state: &mut AppState, protocol: PreviewProtocol, background: [u8; 4]) {
    let Some(picker_type) = protocol.picker_type() else {
        return;
    };
    let Some(cache) = &mut state.image_preview_cache else {
        return;
    };
    if cache
        .placed
        .as_ref()
        .is_some_and(|placed| placed.kind == protocol && placed.background == background)
    {
        return;
    }
    let Outcome::Ready(image) = &cache.outcome else {
        return;
    };
    let image = image.clone();
    if let Some(old) = cache.placed.take()
        && let Some(seq) = kitty_delete(&old)
    {
        cache.pending_cleanup.push(seq);
    }
    let mut picker = ratatui_image::picker::Picker::from_fontsize(font_size());
    picker.set_protocol_type(picker_type);
    picker.set_background_color(background);
    cache.placed = Some(Placed {
        protocol: picker.new_resize_protocol(image),
        uploaded: false,
        kind: protocol,
        background,
    });
}

fn render_pixels(
    frame: &mut Frame,
    state: &mut AppState,
    loading_area: Rect,
    draw: Rect,
    text_fg: ratatui::style::Color,
    bg: ratatui::style::Color,
    loading: &str,
) -> bool {
    let loading_w = UnicodeWidthStr::width(loading) as u16;
    let x = loading_area.x + loading_area.width.saturating_sub(loading_w) / 2;
    let y = loading_area.y + loading_area.height / 2;
    frame.buffer_mut().set_stringn(
        x,
        y,
        loading,
        loading_area.width as usize,
        Style::default().fg(text_fg).bg(bg),
    );
    let Some(cache) = &mut state.image_preview_cache else {
        return true;
    };
    let failed = {
        let Some(placed) = cache.placed.as_mut() else {
            return true;
        };
        frame.render_stateful_widget(
            ratatui_image::StatefulImage::default().resize(preview_resize()),
            draw,
            &mut placed.protocol,
        );
        let failed = matches!(placed.protocol.last_encoding_result(), Some(Err(_)));
        if !failed {
            placed.uploaded = true;
        }
        failed
    };
    if !failed {
        return false;
    }
    // The encode failed inside this frame. Drop the placement and let the
    // caller paint the text card over the same cells.
    if let Some(seq) = cache.placed.as_ref().and_then(kitty_delete) {
        cache.pending_cleanup.push(seq);
    }
    cache.outcome = Outcome::Failed;
    cache.placed = None;
    frame.render_widget(Clear, loading_area);
    true
}

fn kitty_delete(placed: &Placed) -> Option<String> {
    if !placed.uploaded && !matches!(placed.kind, PreviewProtocol::Kitty) {
        return None;
    }
    match placed.protocol.protocol_type() {
        ratatui_image::protocol::StatefulProtocolType::Kitty(kitty) => {
            Some(format!("\x1b_Ga=d,d=I,i={},q=2\x1b\\", kitty.unique_id))
        }
        _ => None,
    }
}

fn current_protocol() -> PreviewProtocol {
    let env = leveler_core::environment();
    let term = env.var("TERM").unwrap_or_default();
    let features = env.var("TERM_FEATURES");
    let program = env.var("TERM_PROGRAM");
    let term_lc = env.var("LC_TERMINAL");
    protocol_for(ProtocolFacts {
        windows: cfg!(target_os = "windows"),
        tmux: session_is_tmux(env.var_os("TMUX").is_some(), &term),
        brand: brand_from(TerminalSignals {
            term_program: program.as_deref(),
            term: Some(term.as_str()),
            wezterm_version: env.var("WEZTERM_VERSION").as_deref(),
            iterm_session: env.var_os("ITERM_SESSION_ID").is_some()
                || env.var_os("ITERM_PROFILE").is_some(),
            lc_terminal: term_lc.as_deref(),
            kitty_window: env.var_os("KITTY_WINDOW_ID").is_some(),
        }),
        term_features: features.as_deref(),
        ssh: env.var_os("SSH_CONNECTION").is_some(),
    })
}

fn current_cell_aspect() -> f64 {
    let Ok(size) = crossterm::terminal::window_size() else {
        return DEFAULT_CELL_ASPECT;
    };
    cell_aspect(size.width, size.height, size.columns, size.rows)
}

fn cell_aspect(width_px: u16, height_px: u16, columns: u16, rows: u16) -> f64 {
    if columns == 0 || rows == 0 || width_px == 0 || height_px == 0 {
        return DEFAULT_CELL_ASPECT;
    }
    let aspect =
        (f64::from(width_px) / f64::from(columns)) / (f64::from(height_px) / f64::from(rows));
    if (0.3..=0.8).contains(&aspect) {
        aspect
    } else {
        DEFAULT_CELL_ASPECT
    }
}

fn font_size() -> (u16, u16) {
    crossterm::terminal::window_size()
        .ok()
        .and_then(|size| {
            let w = size.width.checked_div(size.columns)?;
            let h = size.height.checked_div(size.rows)?;
            (w > 0 && h > 0).then_some((w, h))
        })
        .unwrap_or((10, 20))
}

fn overlay_rect(
    area: Rect,
    show_pixels: bool,
    has_path: bool,
    dims: (u32, u32),
    cell_aspect: f64,
) -> Option<(Rect, Option<ImagePlacement>)> {
    let min_h = if show_pixels {
        MIN_PIXEL_HEIGHT
    } else {
        MIN_TEXT_HEIGHT
    };
    if area.width < MIN_BOX_WIDTH || area.height < min_h {
        return None;
    }
    if show_pixels {
        let footer = u16::from(has_path);
        let max_cols = area.width.saturating_sub(2).max(4);
        let max_rows = area.height.saturating_sub(2).saturating_sub(footer).max(2);
        let (cols, rows) = fit_cells(dims.0, dims.1, max_cols, max_rows, cell_aspect);
        let width = cols.saturating_add(2).clamp(MIN_BOX_WIDTH, area.width);
        let height = rows
            .saturating_add(2)
            .saturating_add(footer)
            .clamp(MIN_PIXEL_HEIGHT, area.height);
        let x = area.x + area.width.saturating_sub(width) / 2;
        let y = area.y + area.height.saturating_sub(height) / 2;
        let inner_width = width.saturating_sub(2);
        let inner_height = height.saturating_sub(2).saturating_sub(footer);
        let rect = Rect {
            x,
            y,
            width,
            height,
        };
        let placement = ImagePlacement {
            cols,
            rows,
            x: x + 1 + inner_width.saturating_sub(cols) / 2,
            y: y + 1 + inner_height.saturating_sub(rows) / 2,
        };
        return Some((rect, Some(placement)));
    }
    let width =
        ((f32::from(area.width) * TEXT_WIDTH_RATIO) as u16).clamp(MIN_BOX_WIDTH, area.width);
    let height = (MIN_TEXT_HEIGHT).clamp(MIN_TEXT_HEIGHT, area.height);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height);
    Some((
        Rect {
            x,
            y,
            width,
            height,
        },
        None,
    ))
}

/// Fit leaves a source smaller than the card at its native pixels, in the
/// corner. Scale grows it to the fitted cell rect, which covers the loading line.
fn preview_resize() -> ratatui_image::Resize {
    ratatui_image::Resize::Scale(Some(image::imageops::FilterType::Lanczos3))
}

fn fit_cells(img_w: u32, img_h: u32, max_cols: u16, max_rows: u16, cell_aspect: f64) -> (u16, u16) {
    if img_w == 0 || img_h == 0 || max_cols == 0 || max_rows == 0 {
        return (max_cols.max(1), max_rows.max(1));
    }
    let cols_per_row = ((f64::from(img_w) / f64::from(img_h)) / cell_aspect.max(0.01)).max(0.01);
    let rows_by_width = (f64::from(max_cols) / cols_per_row).round() as u16;
    if rows_by_width > 0 && rows_by_width <= max_rows {
        return (max_cols, rows_by_width.max(1));
    }
    let cols_by_height = (f64::from(max_rows) * cols_per_row).round() as u16;
    (cols_by_height.min(max_cols).max(1), max_rows.max(1))
}

fn meta_line(
    mime: &str,
    width: Option<u32>,
    height: Option<u32>,
    size: u64,
    filename: &str,
) -> String {
    let mut parts = vec![format_mime(mime)];
    if let (Some(w), Some(h)) = (width, height) {
        parts.push(format!("{w}×{h}"));
    }
    parts.push(format_bytes(size));
    if !filename.is_empty() {
        parts.push(filename.to_string());
    }
    parts.join(" · ")
}

fn filename_of<'a>(source: Option<&'a str>, name: &'a str) -> &'a str {
    source
        .and_then(|path| Path::new(path).file_name())
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(name)
}

fn format_mime(mime: &str) -> String {
    match mime {
        "image/png" => "PNG".to_string(),
        "image/jpeg" => "JPEG".to_string(),
        "image/tiff" => "TIFF".to_string(),
        "image/gif" => "GIF".to_string(),
        "image/webp" => "WebP".to_string(),
        "image/bmp" => "BMP".to_string(),
        other => other.to_string(),
    }
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{} KB", bytes / 1024)
    } else {
        format!("{bytes} B")
    }
}

fn border_title(title: &str, meta: &str, width: usize) -> String {
    let full = format!("{title}─ {meta} ");
    if UnicodeWidthStr::width(full.as_str()) + 2 < width {
        full
    } else {
        title.to_string()
    }
}

fn truncate_width(text: String, width: usize) -> String {
    if UnicodeWidthStr::width(text.as_str()) <= width {
        return text;
    }
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + cw > width {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out
}

fn brand_from_program(program: &str) -> Option<PreviewBrand> {
    let normalized: String = program
        .trim()
        .chars()
        .filter(|ch| !matches!(ch, ' ' | '-' | '_' | '.'))
        .flat_map(|ch| ch.to_lowercase())
        .collect();
    match normalized.as_str() {
        "ghostty" => Some(PreviewBrand::Ghostty),
        "kitty" => Some(PreviewBrand::Kitty),
        "wezterm" => Some(PreviewBrand::WezTerm),
        "warp" | "warpterminal" => Some(PreviewBrand::Warp),
        "iterm" | "iterm2" | "itermapp" => Some(PreviewBrand::ITerm2),
        _ => None,
    }
}

fn dim_area(buf: &mut ratatui::buffer::Buffer, area: Rect) {
    let right = area.x.saturating_add(area.width).min(buf.area.width);
    let bottom = area.y.saturating_add(area.height).min(buf.area.height);
    for y in area.y..bottom {
        for x in area.x..right {
            if let Some(cell) = buf.cell_mut((x, y)) {
                let style = cell.style().add_modifier(Modifier::DIM);
                cell.set_style(style);
            }
        }
    }
}

fn rgba(color: ratatui::style::Color) -> [u8; 4] {
    match color {
        ratatui::style::Color::Rgb(r, g, b) => [r, g, b, 255],
        _ => [0, 0, 0, 0],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::Action;
    use crate::reducer::reduce;
    use crate::state::Boot;
    use crate::theme::Theme;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
    use leveler_client_protocol::{
        AttachmentId, AttachmentKind, AttachmentRef, RuntimeEvent, SessionId,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn english() -> PreviewCopy<'static> {
        PreviewCopy {
            kind: "Image",
            format: "Format",
            dimensions: "Dimensions",
            size: "Size",
            path: "Path",
            unavailable: "Preview unavailable",
            pending: "Preview pending",
            loading: "Loading...",
        }
    }

    fn facts<'a>(protocol: PreviewProtocol, path: Option<&'a str>) -> PreviewFacts<'a> {
        PreviewFacts {
            protocol,
            pixels_ready: false,
            decode_failed: false,
            pixels_pending: false,
            source_path: path,
            mime: "image/png",
            width: Some(1280),
            height: Some(720),
            size_bytes: 186_368,
            filename: "shot.png",
            number: 1,
            cell_aspect: 0.5,
            copy: english(),
        }
    }

    fn area(w: u16, h: u16) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        }
    }

    #[test]
    fn text_card_sits_at_the_bottom_and_keeps_the_path_in_the_footer() {
        let plan = plan_overlay(
            area(80, 20),
            &facts(PreviewProtocol::None, Some("/tmp/a.png")),
        )
        .unwrap();
        assert!(!plan.show_pixels);
        assert_eq!(plan.rect.width, 60);
        assert_eq!(plan.rect.height, 6);
        assert_eq!(plan.rect.x, 10);
        assert_eq!(plan.rect.y, 14);
        assert!(plan.path_footer.unwrap().starts_with("Path: /tmp/a.png"));
        assert!(plan.body.iter().all(|line| !line.starts_with("Path:")));
        assert_eq!(plan.body[0], "Format: PNG");
        assert_eq!(plan.body[1], "Dimensions: 1280×720");
        assert_eq!(plan.body[2], "Size: 182 KB");
    }

    #[test]
    fn a_terminal_without_graphics_never_says_pending_and_never_draws_pixels() {
        let mut pending = facts(PreviewProtocol::None, None);
        pending.pixels_pending = true;
        pending.pixels_ready = false;
        let plan = plan_overlay(area(80, 20), &pending).unwrap();
        assert!(!plan.show_pixels);
        assert!(plan.path_footer.is_none());
        assert!(plan.body.iter().all(|line| line != "Preview pending"));
        assert!(plan.body.iter().any(|line| line == "Size: 182 KB"));
    }

    #[test]
    fn pending_is_only_when_the_protocol_can_draw_and_pixels_are_not_ready() {
        let mut pending = facts(PreviewProtocol::Kitty, None);
        pending.pixels_pending = true;
        let plan = plan_overlay(area(80, 20), &pending).unwrap();
        assert!(!plan.show_pixels);
        assert!(plan.body.iter().any(|line| line == "Preview pending"));
        assert!(plan.body.iter().all(|line| !line.starts_with("Size:")));
    }

    #[test]
    fn a_failed_decode_says_unavailable_even_when_kitty_is_available() {
        let mut failed = facts(PreviewProtocol::Kitty, Some("/tmp/a.png"));
        failed.decode_failed = true;
        failed.pixels_ready = true;
        let plan = plan_overlay(area(80, 20), &failed).unwrap();
        assert!(!plan.show_pixels);
        assert!(plan.body.iter().any(|line| line == "Preview unavailable"));
    }

    #[test]
    fn pixels_require_both_a_protocol_and_a_ready_image() {
        let mut ready = facts(PreviewProtocol::Kitty, None);
        ready.pixels_ready = true;
        let plan = plan_overlay(area(100, 40), &ready).unwrap();
        assert!(plan.show_pixels);
        assert!(plan.body.is_empty());
        assert!(plan.path_footer.is_none());

        ready.protocol = PreviewProtocol::None;
        let plan = plan_overlay(area(100, 40), &ready).unwrap();
        assert!(!plan.show_pixels);
    }

    #[test]
    fn the_card_is_omitted_when_the_area_cannot_hold_it() {
        assert!(plan_overlay(area(27, 20), &facts(PreviewProtocol::None, None)).is_none());
        assert!(plan_overlay(area(40, 5), &facts(PreviewProtocol::None, None)).is_none());
        let mut pixels = facts(PreviewProtocol::Kitty, None);
        pixels.pixels_ready = true;
        assert!(
            plan_overlay(area(40, 7), &pixels).is_none(),
            "a pixel card needs 8 rows, and does not fall back to text in that space"
        );
        assert!(plan_overlay(area(40, 7), &facts(PreviewProtocol::None, None)).is_some());
    }

    #[test]
    fn a_wide_image_is_centered_and_at_least_the_pixel_minimum() {
        let mut wide = facts(PreviewProtocol::Kitty, None);
        wide.pixels_ready = true;
        wide.width = Some(1000);
        wide.height = Some(100);
        let plan = plan_overlay(area(80, 30), &wide).unwrap();
        assert!(plan.show_pixels);
        assert!(plan.rect.height >= MIN_PIXEL_HEIGHT);
        assert_eq!(
            plan.rect.y,
            (30 - plan.rect.height) / 2,
            "pixel cards are centered, not pinned to the bottom"
        );
        let place = plan.placement.expect("a pixel card places the image");
        let (cols, rows) = fit_cells(1000, 100, 78, 28, 0.5);
        let inner_w = plan.rect.width - 2;
        let inner_h = plan.rect.height - 2;
        assert_eq!((place.cols, place.rows), (cols, rows));
        assert!(
            rows < inner_h,
            "the minimum card is taller than this image; the image must not stretch to the chrome"
        );
        assert_eq!(place.x, plan.rect.x + 1 + (inner_w - cols) / 2);
        assert_eq!(place.y, plan.rect.y + 1 + (inner_h - rows) / 2);
        assert!(
            matches!(preview_resize(), ratatui_image::Resize::Scale(_)),
            "Fit will not grow a bitmap past its own pixels"
        );
    }

    #[test]
    fn a_ready_image_fills_the_fitted_cells() {
        let mut shot = facts(PreviewProtocol::Kitty, Some("/tmp/cl-preview-ws/shot.png"));
        shot.pixels_ready = true;
        shot.width = Some(480);
        shot.height = Some(270);
        let plan = plan_overlay(area(200, 40), &shot).unwrap();
        let footer = u16::from(plan.path_footer.is_some());
        let (cols, rows) = fit_cells(480, 270, 198, 40 - 2 - footer, 0.5);
        let place = plan.placement.expect("pixels");
        assert_eq!((place.cols, place.rows), (cols, rows));
        let inner_w = plan.rect.width - 2;
        let inner_h = plan.rect.height - 2 - footer;
        assert_eq!(
            (place.cols, place.rows),
            (inner_w, inner_h),
            "the card is the fitted image plus chrome, not the whole conversation"
        );
        assert_eq!(place.x, plan.rect.x + 1);
        assert_eq!(place.y, plan.rect.y + 1);
        assert!(plan.rect.width < 200);
        assert!(plan.rect.x > 0, "the card is centered in the conversation");
    }

    #[test]
    fn a_long_path_keeps_its_head_and_tail() {
        let path = format!("/var/folders/dq/{}shot.png", "m".repeat(80));
        let line = path_line("Path", &path, 28);
        assert!(line.starts_with("Path: /var/"));
        assert!(line.ends_with("shot.png"));
        assert!(line.contains("..."));
        assert!(!line.contains(&"m".repeat(80)));
        assert!(UnicodeWidthStr::width(line.as_str()) <= 28);
    }

    #[test]
    fn missing_dimensions_drop_that_line_and_an_unknown_mime_is_kept() {
        let mut bare = facts(PreviewProtocol::None, None);
        bare.width = None;
        bare.height = None;
        bare.mime = "image/heic";
        bare.filename = "";
        let plan = plan_overlay(area(80, 20), &bare).unwrap();
        assert_eq!(plan.body[0], "Format: image/heic");
        assert!(
            plan.body
                .iter()
                .all(|line| !line.starts_with("Dimensions:"))
        );
        assert!(!plan.meta.contains('×'));
    }

    #[test]
    fn cursor_on_the_chip_or_its_right_edge_beats_hover_and_a_fresh_insert() {
        let tokens = [(0..10, 1), (12..22, 2)];
        assert_eq!(
            preview_index(&tokens, 4, 4, Some(1), Some((1, 30)), 2),
            Some(0),
            "the chip under the cursor wins over the pointer"
        );
        assert_eq!(preview_index(&tokens, 10, 10, None, None, 2), Some(0));
        assert_eq!(
            preview_index(&tokens, 11, 11, None, None, 2),
            None,
            "the space after the chip is not the chip"
        );
        assert_eq!(preview_index(&tokens, 11, 11, Some(1), None, 2), Some(1));
        assert_eq!(
            preview_index(&tokens, 30, 30, None, Some((1, 30)), 2),
            Some(1)
        );
        assert_eq!(
            preview_index(&tokens, 31, 31, None, Some((1, 30)), 2),
            None,
            "moving the cursor drops the just-inserted preview"
        );
    }

    #[test]
    fn a_chip_hit_stops_at_the_closing_bracket() {
        let buffer = "[图片 #1] next";
        let token = buffer.find(']').unwrap() + 1;
        let hits = chip_hits(
            buffer,
            &[0..token],
            &[ChipSlice {
                start_byte: 0,
                end_byte: buffer.len(),
                prompt_cols: 2,
                row: 4,
            }],
            4,
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits[0].x1 - hits[0].x0,
            UnicodeWidthStr::width(&buffer[..token]) as u16
        );
        assert_eq!(hits[0].row, 4);
    }

    #[test]
    fn graphics_follow_the_terminal_and_stop_for_windows_and_tmux() {
        let kitty = ProtocolFacts {
            windows: false,
            tmux: false,
            brand: PreviewBrand::Kitty,
            term_features: None,
            ssh: false,
        };
        assert_eq!(protocol_for(kitty), PreviewProtocol::Kitty);
        assert_eq!(
            protocol_for(ProtocolFacts {
                brand: PreviewBrand::Ghostty,
                ..kitty
            }),
            PreviewProtocol::Kitty
        );
        assert_eq!(
            protocol_for(ProtocolFacts {
                brand: PreviewBrand::WezTerm,
                ..kitty
            }),
            PreviewProtocol::Kitty
        );
        assert_eq!(
            protocol_for(ProtocolFacts {
                brand: PreviewBrand::Warp,
                ..kitty
            }),
            PreviewProtocol::Kitty
        );
        assert_eq!(
            protocol_for(ProtocolFacts {
                windows: true,
                brand: PreviewBrand::Ghostty,
                ..kitty
            }),
            PreviewProtocol::None
        );
        assert_eq!(
            protocol_for(ProtocolFacts {
                tmux: true,
                ..kitty
            }),
            PreviewProtocol::None
        );
        assert!(!session_is_tmux(false, "screen"));
        assert!(session_is_tmux(false, "tmux-256color"));
        assert!(session_is_tmux(true, "xterm-ghostty"));

        let iterm = ProtocolFacts {
            brand: PreviewBrand::ITerm2,
            ..kitty
        };
        assert_eq!(protocol_for(iterm), PreviewProtocol::None);
        assert_eq!(
            protocol_for(ProtocolFacts {
                term_features: Some("T"),
                ssh: true,
                ..iterm
            }),
            PreviewProtocol::None
        );
        assert_eq!(
            protocol_for(ProtocolFacts {
                term_features: Some("FT"),
                ..iterm
            }),
            PreviewProtocol::ITerm2
        );
        assert_eq!(
            protocol_for(ProtocolFacts { ssh: true, ..iterm }),
            PreviewProtocol::ITerm2
        );
    }

    #[test]
    fn brand_detection_prefers_the_program_name() {
        assert_eq!(
            brand_from(TerminalSignals {
                term_program: Some("iTerm.app"),
                term: Some("xterm-ghostty"),
                wezterm_version: None,
                iterm_session: false,
                lc_terminal: None,
                kitty_window: false,
            }),
            PreviewBrand::ITerm2
        );
        assert_eq!(
            brand_from(TerminalSignals {
                term_program: None,
                term: Some("xterm"),
                wezterm_version: Some("20240203"),
                iterm_session: false,
                lc_terminal: None,
                kitty_window: false,
            }),
            PreviewBrand::WezTerm
        );
        assert_eq!(
            brand_from(TerminalSignals {
                term_program: None,
                term: Some("xterm-ghostty"),
                wezterm_version: None,
                iterm_session: false,
                lc_terminal: None,
                kitty_window: false,
            }),
            PreviewBrand::Ghostty
        );
        assert_eq!(
            brand_from(TerminalSignals {
                term_program: Some("WarpTerminal"),
                term: None,
                wezterm_version: None,
                iterm_session: false,
                lc_terminal: None,
                kitty_window: false,
            }),
            PreviewBrand::Warp
        );
    }

    #[test]
    fn replacing_the_cached_image_keeps_the_queued_delete() {
        let mut state = test_state();
        state.image_preview_cache = Some(PreviewCache {
            sha256: "old".into(),
            outcome: Outcome::Absent,
            placed: None,
            pending_cleanup: vec!["OLD".into()],
        });
        let shown = ShownImage {
            number: 1,
            sha256: "abc".into(),
            preview_path: None,
            source_path: None,
            mime: "image/png".into(),
            name: "shot.png".into(),
            width: Some(1280),
            height: Some(720),
            size_bytes: 186_368,
        };
        let (ready, failed) = load_outcome(&mut state, &shown, PreviewProtocol::Kitty);
        assert!(!ready);
        assert!(!failed);
        assert_eq!(take_cleanup(&mut state), vec!["OLD".to_string()]);
    }

    #[test]
    fn a_preview_path_must_be_the_media_files_own_name() {
        let media = Path::new("/tmp/state/media");
        assert!(preview_file(media, "abc", "/tmp/state/media/abc.png").is_some());
        assert!(preview_file(media, "abc", "/tmp/evil.png").is_none());
        assert!(preview_file(media, "abc", "/tmp/state/media/other.png").is_none());
        assert!(preview_file(media, "abc", "/etc/passwd").is_none());
    }

    fn test_state() -> AppState {
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
                locale: crate::i18n::Locale::Zh,
                untrusted_config: Vec::new(),
                model_notice: None,
                thinking: None,
            },
        )
    }

    fn image(path: Option<&str>) -> AttachmentRef {
        AttachmentRef {
            id: AttachmentId::new("a1"),
            kind: AttachmentKind::Image,
            name: "shot.png".into(),
            mime_type: "image/png".into(),
            size_bytes: 186_368,
            sha256: "abc".into(),
            width: Some(1280),
            height: Some(720),
            source_path: path.map(str::to_string),
            preview_path: None,
        }
    }

    #[test]
    fn inserting_an_image_previews_it_without_putting_the_path_in_the_sentence() {
        let mut state = test_state();
        let path = format!("/var/folders/dq/{}shot.png", "m".repeat(80));
        reduce(
            &mut state,
            Action::Runtime(RuntimeEvent::AttachmentAdded {
                command_id: None,
                attachment: image(Some(&path)),
            }),
        );
        assert_eq!(state.composer.text(), "[图片 #1] ");
        assert!(!state.composer.text().contains("folders"));
        assert_eq!(selected_image(&state), Some(0));
        assert!(state.image_post_insert.is_some());

        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::empty())),
        );
        assert_eq!(selected_image(&state), None);
        assert!(!state.composer.text().contains(&path));

        state.image_post_insert = Some((0, state.composer.cursor()));
        assert_eq!(selected_image(&state), Some(0));
        reduce(
            &mut state,
            Action::Mouse(MouseEvent {
                kind: MouseEventKind::Moved,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::empty(),
            }),
        );
        assert!(state.image_post_insert.is_none());
        assert_eq!(selected_image(&state), None);

        state.image_chip_hits = vec![ChipHit {
            index: 0,
            row: 3,
            x0: 4,
            x1: 13,
        }];
        reduce(
            &mut state,
            Action::Mouse(MouseEvent {
                kind: MouseEventKind::Moved,
                column: 5,
                row: 3,
                modifiers: KeyModifiers::empty(),
            }),
        );
        assert_eq!(state.image_hover, Some(0));
        assert_eq!(selected_image(&state), Some(0));
    }

    #[test]
    fn the_text_card_paints_above_the_composer_and_not_into_the_sentence() {
        let mut state = test_state();
        let path = format!("/var/folders/dq/{}shot.png", "m".repeat(80));
        reduce(
            &mut state,
            Action::Runtime(RuntimeEvent::AttachmentAdded {
                command_id: None,
                attachment: image(Some(&path)),
            }),
        );
        state.conv.rect = Some((0, 0, 80, 20));
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| paint_with(frame, &mut state, PreviewProtocol::None, Some(0.5)))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut painted = String::new();
        for y in 0..buf.area.height {
            let mut x = 0u16;
            while x < buf.area.width {
                let sym = buf.cell((x, y)).map(|cell| cell.symbol()).unwrap_or(" ");
                painted.push_str(sym);
                x += UnicodeWidthStr::width(sym).max(1) as u16;
            }
            painted.push('\n');
        }
        assert!(painted.contains("图片 #1"), "{painted}");
        assert!(painted.contains("格式:"), "{painted}");
        assert!(painted.contains("尺寸:"), "{painted}");
        assert!(painted.contains("大小:"), "{painted}");
        assert!(painted.contains("路径:"), "{painted}");
        assert!(painted.contains("..."), "{painted}");
        assert!(painted.contains("shot.png"), "{painted}");
        assert!(!painted.contains(&path), "{painted}");
        assert_eq!(state.composer.text(), "[图片 #1] ");
        assert!(
            !painted.contains("Preview pending"),
            "a text terminal does not announce a pixel preview"
        );
    }

    fn image_with(sha: &str, path: Option<&str>) -> AttachmentRef {
        let mut attachment = image(path);
        attachment.sha256 = sha.into();
        attachment.id = AttachmentId::new(sha);
        attachment
    }

    fn send_attached(state: &mut AppState, attachment: AttachmentRef) {
        send_attached_note(state, attachment, None);
    }

    fn send_attached_note(state: &mut AppState, attachment: AttachmentRef, note: Option<char>) {
        state.vision = true;
        state.status = leveler_client_protocol::RuntimeStatus::Idle;
        reduce(
            state,
            Action::Runtime(RuntimeEvent::AttachmentAdded {
                command_id: None,
                attachment,
            }),
        );
        if let Some(note) = note {
            reduce(
                state,
                Action::Key(KeyEvent::new(KeyCode::Char(note), KeyModifiers::empty())),
            );
        }
        reduce(
            state,
            Action::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty())),
        );
    }

    fn publish_current(state: &mut AppState) {
        let width = 80usize;
        let height = 30usize;
        state.conv.rect = Some((0, 2, width as u16, height as u16));
        let (lines, _, _, _, spans, _, _) = state.conversation_build(width);
        let scroll = crate::conversation::geometry::effective_scroll(
            state.conv.scroll,
            state.conv.auto_scroll,
            lines.len(),
            height,
        );
        publish_sent_chip_hits(
            state,
            &spans,
            scroll,
            Rect {
                x: 0,
                y: 2,
                width: 80,
                height: 30,
            },
            lines.len(),
            height,
        );
    }

    fn hover_sent(state: &mut AppState, item: usize) {
        let hit = state
            .sent_image_hits
            .iter()
            .find(|hit| hit.item == item)
            .expect("sent chip")
            .clone();
        reduce(
            state,
            Action::Mouse(MouseEvent {
                kind: MouseEventKind::Moved,
                column: hit.x0,
                row: hit.row,
                modifiers: KeyModifiers::empty(),
            }),
        );
    }

    #[test]
    fn a_sent_chip_previews_when_the_pointer_is_on_it() {
        let mut state = test_state();
        let path = format!("/var/folders/dq/{}shot.png", "m".repeat(40));
        send_attached(&mut state, image(Some(&path)));
        assert!(state.pending_attachments.is_empty());
        assert!(state.composer.text().is_empty());
        let shown_text = state
            .transcript
            .items()
            .iter()
            .find_map(|item| match item {
                crate::transcript::TranscriptItem::User(text) => Some(text.clone()),
                _ => None,
            })
            .expect("user row");
        assert!(shown_text.contains("[图片 #1]"), "{shown_text}");
        assert!(!shown_text.contains("folders"), "{shown_text}");
        publish_current(&mut state);
        let item = state
            .sent_images
            .iter()
            .find(|sent| sent.text.contains("[图片 #1]"))
            .expect("the sent row keeps its image")
            .item;
        let hit = state
            .sent_image_hits
            .iter()
            .find(|hit| hit.item == item)
            .expect("the chip is hoverable");
        assert_eq!(hit.x0, 3, "the gutter is not part of the chip");
        assert_eq!(hit.x1 - hit.x0, 9);
        hover_sent(&mut state, item);
        let shown = shown_image(&state).expect("hover previews the sent image");
        assert_eq!(shown.sha256, "abc");
        assert_eq!(shown.source_path.as_deref(), Some(path.as_str()));
        assert_eq!(shown.number, 1);
        state.workbench_focus = WorkbenchFocus::Conversation;
        assert!(preview_open(&state));
    }

    #[test]
    fn each_sent_row_previews_its_own_image() {
        let mut state = test_state();
        // Identical chip text would collapse into the one user row the
        // transcript already dedupes. The note is what makes them two rows.
        send_attached_note(&mut state, image_with("aaa", Some("/tmp/a.png")), Some('a'));
        send_attached_note(&mut state, image_with("bbb", Some("/tmp/b.png")), Some('b'));
        publish_current(&mut state);
        let first = state.sent_images[0].item;
        let second = state.sent_images[1].item;
        hover_sent(&mut state, first);
        assert_eq!(shown_image(&state).expect("first").sha256, "aaa");
        hover_sent(&mut state, second);
        assert_eq!(shown_image(&state).expect("second").sha256, "bbb");
    }

    #[test]
    fn the_composer_chip_wins_over_a_sent_chip_under_the_pointer() {
        let mut state = test_state();
        send_attached(&mut state, image_with("sent", Some("/tmp/sent.png")));
        publish_current(&mut state);
        let item = state.sent_images[0].item;
        state.vision = true;
        state.status = leveler_client_protocol::RuntimeStatus::Idle;
        reduce(
            &mut state,
            Action::Runtime(RuntimeEvent::AttachmentAdded {
                command_id: None,
                attachment: image_with("draft", Some("/tmp/draft.png")),
            }),
        );
        state.sent_image_hover = Some((item, 0));
        assert_eq!(
            shown_image(&state).map(|shown| shown.sha256),
            Some("draft".into()),
            "a just-inserted composer chip wins over the sent row"
        );
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::empty())),
        );
        hover_sent(&mut state, item);
        assert_eq!(
            shown_image(&state).map(|shown| shown.sha256),
            Some("sent".into())
        );
        state.image_chip_hits = vec![ChipHit {
            index: 0,
            row: 20,
            x0: 4,
            x1: 13,
        }];
        reduce(
            &mut state,
            Action::Mouse(MouseEvent {
                kind: MouseEventKind::Moved,
                column: 5,
                row: 20,
                modifiers: KeyModifiers::empty(),
            }),
        );
        assert_eq!(
            shown_image(&state).map(|shown| shown.sha256),
            Some("draft".into())
        );
    }

    #[test]
    fn rebuilding_the_transcript_drops_the_sent_preview() {
        let mut state = test_state();
        send_attached(&mut state, image(Some("/tmp/shot.png")));
        assert!(!state.sent_images.is_empty());
        forget_sent_images(&mut state);
        publish_current(&mut state);
        assert!(state.sent_image_hits.is_empty());
        assert!(shown_image(&state).is_none());
    }
}
