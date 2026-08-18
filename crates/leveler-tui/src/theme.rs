//! Semantic color tokens. Components consume roles (`theme.text.primary`),
//! never raw RGB / ANSI / `DIM`, so a palette swap cannot silently lose contrast.
//!
//! Named themes paint an opaque canvas. Body text is a real token, not
//! `Color::Reset`, so readability does not depend on the terminal profile.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::Block;

/// Stable theme identifiers selectable via `/theme` and stored in config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThemeId {
    /// Detect terminal polarity, then load Dark or Light. Never Reset.
    Auto,
    /// Opaque dark palette.
    Dark,
    /// Opaque light palette.
    Light,
    /// Opaque high-contrast dark palette.
    HighContrast,
}

impl ThemeId {
    /// The theme a user who never chose one gets.
    ///
    /// Auto picks Dark or Light from the environment and still paints an
    /// opaque CodeLeveler surface. It does not inherit the terminal's colors.
    pub const DEFAULT: ThemeId = ThemeId::Auto;

    /// All named themes, in cycle / picker order (default first).
    pub const ALL: [ThemeId; 4] = [
        ThemeId::Auto,
        ThemeId::Dark,
        ThemeId::Light,
        ThemeId::HighContrast,
    ];

    /// Palettes that always have a fixed polarity (contrast tests use these).
    pub const FIXED: [ThemeId; 3] = [ThemeId::Dark, ThemeId::Light, ThemeId::HighContrast];

    /// Wire / config / slash value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Dark => "dark",
            Self::Light => "light",
            Self::HighContrast => "high-contrast",
        }
    }

    /// Parse a stored or CLI wire value (case-insensitive).
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "dark" => Some(Self::Dark),
            "light" => Some(Self::Light),
            "high-contrast" | "hc" => Some(Self::HighContrast),
            _ => None,
        }
    }

    /// Next theme in the cycle (for bare `/theme` confirm-after-down).
    pub fn cycle_next(self) -> Self {
        match self {
            Self::Auto => Self::Dark,
            Self::Dark => Self::Light,
            Self::Light => Self::HighContrast,
            Self::HighContrast => Self::Auto,
        }
    }
}

impl std::fmt::Display for ThemeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Application surfaces. Every reading area sits on one of these, never on
/// the terminal's default background.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfaceColors {
    pub canvas: Color,
    pub panel: Color,
    pub elevated: Color,
    pub input: Color,
    pub selection: Color,
}

/// Text roles. Hierarchy is a different *color*, not `Modifier::DIM`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextColors {
    pub primary: Color,
    pub secondary: Color,
    pub muted: Color,
    pub disabled: Color,
    pub inverse: Color,
}

/// Chrome outlines. `normal` and `focus` are contrast-gated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BorderColors {
    pub subtle: Color,
    pub normal: Color,
    pub strong: Color,
    pub focus: Color,
}

/// Brand / interactive emphasis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccentColors {
    pub primary: Color,
    pub secondary: Color,
    pub subtle: Color,
}

/// Status semantics (icons and short labels — not whole blocks).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusColors {
    pub success: Color,
    pub warning: Color,
    pub error: Color,
    pub info: Color,
    pub running: Color,
}

/// Diff / edit highlighting. Markers (`+`/`-`) carry meaning alongside color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffColors {
    pub added: Color,
    pub removed: Color,
    pub modified: Color,
    pub context: Color,
    pub added_bg: Color,
    pub removed_bg: Color,
}

/// A semantic theme. Components reference tokens, never literals.
#[derive(Debug, Clone)]
pub struct Theme {
    /// Which named id this is (Auto stays Auto even after polarity is resolved).
    pub id: ThemeId,
    /// True when every role is terminal-default (`NO_COLOR`).
    pub monochrome: bool,
    pub surface: SurfaceColors,
    pub text: TextColors,
    pub border: BorderColors,
    pub accent: AccentColors,
    pub status: StatusColors,
    pub diff: DiffColors,
}

const fn rgb(hex: u32) -> Color {
    Color::Rgb(
        ((hex >> 16) & 0xff) as u8,
        ((hex >> 8) & 0xff) as u8,
        (hex & 0xff) as u8,
    )
}

