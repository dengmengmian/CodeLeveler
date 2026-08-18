//! Conversation empty-state: a terminal-native hero card.
//!
//! Shown only while Conversation has no real work. First real message hides
//! it. Copy comes from [`UiText`]; command descriptions reuse slash briefs.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::i18n::UiText;
use crate::state::AppState;
use crate::theme::Theme;
use crate::transcript::TranscriptItem;

/// First-run commands only. Not a catalog — `/` still lists the rest.
const TIP_COMMANDS: &[&str] = &["/feature-dev", "/model", "/help"];

const CTA_CARET: &str = "› ";

const PREFERRED_BOX_W: usize = 78;
const MIN_BOX_W: usize = 56;
const MAX_BOX_W: usize = 84;
const H_MARGIN: usize = 2;
const INNER_PAD_X: usize = 2;

/// Whether Conversation is still empty of real turns (welcome/btw ignored).
pub(crate) fn conversation_is_empty(state: &AppState) -> bool {
    !state.transcript.items().iter().any(|item| {
        matches!(
            item,
            TranscriptItem::User(_)
                | TranscriptItem::Assistant(_)
                | TranscriptItem::ToolGroup(_)
                | TranscriptItem::SubAgent(_)
                | TranscriptItem::Completion(_)
                | TranscriptItem::Error(_)
                | TranscriptItem::Note(_)
                | TranscriptItem::TurnEnd(_)
                | TranscriptItem::UserShell(_)
                | TranscriptItem::Recap(_)
        )
    })
}

/// Splash lines for the empty Conversation viewport.
pub(crate) fn splash_lines(
    state: &AppState,
    width: usize,
    height: usize,
    theme: &Theme,
    t: &UiText,
) -> Vec<Line<'static>> {
    let width = width.max(16);
    let height = height.max(1);
    let sty = SplashStyle::from_theme(theme);

    let version_s = state.version();
    let brand_text = format!("CodeLeveler  v{version_s}");
    let name_col = command_name_col();
    let extras = untrusted_extras(state, t);
    let extra_refs: Vec<&str> = extras.iter().map(String::as_str).collect();
    let right_needed = content_width(t, &brand_text, name_col, &extra_refs);

    let mode = layout_mode(width, height);
    if matches!(mode, Mode::Stack) {
        return fallback_stack(&brand_text, t, name_col, width, &sty);
    }

    let mascot = match mode {
        Mode::Wide => Some(MascotKind::Wide),
        Mode::Medium => Some(MascotKind::Medium),
        Mode::Narrow => Some(MascotKind::Mark),
        Mode::Stack => None,
    };
    let (mascot_rows, mascot_w) = mascot
        .map(|k| (k.rows(), k.width()))
        .unwrap_or((&[] as &[MascotRow], 0));
    let gap = match mode {
        Mode::Wide => 3,
        Mode::Medium => 2,
        Mode::Narrow | Mode::Stack => 2,
    };

    let chrome = 2 + INNER_PAD_X * 2; // │…│ plus inner pads
    let inner_needed = mascot_w.saturating_add(gap).saturating_add(right_needed);
    let mut box_w = PREFERRED_BOX_W
        .min(width.saturating_sub(H_MARGIN))
        .clamp(MIN_BOX_W.min(width.saturating_sub(H_MARGIN)), MAX_BOX_W);
    box_w = box_w.max((inner_needed + chrome).min(MAX_BOX_W));
    box_w = box_w.min(width.saturating_sub(H_MARGIN));
    if box_w < 20 {
        return fallback_stack(&brand_text, t, name_col, width, &sty);
    }

    let inner = box_w.saturating_sub(2);
    let content_budget = inner.saturating_sub(INNER_PAD_X * 2);
    let right_w = content_budget
        .saturating_sub(mascot_w)
        .saturating_sub(if mascot_w > 0 { gap } else { 0 });
    let box_left_pad = width.saturating_sub(box_w) / 2;
    let show_section_gap = height >= 14;

    let mut right: Vec<Vec<Span<'static>>> = Vec::new();
    right.push(vec![
        Span::styled("CodeLeveler".to_string(), sty.brand),
        Span::styled(format!("  v{version_s}"), sty.version),
    ]);
    right.push(vec![Span::styled(
        truncate_w(t.splash_tagline, right_w),
        sty.tagline,
    )]);
    if show_section_gap {
        right.push(Vec::new());
    }
    right.push(cta_spans(t, right_w, &sty));
    if show_section_gap {
        right.push(Vec::new());
    }
    for name in TIP_COMMANDS {
        let label = crate::screen::slash_popup_label(name, t);
        let label = truncate_w(label, right_w.saturating_sub(name_col));
        right.push(vec![
            Span::styled(pad_to(name, name_col), sty.cmd),
            Span::styled(label, sty.desc),
        ]);
    }
    if show_section_gap {
        right.push(Vec::new());
    }
    right.push(vec![Span::styled(
        truncate_w(t.splash_more_commands, right_w),
        sty.muted,
    )]);

    if !state.untrusted_config.is_empty() {
        let warn = Style::default().fg(theme.status.warning);
        right.push(Vec::new());
        right.push(vec![Span::styled(
            truncate_w(&format!("⚠ {}", t.untrusted_config_title), right_w),
            warn,
        )]);
        for path in &state.untrusted_config {
            right.push(vec![Span::styled(
                truncate_w(&format!("  {path}"), right_w),
                sty.muted,
            )]);
        }
        right.push(vec![Span::styled(
            truncate_w(t.untrusted_config_hint, right_w),
            sty.muted,
        )]);
    }

    let body_rows = right.len().max(mascot_rows.len());
    while right.len() < body_rows {
        right.push(Vec::new());
    }

    let show_vpad = height >= 16;
    let mut out = Vec::new();
    out.push(border_line("╭", "╮", box_w, box_left_pad, sty.border));
    if show_vpad {
        out.push(blank_row(box_w, box_left_pad, sty.border));
    }
    for (i, right_spans) in right.into_iter().enumerate() {
        let mascot_spans = mascot_rows
            .get(i)
            .map(|row| paint_mascot_row(row, &sty))
            .unwrap_or_default();
        out.push(content_row(
            mascot_spans,
            mascot_w,
            gap,
            right_spans,
            right_w,
            box_left_pad,
            &sty,
        ));
    }
    if show_vpad {
        out.push(blank_row(box_w, box_left_pad, sty.border));
    }
    out.push(border_line("╰", "╯", box_w, box_left_pad, sty.border));
    out
}

