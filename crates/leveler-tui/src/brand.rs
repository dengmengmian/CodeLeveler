//! CodeLeveler Level Mark — terminal-native brand geometry.
//!
//! Three sizes share one silhouette: right-aligned rise, notched terrace,
//! left foundation pier, forward execution bar. Splash owns layout and copy;
//! this module only supplies the mark and its semantic inks.

use ratatui::style::Style;
use ratatui::text::Span;
use unicode_width::UnicodeWidthStr;

use crate::theme::Theme;

/// Formal sizes of the Level Mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BrandMarkSize {
    /// Splash / large empty state. ~13×5.
    Master,
    /// Medium terminal brand column. ~8×4.
    Compact,
    /// Narrow lockup. ~5×2.
    Micro,
}

/// Discrete Level Mark inks. Geometry first; color is optional tint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BrandInk {
    Foundation,
    Primary,
    Highlight,
    Blank,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct BrandSpan {
    text: &'static str,
    ink: BrandInk,
}

pub(crate) type BrandRow = &'static [BrandSpan];

/// Styles resolved from [`Theme::brand`]. Never constructed from raw RGB.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BrandStyles {
    foundation: Style,
    primary: Style,
    highlight: Style,
}

impl BrandStyles {
    pub(crate) fn from_theme(theme: &Theme) -> Self {
        Self {
            foundation: Style::default().fg(theme.brand.foundation),
            primary: Style::default().fg(theme.brand.primary),
            highlight: Style::default().fg(theme.brand.highlight),
        }
    }

    fn ink(self, ink: BrandInk) -> Style {
        match ink {
            BrandInk::Foundation => self.foundation,
            BrandInk::Primary => self.primary,
            BrandInk::Highlight => self.highlight,
            BrandInk::Blank => Style::default(),
        }
    }
}

/// Selected production mark for `size`.
pub(crate) fn level_mark(size: BrandMarkSize) -> &'static [BrandRow] {
    match size {
        BrandMarkSize::Master => MARK_MASTER,
        BrandMarkSize::Compact => MARK_COMPACT,
        BrandMarkSize::Micro => MARK_MICRO,
    }
}

/// Display width of the widest row (all rows are padded to this).
pub(crate) fn level_mark_width(size: BrandMarkSize) -> usize {
    rows_width(level_mark(size))
}

pub(crate) fn paint_mark_row(row: BrandRow, styles: BrandStyles) -> Vec<Span<'static>> {
    row.iter()
        .map(|cell| Span::styled(cell.text.to_string(), styles.ink(cell.ink)))
        .collect()
}

#[cfg(test)]
pub(crate) fn mark_plain(size: BrandMarkSize) -> String {
    rows_plain(level_mark(size))
}

fn rows_width(rows: &[BrandRow]) -> usize {
    rows.iter()
        .map(|row| row.iter().map(|s| UnicodeWidthStr::width(s.text)).sum())
        .max()
        .unwrap_or(0)
}

