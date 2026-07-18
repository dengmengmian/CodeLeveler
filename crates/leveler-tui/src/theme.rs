//! Semantic color palette. Components reference roles (`accent`, `error`, …),
//! never raw colors, so a theme swap or `NO_COLOR` is a single change.

use ratatui::style::Color;

/// Stable theme identifiers selectable via `/theme` and stored in config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThemeId {
    /// Cool cyan brand palette (historical default "Ion").
    Ion,
    /// Deeper blue-tinted night palette.
    Night,
    /// Light palette for bright terminals.
    Day,
}

impl ThemeId {
    /// All named (non-monochrome) themes, in cycle order.
    pub const ALL: [ThemeId; 3] = [ThemeId::Ion, ThemeId::Night, ThemeId::Day];

    /// Wire / config / slash value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ion => "ion",
            Self::Night => "night",
            Self::Day => "day",
        }
    }

    /// Parse a stored or CLI wire value (case-insensitive). Aliases: `dark`→ion,
    /// `light`→day.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "ion" | "dark" => Some(Self::Ion),
            "night" => Some(Self::Night),
            "day" | "light" => Some(Self::Day),
            _ => None,
        }
    }

    /// Next theme in the cycle (for bare `/theme`).
    pub fn cycle_next(self) -> Self {
        match self {
            Self::Ion => Self::Night,
            Self::Night => Self::Day,
            Self::Day => Self::Ion,
        }
    }
}

impl std::fmt::Display for ThemeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A semantic theme. Every field is a role, not a literal used ad hoc.
#[derive(Debug, Clone)]
pub struct Theme {
    /// Which named palette this is (even when monochrome override is active,
    /// `id` records the user's preference so `/theme` can cycle correctly).
    pub id: ThemeId,
    /// True when every role is terminal-default (NO_COLOR / monochrome).
    pub monochrome: bool,
    pub text: Color,
    pub muted: Color,
    pub accent: Color,
    pub success: Color,
    pub warning: Color,
    pub error: Color,
    pub border: Color,
    pub user_message: Color,
    pub assistant_message: Color,
    pub tool: Color,
    pub diff_add: Color,
    pub diff_remove: Color,
    pub attachment: Color,
    /// Soft background for fenced code blocks (Reset = transparent / none).
    pub code_bg: Color,
}

impl Theme {
    /// Cool cyan brand palette (default).
    pub fn ion() -> Self {
        Self {
            id: ThemeId::Ion,
            monochrome: false,
            text: Color::Reset,
            muted: Color::Rgb(0x9A, 0xA3, 0xA7),
            accent: Color::Rgb(0x22, 0xD3, 0xEE),
            success: Color::Rgb(0x7D, 0xC8, 0x5F),
            warning: Color::Rgb(0xF5, 0x9E, 0x42),
            error: Color::Rgb(0xF2, 0x5F, 0x7A),
            border: Color::Rgb(0x62, 0x6B, 0x70),
            user_message: Color::Rgb(0xE6, 0xEA, 0xEC),
            assistant_message: Color::Reset,
            tool: Color::Rgb(0x67, 0xE8, 0xF9),
            diff_add: Color::Rgb(0x7D, 0xC8, 0x5F),
            diff_remove: Color::Rgb(0xF2, 0x5F, 0x7A),
            attachment: Color::Rgb(0xA7, 0x8B, 0xFA),
            code_bg: Color::Rgb(0x1A, 0x1F, 0x24),
        }
    }

    /// Historical alias for [`Self::ion`].
    pub fn dark() -> Self {
        Self::ion()
    }

    /// Deep blue-tinted night palette.
    pub fn night() -> Self {
        Self {
            id: ThemeId::Night,
            monochrome: false,
            text: Color::Rgb(0xC0, 0xCA, 0xF5),
            muted: Color::Rgb(0x56, 0x5F, 0x89),
            accent: Color::Rgb(0xBB, 0x9A, 0xF7),
            success: Color::Rgb(0x9E, 0xCE, 0x6A),
            warning: Color::Rgb(0xE0, 0xAF, 0x68),
            error: Color::Rgb(0xF7, 0x76, 0x8E),
            border: Color::Rgb(0x3B, 0x42, 0x61),
            user_message: Color::Rgb(0xC0, 0xCA, 0xF5),
            assistant_message: Color::Rgb(0xA9, 0xB1, 0xD6),
            tool: Color::Rgb(0x7D, 0xCF, 0xFF),
            diff_add: Color::Rgb(0x9E, 0xCE, 0x6A),
            diff_remove: Color::Rgb(0xF7, 0x76, 0x8E),
            attachment: Color::Rgb(0xFF, 0x9E, 0x64),
            code_bg: Color::Rgb(0x16, 0x16, 0x1E),
        }
    }