#[derive(Clone, Copy)]
enum Mode {
    Wide,
    Medium,
    Narrow,
    Stack,
}

fn layout_mode(width: usize, height: usize) -> Mode {
    if height < 10 || width < 40 {
        Mode::Stack
    } else if width >= 72 && height >= 14 {
        Mode::Wide
    } else if width >= 56 {
        Mode::Medium
    } else {
        Mode::Narrow
    }
}

fn cta_spans(t: &UiText, right_w: usize, sty: &SplashStyle) -> Vec<Span<'static>> {
    let caret_w = disp_w(CTA_CARET);
    let text = truncate_w(t.splash_start_task, right_w.saturating_sub(caret_w));
    vec![
        Span::styled(CTA_CARET.to_string(), sty.caret),
        Span::styled(text, sty.cta),
    ]
}

fn untrusted_extras(state: &AppState, t: &UiText) -> Vec<String> {
    if state.untrusted_config.is_empty() {
        return Vec::new();
    }
    let mut extras = vec![format!("⚠ {}", t.untrusted_config_title)];
    extras.extend(state.untrusted_config.iter().map(|p| format!("  {p}")));
    extras.push(t.untrusted_config_hint.to_string());
    extras
}

struct SplashStyle {
    brand: Style,
    version: Style,
    tagline: Style,
    caret: Style,
    cta: Style,
    muted: Style,
    cmd: Style,
    desc: Style,
    border: Style,
    fur: Style,
    dark: Style,
    face: Style,
    detail: Style,
}

