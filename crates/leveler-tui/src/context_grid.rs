//! Context Map geometry.
//!
//! The `/context` map is a fixed grid of equal cells, one cell per equal share
//! of the context window. This module owns only the projection from the
//! runtime's [`ContextAccounting`] snapshot to cell counts and cell glyphs — it
//! never estimates a token and never reads a transcript. The snapshot is the
//! single source of truth; the map is a drawing of it.

use leveler_model::ContextAccounting;

/// The map's shape: `cols * rows` equal cells. Kept small and stable so the map
/// reads at a glance instead of being a chart, and so it does not flicker
/// between shapes on a one-column resize.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridShape {
    pub cols: usize,
    pub rows: usize,
}

impl GridShape {
    pub const fn cells(self) -> usize {
        self.cols * self.rows
    }

    /// The shape for a terminal of `width` columns. Two buckets, no in-between:
    /// a map that changes shape as the terminal resizes by one cell is harder
    /// to read than one that stays put.
    pub fn for_width(width: usize) -> Self {
        if width < 64 {
            Self { cols: 10, rows: 5 }
        } else {
            Self { cols: 12, rows: 6 }
        }
    }
}

/// One participant in the map: a top-level category, or the free remainder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GridSlice {
    /// Stable category key (`messages`, `system`, …) or `free`.
    pub key: String,
    pub tokens: u64,
    /// The free remainder is the only slice that is not "used".
    pub free: bool,
}

/// The top-level slices of a snapshot in display order, plus free when the
/// window is known. Unknown windows have no free remainder to draw.
pub fn slices_from(acc: &ContextAccounting) -> Vec<GridSlice> {
    let mut out: Vec<GridSlice> = acc
        .categories
        .iter()
        .map(|c| GridSlice {
            key: c.name.clone(),
            tokens: c.tokens,
            free: false,
        })
        .collect();
    if let Some(free) = acc.free_tokens {
        out.push(GridSlice {
            key: "free".to_string(),
            tokens: free,
            free: true,
        });
    }
    out
}

/// Split `cells` cells across `slices` in proportion to their tokens.
///
/// Largest-remainder: every slice gets its floor share, then the leftover cells
/// go to the largest fractional parts, so the counts always sum to exactly
/// `cells` and never overshoot. Ties break on order (the snapshot is already
/// sorted by size), which keeps the map deterministic across redraws.
///
/// A slice with a token count but a rounding-to-zero share still competes for
/// the leftover cells; one is never silently dropped while `cells` allows it.
pub fn allocate(slices: &[GridSlice], cells: usize) -> Vec<usize> {
    let mut counts = vec![0usize; slices.len()];
    if cells == 0 || slices.is_empty() {
        return counts;
    }
    let total: u128 = slices.iter().map(|s| u128::from(s.tokens)).sum();
    if total == 0 {
        // Nothing to draw: the whole map is free when there is a free slice.
        if let Some(i) = slices.iter().position(|s| s.free) {
            counts[i] = cells;
        }
        return counts;
    }

    // floor(tokens * cells / total) and the remainder for the leftover pass.
    let mut remainders: Vec<(u128, usize)> = Vec::with_capacity(slices.len());
    let mut assigned = 0usize;
    for (i, slice) in slices.iter().enumerate() {
        let scaled = u128::from(slice.tokens) * cells as u128;
        let floor = (scaled / total) as usize;
        counts[i] = floor;
        assigned += floor;
        remainders.push((scaled % total, i));
    }
    // Highest remainder first; ties on the earlier slice.
    remainders.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut leftover = cells.saturating_sub(assigned);
    for (_, i) in remainders {
        if leftover == 0 {
            break;
        }
        counts[i] += 1;
        leftover -= 1;
    }
    counts
}

/// One drawn cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GridCell {
    /// Category key this cell belongs to (`free` for the remainder).
    pub key: String,
    pub free: bool,
    /// The cell sits exactly on the runtime's fold threshold, when one is
    /// known. A position marker on a free cell — not a category.
    pub fold_boundary: bool,
}

