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

/// Indentation *inside* a Secondary/Detail page's content gutter. The shell
/// already insets the whole content rect by [`WORKSPACE_GUTTER_X`]; these name
/// the levels within a page so a Detail Page body never scatters magic spaces.
///
/// A section title sits at the gutter itself (`""`); the section's own body
/// sits at [`DETAIL_BODY_INDENT`]; a body's nested detail is indented once more
/// from there (the activity stream owns that rail via its own token).
pub const DETAIL_BODY_INDENT: &str = "  ";

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

/// Which pane currently owns the keyboard on a List+Detail / Split page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SplitFocus {
    #[default]
    Primary,
    Detail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitAxis {
    Horizontal,
    Vertical,
}

/// Sizing policy for the content rect of one page. The host still owns the
/// card and its chrome; this spec only divides the rect it hands the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SplitSpec {
    pub axis: SplitAxis,
    pub primary_pct: u16,
    pub primary_min: u16,
    pub detail_min: u16,
    /// Minimum content width for a horizontal split to be readable.
    pub min_dual_width: u16,
    /// Minimum content height for any split to be readable.
    pub min_dual_height: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SplitLayout {
    pub primary: Rect,
    /// One-cell separator between the panes. Zero-sized in single mode.
    pub divider: Rect,
    pub detail: Rect,
    pub dual: bool,
    pub axis: SplitAxis,
}

/// Divide a real Content Rect. Returns `None` when the two panes cannot both
/// meet their readable minimum; the page then uses [`single_pane`].
pub fn split_pane(area: Rect, spec: SplitSpec) -> Option<SplitLayout> {
    if area.width == 0 || area.height == 0 {
        return None;
    }
    match spec.axis {
        SplitAxis::Horizontal => {
            if area.width < spec.min_dual_width || area.height < spec.min_dual_height {
                return None;
            }
            let divider = 1u16;
            let usable = area.width.saturating_sub(divider);
            let detail_min = spec.detail_min.min(usable.saturating_sub(spec.primary_min));
            if usable < spec.primary_min.saturating_add(detail_min) {
                return None;
            }
            let mut primary = (u32::from(usable) * u32::from(spec.primary_pct) / 100) as u16;
            primary = primary
                .max(spec.primary_min)
                .min(usable.saturating_sub(detail_min));
            let detail = usable.saturating_sub(primary);
            Some(SplitLayout {
                primary: Rect {
                    width: primary,
                    ..area
                },
                divider: Rect {
                    x: area.x + primary,
                    y: area.y,
                    width: divider,
                    height: area.height,
                },
                detail: Rect {
                    x: area.x + primary + divider,
                    width: detail,
                    ..area
                },
                dual: true,
                axis: spec.axis,
            })
        }
        SplitAxis::Vertical => {
            if area.height < spec.min_dual_height {
                return None;
            }
            let divider = 1u16;
            let usable = area.height.saturating_sub(divider);
            let detail_min = spec.detail_min.min(usable.saturating_sub(spec.primary_min));
            if usable < spec.primary_min.saturating_add(detail_min) {
                return None;
            }
            let mut primary = (u32::from(usable) * u32::from(spec.primary_pct) / 100) as u16;
            primary = primary
                .max(spec.primary_min)
                .min(usable.saturating_sub(detail_min));
            let detail = usable.saturating_sub(primary);
            Some(SplitLayout {
                primary: Rect {
                    height: primary,
                    ..area
                },
                divider: Rect {
                    x: area.x,
                    y: area.y + primary,
                    width: area.width,
                    height: divider,
                },
                detail: Rect {
                    y: area.y + primary + divider,
                    height: detail,
                    ..area
                },
                dual: true,
                axis: spec.axis,
            })
        }
    }
}

/// A one-pane layout on the same content rect. `focus` selects which pane is
/// drawn; the other is zero-sized, so every page can keep one geometry path.
pub fn single_pane(area: Rect, focus: SplitFocus) -> SplitLayout {
    let empty = Rect {
        x: area.x,
        y: area.y,
        width: 0,
        height: 0,
    };
    SplitLayout {
        primary: if focus == SplitFocus::Primary {
            area
        } else {
            empty
        },
        divider: empty,
        detail: if focus == SplitFocus::Detail {
            area
        } else {
            empty
        },
        dual: false,
        axis: SplitAxis::Horizontal,
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

    fn rect(x: u16, y: u16, width: u16, height: u16) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    const H: SplitSpec = SplitSpec {
        axis: SplitAxis::Horizontal,
        primary_pct: 35,
        primary_min: 20,
        detail_min: 32,
        min_dual_width: 70,
        min_dual_height: 8,
    };

    #[test]
    fn a_wide_content_rect_splits_without_overlap() {
        let area = rect(3, 2, 100, 20);
        let split = split_pane(area, H).expect("wide");
        assert!(split.dual);
        assert!(split.primary.width >= 20);
        assert!(split.detail.width >= 32);
        assert_eq!(split.primary.x + split.primary.width, split.divider.x);
        assert_eq!(split.divider.x + 1, split.detail.x);
        assert_eq!(split.detail.x + split.detail.width, area.x + area.width);
        assert_eq!(split.divider.height, area.height);
    }

    #[test]
    fn a_narrow_content_rect_refuses_dual_and_single_pane_follows_focus() {
        let area = rect(0, 0, 60, 20);
        assert!(split_pane(area, H).is_none());
        let primary = single_pane(area, SplitFocus::Primary);
        assert_eq!(primary.primary, area);
        assert_eq!(primary.detail.width, 0);
        let detail = single_pane(area, SplitFocus::Detail);
        assert_eq!(detail.primary.width, 0);
        assert_eq!(detail.detail, area);
    }

    #[test]
    fn vertical_split_keeps_both_panes_inside_the_content_rect() {
        let area = rect(2, 1, 40, 18);
        let spec = SplitSpec {
            axis: SplitAxis::Vertical,
            primary_pct: 33,
            primary_min: 3,
            detail_min: 5,
            min_dual_width: 1,
            min_dual_height: 10,
        };
        let split = split_pane(area, spec).expect("tall");
        assert_eq!(split.primary.y + split.primary.height, split.divider.y);
        assert_eq!(split.divider.y + 1, split.detail.y);
        assert_eq!(split.detail.y + split.detail.height, area.y + area.height);
        assert!(split.primary.height >= 3 && split.detail.height >= 5);
    }

    #[test]
    fn split_is_never_negative_at_tiny_sizes() {
        for width in 0..=100 {
            for height in 0..=20 {
                let area = rect(0, 0, width, height);
                if let Some(split) = split_pane(area, H) {
                    assert!(split.primary.width > 0 && split.detail.width > 0);
                    assert!(split.primary.x + split.primary.width <= area.width as u16 + area.x);
                }
            }
        }
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
