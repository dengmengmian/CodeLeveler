//! Semantic color tokens. Components consume roles (`theme.text.primary`),
//! never raw RGB / ANSI / `DIM`, so a palette swap cannot silently lose contrast.
//!
//! Named themes paint an opaque canvas. Body text is a real token, not
//! `Color::Reset`, so readability does not depend on the terminal profile.
//! Level Mark inks live on `theme.brand` (`foundation` / `primary` / `highlight`).

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

/// What the terminal can actually display.
///
/// Resolved once from the environment into the palette itself, so a renderer
/// never has to guess: a `Color::Rgb` token is truecolor, an indexed token is
/// 256-colour, a named ANSI token is 16-colour, and a monochrome theme is
/// `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorDepth {
    None,
    Ansi16,
    Ansi256,
    TrueColor,
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
    /// The user's own turn. Brighter than the canvas and the generic elevated
    /// surface so the prompt reads as a distinct card, but still well below
    /// primary ink: the card identifies the message, it does not compete with
    /// its text.
    pub prompt: Color,
}

/// Text roles. Hierarchy is a different *color*, not `Modifier::DIM`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextColors {
    pub primary: Color,
    /// The opening paragraph of an assistant's final answer (its one-line
    /// conclusion). Brighter than body, quieter than an accent.
    pub final_lead: Color,
    /// `**strong**` emphasis inside prose: brighter than body, never an accent.
    pub strong: Color,
    pub secondary: Color,
    /// Inline code, paths, identifiers, and commands inside prose.
    pub code: Color,
    pub muted: Color,
    pub disabled: Color,
    pub inverse: Color,
    /// Live work the user is watching (a running command): just under prose.
    pub active: Color,
    /// Facts about a row (duration, line count, exit code): under its body.
    pub meta: Color,
    /// Structure, not content (the `›` anchor, tree rails): the faintest ink.
    pub subtle: Color,
}

/// The transcript's luminance hierarchy, brightest first. Renderers pick a
/// role; [`Theme::ink`] is the one place a role becomes a color, so the
/// ladder cannot drift apart between live, replay and history.
///
/// Prose is what the user reads; the live action is what they watch; a
/// settled tool call is evidence that should recede as it piles up; its
/// metadata recedes further; connectors only give the rows their shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ink {
    /// Agent prose and anything the user must read now.
    Prose,
    /// A running / live activity row.
    Active,
    /// A finished tool call's body.
    Settled,
    /// Duration, counts, exit codes, state words beside a row.
    Meta,
    /// Anchors, rails, hints: structure only.
    Subtle,
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

/// The Plan indicator's ink: its own soft purple, plus the dim end of the
/// breathing animation the running glyph moves between.
///
/// The Plan is a second object under the same task as the runtime status, so
/// it needs to read as its own thing without becoming a second brand color —
/// hence a token pair rather than a borrowed accent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanColors {
    /// The indicator's identity ink: glyph, progress and the entry arrow.
    pub primary: Color,
    /// The dim end of the running glyph's breath.
    pub dim: Color,
}

/// Discrete stops the breath moves through. One per busy tick at the 1.2 s
/// period, so a smooth RGB interpolation never repaints faster than the
/// terminal already does.
const BREATH_STEPS: u8 = 8;

impl PlanColors {
    /// Ink for the running glyph at `level` (`1.0` = [`Self::primary`],
    /// `0.0` = [`Self::dim`]).
    ///
    /// A terminal cell has no alpha, so the breath is a step between two shades
    /// of one hue. RGB endpoints interpolate over [`BREATH_STEPS`] stops; a
    /// palette with no RGB value for them (the 256-colour compatibility path,
    /// `NO_COLOR`) switches between the two tokens instead of inventing an
    /// in-between the profile cannot paint.
    pub fn breathe(&self, level: f32) -> Color {
        let (Color::Rgb(r0, g0, b0), Color::Rgb(r1, g1, b1)) = (self.dim, self.primary) else {
            return if level < 0.5 { self.dim } else { self.primary };
        };
        let step = (level.clamp(0.0, 1.0) * f32::from(BREATH_STEPS)).round();
        let mix = |from: u8, to: u8| {
            (f32::from(from) + (f32::from(to) - f32::from(from)) * (step / f32::from(BREATH_STEPS)))
                .round() as u8
        };
        Color::Rgb(mix(r0, r1), mix(g0, g1), mix(b0, b1))
    }
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

/// Level Mark inks. Geometry first; these only tint the rise.
/// Never construct these in a renderer — always take them from a [`Theme`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrandColors {
    /// Base / foundation (deeper CodeLeveler Blue).
    pub foundation: Color,
    /// Mid levels (primary CodeLeveler Blue).
    pub primary: Color,
    /// Apex (brighter accent blue).
    pub highlight: Color,
}

/// Diff / edit highlighting. Markers (`+`/`-`) carry meaning alongside color.
/// `*_bg` is the row band; `*_inline_bg` is the stronger band under the
/// characters that actually changed inside a replaced line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffColors {
    pub added: Color,
    pub removed: Color,
    pub modified: Color,
    pub context: Color,
    pub added_bg: Color,
    pub removed_bg: Color,
    pub added_inline_bg: Color,
    pub removed_inline_bg: Color,
}

/// Semantic code-token inks. A renderer classifies a token once and asks for
/// its role; it never invents a colour per language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyntaxColors {
    pub keyword: Color,
    pub string: Color,
    pub type_name: Color,
    pub function: Color,
    pub variable: Color,
    pub comment: Color,
    pub number: Color,
    pub plain: Color,
}

/// User shell execution inks. Running is its own violet — a local command is
/// the user's work, not the runtime's blue — and the breath moves between two
/// shades of that same hue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShellColors {
    pub running: Color,
    /// Dim end of the running rail/icon breath.
    pub running_dim: Color,
    pub success: Color,
    pub error: Color,
    /// Explicit cancellation: stopped, not succeeded and not failed.
    pub cancelled: Color,
}