impl SplashStyle {
    fn from_theme(theme: &Theme) -> Self {
        Self {
            brand: Style::default()
                .fg(theme.accent.primary)
                .add_modifier(Modifier::BOLD),
            version: Style::default().fg(theme.text.muted),
            tagline: Style::default().fg(theme.text.secondary),
            caret: Style::default().fg(theme.accent.primary),
            cta: Style::default()
                .fg(theme.text.primary)
                .add_modifier(Modifier::BOLD),
            muted: Style::default().fg(theme.text.muted),
            cmd: Style::default().fg(theme.accent.primary),
            desc: Style::default().fg(theme.text.secondary),
            border: Style::default().fg(theme.border.subtle),
            fur: Style::default().fg(theme.mascot.fur),
            dark: Style::default().fg(theme.mascot.dark),
            face: Style::default().fg(theme.mascot.face),
            detail: Style::default().fg(theme.mascot.detail),
        }
    }

    fn ink(&self, ink: Ink) -> Style {
        match ink {
            Ink::Fur => self.fur,
            Ink::Dark => self.dark,
            Ink::Face => self.face,
            Ink::Detail => self.detail,
            Ink::Brand => self.brand,
            Ink::Blank => Style::default(),
        }
    }
}

#[derive(Clone, Copy)]
enum Ink {
    Fur,
    Dark,
    Face,
    Detail,
    Brand,
    Blank,
}

#[derive(Clone, Copy)]
struct MascotSpan {
    text: &'static str,
    ink: Ink,
}

type MascotRow = &'static [MascotSpan];

#[derive(Clone, Copy)]
enum MascotKind {
    Wide,
    Medium,
    Mark,
}

impl MascotKind {
    fn rows(self) -> &'static [MascotRow] {
        match self {
            Self::Wide => MASCOT_WIDE,
            Self::Medium => MASCOT_MEDIUM,
            Self::Mark => MASCOT_MARK,
        }
    }

    fn width(self) -> usize {
        self.rows()
            .iter()
            .map(|row| row.iter().map(|s| disp_w(s.text)).sum())
            .max()
            .unwrap_or(0)
    }
}

/// 14×8 red-panda mark. Rows are display-width aligned.
const MASCOT_WIDE: &[MascotRow] = &[
    &[
        sp("    ", Ink::Blank),
        sp("▄▄██▄▄", Ink::Fur),
        sp("    ", Ink::Blank),
    ],
    &[
        sp("  ", Ink::Blank),
        sp("▄████████▄", Ink::Fur),
        sp("  ", Ink::Blank),
    ],
    &[
        sp(" ", Ink::Blank),
        sp("██", Ink::Fur),
        sp(" ", Ink::Blank),
        sp("▀████▀", Ink::Dark),
        sp(" ", Ink::Blank),
        sp("██", Ink::Fur),
        sp(" ", Ink::Blank),
    ],
    &[
        sp(" ", Ink::Blank),
        sp("██", Ink::Fur),
        sp("  ", Ink::Blank),
        sp("•", Ink::Detail),
        sp("  ", Ink::Blank),
        sp("•", Ink::Detail),
        sp("  ", Ink::Blank),
        sp("██", Ink::Fur),
        sp(" ", Ink::Blank),
    ],
    &[
        sp("  ", Ink::Blank),
        sp("█▄", Ink::Fur),
        sp("  ", Ink::Blank),
        sp("▽", Ink::Face),
        sp("  ", Ink::Blank),
        sp("▄█", Ink::Fur),
        sp("   ", Ink::Blank),
    ],
    &[
        sp("   ", Ink::Blank),
        sp("▀██████▀", Ink::Fur),
        sp("   ", Ink::Blank),
    ],
    &[
        sp("  ", Ink::Blank),
        sp("▄██▀", Ink::Fur),
        sp("  ", Ink::Blank),
        sp("▀██▄", Ink::Fur),
        sp("  ", Ink::Blank),
    ],
    &[
        sp("    ", Ink::Blank),
        sp("╲", Ink::Fur),
        sp(" ", Ink::Blank),
        sp("CL", Ink::Brand),
        sp(" ", Ink::Blank),
        sp("╱", Ink::Fur),
        sp("    ", Ink::Blank),
    ],
];