#[cfg(test)]
fn rows_plain(rows: &[BrandRow]) -> String {
    rows.iter()
        .map(|row| row.iter().map(|s| s.text).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

const fn sp(text: &'static str, ink: BrandInk) -> BrandSpan {
    BrandSpan { text, ink }
}

// ---------------------------------------------------------------------------
// Selected mark — Candidate A "Cantilever"
//
// Refined from the geometric direction (not a paste of the sketch):
//   * 13-cell canvas so the forward bar has a reserved right column
//   * terrace inset one cell so the notch reads as a cut, not a stair
//   * last-row gap opened so the left pier and the runway stay distinct
// ---------------------------------------------------------------------------

const MARK_MASTER: &[BrandRow] = &[
    &[
        sp("        ", BrandInk::Blank),
        sp("██", BrandInk::Highlight),
        sp("   ", BrandInk::Blank),
    ],
    &[
        sp("      ", BrandInk::Blank),
        sp("████", BrandInk::Highlight),
        sp("   ", BrandInk::Blank),
    ],
    &[
        sp("    ", BrandInk::Blank),
        sp("██████", BrandInk::Primary),
        sp("   ", BrandInk::Blank),
    ],
    &[
        sp("  ", BrandInk::Blank),
        sp("████", BrandInk::Primary),
        sp("  ", BrandInk::Blank),
        sp("██", BrandInk::Primary),
        sp("   ", BrandInk::Blank),
    ],
    &[
        sp("██", BrandInk::Foundation),
        sp("      ", BrandInk::Blank),
        sp("█████", BrandInk::Foundation),
    ],
];

const MARK_COMPACT: &[BrandRow] = &[
    &[
        sp("     ", BrandInk::Blank),
        sp("██", BrandInk::Highlight),
        sp(" ", BrandInk::Blank),
    ],
    &[
        sp("   ", BrandInk::Blank),
        sp("████", BrandInk::Primary),
        sp(" ", BrandInk::Blank),
    ],
    &[
        sp(" ", BrandInk::Blank),
        sp("██", BrandInk::Primary),
        sp("  ", BrandInk::Blank),
        sp("██", BrandInk::Primary),
        sp(" ", BrandInk::Blank),
    ],
    &[
        sp("██", BrandInk::Foundation),
        sp("  ", BrandInk::Blank),
        sp("████", BrandInk::Foundation),
    ],
];

const MARK_MICRO: &[BrandRow] = &[
    &[
        sp("  ", BrandInk::Blank),
        sp("██", BrandInk::Highlight),
        sp(" ", BrandInk::Blank),
    ],
    &[
        sp("█", BrandInk::Foundation),
        sp(" ", BrandInk::Blank),
        sp("███", BrandInk::Primary),
    ],
];

#[cfg(test)]
pub(crate) fn candidate_plain(id: char) -> String {
    match id {
        'A' => rows_plain(CANDIDATE_A),
        'B' => rows_plain(CANDIDATE_B),
        'C' => rows_plain(CANDIDATE_C),
        _ => String::new(),
    }
}

/// Candidate A — Cantilever (selected). Same geometry as [`MARK_MASTER`].
#[cfg(test)]
const CANDIDATE_A: &[BrandRow] = MARK_MASTER;

/// Candidate B — Spine Rise. Tighter notch, more mass on the right spine.
#[cfg(test)]
const CANDIDATE_B: &[BrandRow] = &[
    &[sp("       ██  ", BrandInk::Highlight)],
    &[sp("     ████  ", BrandInk::Highlight)],
    &[sp("   ██████  ", BrandInk::Primary)],
    &[sp(" ████ ███  ", BrandInk::Primary)],
    &[sp("██    █████", BrandInk::Foundation)],
];

/// Candidate C — Tall Wedge. Extra row of progression, wider canvas.
#[cfg(test)]
const CANDIDATE_C: &[BrandRow] = &[
    &[sp("         ██   ", BrandInk::Highlight)],
    &[sp("       ████   ", BrandInk::Highlight)],
    &[sp("     ██████   ", BrandInk::Primary)],
    &[sp("   ████  ██   ", BrandInk::Primary)],
    &[sp(" ██      ███  ", BrandInk::Foundation)],
    &[sp("██       █████", BrandInk::Foundation)],
];

#[cfg(test)]
mod tests {
    use super::*;

    fn row_text(row: BrandRow) -> String {
        row.iter().map(|s| s.text).collect()
    }

    fn assert_aligned(rows: &[BrandRow], min_w: usize, max_w: usize, min_h: usize, max_h: usize) {
        let w = rows_width(rows);
        assert!(
            (min_w..=max_w).contains(&w),
            "width {w} not in {min_w}..={max_w}\n{}",
            rows_plain(rows)
        );
        assert!(
            (min_h..=max_h).contains(&rows.len()),
            "height {} not in {min_h}..={max_h}",
            rows.len()
        );
        for row in rows {
            let row_w: usize = row.iter().map(|s| UnicodeWidthStr::width(s.text)).sum();
            assert_eq!(row_w, w, "ragged row {row_w} != {w}: {:?}", row_text(row));
            for cell in *row {
                for ch in cell.text.chars() {
                    let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                    assert_eq!(cw, 1, "non-cell glyph {ch:?} width {cw}");
                    assert!(
                        ch == ' ' || ch == '█',
                        "Level Mark uses only space and full block, got {ch:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn selected_master_is_candidate_a_not_the_raw_sketch() {
        let plain = mark_plain(BrandMarkSize::Master);
        let sketch = "\
       ██
     ████
   ██████
 ████  ██
██     █████";
        assert_ne!(
            plain.replace(' ', "."),
            sketch.replace(' ', "."),
            "production mark must be a designed refinement, not the sketch paste"
        );
        assert_eq!(
            plain,
            [
                "        ██   ",
                "      ████   ",
                "    ██████   ",
                "  ████  ██   ",
                "██      █████",
            ]
            .join("\n")
        );
        assert_eq!(level_mark_width(BrandMarkSize::Master), 13);
        assert_eq!(level_mark(BrandMarkSize::Master).len(), 5);
    }

    #[test]
    fn compact_and_micro_share_cantilever_dna() {
        let compact = mark_plain(BrandMarkSize::Compact);
        assert_eq!(
            compact,
            ["     ██ ", "   ████ ", " ██  ██ ", "██  ████"].join("\n")
        );
        assert_eq!(level_mark_width(BrandMarkSize::Compact), 8);
        assert_eq!(level_mark(BrandMarkSize::Compact).len(), 4);

        let micro = mark_plain(BrandMarkSize::Micro);
        assert_eq!(micro, ["  ██ ", "█ ███"].join("\n"));
        assert_eq!(level_mark_width(BrandMarkSize::Micro), 5);
        assert_eq!(level_mark(BrandMarkSize::Micro).len(), 2);
    }

    #[test]
    fn all_sizes_are_display_aligned_and_sized() {
        assert_aligned(level_mark(BrandMarkSize::Master), 12, 16, 5, 7);
        assert_aligned(level_mark(BrandMarkSize::Compact), 6, 10, 3, 4);
        assert_aligned(level_mark(BrandMarkSize::Micro), 2, 6, 1, 2);
    }

    #[test]
    fn mark_has_apex_notch_pier_and_runway() {
        let rows: Vec<String> = mark_plain(BrandMarkSize::Master)
            .lines()
            .map(str::to_string)
            .collect();
        assert!(rows[0].trim_start().starts_with("██"), "apex: {rows:?}");
        assert!(rows[3].contains("████  ██"), "notched terrace: {rows:?}");
        assert!(
            rows[4].starts_with("██") && rows[4].ends_with("█████"),
            "pier + runway: {rows:?}"
        );
        let first_block = rows[0].find('█').unwrap();
        let last_block = rows[0].rfind('█').unwrap();
        assert!(
            first_block > 4 && last_block < 12,
            "apex should sit on the right half: {rows:?}"
        );
    }

    #[test]
    fn mark_is_not_a_letter_mascot_or_cl_monogram() {
        for size in [
            BrandMarkSize::Master,
            BrandMarkSize::Compact,
            BrandMarkSize::Micro,
        ] {
            let plain = mark_plain(size);
            for needle in ["CL", "C", "L", "◆", "•", "▿", "▀", "▄", "▟", "👻"] {
                assert!(
                    !plain.contains(needle),
                    "{size:?} still contains {needle:?}:\n{plain}"
                );
            }
        }
    }

    #[test]
    fn candidates_are_distinct_and_letterless() {
        let a = candidate_plain('A');
        let b = candidate_plain('B');
        let c = candidate_plain('C');
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(b, c);
        for (id, plain) in [('A', a), ('B', b), ('C', c)] {
            assert!(plain.contains('█'), "{id} empty");
            assert!(!plain.contains('C') && !plain.contains('L'), "{id} letters");
        }
        assert_eq!(candidate_plain('A'), mark_plain(BrandMarkSize::Master));
    }

    #[test]
    fn brand_styles_come_from_theme_tokens() {
        let theme = Theme::dark();
        let styles = BrandStyles::from_theme(&theme);
        let spans = paint_mark_row(level_mark(BrandMarkSize::Master)[0], styles);
        assert!(spans.iter().any(|s| s.content.contains('█')));
        assert_eq!(
            spans
                .iter()
                .find(|s| s.content.contains('█'))
                .and_then(|s| s.style.fg),
            Some(theme.brand.highlight)
        );
    }
}
