//! Semantic color palette. Components reference roles (`accent`, `error`, …),
//! never raw colors, so a theme swap or `NO_COLOR` is a single change.

use ratatui::style::Color;

/// Stable theme identifiers selectable via `/theme` and stored in config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThemeId {
    /// Cool cyan brand palette (the historical default).
    Ion,
    /// Deeper blue-tinted night palette.
    Night,
    /// Light palette for bright terminals; the product default.
    Day,
}

impl ThemeId {
    /// The theme a user who never chose one gets.
    ///
    /// R007-F4: this used to be `ALL[0]` re-spelled at each call site, so the
    /// answer lived in an array's order. `Day` is the choice the reported
    /// screenshots argue for — a light terminal — and with body text now
    /// inheriting the terminal foreground, the default only decides accent
    /// hues, not whether text can be read.
    pub const DEFAULT: ThemeId = ThemeId::Day;

    /// All named (non-monochrome) themes, in cycle order (default first).
    pub const ALL: [ThemeId; 3] = [ThemeId::Day, ThemeId::Ion, ThemeId::Night];

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
            Self::Day => Self::Ion,
            Self::Ion => Self::Night,
            Self::Night => Self::Day,
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
    /// De-emphasized text: fold hints, auxiliary info (weaker than `muted`).
    pub dim: Color,
    pub accent: Color,
    /// Headings and the user-message bar (distinct from `accent` links/tools).
    pub heading: Color,
    pub success: Color,
    pub warning: Color,
    pub error: Color,
    pub border: Color,
    pub user_message: Color,
    pub assistant_message: Color,
    pub tool: Color,
    pub diff_add: Color,
    pub diff_remove: Color,
    /// Inline `code` spans.
    pub code: Color,
    /// Shell prompt marker (`$`) in command blocks.
    pub shell_prompt: Color,
    pub attachment: Color,
    /// Soft background for fenced code blocks (Reset = transparent / none).
    pub code_bg: Color,
}

impl Theme {
    /// Cool cyan brand palette.
    pub fn ion() -> Self {
        Self {
            id: ThemeId::Ion,
            monochrome: false,
            // R007-F4: body text inherits the terminal foreground. A hardcoded
            // near-white is unreadable on a light profile, and the user who hit
            // that had never chosen a theme at all.
            text: Color::Reset,
            muted: Color::Rgb(0x90, 0x97, 0x9F),
            dim: Color::Rgb(0x69, 0x70, 0x78),
            accent: Color::Rgb(0x56, 0xB6, 0xE9),
            heading: Color::Rgb(0x45, 0xC7, 0xD9),
            success: Color::Rgb(0x73, 0xC9, 0x91),
            warning: Color::Rgb(0xD7, 0xBA, 0x7D),
            error: Color::Rgb(0xF0, 0x6A, 0x7A),
            border: Color::Rgb(0x4A, 0x51, 0x58),
            // Match `text` so user and assistant turns share one readable fg;
            // the heading bar + bold carry the turn distinction.
            user_message: Color::Reset,
            assistant_message: Color::Reset,
            tool: Color::Rgb(0x56, 0xB6, 0xE9),
            diff_add: Color::Rgb(0x73, 0xC9, 0x91),
            diff_remove: Color::Rgb(0xF0, 0x6A, 0x7A),
            code: Color::Rgb(0xC8, 0xCD, 0xD2),
            shell_prompt: Color::Rgb(0xC5, 0x86, 0xC0),
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
            text: Color::Reset,
            muted: Color::Rgb(0x90, 0x97, 0x9F),
            dim: Color::Rgb(0x69, 0x70, 0x78),
            accent: Color::Rgb(0x56, 0xB6, 0xE9),
            heading: Color::Rgb(0x45, 0xC7, 0xD9),
            success: Color::Rgb(0x73, 0xC9, 0x91),
            warning: Color::Rgb(0xD7, 0xBA, 0x7D),
            error: Color::Rgb(0xF0, 0x6A, 0x7A),
            border: Color::Rgb(0x4A, 0x51, 0x58),
            // Night keeps its blue-tinted identity in the semantic roles; the
            // body fg is the terminal's, like every other named theme.
            user_message: Color::Reset,
            assistant_message: Color::Reset,
            tool: Color::Rgb(0x56, 0xB6, 0xE9),
            diff_add: Color::Rgb(0x73, 0xC9, 0x91),
            diff_remove: Color::Rgb(0xF0, 0x6A, 0x7A),
            code: Color::Rgb(0xC8, 0xCD, 0xD2),
            shell_prompt: Color::Rgb(0xC5, 0x86, 0xC0),
            attachment: Color::Rgb(0xFF, 0x9E, 0x64),
            code_bg: Color::Rgb(0x16, 0x16, 0x1E),
        }
    }