/// 10×6 compact variant.
const MASCOT_MEDIUM: &[MascotRow] = &[
    &[
        sp("  ", Ink::Blank),
        sp("▄▄██▄▄", Ink::Fur),
        sp("  ", Ink::Blank),
    ],
    &[sp("▄████████▄", Ink::Fur)],
    &[
        sp("██", Ink::Fur),
        sp(" ", Ink::Blank),
        sp("•", Ink::Detail),
        sp("  ", Ink::Blank),
        sp("•", Ink::Detail),
        sp(" ", Ink::Blank),
        sp("██", Ink::Fur),
    ],
    &[
        sp(" ", Ink::Blank),
        sp("█▄", Ink::Fur),
        sp(" ", Ink::Blank),
        sp("▽", Ink::Face),
        sp(" ", Ink::Blank),
        sp("▄█", Ink::Fur),
        sp("  ", Ink::Blank),
    ],
    &[
        sp("  ", Ink::Blank),
        sp("▀████▀", Ink::Fur),
        sp("  ", Ink::Blank),
    ],
    &[
        sp("   ", Ink::Blank),
        sp("╲", Ink::Fur),
        sp("CL", Ink::Brand),
        sp("╱", Ink::Fur),
        sp("   ", Ink::Blank),
    ],
];

const MASCOT_MARK: &[MascotRow] = &[&[sp("CL", Ink::Brand)]];

const fn sp(text: &'static str, ink: Ink) -> MascotSpan {
    MascotSpan { text, ink }
}

fn paint_mascot_row(row: MascotRow, sty: &SplashStyle) -> Vec<Span<'static>> {
    row.iter()
        .map(|cell| Span::styled(cell.text.to_string(), sty.ink(cell.ink)))
        .collect()
}

fn command_name_col() -> usize {
    TIP_COMMANDS
        .iter()
        .map(|n| disp_w(n))
        .max()
        .unwrap_or(0)
        .saturating_add(3)
}

fn content_width(t: &UiText, brand_text: &str, name_col: usize, extras: &[&str]) -> usize {
    let mut w = disp_w(brand_text);
    w = w.max(disp_w(t.splash_tagline));
    w = w.max(disp_w(CTA_CARET) + disp_w(t.splash_start_task));
    w = w.max(disp_w(t.splash_more_commands));
    for name in TIP_COMMANDS {
        let label = crate::screen::slash_popup_label(name, t);
        w = w.max(name_col + disp_w(label));
    }
    for extra in extras {
        w = w.max(disp_w(extra));
    }
    w
}

fn content_row(
    mascot: Vec<Span<'static>>,
    mascot_w: usize,
    gap: usize,
    right_spans: Vec<Span<'static>>,
    right_w: usize,
    left_pad: usize,
    sty: &SplashStyle,
) -> Line<'static> {
    let mut spans = Vec::new();
    if left_pad > 0 {
        spans.push(Span::raw(" ".repeat(left_pad)));
    }
    spans.push(Span::styled("│".to_string(), sty.border));
    spans.push(Span::raw(" ".repeat(INNER_PAD_X)));

    let mut used_m = 0usize;
    for s in mascot {
        used_m += disp_w(s.content.as_ref());
        spans.push(s);
    }
    if mascot_w > used_m {
        spans.push(Span::raw(" ".repeat(mascot_w - used_m)));
    }
    if mascot_w > 0 {
        spans.push(Span::raw(" ".repeat(gap)));
    }

    let mut used = 0usize;
    for s in right_spans {
        used += disp_w(s.content.as_ref());
        spans.push(s);
    }
    if right_w > used {
        spans.push(Span::raw(" ".repeat(right_w - used)));
    }

    spans.push(Span::raw(" ".repeat(INNER_PAD_X)));
    spans.push(Span::styled("│".to_string(), sty.border));
    Line::from(spans)
}

fn blank_row(box_w: usize, left_pad: usize, border: Style) -> Line<'static> {
    let inner = box_w.saturating_sub(2);
    let mut spans = Vec::new();
    if left_pad > 0 {
        spans.push(Span::raw(" ".repeat(left_pad)));
    }
    spans.push(Span::styled("│".to_string(), border));
    spans.push(Span::raw(" ".repeat(inner)));
    spans.push(Span::styled("│".to_string(), border));
    Line::from(spans)
}