impl ShellColors {
    /// Ink at `level` (`1.0` = running, `0.0` = running_dim). The terminal
    /// cell has no alpha, so this steps between two shades; a palette without
    /// RGB switches at the midpoint.
    pub fn breathe(&self, level: f32) -> Color {
        let (Color::Rgb(r0, g0, b0), Color::Rgb(r1, g1, b1)) = (self.running_dim, self.running)
        else {
            return if level < 0.5 {
                self.running_dim
            } else {
                self.running
            };
        };
        let step = (level.clamp(0.0, 1.0) * f32::from(BREATH_STEPS)).round();
        let mix = |from: u8, to: u8| {
            (f32::from(from) + (f32::from(to) - f32::from(from)) * (step / f32::from(BREATH_STEPS)))
                as u8
        };
        Color::Rgb(mix(r0, r1), mix(g0, g1), mix(b0, b1))
    }
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
    /// Plan indicator ink (see [`PlanColors`]).
    pub plan: PlanColors,
    /// User shell execution inks (see [`ShellColors`]).
    pub shell: ShellColors,
    pub status: StatusColors,
    pub diff: DiffColors,
    /// Code syntax inks (see [`SyntaxColors`]).
    pub syntax: SyntaxColors,
    pub brand: BrandColors,
}

const fn rgb(hex: u32) -> Color {
    Color::Rgb(
        ((hex >> 16) & 0xff) as u8,
        ((hex >> 8) & 0xff) as u8,
        (hex & 0xff) as u8,
    )
}

/// Whether this Apple Terminal host needs the 256-colour compatibility path.
/// Kept pure so the macOS-version boundary is testable without changing the
/// process environment.
fn old_apple_terminal(term_program: Option<&str>, macos_product_version: Option<&str>) -> bool {
    if term_program != Some("Apple_Terminal") {
        return false;
    }
    macos_product_version
        .and_then(|version| version.split('.').next())
        .and_then(|major| major.parse::<u32>().ok())
        .is_none_or(|major| major < 26)
}

/// Rebuild a palette with every colour token passed through `map`.
///
/// Degradations are total-palette rewrites, never per-widget fallbacks: a
/// widget that asks for `theme.diff.added_bg` gets the best colour the
/// terminal can paint without knowing which terminal it is.
fn map_theme_colors(theme: Theme, map: impl Fn(Color) -> Color) -> Theme {
    Theme {
        id: theme.id,
        monochrome: theme.monochrome,
        surface: SurfaceColors {
            canvas: map(theme.surface.canvas),
            panel: map(theme.surface.panel),
            elevated: map(theme.surface.elevated),
            input: map(theme.surface.input),
            selection: map(theme.surface.selection),
            prompt: map(theme.surface.prompt),
        },
        text: TextColors {
            primary: map(theme.text.primary),
            final_lead: map(theme.text.final_lead),
            strong: map(theme.text.strong),
            secondary: map(theme.text.secondary),
            code: map(theme.text.code),
            muted: map(theme.text.muted),
            disabled: map(theme.text.disabled),
            inverse: map(theme.text.inverse),
            active: map(theme.text.active),
            meta: map(theme.text.meta),
            subtle: map(theme.text.subtle),
        },
        border: BorderColors {
            subtle: map(theme.border.subtle),
            normal: map(theme.border.normal),
            strong: map(theme.border.strong),
            focus: map(theme.border.focus),
        },
        accent: AccentColors {
            primary: map(theme.accent.primary),
            secondary: map(theme.accent.secondary),
            subtle: map(theme.accent.subtle),
        },
        plan: PlanColors {
            primary: map(theme.plan.primary),
            dim: map(theme.plan.dim),
        },
        shell: ShellColors {
            running: map(theme.shell.running),
            running_dim: map(theme.shell.running_dim),
            success: map(theme.shell.success),
            error: map(theme.shell.error),
            cancelled: map(theme.shell.cancelled),
        },
        status: StatusColors {
            success: map(theme.status.success),
            warning: map(theme.status.warning),
            error: map(theme.status.error),
            info: map(theme.status.info),
            running: map(theme.status.running),
        },
        diff: DiffColors {
            added: map(theme.diff.added),
            removed: map(theme.diff.removed),
            modified: map(theme.diff.modified),
            context: map(theme.diff.context),
            added_bg: map(theme.diff.added_bg),
            removed_bg: map(theme.diff.removed_bg),
            added_inline_bg: map(theme.diff.added_inline_bg),
            removed_inline_bg: map(theme.diff.removed_inline_bg),
        },
        syntax: SyntaxColors {
            keyword: map(theme.syntax.keyword),
            string: map(theme.syntax.string),
            type_name: map(theme.syntax.type_name),
            function: map(theme.syntax.function),
            variable: map(theme.syntax.variable),
            comment: map(theme.syntax.comment),
            number: map(theme.syntax.number),
            plain: map(theme.syntax.plain),
        },
        brand: BrandColors {
            foundation: map(theme.brand.foundation),
            primary: map(theme.brand.primary),
            highlight: map(theme.brand.highlight),
        },
    }
}

/// Replace every RGB token with its nearest xterm-256 colour.
pub(crate) fn indexed_256_theme(theme: Theme) -> Theme {
    let light = relative_luminance(theme.surface.canvas).is_some_and(|lum| lum > 0.5);
    let mut mapped = map_theme_colors(theme, |color| match color {
        Color::Rgb(r, g, b) => Color::Indexed(nearest_xterm_256(r, g, b)),
        other => other,
    });
    // The xterm cube has no soft dark green/red: the nearest hue step is the
    // saturated `#005F00` / `#5F0000` slab that made a whole diff row shout.
    // Give the row area a neutral step of the same polarity (235 = #262626 on
    // dark, 254 = #E4E4E4 on light) and let the direction marker carry the
    // hue; the inline step stays tinted because it covers only the changed
    // token, not the row.
    let (row, added_inline, removed_inline) = if light {
        (254, 194, 224) // neutral, light green, light pink
    } else {
        (235, 28, 88) // neutral, #008700, #870000
    };
    mapped.diff.added_bg = Color::Indexed(row);
    mapped.diff.removed_bg = Color::Indexed(row);
    mapped.diff.added_inline_bg = Color::Indexed(added_inline);
    mapped.diff.removed_inline_bg = Color::Indexed(removed_inline);
    mapped
}

/// The 16 ANSI colours in their conventional xterm values.
///
/// Terminals remap these, so they are a reference for degradation and
/// luminance only — never a colour a renderer should print as RGB.
const ANSI16_RGB: [(u8, u8, u8); 16] = [
    (0, 0, 0),
    (205, 49, 49),
    (13, 188, 121),
    (229, 229, 16),
    (36, 114, 200),
    (188, 63, 188),
    (17, 168, 205),
    (229, 229, 229),
    (102, 102, 102),
    (241, 76, 76),
    (35, 209, 139),
    (245, 245, 67),
    (59, 142, 234),
    (214, 112, 214),
    (41, 184, 219),
    (255, 255, 255),
];

