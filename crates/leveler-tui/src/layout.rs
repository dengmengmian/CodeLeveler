//! Workspace spacing tokens. Conversation, Input, and Footer inset through
//! these — never a one-off `+ 2` at a call site.

use ratatui::layout::Rect;

/// Horizontal safe margin on each side of Conversation, Input, and Footer.
pub const WORKSPACE_GUTTER_X: u16 = 2;

/// Conversation viewport breathing room above the first visible line.
pub const CONVERSATION_PADDING_TOP: u16 = 1;

/// Conversation viewport breathing room below the last visible line.
pub const CONVERSATION_PADDING_BOTTOM: u16 = 1;

/// Cells between the input box border and the prompt / trailing chrome.
pub const INPUT_INTERNAL_PADDING_X: u16 = 1;

/// Blank rows under the footer strip so Context is not flush on the edge.
pub const FOOTER_BOTTOM_PADDING: u16 = 1;

/// Horizontal inset: `x + gutter`, `width - 2*gutter`. Never underflows.
pub fn horizontal_inset(outer: Rect, gutter: u16) -> Rect {
    let width = outer.width.saturating_sub(gutter.saturating_mul(2));
    let x = if width == 0 {
        outer.x
    } else {
        outer.x.saturating_add(gutter)
    };
    Rect {
        x,
        y: outer.y,
        width,
        height: outer.height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn horizontal_inset_is_two_cells_each_side() {
        let outer = Rect {
            x: 10,
            y: 3,
            width: 100,
            height: 5,
        };
        let inner = horizontal_inset(outer, WORKSPACE_GUTTER_X);
        assert_eq!(inner.x, 12);
        assert_eq!(inner.width, 96);
        assert_eq!(inner.y, 3);
        assert_eq!(inner.height, 5);
    }

    #[test]
    fn horizontal_inset_narrow_widths_do_not_underflow() {
        for w in [0u16, 1, 2, 3, 4] {
            let outer = Rect {
                x: 3,
                y: 1,
                width: w,
                height: 2,
            };
            let inner = horizontal_inset(outer, WORKSPACE_GUTTER_X);
            let outer_right = outer.x.saturating_add(outer.width);
            let inner_right = inner.x.saturating_add(inner.width);
            assert!(
                inner_right <= outer_right || w == 0,
                "w={w}: inner {inner:?} overflows outer {outer:?}"
            );
        }
    }
}