    /// Light theme for bright terminal backgrounds.
    pub fn day() -> Self {
        Self {
            id: ThemeId::Day,
            monochrome: false,
            text: Color::Reset,
            muted: Color::Rgb(0x57, 0x60, 0x6A),
            dim: Color::Rgb(0x8C, 0x95, 0x9F),
            accent: Color::Rgb(0x09, 0x69, 0xDA),
            heading: Color::Rgb(0x0E, 0x74, 0x90),
            success: Color::Rgb(0x1A, 0x7F, 0x37),
            warning: Color::Rgb(0x9A, 0x67, 0x00),
            error: Color::Rgb(0xCF, 0x22, 0x2E),
            border: Color::Rgb(0xD0, 0xD7, 0xDE),
            user_message: Color::Reset,
            assistant_message: Color::Reset,
            tool: Color::Rgb(0x09, 0x69, 0xDA),
            diff_add: Color::Rgb(0x1A, 0x7F, 0x37),
            diff_remove: Color::Rgb(0xCF, 0x22, 0x2E),
            code: Color::Rgb(0x33, 0x39, 0x3F),
            shell_prompt: Color::Rgb(0x82, 0x50, 0xDF),
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
            // A NO_COLOR user has not chosen a palette either, so the remembered
            // preference is the same explicit default (R007-F4).
            id: ThemeId::DEFAULT,
            monochrome: true,
            text: Color::Reset,
            muted: Color::Reset,
            dim: Color::Reset,
            accent: Color::Reset,
            heading: Color::Reset,
            success: Color::Reset,
            warning: Color::Reset,
            error: Color::Reset,
            border: Color::Reset,
            user_message: Color::Reset,
            assistant_message: Color::Reset,
            tool: Color::Reset,
            diff_add: Color::Reset,
            diff_remove: Color::Reset,
            code: Color::Reset,
            shell_prompt: Color::Reset,
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
}

impl Default for Theme {
    fn default() -> Self {
        Self::named(ThemeId::DEFAULT)
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
            assert_ne!(t.heading, Color::Reset, "{id} heading should be colored");
            assert_ne!(t.dim, Color::Reset, "{id} dim should be colored");
            assert_ne!(t.code, Color::Reset, "{id} code should be colored");
            assert_ne!(
                t.shell_prompt,
                Color::Reset,
                "{id} shell_prompt should be colored"
            );
            assert_eq!(t.id, id);
        }
    }

    #[test]
    fn ion_message_turns_follow_text_color() {
        // User and assistant turns share one body fg; the distinction comes
        // from the heading bar + bold, not hue. That fg is the terminal's own
        // (R007-F4) — "light terminals are served by day()" assumed the user
        // would switch themes, and the reported case never did.
        let t = Theme::ion();
        assert_eq!(t.user_message, t.text);
        assert_eq!(t.assistant_message, t.text);
    }

    #[test]
    fn no_color_override_wins() {
        for id in ThemeId::ALL {
            let t = Theme::resolve(id, true);
            assert!(t.is_monochrome(), "{id}");
            assert_eq!(t.accent, Color::Reset);
            assert_eq!(t.diff_add, Color::Reset);
            assert_eq!(t.heading, Color::Reset);
            assert_eq!(t.dim, Color::Reset);
            assert_eq!(t.code, Color::Reset);
            assert_eq!(t.shell_prompt, Color::Reset);
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
    fn resolve_str_unknown_falls_back_to_the_default() {
        let t = Theme::resolve_str("unknown-theme", false);
        assert_eq!(t.id, ThemeId::DEFAULT);
        assert!(!t.is_monochrome());
    }

    /// R007-F4: the product had no default — it had a first element. Every
    /// fallback read `ThemeId::Ion` by hand, so the answer to "which theme does
    /// a user who never chose one get?" lived in three places and in the order
    /// of an array.
    #[test]
    fn every_fallback_resolves_to_one_explicit_default() {
        assert_eq!(ThemeId::DEFAULT, ThemeId::Day);
        assert_eq!(Theme::default().id, ThemeId::DEFAULT);
        assert_eq!(
            Theme::no_color().id,
            ThemeId::DEFAULT,
            "a NO_COLOR user has not chosen a theme either"
        );
        assert_eq!(
            Theme::resolve_str("unknown-theme", false).id,
            ThemeId::DEFAULT
        );
        assert_eq!(
            ThemeId::ALL[0],
            ThemeId::DEFAULT,
            "the picker lists the default first — but DEFAULT is the constant, \
             not the position"
        );
    }

    /// R007-F4: body text was a hardcoded near-white (ion/night) or near-black
    /// (day). A user on the opposite background got white-on-white — and, since
    /// there was no explicit default, got it without ever choosing a theme.
    /// Body text now inherits the terminal foreground, so it reads on any
    /// background without the user switching themes.
    #[test]
    fn body_text_inherits_the_terminal_foreground() {
        for id in ThemeId::ALL {
            let t = Theme::named(id);
            assert_eq!(t.text, Color::Reset, "{id} body text");
            assert_eq!(t.user_message, Color::Reset, "{id} user message");
            assert_eq!(t.assistant_message, Color::Reset, "{id} assistant message");
        }
    }

    /// The contrast fix must not become "everything is Reset": a theme that
    /// cannot say `error` in red has lost the information color carries.
    #[test]
    fn semantic_roles_survive_the_contrast_fix() {
        for id in ThemeId::ALL {
            let t = Theme::named(id);
            for (role, color) in [
                ("accent", t.accent),
                ("heading", t.heading),
                ("success", t.success),
                ("warning", t.warning),
                ("error", t.error),
                ("diff_add", t.diff_add),
                ("diff_remove", t.diff_remove),
                ("tool", t.tool),
                ("muted", t.muted),
                ("dim", t.dim),
            ] {
                assert_ne!(color, Color::Reset, "{id} {role} must stay colored");
            }
        }
    }
}