/// The named ratatui colour for one of the 16 ANSI slots.
///
/// Named variants (not `Indexed(0..=15)`) matter: ratatui emits `Indexed` as
/// `38;5;N`, which a 16-colour terminal does not understand.
fn ansi16_named(index: usize) -> Color {
    match index {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        7 => Color::Gray,
        8 => Color::DarkGray,
        9 => Color::LightRed,
        10 => Color::LightGreen,
        11 => Color::LightYellow,
        12 => Color::LightBlue,
        13 => Color::LightMagenta,
        14 => Color::LightCyan,
        _ => Color::White,
    }
}

/// Every RGB token to the nearest of the 16 classic ANSI colours.
pub(crate) fn ansi16_theme(theme: Theme) -> Theme {
    let light = relative_luminance(theme.surface.canvas).is_some_and(|lum| lum > 0.5);
    let mut mapped = map_theme_colors(theme, |color| match color {
        Color::Rgb(r, g, b) => nearest_ansi16(r, g, b),
        other => other,
    });
    // Nearest-RGB collapsing would fold several code roles onto the same
    // named slot (and pick a mid-grey comment that misses 4.5:1 on black).
    // Assign the semantic roles directly instead: fewer distinct inks are
    // unavoidable at this depth, but the ones that remain are legible and
    // still separate keyword / string / type / call.
    mapped.syntax = if light {
        SyntaxColors {
            keyword: Color::Magenta,
            string: Color::Red,
            type_name: Color::Blue,
            function: Color::DarkGray,
            variable: Color::Black,
            comment: Color::DarkGray,
            number: Color::Red,
            plain: Color::Black,
        }
    } else {
        SyntaxColors {
            keyword: Color::LightMagenta,
            string: Color::LightYellow,
            type_name: Color::LightCyan,
            function: Color::LightBlue,
            variable: Color::White,
            comment: Color::Gray,
            number: Color::Yellow,
            plain: Color::White,
        }
    };
    mapped
}

fn nearest_ansi16(r: u8, g: u8, b: u8) -> Color {
    let mut nearest = 0usize;
    let mut nearest_distance = u32::MAX;
    for (index, (cr, cg, cb)) in ANSI16_RGB.into_iter().enumerate() {
        let dr = i32::from(r) - i32::from(cr);
        let dg = i32::from(g) - i32::from(cg);
        let db = i32::from(b) - i32::from(cb);
        let distance = (dr * dr + dg * dg + db * db) as u32;
        if distance < nearest_distance {
            nearest = index;
            nearest_distance = distance;
        }
    }
    ansi16_named(nearest)
}

/// Nearest colour in the stable xterm cube/grayscale range. Indices 0–15 are
/// deliberately excluded because terminal profiles may remap those colours.
fn nearest_xterm_256(r: u8, g: u8, b: u8) -> u8 {
    let mut nearest = 16u8;
    let mut nearest_distance = u32::MAX;
    for index in 16u8..=255 {
        let (cr, cg, cb) = xterm_256_rgb(index);
        let dr = i32::from(r) - i32::from(cr);
        let dg = i32::from(g) - i32::from(cg);
        let db = i32::from(b) - i32::from(cb);
        let distance = (dr * dr + dg * dg + db * db) as u32;
        if distance < nearest_distance {
            nearest = index;
            nearest_distance = distance;
        }
    }
    nearest
}

pub(crate) fn xterm_256_rgb(index: u8) -> (u8, u8, u8) {
    if index >= 232 {
        let value = 8 + (index - 232) * 10;
        return (value, value, value);
    }
    let cube = index - 16;
    let channel = |level: u8| if level == 0 { 0 } else { 55 + level * 40 };
    (
        channel(cube / 36),
        channel((cube % 36) / 6),
        channel(cube % 6),
    )
}

fn macos_product_version() -> Option<&'static str> {
    static VERSION: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    VERSION
        .get_or_init(|| {
            #[cfg(target_os = "macos")]
            {
                let output = std::process::Command::new("/usr/bin/sw_vers")
                    .arg("-productVersion")
                    .stdin(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .env_clear()
                    .output()
                    .ok()?;
                output
                    .status
                    .success()
                    .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
            }
            #[cfg(not(target_os = "macos"))]
            {
                None
            }
        })
        .as_deref()
}

fn terminal_needs_indexed_colors() -> bool {
    old_apple_terminal(
        leveler_core::environment().var("TERM_PROGRAM").as_deref(),
        macos_product_version(),
    )
}

/// TERM names that negotiate truecolor without advertising it via `COLORTERM`.
const TRUE_COLOR_TERMS: &[&str] = &[
    "xterm-kitty",
    "xterm-ghostty",
    "alacritty",
    "wezterm",
    "foot",
    "foot-direct",
    "rio",
    "contour",
];

/// Classify the terminal's real colour capability.
///
/// `COLORTERM` is the only widely reliable truecolor signal; a `*-256color`
/// TERM name is the standard 256-colour signal. Anything else that names a
/// terminal gets the 16-colour palette. An empty TERM (pipes, embedded hosts,
/// test processes) keeps the full palette rather than guessing a downgrade.
pub fn terminal_color_depth() -> ColorDepth {
    let env = leveler_core::environment();
    color_depth_for(
        env.var("TERM").as_deref(),
        env.var("COLORTERM").as_deref(),
        terminal_needs_indexed_colors(),
    )
}

