//! Conversation empty-state: a terminal-native brand hero card.
//!
//! Shown only while Conversation has no real work. First real message hides
//! it. Copy comes from [`UiText`]; the Level Mark comes from [`crate::brand`].

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::brand::{BrandMarkSize, BrandStyles, level_mark, level_mark_width, paint_mark_row};
use crate::i18n::UiText;
use crate::state::AppState;
use crate::theme::Theme;
use crate::transcript::TranscriptItem;

/// First-run commands only. Not a catalog — `/` still lists the rest.
const TIP_COMMANDS: &[&str] = &["/feature-dev", "/model", "/help"];

const CTA_CARET: &str = "› ";
const WORDMARK: &str = "CodeLeveler";

const PREFERRED_BOX_W: usize = 80;
const PREFERRED_BOX_H: usize = 16;
const MIN_BOX_W: usize = 56;
const MAX_BOX_W: usize = 86;
const WIDE_H_MARGIN: usize = 4;
const NARROW_H_MARGIN: usize = 2;
const INNER_PAD_X: usize = 2;
const WIDE_GAP: usize = 5;
const WIDE_CONTENT_W: usize = 42;

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
                | TranscriptItem::GoalRecap(_)
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
    let brand_text = format!("{WORDMARK}  v{version_s}");
    let name_col = command_name_col();
    let extras = untrusted_extras(state, t);
    let extra_refs: Vec<&str> = extras.iter().map(String::as_str).collect();
    let right_needed = content_width(t, &brand_text, name_col, &extra_refs);

    let mode = layout_mode(width, height);
    if matches!(mode, Mode::Stack) {
        return fallback_stack(&brand_text, t, name_col, width, &sty);
    }

    let h_margin = match mode {
        Mode::Wide => WIDE_H_MARGIN,
        Mode::Medium | Mode::Narrow | Mode::Stack => NARROW_H_MARGIN,
    };
    let gap = match mode {
        Mode::Wide => WIDE_GAP,
        Mode::Medium => 2,
        Mode::Narrow | Mode::Stack => 2,
    };
    let mark_size = match mode {
        Mode::Wide => Some(BrandMarkSize::Master),
        Mode::Medium => Some(BrandMarkSize::Compact),
        Mode::Narrow => Some(BrandMarkSize::Micro),
        Mode::Stack => None,
    };

    let chrome = 2 + INNER_PAD_X * 2;
    let mut box_w = PREFERRED_BOX_W
        .min(width.saturating_sub(h_margin))
        .clamp(MIN_BOX_W.min(width.saturating_sub(h_margin)), MAX_BOX_W);
    box_w = box_w.min(width.saturating_sub(h_margin));
    if box_w < 20 {
        return fallback_stack(&brand_text, t, name_col, width, &sty);
    }

    let inner = box_w.saturating_sub(2);
    let content_budget = inner.saturating_sub(INNER_PAD_X * 2);

    // Wordmark always beats the graphic. Hide the mark when it would clip copy.
    let mark_size = mark_size.filter(|size| {
        let mark_w = level_mark_width(*size);
        mark_w + gap + right_needed <= content_budget
    });
    let (mark_rows, mark_w) = match mark_size {
        Some(size) => (level_mark(size), level_mark_width(size)),
        None => (&[] as &[_], 0),
    };

    let inner_needed = mark_w.saturating_add(gap).saturating_add(right_needed);
    box_w = box_w.max((inner_needed + chrome).min(MAX_BOX_W));
    box_w = box_w.min(width.saturating_sub(h_margin));
    if box_w < 20 {
        return fallback_stack(&brand_text, t, name_col, width, &sty);
    }

    let inner = box_w.saturating_sub(2);
    let content_budget = inner.saturating_sub(INNER_PAD_X * 2);
    let content_w = hero_content_width(mode, right_needed, content_budget, mark_w, gap);
    let group_w = mark_w + if mark_w > 0 { gap } else { 0 } + content_w;
    let group_left = content_budget.saturating_sub(group_w) / 2;
    let box_left_pad = width.saturating_sub(box_w) / 2;
    let show_section_gap = height >= 14;

    let mut right: Vec<Vec<Span<'static>>> = Vec::new();
    right.push(vec![
        Span::styled(WORDMARK.to_string(), sty.brand),
        Span::styled(format!("  v{version_s}"), sty.version),
    ]);
    right.push(vec![Span::styled(
        truncate_w(t.splash_tagline, content_w),
        sty.tagline,
    )]);
    if show_section_gap {
        right.push(Vec::new());
    }
    right.push(cta_spans(t, content_w, &sty));
    if show_section_gap {
        right.push(Vec::new());
    }
    for name in TIP_COMMANDS {
        let label = crate::screen::slash_popup_label(name, t);
        let label = truncate_w(label, content_w.saturating_sub(name_col));
        right.push(vec![
            Span::styled(pad_to(name, name_col), sty.cmd),
            Span::styled(label, sty.desc),
        ]);
    }
    if show_section_gap {
        right.push(Vec::new());
    }
    right.push(more_spans(t.splash_more_commands, content_w, &sty));

    if !state.untrusted_config.is_empty() {
        let warn = Style::default().fg(theme.status.warning);
        right.push(Vec::new());
        right.push(vec![Span::styled(
            truncate_w(&format!("⚠ {}", t.untrusted_config_title), content_w),
            warn,
        )]);
        for path in &state.untrusted_config {
            right.push(vec![Span::styled(
                truncate_w(&format!("  {path}"), content_w),
                sty.muted,
            )]);
        }
        right.push(vec![Span::styled(
            truncate_w(t.untrusted_config_hint, content_w),
            sty.muted,
        )]);
    }

    let body_rows = right.len().max(mark_rows.len());
    while right.len() < body_rows {
        right.push(Vec::new());
    }

    let mut body = Vec::new();
    for (i, right_spans) in right.into_iter().enumerate() {
        let mark_spans = mark_rows
            .get(i)
            .map(|row| paint_mark_row(row, sty.mark))
            .unwrap_or_default();
        body.push(hero_row(
            mark_spans,
            mark_w,
            gap,
            right_spans,
            content_w,
            group_left,
            box_left_pad,
            box_w,
            &sty,
        ));
    }

    let min_h = body.len() + 2;
    let box_h = PREFERRED_BOX_H.min(height.max(min_h)).max(min_h);
    let extra = box_h.saturating_sub(2).saturating_sub(body.len());
    let vpad_top = extra / 2;
    let vpad_bot = extra - vpad_top;

    let mut out = Vec::new();
    out.push(border_line("╭", "╮", box_w, box_left_pad, sty.border));
    for _ in 0..vpad_top {
        out.push(blank_row(box_w, box_left_pad, sty.border));
    }
    out.extend(body);
    for _ in 0..vpad_bot {
        out.push(blank_row(box_w, box_left_pad, sty.border));
    }
    out.push(border_line("╰", "╯", box_w, box_left_pad, sty.border));
    out
}

