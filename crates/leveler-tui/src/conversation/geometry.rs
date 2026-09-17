//! Authoritative Conversation geometry.
//!
//! Single source for every layout fact the viewport, the renderer, and the
//! mouse path share: content width, viewport height, live-edge scroll,
//! bottom-alignment padding, and the screen→content mapping. The renderer and
//! the reducer both call THESE functions; neither keeps its own copy of a
//! formula.

use ratatui::layout::Rect;

use crate::state::AppState;

/// Horizontal safe margin on each side of the Conversation viewport.
/// Same token as the rest of the workbench (`WORKSPACE_GUTTER_X`).
pub const GUTTER_X: u16 = crate::layout::WORKSPACE_GUTTER_X;
pub const PADDING_TOP: u16 = crate::layout::CONVERSATION_PADDING_TOP;
pub const PADDING_BOTTOM: u16 = crate::layout::CONVERSATION_PADDING_BOTTOM;

/// Scrollable content rect inside the Conversation outer slot.
/// Horizontal [`GUTTER_X`] plus one row above and below — wrap, paint, and
/// hit-testing all use this same rect.
pub fn content_rect(outer: Rect) -> Rect {
    let inner = crate::layout::horizontal_inset(outer, GUTTER_X);
    let height = inner
        .height
        .saturating_sub(PADDING_TOP.saturating_add(PADDING_BOTTOM));
    let y = if height == 0 {
        inner.y
    } else {
        inner.y.saturating_add(PADDING_TOP)
    };
    Rect {
        x: inner.x,
        y,
        width: inner.width,
        height,
    }
}

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
        .unwrap_or_else(|| {
            state
                .size
                .0
                .saturating_sub(GUTTER_X.saturating_mul(2))
                .max(1) as usize
        })
}

/// Viewport height in rows, from the authoritative painted rect. The
/// rows-minus-chrome estimate only applies before the first render.
pub fn viewport_height(state: &AppState) -> usize {
    if let Some((_, _, _, rh)) = state.conv.rect {
        rh.max(1) as usize
    } else {
        state
            .size
            .1
            .saturating_sub(12)
            .saturating_sub(PADDING_TOP.saturating_add(PADDING_BOTTOM))
            .max(3) as usize
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

/// Blank rows the renderer actually paints above the content this frame.
/// Policy lives HERE, not in the viewport: an empty session's splash is
/// vertically centered (half the padding above), a normal short transcript
/// is bottom-aligned (all of it above), and a full viewport has none. The
/// painter and `screen_to_content` both consume this — if they ever used
/// different padding, painted rows and mapped rows would disagree.
pub fn painted_top_padding(state: &AppState, total: usize, height: usize) -> usize {
    let pad = top_padding(total, height);
    if crate::splash::conversation_is_empty(state) {
        pad / 2
    } else {
        pad
    }
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
        crate::conversation::build::conversation_line_count(state, width)
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
    let pad = painted_top_padding(state, total, height);
    let abs_row = (scroll + viewport_row.saturating_sub(pad)).min(total.saturating_sub(1));
    let abs_col = (clamped_col - rx) as usize;
    Some(crate::selection::TextPos {
        row: abs_row,
        col: abs_col,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_client_protocol::SessionId;

    fn test_state() -> AppState {
        let mut s = AppState::new(
            crate::theme::Theme::no_color(),
            crate::state::Boot {
                session_id: SessionId::new("s1"),
                user: "u".into(),
                version: "0.1.0".into(),
                show_welcome: false,
                draft_path: None,
                history_path: None,
                context_window: 200_000,
                locale: crate::i18n::Locale::Zh,
                untrusted_config: Vec::new(),
                reasoning_effort: None,
            },
        );
        s.size = (80, 40);
        s.conv.rect = Some((0, 2, 80, 20));
        s.conv.auto_scroll = false;
        s.conv.scroll = 0;
        s
    }

    /// The paint↔interaction parity contract: for every painted content row,
    /// the screen cell it was painted at must map back to that same row.
    fn assert_roundtrip(s: &AppState, label: &str) {
        let (_, ry, rw, rh) = s.conv.rect.unwrap();
        let total = crate::conversation::build::conversation_line_count(s, rw as usize);
        let height = rh as usize;
        let pad = painted_top_padding(s, total, height);
        let scroll = effective_scroll(s.conv.scroll, s.conv.auto_scroll, total, height);
        for vis in 0..height.min(total.saturating_sub(scroll)) {
            let content_row = scroll + vis;
            let screen_row = ry + (pad + vis) as u16;
            let mapped = screen_to_content(s, 1, screen_row)
                .unwrap_or_else(|| panic!("{label}: row {screen_row} unmapped"));
            assert_eq!(
                mapped.row, content_row,
                "{label}: painted content row {content_row} at screen row {screen_row} \
                 must map back to itself"
            );
        }
    }

    #[test]
    fn content_rect_insets_two_cells_each_side() {
        let outer = Rect {
            x: 10,
            y: 5,
            width: 100,
            height: 20,
        };
        let inner = content_rect(outer);
        assert_eq!(inner.x, 12);
        assert_eq!(inner.width, 96);
        assert_eq!(inner.y, 6);
        assert_eq!(inner.height, 18);
    }

    #[test]
    fn content_rect_applies_vertical_safe_area() {
        let outer = Rect {
            x: 10,
            y: 5,
            width: 100,
            height: 20,
        };
        let inner = content_rect(outer);
        assert_eq!(inner.y, outer.y + PADDING_TOP);
        assert_eq!(inner.height, outer.height - PADDING_TOP - PADDING_BOTTOM);
    }

    #[test]
    fn content_rect_narrow_widths_do_not_underflow() {
        for w in [0u16, 1, 2, 3, 4] {
            let outer = Rect {
                x: 3,
                y: 1,
                width: w,
                height: 10,
            };
            let inner = content_rect(outer);
            let outer_right = outer.x.saturating_add(outer.width);
            let inner_right = inner.x.saturating_add(inner.width);
            assert!(
                inner_right <= outer_right || w == 0,
                "w={w}: inner {inner:?} overflows outer {outer:?}"
            );
        }
        for h in [0u16, 1, 2, 3, 4] {
            let outer = Rect {
                x: 3,
                y: 1,
                width: 20,
                height: h,
            };
            let inner = content_rect(outer);
            let outer_bottom = outer.y.saturating_add(outer.height);
            let inner_bottom = inner.y.saturating_add(inner.height);
            assert!(
                inner_bottom <= outer_bottom || h == 0,
                "h={h}: inner {inner:?} overflows outer {outer:?}"
            );
        }
    }

    #[test]
    fn short_transcript_paint_and_mapping_agree() {
        let mut s = test_state();
        for i in 0..5 {
            s.transcript.push_user(format!("row {i}"));
        }
        assert_roundtrip(&s, "short bottom-aligned");
    }

    #[test]
    fn empty_splash_paint_and_mapping_agree() {
        // The splash is CENTERED (pad/2 above), unlike the bottom-aligned
        // normal short transcript — the mapping must follow the same policy.
        let s = test_state();
        assert!(crate::splash::conversation_is_empty(&s));
        assert_roundtrip(&s, "centered splash");
    }

    #[test]
    fn long_transcript_mapping_unchanged() {
        let mut s = test_state();
        for i in 0..60 {
            s.transcript.push_user(format!("long row {i}"));
        }
        s.conv.scroll = 17;
        assert_roundtrip(&s, "long scrolled");
    }
}