impl Theme {
    /// Opaque dark palette (coding-agent default look).
    pub fn dark() -> Self {
        Self {
            id: ThemeId::Dark,
            monochrome: false,
            surface: SurfaceColors {
                canvas: rgb(0x111315),
                panel: rgb(0x171A1E),
                elevated: rgb(0x1D2126),
                input: rgb(0x15181C),
                selection: rgb(0x253345),
            },
            text: TextColors {
                primary: rgb(0xE7E9EC),
                secondary: rgb(0xB5BAC1),
                muted: rgb(0x858C95),
                disabled: rgb(0x636A73),
                inverse: rgb(0x111315),
            },
            border: BorderColors {
                subtle: rgb(0x292E35),
                // #3A414A misses the 3:1 border gate on this canvas; lifted.
                normal: rgb(0x5C6672),
                strong: rgb(0x8A94A0),
                focus: rgb(0x4EA1FF),
            },
            accent: AccentColors {
                primary: rgb(0x4EA1FF),
                secondary: rgb(0x78B7FF),
                subtle: rgb(0x1A3050),
            },
            status: StatusColors {
                success: rgb(0x56C271),
                warning: rgb(0xE5B454),
                error: rgb(0xE56B6F),
                info: rgb(0x65A9F3),
                running: rgb(0x4EA1FF),
            },
            diff: DiffColors {
                added: rgb(0x56C271),
                removed: rgb(0xE56B6F),
                modified: rgb(0xE5B454),
                context: rgb(0xB5BAC1),
                added_bg: rgb(0x14301C),
                removed_bg: rgb(0x3A181A),
            },
        }
    }

    /// Opaque light palette (independent of Dark, not an invert).
    pub fn light() -> Self {
        Self {
            id: ThemeId::Light,
            monochrome: false,
            surface: SurfaceColors {
                canvas: rgb(0xF7F8FA),
                panel: rgb(0xFFFFFF),
                elevated: rgb(0xF0F2F5),
                input: rgb(0xFFFFFF),
                selection: rgb(0xDCEBFF),
            },
            text: TextColors {
                primary: rgb(0x20242A),
                secondary: rgb(0x4D5560),
                // Suggested #6F7884 misses 4.5:1 on this canvas; darkened.
                muted: rgb(0x5C6570),
                disabled: rgb(0x7A828C),
                inverse: rgb(0xFFFFFF),
            },
            border: BorderColors {
                subtle: rgb(0xE2E5E9),
                // Suggested #C9CED5 misses the 3:1 border gate; darkened.
                normal: rgb(0x7A828C),
                strong: rgb(0x5A626C),
                focus: rgb(0x0969DA),
            },
            accent: AccentColors {
                primary: rgb(0x0969DA),
                secondary: rgb(0x3B82D0),
                subtle: rgb(0xDCEBFF),
            },
            status: StatusColors {
                success: rgb(0x1A7F37),
                warning: rgb(0x9A6700),
                error: rgb(0xCF222E),
                info: rgb(0x0969DA),
                running: rgb(0x0969DA),
            },
            diff: DiffColors {
                added: rgb(0x1A7F37),
                removed: rgb(0xCF222E),
                modified: rgb(0x9A6700),
                context: rgb(0x4D5560),
                added_bg: rgb(0xE6F4EA),
                removed_bg: rgb(0xFCE8E8),
            },
        }
    }

    /// Opaque high-contrast dark palette.
    pub fn high_contrast() -> Self {
        Self {
            id: ThemeId::HighContrast,
            monochrome: false,
            surface: SurfaceColors {
                canvas: rgb(0x000000),
                panel: rgb(0x000000),
                elevated: rgb(0x0A0A0A),
                input: rgb(0x000000),
                selection: rgb(0x003366),
            },
            text: TextColors {
                primary: rgb(0xFFFFFF),
                secondary: rgb(0xE8E8E8),
                muted: rgb(0xC8C8C8),
                disabled: rgb(0xA0A0A0),
                inverse: rgb(0x000000),
            },
            border: BorderColors {
                subtle: rgb(0x555555),
                normal: rgb(0xAAAAAA),
                strong: rgb(0xDDDDDD),
                focus: rgb(0xFFFF00),
            },
            accent: AccentColors {
                primary: rgb(0x66B3FF),
                secondary: rgb(0x99CCFF),
                subtle: rgb(0x003366),
            },
            status: StatusColors {
                success: rgb(0x3DDA6B),
                warning: rgb(0xFFD24A),
                error: rgb(0xFF6B6B),
                info: rgb(0x66B3FF),
                running: rgb(0x66B3FF),
            },
            diff: DiffColors {
                added: rgb(0x3DDA6B),
                removed: rgb(0xFF6B6B),
                modified: rgb(0xFFD24A),
                context: rgb(0xC8C8C8),
                added_bg: rgb(0x002200),
                removed_bg: rgb(0x330000),
            },
        }
    }