fn border_line(
    left: &str,
    right: &str,
    box_w: usize,
    left_pad: usize,
    border: Style,
) -> Line<'static> {
    let mut spans = Vec::new();
    if left_pad > 0 {
        spans.push(Span::raw(" ".repeat(left_pad)));
    }
    let bar = format!("{left}{}{right}", "─".repeat(box_w.saturating_sub(2)));
    spans.push(Span::styled(bar, border));
    Line::from(spans)
}

fn fallback_stack(
    brand_text: &str,
    t: &UiText,
    name_col: usize,
    width: usize,
    sty: &SplashStyle,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    out.push(Line::from(Span::styled(
        truncate_w(brand_text, width),
        sty.brand,
    )));
    out.push(Line::from(Span::styled(
        truncate_w(t.splash_tagline, width),
        sty.tagline,
    )));
    out.push(Line::from(cta_spans(t, width, sty)));
    for name in TIP_COMMANDS {
        let label = crate::screen::slash_popup_label(name, t);
        let label = truncate_w(label, width.saturating_sub(name_col));
        out.push(Line::from(vec![
            Span::styled(pad_to(name, name_col.min(width)), sty.cmd),
            Span::styled(label, sty.desc),
        ]));
    }
    out.push(Line::from(Span::styled(
        truncate_w(t.splash_more_commands, width),
        sty.muted,
    )));
    out
}