    /// Light theme for bright terminal backgrounds.
    pub fn day() -> Self {
        Self {
            id: ThemeId::Day,
            monochrome: false,
            text: Color::Rgb(0x1F, 0x23, 0x28),
            muted: Color::Rgb(0x6E, 0x77, 0x81),
            accent: Color::Rgb(0x05, 0x5D, 0x9C),
            success: Color::Rgb(0x1A, 0x7F, 0x37),
            warning: Color::Rgb(0x9A, 0x67, 0x00),
            error: Color::Rgb(0xCF, 0x22, 0x2E),
            border: Color::Rgb(0xD0, 0xD7, 0xDE),
            user_message: Color::Rgb(0x24, 0x29, 0x2F),
            assistant_message: Color::Rgb(0x1F, 0x23, 0x28),
            tool: Color::Rgb(0x05, 0x63, 0xA1),
            diff_add: Color::Rgb(0x1A, 0x7F, 0x37),
            diff_remove: Color::Rgb(0xCF, 0x22, 0x2E),
            attachment: Color::Rgb(0x82, 0x50, 0xDF),
            code_bg: Color::Rgb(0xF6, 0xF8, 0xFA),
        }
    }

    /// Palette for a named id (never monochrome).
    pub fn named(id: ThemeId) -> Self {
        match id {
            ThemeId::Ion => Self::ion(),
            ThemeId::Night => Self::night(),
            ThemeId::Day => Self::day(),
        }
    }

    /// A no-color theme: every role resolves to the terminal default so state is
    /// carried by symbols, not color (`NO_COLOR`).
    pub fn no_color() -> Self {
        Self {
            id: ThemeId::Ion,
            monochrome: true,
            text: Color::Reset,
            muted: Color::Reset,
            accent: Color::Reset,
            success: Color::Reset,
            warning: Color::Reset,
            error: Color::Reset,
            border: Color::Reset,
            user_message: Color::Reset,
            assistant_message: Color::Reset,
            tool: Color::Reset,
            diff_add: Color::Reset,
            diff_remove: Color::Reset,
            attachment: Color::Reset,
            code_bg: Color::Reset,
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

    /// Resolve from a config/slash string. Unknown values fall back to Ion.
    pub fn resolve_str(raw: &str, no_color: bool) -> Self {
        Self::resolve(ThemeId::parse(raw).unwrap_or(ThemeId::Ion), no_color)
    }

    /// Whether `NO_COLOR` is set in the environment.
    pub fn env_no_color() -> bool {
        leveler_core::environment().var_os("NO_COLOR").is_some()
    }

    /// Pick the theme from the environment only: `NO_COLOR` forces monochrome,
    /// otherwise Ion.
    pub fn from_env() -> Self {
        Self::resolve(ThemeId::Ion, Self::env_no_color())
    }

    /// Whether this palette is monochrome (no meaningful color).
    pub fn is_monochrome(&self) -> bool {
        self.monochrome
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::ion()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_themes_are_not_monochrome() {
        for id in ThemeId::ALL {
            let t = Theme::named(id);
            assert!(!t.is_monochrome(), "{id}");
            assert_ne!(t.accent, Color::Reset, "{id} accent should be colored");
            assert_eq!(t.id, id);
        }
    }

    #[test]
    fn no_color_override_wins() {
        for id in ThemeId::ALL {
            let t = Theme::resolve(id, true);
            assert!(t.is_monochrome(), "{id}");
            assert_eq!(t.accent, Color::Reset);
            assert_eq!(t.diff_add, Color::Reset);
            assert_eq!(t.id, id, "preference id retained under NO_COLOR");
        }
    }

    #[test]
    fn parse_aliases_and_cycle() {
        assert_eq!(ThemeId::parse("ION"), Some(ThemeId::Ion));
        assert_eq!(ThemeId::parse("dark"), Some(ThemeId::Ion));
        assert_eq!(ThemeId::parse("light"), Some(ThemeId::Day));
        assert_eq!(ThemeId::parse("night"), Some(ThemeId::Night));
        assert_eq!(ThemeId::parse("nope"), None);
        assert_eq!(ThemeId::Ion.cycle_next(), ThemeId::Night);
        assert_eq!(ThemeId::Night.cycle_next(), ThemeId::Day);
        assert_eq!(ThemeId::Day.cycle_next(), ThemeId::Ion);
    }

    #[test]
    fn resolve_str_unknown_falls_back_to_ion() {
        let t = Theme::resolve_str("unknown-theme", false);
        assert_eq!(t.id, ThemeId::Ion);
        assert!(!t.is_monochrome());
    }
}