    /// Palette for a named id (never monochrome). Auto resolves polarity but
    /// keeps `id == Auto`.
    pub fn named(id: ThemeId) -> Self {
        match id {
            ThemeId::Auto => {
                let mut t = if detect_dark_terminal() {
                    Self::dark()
                } else {
                    Self::light()
                };
                t.id = ThemeId::Auto;
                t
            }
            ThemeId::Dark => Self::dark(),
            ThemeId::Light => Self::light(),
            ThemeId::HighContrast => Self::high_contrast(),
        }
    }

    /// A no-color theme: every role is terminal-default (`NO_COLOR`).
    pub fn no_color() -> Self {
        let reset = SurfaceColors {
            canvas: Color::Reset,
            panel: Color::Reset,
            elevated: Color::Reset,
            input: Color::Reset,
            selection: Color::Reset,
        };
        Self {
            id: ThemeId::DEFAULT,
            monochrome: true,
            surface: reset,
            text: TextColors {
                primary: Color::Reset,
                secondary: Color::Reset,
                muted: Color::Reset,
                disabled: Color::Reset,
                inverse: Color::Reset,
            },
            border: BorderColors {
                subtle: Color::Reset,
                normal: Color::Reset,
                strong: Color::Reset,
                focus: Color::Reset,
            },
            accent: AccentColors {
                primary: Color::Reset,
                secondary: Color::Reset,
                subtle: Color::Reset,
            },
            status: StatusColors {
                success: Color::Reset,
                warning: Color::Reset,
                error: Color::Reset,
                info: Color::Reset,
                running: Color::Reset,
            },
            diff: DiffColors {
                added: Color::Reset,
                removed: Color::Reset,
                modified: Color::Reset,
                context: Color::Reset,
                added_bg: Color::Reset,
                removed_bg: Color::Reset,
            },
        }
    }

    /// Monochrome palette that remembers the user's preferred named id.
    pub fn monochrome_with_id(id: ThemeId) -> Self {
        let mut t = Self::no_color();
        t.id = id;
        t
    }

    /// Resolve a theme id, honouring `NO_COLOR` (monochrome wins over any id).
    pub fn resolve(id: ThemeId, no_color: bool) -> Self {
        if no_color {
            Self::monochrome_with_id(id)
        } else {
            Self::named(id)
        }
    }

    /// Resolve from a config/slash string. Unknown values fall back to Auto.
    pub fn resolve_str(raw: &str, no_color: bool) -> Self {
        Self::resolve(ThemeId::parse(raw).unwrap_or(ThemeId::DEFAULT), no_color)
    }

    /// Whether `NO_COLOR` is set in the environment.
    pub fn env_no_color() -> bool {
        leveler_core::environment().var_os("NO_COLOR").is_some()
    }

    /// Whether this palette is monochrome (no meaningful color).
    pub fn is_monochrome(&self) -> bool {
        self.monochrome
    }

    /// Whether the resolved surface is dark (Auto uses the resolved canvas).
    pub fn is_dark(&self) -> bool {
        if self.monochrome {
            return false;
        }
        match relative_luminance(self.surface.canvas) {
            Some(l) => l < 0.5,
            None => true,
        }
    }

    /// Fill `area` with the canvas so the terminal background cannot show through.
    pub fn paint_canvas(&self, frame: &mut Frame, area: Rect) {
        self.paint_surface(frame, area, self.surface.canvas);
    }

    /// Fill `area` with an explicit surface token (input, elevated, …).
    pub fn paint_surface(&self, frame: &mut Frame, area: Rect, bg: Color) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        if self.monochrome {
            frame.render_widget(ratatui::widgets::Clear, area);
            return;
        }
        frame.render_widget(
            Block::default().style(Style::default().bg(bg).fg(self.text.primary)),
            area,
        );
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::named(ThemeId::DEFAULT)
    }
}

/// Parse `COLORFGBG` (`fg;bg`, 0–15). `None` if the value is unusable.
pub fn is_dark_colorfgbg(raw: &str) -> Option<bool> {
    let bg = raw.rsplit(';').next()?.trim();
    let n: u8 = bg.parse().ok()?;
    // Conventional: 0–7 dark ANSI, 8–15 bright / light.
    Some(n < 8)
}

/// Detect a dark terminal. Unrecognised environments default to dark.
pub fn detect_dark_terminal() -> bool {
    leveler_core::environment()
        .var("COLORFGBG")
        .as_deref()
        .and_then(is_dark_colorfgbg)
        .unwrap_or(true)
}

