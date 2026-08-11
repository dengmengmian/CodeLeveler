//! Authoritative Conversation geometry.
//!
//! Single source for every layout fact the viewport, the renderer, and the
//! mouse path share: content width, viewport height, live-edge scroll,
//! bottom-alignment padding, and the screen→content mapping. The renderer and
//! the reducer both call THESE functions; neither keeps its own copy of a
//! formula.

use crate::state::AppState;

/// Content width for layout, wrapping, and selection. Must match the painted
/// viewport (`conv.rect`), not the full terminal width — a mismatch re-wraps
/// lines under the cursor and hit positions "jump". The terminal-width
/// fallback only applies before the first render publishes a rect.
pub fn content_width(state: &AppState) -> usize {
    state
        .conv
        .rect
        .map(|(_, _, w, _)| w as usize)
        .filter(|w| *w > 0)
        .unwrap_or_else(|| state.size.0.max(1) as usize)
}

/// Viewport height in rows, from the authoritative painted rect. The
/// rows-minus-chrome estimate only applies before the first render.
pub fn viewport_height(state: &AppState) -> usize {
    if let Some((_, _, _, rh)) = state.conv.rect {
        rh.max(1) as usize
    } else {
        state.size.1.saturating_sub(12).max(3) as usize
    }
}

/// Highest valid scroll offset for `total` content lines in `height` rows.
pub fn max_scroll(total: usize, height: usize) -> usize {
    total.saturating_sub(height.max(1))
}

/// Blank filler rows painted ABOVE a transcript shorter than the viewport
/// (bottom alignment). Zero once content fills the screen.
pub fn top_padding(total: usize, height: usize) -> usize {
    height.saturating_sub(total)
}

/// The scroll offset actually painted this frame: the live edge while
/// auto-following, the pinned (clamped) offset otherwise.
pub fn effective_scroll(scroll: usize, auto_follow: bool, total: usize, height: usize) -> usize {
    let max = max_scroll(total, height);
    if auto_follow { max } else { scroll.min(max) }
}

/// Map a screen cell → absolute content coordinates, clamping to the
/// Conversation viewport. Used for clicks and while dragging a selection so
/// the endpoint tracks the pointer even in an edge hot zone or briefly
/// outside the rect.
pub fn screen_to_content(
    state: &AppState,
    col: u16,
    row: u16,
) -> Option<crate::selection::TextPos> {
    let (rx, ry, rw, rh) = state.conv.rect?;
    if rw == 0 || rh == 0 {
        return None;
    }
    let clamped_col = col.clamp(rx, rx.saturating_add(rw.saturating_sub(1)));
    let clamped_row = row.clamp(ry, ry.saturating_add(rh.saturating_sub(1)));
    let width = content_width(state);
    let height = rh as usize;
    let total = if state.conv.plain_width == width && !state.conv.plain.is_empty() {
        state.conv.plain.len()
    } else {
        crate::workbench::conversation_line_count(state, width)
    };
    // While selecting we always use the pinned scroll, never auto-follow
    // bottom (the drag anchor must not move under the pointer).
    let scroll = effective_scroll(
        state.conv.scroll,
        state.conv.auto_scroll && !state.conv.selection.dragging,
        total,
        height,
    );
    let viewport_row = (clamped_row - ry) as usize;
    let pad = top_padding(total, height);
    let abs_row = (scroll + viewport_row.saturating_sub(pad)).min(total.saturating_sub(1));
    let abs_col = (clamped_col - rx) as usize;
    Some(crate::selection::TextPos {
        row: abs_row,
        col: abs_col,
    })
}