fn hero_content_width(
    mode: Mode,
    needed: usize,
    budget: usize,
    mark_w: usize,
    gap: usize,
) -> usize {
    let reserved = mark_w + if mark_w > 0 { gap } else { 0 };
    let max_fit = budget.saturating_sub(reserved);
    if max_fit == 0 {
        return 0;
    }
    match mode {
        Mode::Wide => WIDE_CONTENT_W.min(max_fit),
        Mode::Medium | Mode::Narrow | Mode::Stack => needed.min(max_fit),
    }
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

fn more_spans(text: &str, right_w: usize, sty: &SplashStyle) -> Vec<Span<'static>> {
    let clipped = truncate_w(text, right_w);
    if let Some(i) = clipped.find('/') {
        vec![
            Span::styled(clipped[..i].to_string(), sty.muted),
            Span::styled("/".to_string(), sty.caret),
            Span::styled(clipped[i + 1..].to_string(), sty.muted),
        ]
    } else {
        vec![Span::styled(clipped, sty.muted)]
    }
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
    mark: BrandStyles,
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
            mark: BrandStyles::from_theme(theme),
        }
    }
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

fn hero_row(
    mark: Vec<Span<'static>>,
    mark_w: usize,
    gap: usize,
    content_spans: Vec<Span<'static>>,
    content_w: usize,
    group_left: usize,
    box_left_pad: usize,
    box_w: usize,
    sty: &SplashStyle,
) -> Line<'static> {
    let mut spans = Vec::new();
    if box_left_pad > 0 {
        spans.push(Span::raw(" ".repeat(box_left_pad)));
    }
    spans.push(Span::styled("│".to_string(), sty.border));
    spans.push(Span::raw(" ".repeat(INNER_PAD_X)));
    if group_left > 0 {
        spans.push(Span::raw(" ".repeat(group_left)));
    }

    let mut used_m = 0usize;
    for s in mark {
        used_m += disp_w(s.content.as_ref());
        spans.push(s);
    }
    if mark_w > used_m {
        spans.push(Span::raw(" ".repeat(mark_w - used_m)));
    }
    if mark_w > 0 {
        spans.push(Span::raw(" ".repeat(gap)));
    }

    let mut used = 0usize;
    for s in content_spans {
        used += disp_w(s.content.as_ref());
        spans.push(s);
    }
    if content_w > used {
        spans.push(Span::raw(" ".repeat(content_w - used)));
    }

    let used_inner = INNER_PAD_X
        + group_left
        + mark_w
        + if mark_w > 0 { gap } else { 0 }
        + content_w
        + INNER_PAD_X;
    let inner = box_w.saturating_sub(2);
    if inner > used_inner {
        spans.push(Span::raw(" ".repeat(inner - used_inner)));
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
    out.push(Line::from(more_spans(t.splash_more_commands, width, sty)));
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
    use crate::brand::{candidate_plain, mark_plain};
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

    fn box_height(text: &str) -> usize {
        text.lines()
            .filter(|l| {
                let t = l.trim_end();
                t.contains('╭') || t.contains('╰') || t.ends_with('│')
            })
            .count()
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
        let h = box_height(&text);
        assert!(
            (76..=86).contains(&w),
            "120-col card should be 76–86, got {w}:\n{text}"
        );
        assert!(
            (15..=18).contains(&h),
            "tall viewport card should be ~16 rows, got {h}:\n{text}"
        );
        assert!(
            text.contains("██      █████"),
            "master pier+runway missing:\n{text}"
        );
        assert!(!text.contains('◆'), "ghost core leaked:\n{text}");
        assert!(!text.contains('▿'), "ghost mouth leaked:\n{text}");
    }

    #[test]
    fn medium_uses_compact_mark() {
        let text = paint(&state(Locale::Zh), 64, 20);
        assert!(
            text.contains("██  ████"),
            "compact pier+runway missing:\n{text}"
        );
        assert!(
            !text.contains("    ██████"),
            "master mark leaked into medium:\n{text}"
        );
        assert!(text.contains("CodeLeveler"), "{text}");
    }

    #[test]
    fn narrow_uses_micro_or_hides_mark() {
        let wide = paint(&state(Locale::Zh), 80, 24);
        assert!(wide.contains("    ██████"), "master on wide:\n{wide}");
        let narrow = paint(&state(Locale::Zh), 48, 16);
        assert!(narrow.contains("CodeLeveler"), "{narrow}");
        assert!(narrow.contains("/feature-dev"), "{narrow}");
        assert!(
            !narrow.contains("    ██████"),
            "master leaked into narrow:\n{narrow}"
        );
        assert!(!narrow.contains('◆'), "{narrow}");
    }

    #[test]
    fn very_narrow_keeps_wordmark_over_mark() {
        let s = state(Locale::Zh);
        let text = paint(&s, 40, 16);
        assert!(text.contains("CodeLeveler"), "{text}");
        assert!(text.contains("让模型真正可靠地完成任务"), "{text}");
        assert!(text.contains("/feature-dev"), "{text}");
        assert!(
            !text.contains("    ██████"),
            "master leaked into 40-col:\n{text}"
        );
    }

    #[test]
    fn wordmark_is_codeleveler_not_screaming() {
        let text = paint(&state(Locale::Zh), 80, 24);
        assert!(text.contains("CodeLeveler"), "{text}");
        assert!(!text.contains("CODELEVELER"), "{text}");
        assert!(!text.contains("Code Leveler"), "{text}");
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
                theme.brand.foundation,
                theme.brand.primary,
                theme.brand.highlight,
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

    fn col_of(line: &str, needle: &str) -> Option<usize> {
        let i = line.find(needle)?;
        Some(disp_w(&line[..i]))
    }

    fn first_pipe(line: &str) -> Option<usize> {
        col_of(line, "│")
    }

    fn last_pipe(line: &str) -> Option<usize> {
        let i = line.rfind('│')?;
        Some(disp_w(&line[..i]))
    }

    /// Horizontal geometry of the hero group inside the card.
    fn hero_group_metrics(text: &str) -> Option<(usize, usize, usize, usize, usize)> {
        let runway = text.lines().find(|l| l.contains("██      █████"))?;
        let brand = text.lines().find(|l| l.contains("CodeLeveler"))?;
        let mark_left = col_of(runway, "██      █████")?;
        let content_x = col_of(brand, "CodeLeveler")?;
        let inner_left = first_pipe(brand)? + 1 + INNER_PAD_X;
        let inner_right = last_pipe(brand)?.saturating_sub(INNER_PAD_X);
        Some((
            mark_left,
            content_x,
            inner_left,
            inner_right,
            level_mark_width(BrandMarkSize::Master),
        ))
    }

    fn blank_inner(line: &str) -> bool {
        let Some(l) = first_pipe(line) else {
            return false;
        };
        let Some(r) = last_pipe(line) else {
            return false;
        };
        if r <= l + 1 {
            return true;
        }
        let inner_start = line.find('│').unwrap() + '│'.len_utf8();
        let inner_end = line.rfind('│').unwrap();
        line[inner_start..inner_end].chars().all(|c| c == ' ')
    }

    #[test]
    fn hero_group_is_centered_in_the_card() {
        for locale in [Locale::Zh, Locale::En] {
            for (w, h) in [(120usize, 36usize), (100, 30), (80, 24)] {
                let text = paint(&state(locale), w, h);
                let (mark_left, content_x, inner_left, inner_right, mark_w) =
                    hero_group_metrics(&text)
                        .unwrap_or_else(|| panic!("{locale:?} {w}x{h}:\n{text}"));
                let gap = content_x.saturating_sub(mark_left + mark_w);
                assert!(
                    (4..=5).contains(&gap),
                    "{locale:?} {w}x{h} gap {gap} (mark_left={mark_left} content_x={content_x}):\n{text}"
                );
                let content_w = 42; // wide content column
                let group_w = mark_w + gap + content_w;
                assert!(
                    (58..=64).contains(&group_w),
                    "{locale:?} {w}x{h} group_w {group_w}:\n{text}"
                );
                let left_slack = mark_left.saturating_sub(inner_left);
                let right_slack = inner_right.saturating_sub(mark_left + group_w);
                let drift = left_slack as i32 - right_slack as i32;
                assert!(
                    drift.abs() <= 1,
                    "{locale:?} {w}x{h} group not centered (left_slack={left_slack} right_slack={right_slack} drift={drift}):\n{text}"
                );
            }
        }
    }

    #[test]
    fn main_content_shares_one_left_edge() {
        for locale in [Locale::Zh, Locale::En] {
            let mut s = state(locale);
            s.untrusted_config = vec![".leveler/hooks.yaml".to_string()];
            let text = paint(&s, 120, 36);
            let brand_x = col_of(
                text.lines().find(|l| l.contains("CodeLeveler")).unwrap(),
                "CodeLeveler",
            )
            .unwrap();
            let cta_x = col_of(text.lines().find(|l| l.contains('›')).unwrap(), "›").unwrap();
            let cmd_x = col_of(
                text.lines().find(|l| l.contains("/feature-dev")).unwrap(),
                "/feature-dev",
            )
            .unwrap();
            let more_x = text
                .lines()
                .find_map(|l| col_of(l, "输入 /").or_else(|| col_of(l, "Type /")))
                .unwrap();
            let warn_x = col_of(text.lines().find(|l| l.contains('⚠')).unwrap(), "⚠").unwrap();
            assert_eq!(brand_x, cta_x, "cta drifted:\n{text}");
            assert_eq!(brand_x, cmd_x, "commands drifted:\n{text}");
            assert_eq!(brand_x, more_x, "more hint drifted:\n{text}");
            assert_eq!(brand_x, warn_x, "warning drifted:\n{text}");
        }
    }

    #[test]
    fn warning_is_its_own_section_and_does_not_leave_a_hole() {
        let mut s = state(Locale::Zh);
        let off = paint(&s, 120, 36);
        s.untrusted_config = vec![".leveler/hooks.yaml".to_string()];
        let on = paint(&s, 120, 36);

        assert!(!off.contains('⚠'), "{off}");
        assert!(on.contains('⚠'), "{on}");

        let more_idx = on
            .lines()
            .position(|l| l.contains("输入 /") || l.contains("Type /"))
            .unwrap();
        let warn_idx = on.lines().position(|l| l.contains('⚠')).unwrap();
        assert!(
            warn_idx >= more_idx + 2,
            "need a blank row between more and warning:\n{on}"
        );
        assert!(
            on.lines().nth(more_idx + 1).is_some_and(blank_inner),
            "row after more should be blank:\n{on}"
        );

        let off_body = off
            .lines()
            .filter(|l| !blank_inner(l) && l.contains('│'))
            .count();
        let on_body = on
            .lines()
            .filter(|l| !blank_inner(l) && l.contains('│'))
            .count();
        assert!(
            on_body > off_body,
            "warning should add real rows, not fill a reserved hole (off={off_body} on={on_body})\nOFF:\n{off}\nON:\n{on}"
        );

        // No warning-sized hole when hidden: last non-blank inner row is the more hint.
        let off_lines: Vec<&str> = off.lines().collect();
        let last_content = off_lines
            .iter()
            .rposition(|l| l.contains('│') && !blank_inner(l))
            .unwrap();
        assert!(
            off_lines[last_content].contains("输入 /")
                || off_lines[last_content].contains("Type /"),
            "hidden warning left a hole after more:\n{off}"
        );
    }

    #[test]
    fn card_stays_hero_sized_when_group_is_centered() {
        for (w, h) in [(120usize, 36usize), (100, 30), (80, 24)] {
            let text = paint(&state(Locale::Zh), w, h);
            let bw = box_width(&text);
            let bh = box_height(&text);
            assert!(
                (76..=86).contains(&bw),
                "{w}x{h} card width {bw} shrank:\n{text}"
            );
            assert!(
                (15..=18).contains(&bh),
                "{w}x{h} card height {bh} is not a hero:\n{text}"
            );
        }
    }

    #[test]
    fn more_commands_slash_uses_accent_token() {
        let theme = Theme::dark();
        let mut s = state(Locale::Zh);
        s.theme = theme.clone();
        let lines = splash_lines(&s, 80, 24, &s.theme, s.t());
        let mut saw_slash = false;
        for line in &lines {
            for span in &line.spans {
                if span.content.as_ref() == "/" {
                    saw_slash = true;
                    assert_eq!(span.style.fg, Some(theme.accent.primary));
                }
            }
        }
        assert!(saw_slash, "more-commands slash missing");
    }

    fn ansi_fg(color: Color, bold: bool, text: &str) -> String {
        let reset = "\x1b[0m";
        let bold_s = if bold { "\x1b[1m" } else { "" };
        match color {
            Color::Rgb(r, g, b) => format!("\x1b[38;2;{r};{g};{b}m{bold_s}{text}{reset}"),
            Color::Reset => format!("{bold_s}{text}{reset}"),
            _ => text.to_string(),
        }
    }

    fn line_ansi(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|sp| {
                let fg = sp.style.fg.unwrap_or(Color::Reset);
                let bold = sp.style.add_modifier.contains(Modifier::BOLD);
                ansi_fg(fg, bold, sp.content.as_ref())
            })
            .collect()
    }

    fn paint_ansi(s: &AppState, width: usize, height: usize) -> String {
        splash_lines(s, width, height, &s.theme, s.t())
            .iter()
            .map(line_ansi)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Manual board: candidates, then the selected mark on real splash layouts.
    #[test]
    #[ignore = "manual splash preview; run with --ignored --nocapture"]
    fn dump_hero_splash() {
        eprintln!("\n======== LEVEL MARK CANDIDATES ========");
        for (id, name, note) in [
            (
                'A',
                "Cantilever (SELECTED)",
                "notch + left pier + forward runway; 13×5",
            ),
            (
                'B',
                "Spine Rise",
                "tighter notch, heavier right spine; 11×5",
            ),
            (
                'C',
                "Tall Wedge",
                "six-row progression; closer to stairs; 14×6",
            ),
        ] {
            eprintln!("\n──────── {id}  {name} ────────");
            eprintln!("{note}");
            eprintln!("{}", candidate_plain(id));
        }

        eprintln!("\n======== SELECTED SIZES ========");
        for size in [
            BrandMarkSize::Master,
            BrandMarkSize::Compact,
            BrandMarkSize::Micro,
        ] {
            eprintln!("\n── {size:?} ──");
            eprintln!("{}", mark_plain(size));
        }

        for (label, mut theme) in [
            ("DARK", Theme::dark()),
            ("LIGHT", Theme::light()),
            ("HIGH CONTRAST", Theme::high_contrast()),
            ("NO_COLOR", Theme::no_color()),
        ] {
            let mut zh = state(Locale::Zh);
            zh.theme = theme.clone();
            eprintln!("\n======== {label} ZH 80x24 ========");
            eprintln!("{}", paint_ansi(&zh, 80, 24));
            if label == "DARK" {
                theme = Theme::dark();
                zh.theme = theme.clone();
                for (w, h) in [(120u16, 36u16), (100, 30), (80, 24)] {
                    eprintln!("\n-------- DARK ZH {w}x{h} warning OFF --------");
                    eprintln!("{}", paint_ansi(&zh, w as usize, h as usize));
                    zh.untrusted_config = vec![".leveler/hooks.yaml".to_string()];
                    eprintln!("\n-------- DARK ZH {w}x{h} warning ON --------");
                    eprintln!("{}", paint_ansi(&zh, w as usize, h as usize));
                    zh.untrusted_config.clear();
                }
                let mut en = state(Locale::En);
                en.theme = theme;
                for (w, h) in [(120u16, 36u16), (80, 24)] {
                    eprintln!("\n-------- DARK EN {w}x{h} warning OFF --------");
                    eprintln!("{}", paint_ansi(&en, w as usize, h as usize));
                    en.untrusted_config = vec![".leveler/hooks.yaml".to_string()];
                    eprintln!("\n-------- DARK EN {w}x{h} warning ON --------");
                    eprintln!("{}", paint_ansi(&en, w as usize, h as usize));
                    en.untrusted_config.clear();
                }
                let mut mid = state(Locale::Zh);
                mid.theme = Theme::dark();
                eprintln!("\n-------- DARK ZH 64x20 medium --------");
                eprintln!("{}", paint_ansi(&mid, 64, 20));
                eprintln!("\n-------- DARK ZH 48x16 narrow --------");
                eprintln!("{}", paint_ansi(&mid, 48, 16));
                eprintln!("\n-------- DARK ZH 40x16 very narrow --------");
                eprintln!("{}", paint_ansi(&mid, 40, 16));
            }
        }

        let mut en = state(Locale::En);
        en.theme = Theme::dark();
        eprintln!("\n======== DARK EN 80x24 ========");
        eprintln!("{}", paint_ansi(&en, 80, 24));
    }
}