/// Pure classifier behind [`terminal_color_depth`], kept testable without
/// mutating the process environment.
fn color_depth_for(term: Option<&str>, colorterm: Option<&str>, old_apple: bool) -> ColorDepth {
    let colorterm = colorterm.unwrap_or_default().to_ascii_lowercase();
    if colorterm.contains("truecolor") || colorterm.contains("24bit") {
        return ColorDepth::TrueColor;
    }
    if old_apple {
        return ColorDepth::Ansi256;
    }
    let term = term.unwrap_or_default().trim().to_ascii_lowercase();
    if term.is_empty() {
        return ColorDepth::TrueColor;
    }
    if term.contains("256color") || term == "tmux" || term == "screen" {
        return ColorDepth::Ansi256;
    }
    if TRUE_COLOR_TERMS.contains(&term.as_str()) {
        return ColorDepth::TrueColor;
    }
    ColorDepth::Ansi16
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
                // The existing `panel` step: a quiet band just above the
                // canvas, not a card competing with the prose.
                prompt: rgb(0x171A1E),
            },
            text: TextColors {
                primary: rgb(0xE7E9EC),
                // The final answer's opening line: a soft blue that reads as a
                // conclusion, not as body ink and not as a link.
                final_lead: rgb(0xB7D7FA),
                // `**strong**` inside prose. Just above body so emphasis is
                // visible without becoming a second accent color.
                strong: rgb(0xFFFFFF),
                secondary: rgb(0xB5BAC1),
                code: rgb(0x79C6D6),
                muted: rgb(0x858C95),
                disabled: rgb(0x636A73),
                inverse: rgb(0x111315),
                active: rgb(0xC9CDD3),
                meta: rgb(0x6A717A),
                subtle: rgb(0x50565E),
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
            // Soft violet, distinct from the blue accent. The dim end stays a
            // purple (not a grey) so the breath is luminance, not hue.
            plan: PlanColors {
                primary: rgb(0xA78BFA),
                dim: rgb(0x6D5BC4),
            },
            shell: ShellColors {
                running: rgb(0xA78BFA),
                running_dim: rgb(0x6D5BC4),
                success: rgb(0x56C271),
                error: rgb(0xE56B6F),
                cancelled: rgb(0xE5B454),
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
                added_inline_bg: rgb(0x1E4A2D),
                removed_inline_bg: rgb(0x522527),
            },
            // Hues follow the reading conventions the user expects (keywords
            // pink, strings amber, types cyan, calls blue) and every ink clears
            // 4.5:1 on the canvas and on both diff bands.
            syntax: SyntaxColors {
                keyword: rgb(0xE39AD6),
                string: rgb(0xE6C07B),
                type_name: rgb(0x7FD3E8),
                function: rgb(0x8FB8F0),
                variable: rgb(0xE7E9EC),
                comment: rgb(0x8B93A1),
                number: rgb(0xE0A06E),
                plain: rgb(0xE7E9EC),
            },
            brand: BrandColors {
                foundation: rgb(0x2F7FE0),
                primary: rgb(0x4EA1FF),
                highlight: rgb(0x8EC8FF),
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
                // The light counterpart of the dark band: the existing panel
                // step just above the canvas, never a highlight.
                prompt: rgb(0xFFFFFF),
            },
            text: TextColors {
                primary: rgb(0x20242A),
                final_lead: rgb(0x254F7A),
                strong: rgb(0x000000),
                secondary: rgb(0x4D5560),
                code: rgb(0x176B78),
                // Suggested #6F7884 misses 4.5:1 on this canvas; darkened.
                muted: rgb(0x5C6570),
                disabled: rgb(0x7A828C),
                inverse: rgb(0xFFFFFF),
                active: rgb(0x3A4048),
                meta: rgb(0x7A828C),
                subtle: rgb(0xA3A9B0),
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
            // Same violet family, darkened for the light canvas; the breath
            // moves toward the lighter end because that reads as the fade.
            plan: PlanColors {
                primary: rgb(0x7C3AED),
                dim: rgb(0xA78BFA),
            },
            shell: ShellColors {
                running: rgb(0x7C3AED),
                running_dim: rgb(0xA78BFA),
                success: rgb(0x1A7F37),
                error: rgb(0xCF222E),
                cancelled: rgb(0x9A6700),
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
                added_bg: rgb(0xDEF0E4),
                removed_bg: rgb(0xF8DADA),
                added_inline_bg: rgb(0xA7DCBB),
                removed_inline_bg: rgb(0xF2AEAE),
            },
            // The same hue families darkened for the light canvas. The comment
            // grey is darker than the prose muted role so it still clears
            // 4.5:1 on the pink removal band.
            syntax: SyntaxColors {
                keyword: rgb(0xA02666),
                string: rgb(0x7A5200),
                type_name: rgb(0x0B6070),
                function: rgb(0x24509E),
                variable: rgb(0x20242A),
                comment: rgb(0x59616C),
                number: rgb(0x8A4B16),
                plain: rgb(0x20242A),
            },
            brand: BrandColors {
                foundation: rgb(0x0550AE),
                primary: rgb(0x0969DA),
                highlight: rgb(0x218BFF),
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
                prompt: rgb(0x1A1A1A),
            },
            text: TextColors {
                primary: rgb(0xFFFFFF),
                final_lead: rgb(0xB8DEFF),
                strong: rgb(0xFFFFFF),
                secondary: rgb(0xE8E8E8),
                code: rgb(0x80E5FF),
                muted: rgb(0xC8C8C8),
                disabled: rgb(0xA0A0A0),
                inverse: rgb(0x000000),
                active: rgb(0xF0F0F0),
                meta: rgb(0xA8A8A8),
                subtle: rgb(0x8A8A8A),
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
            plan: PlanColors {
                primary: rgb(0xC9A8FF),
                dim: rgb(0x8B62E8),
            },
            shell: ShellColors {
                running: rgb(0xC9A8FF),
                running_dim: rgb(0x8B62E8),
                success: rgb(0x3DDA6B),
                error: rgb(0xFF6B6B),
                cancelled: rgb(0xFFD24A),
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
                added_inline_bg: rgb(0x004400),
                removed_inline_bg: rgb(0x660000),
            },
            syntax: SyntaxColors {
                keyword: rgb(0xFF9BD2),
                string: rgb(0xFFD166),
                type_name: rgb(0x7FE8FF),
                function: rgb(0x9FC4FF),
                variable: rgb(0xFFFFFF),
                comment: rgb(0xB8C0CC),
                number: rgb(0xFFB27A),
                plain: rgb(0xFFFFFF),
            },
            brand: BrandColors {
                foundation: rgb(0x4DA6FF),
                primary: rgb(0x80C0FF),
                highlight: rgb(0xB8DEFF),
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
            prompt: Color::Reset,
        };
        Self {
            id: ThemeId::DEFAULT,
            monochrome: true,
            surface: reset,
            text: TextColors {
                primary: Color::Reset,
                final_lead: Color::Reset,
                strong: Color::Reset,
                secondary: Color::Reset,
                code: Color::Reset,
                muted: Color::Reset,
                disabled: Color::Reset,
                inverse: Color::Reset,
                active: Color::Reset,
                meta: Color::Reset,
                subtle: Color::Reset,
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
            plan: PlanColors {
                primary: Color::Reset,
                dim: Color::Reset,
            },
            shell: ShellColors {
                running: Color::Reset,
                running_dim: Color::Reset,
                success: Color::Reset,
                error: Color::Reset,
                cancelled: Color::Reset,
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
                added_inline_bg: Color::Reset,
                removed_inline_bg: Color::Reset,
            },
            syntax: SyntaxColors {
                keyword: Color::Reset,
                string: Color::Reset,
                type_name: Color::Reset,
                function: Color::Reset,
                variable: Color::Reset,
                comment: Color::Reset,
                number: Color::Reset,
                plain: Color::Reset,
            },
            brand: BrandColors {
                foundation: Color::Reset,
                primary: Color::Reset,
                highlight: Color::Reset,
            },
        }
    }

    /// The color for a role in the transcript's luminance hierarchy.
    pub fn ink(&self, role: Ink) -> Color {
        match role {
            Ink::Prose => self.text.primary,
            Ink::Active => self.text.active,
            // A settled call reads at the level every other secondary fact
            // in the UI already uses; it is `muted`, not a fourth grey.
            Ink::Settled => self.text.muted,
            Ink::Meta => self.text.meta,
            Ink::Subtle => self.text.subtle,
        }
    }

    /// Monochrome palette that remembers the user's preferred named id.
    pub fn monochrome_with_id(id: ThemeId) -> Self {
        let mut t = Self::no_color();
        t.id = id;
        t
    }

    /// Resolve a theme id, honouring `NO_COLOR` (monochrome wins over any id)
    /// and degrading the palette to what the host terminal can paint.
    pub fn resolve(id: ThemeId, no_color: bool) -> Self {
        if no_color {
            return Self::monochrome_with_id(id);
        }
        match terminal_color_depth() {
            ColorDepth::None => Self::monochrome_with_id(id),
            ColorDepth::TrueColor => Self::named(id),
            ColorDepth::Ansi256 => indexed_256_theme(Self::named(id)),
            ColorDepth::Ansi16 => ansi16_theme(Self::named(id)),
        }
    }

    /// The colour capability this resolved palette is built for.
    ///
    /// Derived from the palette itself so there is one authority: renderers
    /// ask the theme what it can paint instead of re-reading the environment.
    pub fn color_depth(&self) -> ColorDepth {
        if self.monochrome {
            return ColorDepth::None;
        }
        match self.surface.canvas {
            Color::Rgb(..) => ColorDepth::TrueColor,
            Color::Indexed(_) => ColorDepth::Ansi256,
            Color::Reset => ColorDepth::None,
            _ => ColorDepth::Ansi16,
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

/// WCAG relative luminance for RGB or the stable xterm-256 cube/grayscale.
/// `Reset` and profile-defined ANSI indices 0–15 have no reliable luminance.
pub fn relative_luminance(color: Color) -> Option<f64> {
    let (r, g, b) = match color {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Indexed(index @ 16..=255) => xterm_256_rgb(index),
        Color::Indexed(index @ 0..=15) => ANSI16_RGB[index as usize],
        Color::Reset => return None,
        named => ansi16_rgb(named)?,
    };
    Some(0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b))
}

/// Reference RGB for a named ANSI colour (the 16-colour degradation target).
fn ansi16_rgb(color: Color) -> Option<(u8, u8, u8)> {
    let index = match color {
        Color::Black => 0,
        Color::Red => 1,
        Color::Green => 2,
        Color::Yellow => 3,
        Color::Blue => 4,
        Color::Magenta => 5,
        Color::Cyan => 6,
        Color::Gray => 7,
        Color::DarkGray => 8,
        Color::LightRed => 9,
        Color::LightGreen => 10,
        Color::LightYellow => 11,
        Color::LightBlue => 12,
        Color::LightMagenta => 13,
        Color::LightCyan => 14,
        Color::White => 15,
        _ => return None,
    };
    Some(ANSI16_RGB[index])
}

/// WCAG contrast ratio between two colours with known luminance.
pub fn contrast_ratio(a: Color, b: Color) -> Option<f64> {
    let la = relative_luminance(a)?;
    let lb = relative_luminance(b)?;
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    Some((hi + 0.05) / (lo + 0.05))
}

impl Theme {
    /// `fg` when it already clears `min` on `bg`; otherwise the nearest
    /// same-hue step that does.
    ///
    /// Inline diff emphasis is the only place a syntax token acquires a new
    /// background after its ink was chosen, so it is the only caller. The
    /// correction moves the channel values toward white or black — preserving
    /// the hue relation that told the reader what kind of token it was — and
    /// re-quantises into the palette's depth.
    pub(crate) fn readable_on(&self, fg: Color, bg: Color, min: f64) -> Color {
        let Some(bg_lum) = relative_luminance(bg) else {
            return fg;
        };
        if contrast_ratio(fg, bg).is_some_and(|ratio| ratio >= min) {
            return fg;
        }
        let Some((r, g, b)) = sample_rgb(fg) else {
            return fg;
        };
        let target = if bg_lum < 0.18 { 255.0 } else { 0.0 };
        let mut last = fg;
        for step in 1..=20 {
            let t = f64::from(step) / 20.0;
            let mix = |c: u8| (f64::from(c) + (target - f64::from(c)) * t).round() as u8;
            let candidate = Color::Rgb(mix(r), mix(g), mix(b));
            let encoded = match fg {
                Color::Rgb(..) => candidate,
                Color::Indexed(index) if index >= 16 => {
                    let (r, g, b) = sample_rgb(candidate).unwrap_or((r, g, b));
                    Color::Indexed(nearest_xterm_256(r, g, b))
                }
                _ => fg,
            };
            last = encoded;
            if contrast_ratio(encoded, bg).is_some_and(|ratio| ratio >= min) {
                return encoded;
            }
        }
        last
    }
}

pub(crate) fn sample_rgb(color: Color) -> Option<(u8, u8, u8)> {
    match color {
        Color::Rgb(r, g, b) => Some((r, g, b)),
        Color::Indexed(index @ 16..=255) => Some(xterm_256_rgb(index)),
        Color::Indexed(index @ 0..=15) => Some(ANSI16_RGB[index as usize]),
        named => ansi16_rgb(named),
    }
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
        ("Final lead", Some(theme.text.final_lead)),
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
        ("Syntax", None),
        ("func main() {", Some(theme.syntax.keyword)),
        ("  \"hello\"", Some(theme.syntax.string)),
        ("  value := 42", Some(theme.syntax.number)),
        ("  Entry", Some(theme.syntax.type_name)),
        ("  handle()", Some(theme.syntax.function)),
        ("  // comment", Some(theme.syntax.comment)),
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
    /// background today; the diff bands ARE painted (added/removed runs and
    /// the stronger inline token band), so they are gated here too.
    fn assert_used_contrast_pairs(theme: &Theme) {
        let s = &theme.surface;
        let t = &theme.text;
        assert_min(t.primary, s.canvas, 7.0, "primary/canvas");
        assert_min(t.primary, s.input, 7.0, "primary/input");
        assert_min(t.primary, s.elevated, 7.0, "primary/elevated");
        assert_min(t.primary, s.prompt, 7.0, "primary/prompt");
        assert_min(
            theme.accent.secondary,
            s.prompt,
            3.0,
            "accent.secondary/prompt",
        );
        assert_min(t.primary, s.selection, 4.5, "primary/selection");
        assert_min(t.final_lead, s.canvas, 4.5, "final_lead/canvas");
        assert_min(t.strong, s.canvas, 4.5, "strong/canvas");
        assert_min(t.secondary, s.canvas, 4.5, "secondary/canvas");
        assert_min(t.code, s.canvas, 4.5, "code/canvas");
        assert_min(t.muted, s.canvas, 4.5, "muted/canvas");
        assert_min(t.muted, s.input, 4.5, "muted/input");
        assert_min(theme.plan.primary, s.canvas, 4.5, "plan.primary/canvas");
        // The composer paints slash commands in this ink on the input surface.
        assert_min(theme.plan.primary, s.input, 4.5, "plan.primary/input");
        assert_min(theme.status.success, s.canvas, 4.5, "success/canvas");
        assert_min(theme.status.warning, s.canvas, 4.5, "warning/canvas");
        assert_min(theme.status.error, s.canvas, 4.5, "error/canvas");
        assert_min(theme.status.info, s.canvas, 4.5, "info/canvas");
        assert_min(theme.status.running, s.canvas, 4.5, "running/canvas");
        assert_min(theme.diff.added, s.canvas, 4.5, "diff.added/canvas");
        assert_min(theme.diff.removed, s.canvas, 4.5, "diff.removed/canvas");
        // A band is a surface step, not a highlight: visible against the
        // canvas, quiet enough to keep syntax colour readable. The inline
        // band is one further step so a changed token still stands out inside
        // an already tinted row.
        assert_min(theme.diff.added_bg, s.canvas, 1.10, "added_bg/canvas");
        assert_min(theme.diff.removed_bg, s.canvas, 1.10, "removed_bg/canvas");
        assert_min(
            theme.diff.added_inline_bg,
            theme.diff.added_bg,
            1.20,
            "added_inline_bg/added_bg",
        );
        assert_min(
            theme.diff.removed_inline_bg,
            theme.diff.removed_bg,
            1.20,
            "removed_inline_bg/removed_bg",
        );
        // Ink the painter puts directly on a band.
        assert_min(t.primary, theme.diff.added_bg, 7.0, "primary/added_bg");
        assert_min(t.primary, theme.diff.removed_bg, 7.0, "primary/removed_bg");
        assert_min(
            t.primary,
            theme.diff.added_inline_bg,
            7.0,
            "primary/added_inline_bg",
        );
        assert_min(
            t.primary,
            theme.diff.removed_inline_bg,
            7.0,
            "primary/removed_inline_bg",
        );
        assert_min(t.secondary, theme.diff.added_bg, 4.5, "secondary/added_bg");
        assert_min(
            t.secondary,
            theme.diff.removed_bg,
            4.5,
            "secondary/removed_bg",
        );
        // The `+`/`-` marker is bold and the band already names the direction,
        // so the marker only has to clear the non-text 3:1 step.
        assert_min(
            theme.diff.added,
            theme.diff.added_bg,
            3.0,
            "added ink/added_bg",
        );
        assert_min(
            theme.diff.removed,
            theme.diff.removed_bg,
            3.0,
            "removed ink/removed_bg",
        );
        // Code tokens are painted on the canvas and on both row bands. Inline
        // emphasis is corrected per token by `readable_on`, so it is gated in
        // the painter tests instead of here.
        for (name, ink) in [
            ("keyword", theme.syntax.keyword),
            ("string", theme.syntax.string),
            ("type", theme.syntax.type_name),
            ("function", theme.syntax.function),
            ("variable", theme.syntax.variable),
            ("comment", theme.syntax.comment),
            ("number", theme.syntax.number),
            ("plain", theme.syntax.plain),
        ] {
            assert_min(ink, s.canvas, 4.5, &format!("syntax.{name}/canvas"));
            assert_min(
                ink,
                theme.diff.added_bg,
                4.5,
                &format!("syntax.{name}/added_bg"),
            );
            assert_min(
                ink,
                theme.diff.removed_bg,
                4.5,
                &format!("syntax.{name}/removed_bg"),
            );
        }
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
            assert_ne!(t.text.final_lead, Color::Reset, "{id} final lead");
            assert_ne!(t.text.strong, Color::Reset, "{id} strong");
            assert_ne!(t.text.code, Color::Reset, "{id} code");
            assert_ne!(t.text.muted, Color::Reset, "{id} muted");
            assert_ne!(t.accent.primary, Color::Reset, "{id} accent");
            assert_eq!(t.id, id);
        }
    }

    /// Each rung of the hierarchy is strictly quieter than the one above it,
    /// in every fixed palette: prose, then the live action, the settled call,
    /// its metadata, and structure last. Measured as distance from the canvas,
    /// so it holds for the light palette too.
    #[test]
    fn the_ink_ladder_descends_in_every_palette() {
        use Ink::*;
        for id in ThemeId::FIXED {
            let theme = Theme::named(id);
            let canvas = relative_luminance(theme.surface.canvas).unwrap();
            let distance = |role| (relative_luminance(theme.ink(role)).unwrap() - canvas).abs();
            let ladder = [Prose, Active, Settled, Meta, Subtle];
            for pair in ladder.windows(2) {
                assert!(
                    distance(pair[0]) > distance(pair[1]),
                    "{id}: {:?} must stand out more than {:?}",
                    pair[0],
                    pair[1]
                );
            }
        }
    }

    /// The quieter rungs stay legible: a live row and a settled one at body
    /// contrast, metadata at the large-text bar, structure still visible.
    #[test]
    fn the_quiet_inks_stay_legible() {
        for id in ThemeId::FIXED {
            let theme = Theme::named(id);
            let t = theme.text;
            let s = theme.surface;
            assert_min(t.active, s.canvas, 4.5, &format!("{id} active/canvas"));
            assert_min(t.meta, s.canvas, 3.0, &format!("{id} meta/canvas"));
            assert_min(t.subtle, s.canvas, 2.0, &format!("{id} subtle/canvas"));
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

    /// The Plan indicator owns its ink: a violet that is not the blue accent,
    /// with a second shade to breathe toward, legible in every fixed palette.
    #[test]
    fn the_plan_indicator_owns_its_purple() {
        for id in ThemeId::FIXED {
            let t = Theme::named(id);
            assert_ne!(
                t.plan.primary, t.accent.primary,
                "{id}: plan is its own ink"
            );
            assert_ne!(
                t.plan.primary, t.plan.dim,
                "{id}: the breath needs two shades"
            );
            assert_ne!(t.plan.dim, t.surface.canvas, "{id}: dim is still a purple");
        }
        let mono = Theme::no_color();
        assert_eq!(mono.plan.primary, Color::Reset);
        assert_eq!(mono.plan.dim, Color::Reset);
    }

    /// The breath interpolates between the two shades and touches both ends;
    /// the stops are finite and monotone so consecutive frames change gently.
    #[test]
    fn the_breath_moves_between_the_two_purples() {
        let plan = Theme::dark().plan;
        assert_eq!(plan.breathe(1.0), plan.primary);
        assert_eq!(plan.breathe(0.0), plan.dim);
        assert_eq!(plan.breathe(2.0), plan.primary, "clamped");
        let mut seen = Vec::new();
        let mut shades = Vec::new();
        for i in 0..=40 {
            let color = plan.breathe(i as f32 / 40.0);
            if shades.last() != Some(&color) {
                shades.push(color);
            }
            if !seen.contains(&color) {
                seen.push(color);
            }
        }
        assert!(
            (2..=BREATH_STEPS as usize + 1).contains(&seen.len()),
            "the breath must use a finite set of stops: {}",
            seen.len()
        );
        let (Color::Rgb(r0, _, b0), Color::Rgb(r1, _, b1)) = (plan.dim, plan.primary) else {
            panic!("dark plan ink is RGB");
        };
        assert!(r0 < r1 && b0 < b1, "the breath only moves luminance");
    }

    /// A profile with no RGB for the two shades (256-colour compatibility,
    /// NO_COLOR) still breathes, just between the two tokens it can paint.
    #[test]
    fn the_breath_degrades_to_the_two_tokens_without_rgb() {
        let plan = indexed_256_theme(Theme::dark()).plan;
        assert!(matches!(plan.primary, Color::Indexed(_)));
        let low = plan.breathe(0.0);
        let high = plan.breathe(1.0);
        assert_ne!(low, high, "the two ends must still differ");
        assert!(
            [plan.primary, plan.dim].contains(&low) && [plan.primary, plan.dim].contains(&high),
            "no invented in-between: {low:?} {high:?}"
        );
    }

    /// The prompt band is a quiet step above the canvas: visible as one
    /// continuous region, never bright enough to become a card.
    #[test]
    fn the_prompt_surface_is_a_visible_but_quiet_step_above_the_canvas() {
        for theme in [Theme::dark(), Theme::light(), Theme::high_contrast()] {
            let prompt = relative_luminance(theme.surface.prompt).expect("fixed palettes are RGB");
            let canvas = relative_luminance(theme.surface.canvas).expect("fixed palettes are RGB");
            assert!(
                prompt > canvas,
                "{}: the prompt band must be brighter than the canvas",
                theme.id.as_str()
            );
            let ratio = contrast_ratio(theme.surface.prompt, theme.surface.canvas)
                .expect("fixed palettes are RGB");
            assert!(
                ratio <= 1.25,
                "{}: prompt/canvas contrast {ratio:.2} is too card-like",
                theme.id.as_str()
            );
        }
    }

    #[test]
    fn no_color_override_wins() {
        for id in ThemeId::ALL {
            let t = Theme::resolve(id, true);
            assert!(t.is_monochrome(), "{id}");
            assert_eq!(t.accent.primary, Color::Reset);
            assert_eq!(t.text.primary, Color::Reset);
            assert_eq!(t.text.final_lead, Color::Reset);
            assert_eq!(t.text.strong, Color::Reset);
            assert_eq!(t.text.code, Color::Reset);
            assert_eq!(t.surface.canvas, Color::Reset);
            assert_eq!(t.id, id, "preference id retained under NO_COLOR");
        }
    }

    #[test]
    fn apple_terminal_before_macos_26_requires_indexed_colors() {
        assert!(old_apple_terminal(Some("Apple_Terminal"), Some("15.7.1")));
        assert!(old_apple_terminal(Some("Apple_Terminal"), None));
        assert!(!old_apple_terminal(Some("Apple_Terminal"), Some("26.0")));
        assert!(!old_apple_terminal(Some("iTerm.app"), Some("15.7.1")));
    }

    /// The 256-colour palette cannot paint a soft directional row band: the
    /// cube's nearest hue step is a saturated slab. The contract is a neutral
    /// row band plus directional target on marker and inline step, and every
    /// syntax ink stays readable on that band.
    #[test]
    fn indexed_256_uses_a_neutral_row_band_with_directional_targets() {
        for theme in [Theme::dark(), Theme::light(), Theme::high_contrast()] {
            let indexed = indexed_256_theme(theme);
            for (name, color) in [
                ("added_bg", indexed.diff.added_bg),
                ("removed_bg", indexed.diff.removed_bg),
            ] {
                let Color::Indexed(index) = color else {
                    panic!("{name}: expected an indexed colour, got {color:?}");
                };
                let (r, g, b) = xterm_256_rgb(index);
                assert!(
                    r == g && g == b,
                    "{name}: index {index} {r},{g},{b} must stay neutral"
                );
            }
            assert_ne!(
                indexed.diff.added_bg, indexed.surface.canvas,
                "the row band must be visible against the canvas"
            );
            for (name, color, dominant) in [
                ("added_inline_bg", indexed.diff.added_inline_bg, 1usize),
                ("removed_inline_bg", indexed.diff.removed_inline_bg, 0),
            ] {
                let Color::Indexed(index) = color else {
                    panic!("{name}: expected an indexed colour, got {color:?}");
                };
                let channels = {
                    let (r, g, b) = xterm_256_rgb(index);
                    [r, g, b]
                };
                let others: Vec<u8> = channels
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != dominant)
                    .map(|(_, c)| *c)
                    .collect();
                assert!(
                    channels[dominant] > others[0] && channels[dominant] > others[1],
                    "{name}: index {index} {channels:?} lost its direction hue"
                );
            }
            assert_ne!(
                indexed.diff.added_bg, indexed.diff.added_inline_bg,
                "the add inline step must stay visible on the row band"
            );
            assert_ne!(
                indexed.diff.removed_bg, indexed.diff.removed_inline_bg,
                "the remove inline step must stay visible on the row band"
            );
            for (name, ink) in [
                ("keyword", indexed.syntax.keyword),
                ("string", indexed.syntax.string),
                ("type", indexed.syntax.type_name),
                ("function", indexed.syntax.function),
                ("variable", indexed.syntax.variable),
                ("comment", indexed.syntax.comment),
                ("number", indexed.syntax.number),
                ("plain", indexed.syntax.plain),
            ] {
                assert_min(
                    ink,
                    indexed.diff.added_bg,
                    4.5,
                    &format!("256 {name}/added_bg"),
                );
            }
        }
    }

    /// The four terminal capabilities the renderer must survive, each with
    /// its unambiguous signal.
    #[test]
    fn terminal_capability_is_classified_from_the_environment() {
        assert_eq!(
            color_depth_for(Some("xterm-256color"), Some("truecolor"), false),
            ColorDepth::TrueColor
        );
        assert_eq!(
            color_depth_for(Some("xterm-256color"), Some("24bit"), false),
            ColorDepth::TrueColor
        );
        assert_eq!(
            color_depth_for(Some("xterm-256color"), None, false),
            ColorDepth::Ansi256
        );
        assert_eq!(
            color_depth_for(Some("tmux"), None, false),
            ColorDepth::Ansi256
        );
        assert_eq!(
            color_depth_for(Some("xterm"), None, false),
            ColorDepth::Ansi16
        );
        assert_eq!(
            color_depth_for(Some("vt100"), None, false),
            ColorDepth::Ansi16
        );
        // Old Apple Terminal can paint 256 but not truecolor.
        assert_eq!(
            color_depth_for(Some("xterm-256color"), None, true),
            ColorDepth::Ansi256
        );
        // No terminal identity keeps the historical full palette.
        assert_eq!(color_depth_for(None, None, false), ColorDepth::TrueColor);
        assert_eq!(
            color_depth_for(Some(""), None, false),
            ColorDepth::TrueColor
        );
        // A modern truecolor terminal that only names itself in TERM.
        assert_eq!(
            color_depth_for(Some("xterm-kitty"), None, false),
            ColorDepth::TrueColor
        );
    }

    /// 16-colour degradation emits named ANSI colours only, and the palette
    /// still knows its own polarity (light stays light).
    #[test]
    fn ansi16_compatibility_uses_named_colors_and_keeps_polarity() {
        for id in ThemeId::FIXED {
            let theme = ansi16_theme(Theme::named(id));
            for (name, color) in [
                ("canvas", theme.surface.canvas),
                ("input", theme.surface.input),
                ("primary", theme.text.primary),
                ("muted", theme.text.muted),
                ("accent", theme.accent.primary),
                ("added", theme.diff.added),
                ("removed", theme.diff.removed),
                ("added_bg", theme.diff.added_bg),
            ] {
                assert!(
                    !matches!(color, Color::Rgb(..) | Color::Indexed(_)),
                    "{id}: {name} must be a named ANSI colour, got {color:?}"
                );
            }
            assert_eq!(theme.color_depth(), ColorDepth::Ansi16);
        }
        assert!(ansi16_theme(Theme::dark()).is_dark());
        assert!(!ansi16_theme(Theme::light()).is_dark());
    }

    /// The 16-colour depth cannot carry all eight syntax roles, but the roles
    /// it keeps must be legible on the canvas and still tell keyword, string,
    /// type and call apart.
    #[test]
    fn ansi16_keeps_the_syntax_roles_legible_and_separate() {
        for (id, minimum) in [(ThemeId::Dark, 6), (ThemeId::Light, 4)] {
            let theme = ansi16_theme(Theme::named(id));
            let inks = [
                theme.syntax.keyword,
                theme.syntax.string,
                theme.syntax.type_name,
                theme.syntax.function,
                theme.syntax.variable,
                theme.syntax.comment,
                theme.syntax.number,
                theme.syntax.plain,
            ];
            let distinct: std::collections::HashSet<_> = inks.iter().collect();
            assert!(
                distinct.len() >= minimum,
                "{id}: 16-colour syntax collapses to {} inks: {inks:?}",
                distinct.len()
            );
            for ink in inks {
                assert!(
                    !matches!(ink, Color::Rgb(..) | Color::Indexed(_)),
                    "{id}: syntax ink must be a named ANSI colour, got {ink:?}"
                );
                assert_min(ink, theme.surface.canvas, 4.5, "syntax/canvas");
            }
        }
    }

    #[test]
    fn color_depth_is_derived_from_the_resolved_palette() {
        assert_eq!(Theme::dark().color_depth(), ColorDepth::TrueColor);
        assert_eq!(
            indexed_256_theme(Theme::dark()).color_depth(),
            ColorDepth::Ansi256
        );
        assert_eq!(
            ansi16_theme(Theme::dark()).color_depth(),
            ColorDepth::Ansi16
        );
        assert_eq!(Theme::no_color().color_depth(), ColorDepth::None);
    }

    #[test]
    fn indexed_compatibility_replaces_rgb_tokens_without_becoming_monochrome() {
        let theme = indexed_256_theme(Theme::dark());
        assert!(!theme.is_monochrome());
        for color in [
            theme.surface.canvas,
            theme.surface.input,
            theme.text.primary,
            theme.text.final_lead,
            theme.text.code,
            theme.text.muted,
            theme.border.focus,
            theme.accent.primary,
            theme.status.error,
            theme.diff.added_bg,
            theme.brand.highlight,
        ] {
            assert!(matches!(color, Color::Indexed(_)), "{color:?}");
        }
    }

    #[test]
    fn indexed_compatibility_preserves_theme_polarity() {
        assert!(indexed_256_theme(Theme::dark()).is_dark());
        assert!(!indexed_256_theme(Theme::light()).is_dark());
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

    /// The final-answer prose is its own reading role: never a value borrowed
    /// from ordinary prose, an accent, code, or a status. If a palette edit ever
    /// collapses it back onto one of those, the final answer stops reading as a
    /// distinct block of writing and only the marker is left to tell it apart.
    #[test]
    fn final_lead_is_an_independent_reading_role() {
        for id in ThemeId::FIXED {
            let t = Theme::named(id);
            for (name, other) in [
                ("primary", t.text.primary),
                ("accent.primary", t.accent.primary),
                ("accent.secondary", t.accent.secondary),
                ("code", t.text.code),
                ("status.success", t.status.success),
                ("status.warning", t.status.warning),
                ("status.error", t.status.error),
            ] {
                assert_ne!(
                    t.text.final_lead, other,
                    "{id}: final_lead must not equal {name}"
                );
            }
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
            "Final lead",
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