/// WCAG relative luminance for an RGB color. `Reset` / named ANSI → `None`.
pub fn relative_luminance(color: Color) -> Option<f64> {
    let (r, g, b) = match color {
        Color::Rgb(r, g, b) => (r, g, b),
        _ => return None,
    };
    Some(0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b))
}

/// WCAG contrast ratio between two RGB colors.
pub fn contrast_ratio(a: Color, b: Color) -> Option<f64> {
    let la = relative_luminance(a)?;
    let lb = relative_luminance(b)?;
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    Some((hi + 0.05) / (lo + 0.05))
}

fn linear(channel: u8) -> f64 {
    let c = f64::from(channel) / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Plain-text theme preview used by `leveler theme preview` and tests.
pub fn preview_theme(theme: &Theme, color: bool) -> String {
    let title = format!("THEME PREVIEW  ({})", theme.id.as_str());
    let rows = [
        ("Typography", None),
        ("Primary text", Some(theme.text.primary)),
        ("Secondary text", Some(theme.text.secondary)),
        ("Muted text", Some(theme.text.muted)),
        ("Disabled text", Some(theme.text.disabled)),
        ("Status", None),
        ("● Running", Some(theme.status.running)),
        ("✓ Success", Some(theme.status.success)),
        ("! Warning", Some(theme.status.warning)),
        ("✕ Error", Some(theme.status.error)),
        ("● Info", Some(theme.status.info)),
        ("Tools", None),
        ("⟳ Reading src/main.rs", Some(theme.text.secondary)),
        ("✓ cargo test", Some(theme.status.success)),
        ("✕ cargo clippy", Some(theme.status.error)),
        ("Input", None),
        ("> Ask CodeLeveler...", Some(theme.accent.primary)),
        ("Diff", None),
        ("+ added line", Some(theme.diff.added)),
        ("- removed line", Some(theme.diff.removed)),
        ("  context line", Some(theme.diff.context)),
        ("Borders", None),
        ("subtle", Some(theme.border.subtle)),
        ("normal", Some(theme.border.normal)),
        ("strong", Some(theme.border.strong)),
        ("focus", Some(theme.border.focus)),
    ];

    let mut out = String::new();
    out.push_str(&title);
    out.push('\n');
    for (label, fg) in rows {
        if fg.is_none() {
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(label);
            out.push('\n');
            out.push_str("──────────────────\n");
            continue;
        }
        if color && let Some(Color::Rgb(r, g, b)) = fg {
            out.push_str(&format!("\x1b[38;2;{r};{g};{b}m{label}\x1b[0m\n"));
            continue;
        }
        out.push_str(label);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_min(fg: Color, bg: Color, min: f64, label: &str) {
        let ratio = contrast_ratio(fg, bg).unwrap_or(0.0);
        assert!(
            ratio + 1e-6 >= min,
            "{label}: contrast {ratio:.2} < {min:.1} (fg={fg:?} bg={bg:?})"
        );
    }

    /// Only pairs that the TUI actually paints. `surface.panel` is never a
    /// background today; `diff.added_bg` / `removed_bg` are unused — diffs are
    /// foreground-only on canvas — so those are not gated.
    fn assert_used_contrast_pairs(theme: &Theme) {
        let s = &theme.surface;
        let t = &theme.text;
        assert_min(t.primary, s.canvas, 7.0, "primary/canvas");
        assert_min(t.primary, s.input, 7.0, "primary/input");
        assert_min(t.primary, s.elevated, 7.0, "primary/elevated");
        assert_min(t.primary, s.selection, 4.5, "primary/selection");
        assert_min(t.secondary, s.canvas, 4.5, "secondary/canvas");
        assert_min(t.muted, s.canvas, 4.5, "muted/canvas");
        assert_min(t.muted, s.input, 4.5, "muted/input");
        assert_min(theme.status.success, s.canvas, 4.5, "success/canvas");
        assert_min(theme.status.warning, s.canvas, 4.5, "warning/canvas");
        assert_min(theme.status.error, s.canvas, 4.5, "error/canvas");
        assert_min(theme.status.info, s.canvas, 4.5, "info/canvas");
        assert_min(theme.status.running, s.canvas, 4.5, "running/canvas");
        assert_min(theme.diff.added, s.canvas, 4.5, "diff.added/canvas");
        assert_min(theme.diff.removed, s.canvas, 4.5, "diff.removed/canvas");
        assert_min(theme.border.normal, s.canvas, 3.0, "border.normal/canvas");
        assert_min(theme.border.focus, s.input, 3.0, "border.focus/input");
    }

    #[test]
    fn named_themes_are_not_monochrome_and_own_their_surface() {
        for id in ThemeId::FIXED {
            let t = Theme::named(id);
            assert!(!t.is_monochrome(), "{id}");
            assert_ne!(t.surface.canvas, Color::Reset, "{id} canvas");
            assert_ne!(t.text.primary, Color::Reset, "{id} primary");
            assert_ne!(t.text.muted, Color::Reset, "{id} muted");
            assert_ne!(t.accent.primary, Color::Reset, "{id} accent");
            assert_eq!(t.id, id);
        }
    }

    #[test]
    fn dark_contrast_gate() {
        assert_used_contrast_pairs(&Theme::dark());
    }

    #[test]
    fn light_contrast_gate() {
        assert_used_contrast_pairs(&Theme::light());
    }

    #[test]
    fn high_contrast_gate() {
        assert_used_contrast_pairs(&Theme::high_contrast());
    }

    #[test]
    fn no_color_override_wins() {
        for id in ThemeId::ALL {
            let t = Theme::resolve(id, true);
            assert!(t.is_monochrome(), "{id}");
            assert_eq!(t.accent.primary, Color::Reset);
            assert_eq!(t.text.primary, Color::Reset);
            assert_eq!(t.surface.canvas, Color::Reset);
            assert_eq!(t.id, id, "preference id retained under NO_COLOR");
        }
    }

    #[test]
    fn parse_only_the_current_ids() {
        assert_eq!(ThemeId::parse("AUTO"), Some(ThemeId::Auto));
        assert_eq!(ThemeId::parse("dark"), Some(ThemeId::Dark));
        assert_eq!(ThemeId::parse("light"), Some(ThemeId::Light));
        assert_eq!(ThemeId::parse("high-contrast"), Some(ThemeId::HighContrast));
        assert_eq!(ThemeId::parse("hc"), Some(ThemeId::HighContrast));
        assert_eq!(ThemeId::parse("nope"), None);
        // Retired names are not themes anymore.
        assert_eq!(ThemeId::parse("ion"), None);
        assert_eq!(ThemeId::parse("night"), None);
        assert_eq!(ThemeId::parse("day"), None);
        assert_eq!(ThemeId::Auto.cycle_next(), ThemeId::Dark);
        assert_eq!(ThemeId::Dark.cycle_next(), ThemeId::Light);
        assert_eq!(ThemeId::Light.cycle_next(), ThemeId::HighContrast);
        assert_eq!(ThemeId::HighContrast.cycle_next(), ThemeId::Auto);
    }

    #[test]
    fn resolve_str_unknown_falls_back_to_the_default() {
        let t = Theme::resolve_str("unknown-theme", false);
        assert_eq!(t.id, ThemeId::DEFAULT);
        assert!(!t.is_monochrome());
    }

    #[test]
    fn every_fallback_resolves_to_one_explicit_default() {
        assert_eq!(ThemeId::DEFAULT, ThemeId::Auto);
        assert_eq!(Theme::no_color().id, ThemeId::DEFAULT);
        assert_eq!(
            Theme::resolve_str("unknown-theme", false).id,
            ThemeId::DEFAULT
        );
        assert_eq!(ThemeId::ALL[0], ThemeId::DEFAULT);
    }

    #[test]
    fn body_text_is_an_owned_token_not_terminal_reset() {
        for id in ThemeId::FIXED {
            let t = Theme::named(id);
            assert_ne!(t.text.primary, Color::Reset, "{id} body text");
        }
    }

    #[test]
    fn colorfgbg_detection() {
        assert_eq!(is_dark_colorfgbg("15;0"), Some(true));
        assert_eq!(is_dark_colorfgbg("0;15"), Some(false));
        assert_eq!(is_dark_colorfgbg("not-a-number"), None);
    }

    #[test]
    fn preview_lists_required_roles() {
        let text = preview_theme(&Theme::dark(), false);
        for needle in [
            "THEME PREVIEW",
            "Primary text",
            "Secondary text",
            "Muted text",
            "Disabled text",
            "● Running",
            "✓ Success",
            "! Warning",
            "✕ Error",
            "Reading src/main.rs",
            "Ask CodeLeveler",
            "+ added line",
            "- removed line",
            "focus",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in {text}");
        }
    }

    #[test]
    fn contrast_math_known_pair() {
        // White on black is 21:1.
        let ratio = contrast_ratio(Color::Rgb(255, 255, 255), Color::Rgb(0, 0, 0)).unwrap();
        assert!((ratio - 21.0).abs() < 0.05, "{ratio}");
    }
}