/// Lay `slices` out as `shape.cells()` cells in slice order.
///
/// `fold_at` is the runtime's fold threshold in tokens; the cell it lands on is
/// marked, so the map shows how much room is left before auto-compaction. It is
/// clamped away when it falls outside the drawn cells or inside the used
/// region (where pressure is already critical and the marker would imply
/// headroom that does not exist).
pub fn draw(
    slices: &[GridSlice],
    shape: GridShape,
    window: u64,
    fold_at: Option<u64>,
) -> Vec<GridCell> {
    let cells = shape.cells();
    let counts = allocate(slices, cells);

    let fold_cell = fold_at.and_then(|at| {
        if window == 0 || at == 0 || at >= window {
            return None;
        }
        Some((u128::from(at) * cells as u128 / u128::from(window)) as usize)
    });

    let mut out = Vec::with_capacity(cells);
    for (slice, count) in slices.iter().zip(counts) {
        for _ in 0..count {
            let index = out.len();
            out.push(GridCell {
                key: slice.key.clone(),
                free: slice.free,
                fold_boundary: slice.free && fold_cell == Some(index),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slice(key: &str, tokens: u64, free: bool) -> GridSlice {
        GridSlice {
            key: key.to_string(),
            tokens,
            free,
        }
    }

    #[test]
    fn allocation_always_fills_every_cell() {
        let slices = vec![
            slice("messages", 51_800, false),
            slice("tool_results", 18_600, false),
            slice("system", 7_200, false),
            slice("free", 24_900, true),
        ];
        for cells in [1usize, 7, 40, 60, 72, 100] {
            let counts = allocate(&slices, cells);
            assert_eq!(
                counts.iter().sum::<usize>(),
                cells,
                "cells={cells} counts={counts:?}"
            );
        }
    }

    #[test]
    fn allocation_is_proportional_to_tokens() {
        let slices = vec![
            slice("big", 75, false),
            slice("small", 25, false),
            slice("free", 0, true),
        ];
        let counts = allocate(&slices, 100);
        assert_eq!(counts, vec![75, 25, 0]);
    }

    #[test]
    fn leftover_cells_follow_the_largest_remainders() {
        // Largest remainder is the rule: a slice whose share rounds to zero
        // stays zero when another slice has the larger fractional part. The
        // important property is that the total is exact and the rule is stable.
        let slices = vec![
            slice("huge", 9_999, false),
            slice("speck", 1, false),
            slice("free", 0, true),
        ];
        let counts = allocate(&slices, 100);
        assert_eq!(counts.iter().sum::<usize>(), 100);
        assert_eq!(counts[2], 0);
        // A slice above the rest of the leftover field always wins one.
        let half = vec![slice("a", 994, false), slice("b", 6, false)];
        let half_counts = allocate(&half, 100);
        assert_eq!(half_counts.iter().sum::<usize>(), 100);
        assert_eq!(half_counts[1], 1, "6 of 1000 tokens is above half a cell");
    }

    #[test]
    fn allocation_is_deterministic_and_stable() {
        let slices = vec![
            slice("a", 1, false),
            slice("b", 1, false),
            slice("c", 1, false),
        ];
        let first = allocate(&slices, 10);
        for _ in 0..5 {
            assert_eq!(allocate(&slices, 10), first);
        }
        assert_eq!(first.iter().sum::<usize>(), 10);
    }

    #[test]
    fn empty_snapshot_paints_free() {
        let slices = vec![slice("free", 128_000, true)];
        assert_eq!(allocate(&slices, 72), vec![72]);
    }

    #[test]
    fn zero_tokens_with_no_free_slice_assigns_nothing() {
        let slices = vec![slice("messages", 0, false)];
        assert_eq!(allocate(&slices, 72), vec![0]);
    }

    #[test]
    fn draw_emits_exactly_one_cell_per_share() {
        let slices = vec![
            slice("messages", 64_000, false),
            slice("free", 64_000, true),
        ];
        let shape = GridShape { cols: 12, rows: 6 };
        let cells = draw(&slices, shape, 128_000, None);
        assert_eq!(cells.len(), 72);
        assert_eq!(cells.iter().filter(|c| c.free).count(), 36);
        assert!(cells.iter().all(|c| !c.fold_boundary));
    }

    #[test]
    fn fold_boundary_lands_on_the_free_side() {
        let slices = vec![slice("messages", 20, false), slice("free", 80, true)];
        let shape = GridShape { cols: 10, rows: 10 };
        let cells = draw(&slices, shape, 100, Some(25));
        let marked: Vec<usize> = cells
            .iter()
            .enumerate()
            .filter(|(_, c)| c.fold_boundary)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(marked, vec![25], "the marker sits at 25% of the window");
        assert!(cells[25].free);
    }

    #[test]
    fn fold_boundary_inside_used_region_is_not_drawn() {
        let slices = vec![slice("messages", 60, false), slice("free", 40, true)];
        let shape = GridShape { cols: 10, rows: 10 };
        let cells = draw(&slices, shape, 100, Some(50));
        assert!(
            cells.iter().all(|c| !c.fold_boundary),
            "a marker under the used fill would imply headroom that is gone"
        );
    }

    #[test]
    fn fold_boundary_past_the_window_is_not_drawn() {
        let slices = vec![slice("messages", 10, false), slice("free", 90, true)];
        let shape = GridShape { cols: 10, rows: 10 };
        let cells = draw(&slices, shape, 100, Some(200));
        assert!(cells.iter().all(|c| !c.fold_boundary));
    }

    #[test]
    fn shape_buckets_are_stable() {
        assert_eq!(GridShape::for_width(120).cells(), 72);
        assert_eq!(GridShape::for_width(80).cells(), 72);
        assert_eq!(GridShape::for_width(63).cells(), 50);
        assert_eq!(GridShape::for_width(40).cells(), 50);
    }
}