fn disp_w(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

fn pad_to(s: &str, target: usize) -> String {
    crate::render::pad_to_width(s, target)
}

fn truncate_w(s: &str, max: usize) -> String {
    crate::render::truncate_display(s, max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Locale;
    use crate::state::Boot;
    use crate::theme::ThemeId;
    use leveler_client_protocol::SessionId;
    use ratatui::style::Color;

    fn state(locale: Locale) -> AppState {
        AppState::new(
            Theme::no_color(),
            Boot {
                session_id: SessionId::new("s1"),
                user: "u".into(),
                version: "0.1.4".into(),
                show_welcome: true,
                draft_path: None,
                history_path: None,
                context_window: 0,
                locale,
                untrusted_config: Vec::new(),
                reasoning_effort: None,
            },
        )
    }

    fn line_plain(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|sp| sp.content.as_ref())
            .collect::<String>()
    }

    fn joined(lines: &[Line<'static>]) -> String {
        lines.iter().map(line_plain).collect::<Vec<_>>().join("\n")
    }

    fn paint(s: &AppState, width: usize, height: usize) -> String {
        joined(&splash_lines(s, width, height, &s.theme, s.t()))
    }

    fn box_width(text: &str) -> usize {
        text.lines()
            .filter_map(|l| {
                let t = l.trim_end();
                (t.contains('╭') || t.contains('╰') || t.ends_with('│')).then(|| disp_w(t.trim()))
            })
            .max()
            .unwrap_or(0)
    }

    #[test]
    fn mascot_rows_are_display_aligned() {
        for kind in [MascotKind::Wide, MascotKind::Medium, MascotKind::Mark] {
            let w = kind.width();
            for row in kind.rows() {
                let row_w: usize = row.iter().map(|s| disp_w(s.text)).sum();
                assert_eq!(row_w, w, "mascot row misaligned: {row_w} != {w}");
            }
            assert!(w <= 16, "mascot wider than 16: {w}");
            assert!(kind.rows().len() <= 9, "mascot taller than 9");
        }
    }

    #[test]
    fn zh_tagline_not_english() {
        let text = paint(&state(Locale::Zh), 80, 24);
        assert!(text.contains("让模型真正可靠地完成任务"), "{text}");
        assert!(
            !text.contains("Make models reliably complete tasks"),
            "{text}"
        );
        assert!(!text.contains("AI Coding Agent"), "{text}");
    }

    #[test]
    fn en_tagline_not_chinese() {
        let text = paint(&state(Locale::En), 80, 24);
        assert!(
            text.contains("Make models reliably complete tasks"),
            "{text}"
        );
        assert!(!text.contains("让模型真正可靠地完成任务"), "{text}");
        assert!(!text.contains("AI Coding Agent"), "{text}");
    }

    #[test]
    fn zh_cta_and_more() {
        let text = paint(&state(Locale::Zh), 80, 24);
        assert!(text.contains("输入任务，直接开始"), "{text}");
        assert!(text.contains('›'), "cta caret missing: {text}");
        assert!(text.contains("输入 / 查看更多命令"), "{text}");
    }

    #[test]
    fn en_cta_and_more() {
        let text = paint(&state(Locale::En), 80, 24);
        assert!(text.contains("Start by entering a task"), "{text}");
        assert!(text.contains('›'), "{text}");
        assert!(text.contains("Type / for more commands"), "{text}");
    }

    #[test]
    fn both_locales_list_the_three_commands() {
        for locale in [Locale::Zh, Locale::En] {
            let text = paint(&state(locale), 80, 24);
            for cmd in TIP_COMMANDS {
                assert!(text.contains(cmd), "{locale:?} missing {cmd}: {text}");
            }
        }
    }

    #[test]
    fn splash_does_not_list_advanced_commands() {
        for locale in [Locale::Zh, Locale::En] {
            let text = paint(&state(locale), 80, 24);
            for cmd in ["/plan", "/goal", "/skill", "/permission", "/work-mode"] {
                assert!(!text.contains(cmd), "{locale:?} leaked {cmd}: {text}");
            }
        }
    }

    #[test]
    fn slash_registry_still_has_commands_removed_from_splash() {
        for locale in [Locale::Zh, Locale::En] {
            let listed = crate::screen::slash_commands(locale.text());
            for cmd in ["/plan", "/goal", "/skill", "/permission", "/work-mode"] {
                assert!(
                    listed.iter().any(|(n, _)| *n == cmd),
                    "{locale:?} registry lost {cmd}"
                );
            }
        }
    }

    #[test]
    fn zh_command_descriptions() {
        let text = paint(&state(Locale::Zh), 80, 24);
        assert!(text.contains("功能实现"), "{text}");
        assert!(text.contains("切换模型"), "{text}");
        assert!(text.contains("查看帮助"), "{text}");
    }

    #[test]
    fn en_command_descriptions() {
        let text = paint(&state(Locale::En), 80, 24);
        assert!(text.contains("Implement a feature"), "{text}");
        assert!(text.contains("Switch model"), "{text}");
        assert!(text.contains("View help"), "{text}");
    }

    #[test]
    fn empty_conversation_is_splash() {
        assert!(conversation_is_empty(&state(Locale::Zh)));
    }

    #[test]
    fn first_user_message_hides_splash() {
        let mut s = state(Locale::Zh);
        s.transcript.push_user("hello".into());
        assert!(!conversation_is_empty(&s));
    }

    #[test]
    fn wide_card_is_a_hero_not_a_hint() {
        let text = paint(&state(Locale::Zh), 120, 36);
        let w = box_width(&text);
        assert!(
            (72..=84).contains(&w),
            "120-col card should be 72–84, got {w}:\n{text}"
        );
        assert!(text.contains('▄') || text.contains('▀'), "mascot missing");
        assert!(text.contains("CL"), "brand mark on mascot: {text}");
    }

    #[test]
    fn mascot_hides_on_narrow_layout() {
        let wide = paint(&state(Locale::Zh), 80, 24);
        assert!(wide.contains('▄') || wide.contains('▀'), "{wide}");
        let narrow = paint(&state(Locale::Zh), 40, 16);
        assert!(narrow.contains("CodeLeveler"), "{narrow}");
        assert!(narrow.contains("/feature-dev"), "{narrow}");
        assert!(
            !narrow.contains("▀██████▀"),
            "full mascot leaked into narrow: {narrow}"
        );
    }

    #[test]
    fn width_matrix_does_not_overflow_or_overlap() {
        let name_col = command_name_col();
        for locale in [Locale::Zh, Locale::En] {
            let s = state(locale);
            for width in [120, 100, 80, 72, 64, 56, 48, 40] {
                let lines = splash_lines(&s, width, 24, &s.theme, s.t());
                let text = joined(&lines);
                for cmd in TIP_COMMANDS {
                    assert!(text.contains(cmd), "{locale:?} {width}: missing {cmd}");
                }
                for line in &lines {
                    let plain = line_plain(line);
                    let w = disp_w(plain.trim_end());
                    assert!(w <= width, "{locale:?} w={width} overflow {w}: {plain:?}");
                    if let Some(pos) = plain.find("/feature-dev") {
                        let after = &plain[pos + "/feature-dev".len()..];
                        let pad = after.chars().take_while(|c| *c == ' ').count();
                        let start = "/feature-dev".len() + pad;
                        assert!(
                            start >= name_col || after.trim_start().is_empty(),
                            "{locale:?} w={width} desc overlaps name: {plain:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn height_matrix_does_not_panic() {
        for locale in [Locale::Zh, Locale::En] {
            let s = state(locale);
            for height in [36, 24, 20, 16, 12] {
                let text = paint(&s, 80, height);
                assert!(text.contains("CodeLeveler"), "{locale:?} h={height}");
                assert!(text.contains("/feature-dev"), "{locale:?} h={height}");
            }
        }
    }

    #[test]
    fn themes_use_only_semantic_tokens() {
        for id in ThemeId::FIXED {
            let theme = Theme::named(id);
            let mut s = state(Locale::Zh);
            s.theme = theme.clone();
            let lines = splash_lines(&s, 80, 24, &s.theme, s.t());
            let allowed = [
                theme.accent.primary,
                theme.text.primary,
                theme.text.secondary,
                theme.text.muted,
                theme.border.subtle,
                theme.border.normal,
                theme.status.warning,
                theme.mascot.fur,
                theme.mascot.dark,
                theme.mascot.face,
                theme.mascot.detail,
            ];
            for line in &lines {
                for span in &line.spans {
                    if let Some(fg) = span.style.fg {
                        assert!(
                            allowed.contains(&fg) || fg == Color::Reset,
                            "{id:?} unexpected fg {fg:?} on {:?}",
                            span.content
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn splash_warns_about_ignored_in_repo_config() {
        let mut s = state(Locale::Zh);
        let clean = paint(&s, 80, 24);
        assert!(!clean.contains('⚠'), "clean repo shows no warning: {clean}");

        s.untrusted_config = vec![".leveler/hooks.yaml".to_string()];
        let text = paint(&s, 80, 24);
        assert!(text.contains('⚠'), "{text}");
        assert!(text.contains("hooks.yaml"), "name the file: {text}");
        assert!(text.contains("leveler trust"), "name the fix: {text}");
    }

    #[test]
    fn warning_rows_keep_the_box_aligned() {
        let mut s = state(Locale::Zh);
        s.untrusted_config = vec![
            ".leveler/hooks.yaml".to_string(),
            ".leveler/permissions.yaml".to_string(),
        ];
        assert_box_aligned(&splash_lines(&s, 80, 24, &s.theme, s.t()));
    }

    #[test]
    fn box_rows_are_width_aligned() {
        let s = state(Locale::Zh);
        assert_box_aligned(&splash_lines(&s, 80, 24, &s.theme, s.t()));
        let s = state(Locale::En);
        assert_box_aligned(&splash_lines(&s, 80, 24, &s.theme, s.t()));
    }

    fn assert_box_aligned(lines: &[Line<'static>]) {
        let edges: Vec<usize> = lines
            .iter()
            .filter_map(|l| {
                let plain = line_plain(l);
                let trimmed = plain.trim_end();
                (trimmed.ends_with('│') || trimmed.ends_with('╮') || trimmed.ends_with('╯'))
                    .then(|| disp_w(trimmed))
            })
            .collect();
        assert!(edges.len() >= 5, "{edges:?}");
        assert!(
            edges.iter().all(|w| *w == edges[0]),
            "right borders misaligned: {edges:?}"
        );
    }

    #[test]
    fn narrow_terminal_keeps_copy_without_panic() {
        let s = state(Locale::Zh);
        let text = paint(&s, 40, 16);
        assert!(text.contains("CodeLeveler"), "{text}");
        assert!(text.contains("让模型真正可靠地完成任务"), "{text}");
        assert!(text.contains("/feature-dev"), "{text}");
    }
}
