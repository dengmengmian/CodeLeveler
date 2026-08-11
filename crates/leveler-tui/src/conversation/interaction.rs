//! Conversation interaction: what a screen cell MEANS, and the state ops
//! interactions perform (scroll, selection edge auto-scroll, plain cache).
//!
//! The reducer asks `hit_test` for the semantic target of a click and then
//! decides policy (toggle / open / select). It never re-derives which row is
//! a disclosure, where the bottom padding starts, or how the viewport is
//! scrolled — those facts live here and in `geometry`.

use crate::state::AppState;

use super::geometry;

/// Rows near the viewport edge that arm continuous scroll while selecting.
const SELECTION_EDGE_ROWS: u16 = 2;

/// The semantic target under a Conversation screen cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hit {
    /// A tool-disclosure header row → the transcript item to toggle.
    Disclosure { item: usize },
    /// A row containing an http(s) URL under the pointer. Carries the content
    /// position too: without a modifier the same cell may begin a selection.
    Url {
        url: String,
        pos: crate::selection::TextPos,
    },
    /// Plain content — selectable text.
    Text(crate::selection::TextPos),
    /// Not mappable (no rect yet / zero-sized viewport).
    Outside,
}

/// Resolve the semantic target at screen (col, row). Assumes `ensure_plain`
/// ran for the current width (URL detection reads the plain cache).
///
/// Precedence is part of the product contract: a disclosure row toggles even
/// if its text happens to contain a URL.
pub fn hit_test(state: &AppState, col: u16, row: u16) -> Hit {
    let Some(pos) = geometry::screen_to_content(state, col, row) else {
        return Hit::Outside;
    };
    if let Some(item) = state.disclosure_item_at(geometry::content_width(state), pos.row) {
        return Hit::Disclosure { item };
    }
    if let Some(url) = url_at(state, pos) {
        return Hit::Url { url, pos };
    }
    Hit::Text(pos)
}

/// The http(s) URL under display column `pos.col` of plain line `pos.row`.
pub fn url_at(state: &AppState, pos: crate::selection::TextPos) -> Option<String> {
    crate::url_link::url_at(state.conv.plain.get(pos.row)?, pos.col)
}

/// Rebuild the plain-text projection if it is missing or was built for a
/// different width. Backs selection extraction and URL hit-testing.
pub fn ensure_plain(state: &mut AppState) {
    let width = geometry::content_width(state);
    if state.conv.plain_width == width && !state.conv.plain.is_empty() {
        return;
    }
    let lines = state.conversation_lines(width);
    state.conv.plain = lines.iter().map(crate::selection::line_to_plain).collect();
    state.conv.plain_width = width;
}

/// Scroll by `delta` lines (negative = up). Scrolling up leaves auto-follow;
/// reaching the bottom re-enables it and clears the unread badge.
pub fn scroll_by(state: &mut AppState, delta: i32) {
    let height = geometry::viewport_height(state);
    let width = geometry::content_width(state);
    let total = crate::workbench::conversation_line_count(state, width);
    let max_scroll = geometry::max_scroll(total, height);
    if delta < 0 {
        state.conv.auto_scroll = false;
        state.conv.scroll = state
            .conv
            .scroll
            .saturating_sub((-delta) as usize)
            .min(max_scroll);
    } else {
        let next = (state.conv.scroll + delta as usize).min(max_scroll);
        state.conv.scroll = next;
        if next >= max_scroll {
            state.conv.auto_scroll = true;
            state.conv.unread = 0;
        }
    }
}

/// Scroll during text selection: never re-enable auto-follow (streaming must
/// not yank the viewport while the user is still choosing text).
pub fn scroll_pinned_by(state: &mut AppState, delta: i32) {
    if delta == 0 {
        return;
    }
    let height = geometry::viewport_height(state);
    let width = geometry::content_width(state);
    let total = crate::workbench::conversation_line_count(state, width);
    let max_scroll = geometry::max_scroll(total, height);
    state.conv.auto_scroll = false;
    if delta < 0 {
        state.conv.scroll = state
            .conv
            .scroll
            .saturating_sub((-delta) as usize)
            .min(max_scroll);
    } else {
        state.conv.scroll = (state.conv.scroll + delta as usize).min(max_scroll);
    }
}

/// Return the viewport to the live edge (auto-follow resumes).
pub fn jump_to_live_edge(state: &mut AppState) {
    state.conv.auto_scroll = true;
    state.conv.unread = 0;
    let height = geometry::viewport_height(state);
    let width = geometry::content_width(state);
    let total = crate::workbench::conversation_line_count(state, width);
    state.conv.scroll = geometry::max_scroll(total, height);
}

/// Accelerate scroll while the pointer stays in an edge hot zone.
pub fn edge_scroll_step(streak: u32) -> usize {
    match streak {
        0..=2 => 1,
        3..=6 => 2,
        7..=12 => 3,
        _ => 5,
    }
}

/// Top / bottom Conversation rows (or outside those edges) drive auto-scroll
/// while dragging a selection.
pub fn update_selection_edge(state: &mut AppState, col: u16, row: u16) {
    let Some((rx, ry, rw, rh)) = state.conv.rect else {
        state.conv.selection_edge_dir = 0;
        state.conv.selection_edge_streak = 0;
        return;
    };
    if rh == 0 {
        state.conv.selection_edge_dir = 0;
        return;
    }
    // Horizontally outside the conversation: stop edge scroll (still may clamp select).
    let in_x = col >= rx && col < rx.saturating_add(rw);
    if !in_x {
        state.conv.selection_edge_dir = 0;
        state.conv.selection_edge_streak = 0;
        return;
    }
    let edge = SELECTION_EDGE_ROWS.min(rh);
    let top_end = ry.saturating_add(edge.saturating_sub(1));
    let bottom_start = ry.saturating_add(rh.saturating_sub(edge));
    let dir = if row <= top_end || row < ry {
        -1
    } else if row >= bottom_start || row >= ry.saturating_add(rh) {
        1
    } else {
        0
    };
    if dir == 0 {
        state.conv.selection_edge_dir = 0;
        state.conv.selection_edge_streak = 0;
    } else if state.conv.selection_edge_dir != dir {
        state.conv.selection_edge_dir = dir;
        state.conv.selection_edge_streak = 0;
    }
}

/// Stop continuous edge scroll (button released / selection cleared).
pub fn clear_selection_drag(state: &mut AppState) {
    state.conv.selection_edge_dir = 0;
    state.conv.selection_edge_streak = 0;
    state.conv.selection_last_mouse = None;
}
