//! Conversation welcome card. Rendering and menu hit-testing share one layout.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::brand::{BrandMarkSize, BrandStyles, level_mark, level_mark_width, paint_mark_row};
use crate::i18n::{Locale, UiText};
use crate::state::AppState;
use crate::theme::Theme;
use crate::transcript::TranscriptItem;

mod native;
pub(crate) use native::NativeMascot;

/// Whether Conversation is still empty of real turns (welcome/btw ignored).
pub(crate) fn conversation_is_empty(state: &AppState) -> bool {
    !state.transcript.items().iter().any(|item| {
        matches!(
            item,
            TranscriptItem::User(_)
                | TranscriptItem::UserInput(_)
                | TranscriptItem::Assistant(_)
                | TranscriptItem::ToolGroup(_)
                | TranscriptItem::SubAgent(_)
                | TranscriptItem::Completion(_)
                | TranscriptItem::Error(_)
                | TranscriptItem::Note(_)
                | TranscriptItem::AwaySummary(_)
                | TranscriptItem::TurnEnd(_)
                | TranscriptItem::UserShell(_)
                | TranscriptItem::Recap(_)
                | TranscriptItem::GoalRecap(_)
                // A Thought is conversation content: a replay that carried
                // reasoning is not an empty session.
                | TranscriptItem::Thought(_)
        )
    })
}

const TIP_COMMANDS: [&str; 4] = ["/goal", "/diff", "/context", "/theme"];

pub(crate) fn tip_index(seed: u64) -> usize {
    (seed % TIP_COMMANDS.len() as u64) as usize
}

fn tip(state: &AppState) -> (&'static str, &'static str) {
    let index = state.welcome_tip % TIP_COMMANDS.len();
    let descriptions = match state.locale {
        Locale::Zh => [
            "围绕一个目标，让执行与验收可追溯。",
            "查看当前项目的代码差异。",
            "查看上下文占用与用量统计。",
            "切换适合当前终端的配色。",
        ],
        Locale::En => [
            "Drive one explicit goal to a verifiable finish.",
            "Review code changes in this project.",
            "Inspect context usage and token statistics.",
            "Switch the terminal's color theme.",
        ],
    };
    (TIP_COMMANDS[index], descriptions[index])
}

pub(crate) fn action_command(index: usize) -> Option<&'static str> {
    ["sessions", "help", "model"].get(index).copied()
}

fn action_label(locale: Locale, index: usize) -> &'static str {
    match locale {
        Locale::Zh => ["历史会话", "查看帮助", "选择模型"][index],
        Locale::En => ["Session history", "View help", "Select model"][index],
    }
}

struct MenuHit {
    row: usize,
    start: usize,
    end: usize,
    index: usize,
}

struct WelcomeLayout {
    lines: Vec<Line<'static>>,
    actions: Vec<MenuHit>,
    image: Option<ratatui::layout::Rect>,
}

pub(crate) fn splash_lines(
    state: &AppState,
    width: usize,
    height: usize,
    theme: &Theme,
    t: &UiText,
) -> Vec<Line<'static>> {
    layout(state, width, height, theme, t).lines
}

/// Coordinates are absolute conversation coordinates from the shared viewport mapper.
pub(crate) fn action_at(
    state: &AppState,
    width: usize,
    height: usize,
    row: usize,
    col: usize,
) -> Option<usize> {
    if !conversation_is_empty(state) {
        return None;
    }
    layout(state, width, height, &state.theme, state.t())
        .actions
        .iter()
        .find(|hit| hit.row == row && (hit.start..hit.end).contains(&col))
        .map(|hit| hit.index)
}

fn layout(
    state: &AppState,
    width: usize,
    height: usize,
    theme: &Theme,
    t: &UiText,
) -> WelcomeLayout {
    let width = width.max(1);
    let bordered = width >= 40 && height >= 12;
    let card_w = width.saturating_sub(if bordered { 2 } else { 0 }).min(86);
    let left = (width - card_w) / 2;
    let inset = if bordered { 3 } else { 0 };
    let inner = card_w.saturating_sub(inset * 2).max(1);
    let style = Style::default().fg(theme.text.primary);
    let muted = Style::default().fg(theme.text.muted);
    let accent = Style::default().fg(theme.accent.primary);
    let strong = accent.add_modifier(Modifier::BOLD);
    let zh = state.locale == Locale::Zh;
    let tagline = if zh {
        "让每个模型发挥最大能力"
    } else {
        "Let every model reach its full potential"
    };
    let ready = if zh {
        "准备就绪，描述你的目标即可开始"
    } else {
        "Ready. Describe your goal to get started."
    };
    let mut body = Vec::<Vec<Span<'static>>>::new();
    let mut menu_rows = Vec::new();
    let columns = inner >= 66;
    let right_w = if columns { 34.min(inner / 2) } else { inner };
    let left_w = if columns { inner - right_w - 2 } else { inner };
    let menu_x = if columns { left_w + 2 } else { 0 };
    let lower_rows = if columns {
        3
    } else if height >= 16 {
        6
    } else {
        5
    };

    // Keep navigation available before spending vertical space on illustration.
    let hero_h = if bordered {
        height.saturating_sub(10).clamp(3, 14)
    } else {
        0
    };
    // Terminal cells are about twice as tall as they are wide: a cell slot that
    // preserves the illustration's aspect needs 2 * src_w / src_h columns per row.
    let live_image = state.welcome_image.as_ref().filter(|image| image.usable());
    let (src_w, src_h) = live_image.map(NativeMascot::aspect).unwrap_or((2, 1));
    let src_w = usize::try_from(src_w).unwrap_or(2).max(1);
    let src_h = usize::try_from(src_h).unwrap_or(1).max(1);
    let mascot_w = (hero_h * 2 * src_w / src_h)
        .min(22)
        .min(inner.saturating_sub(34));
    let mascot_h = (mascot_w * src_h / (2 * src_w)).min(hero_h);
    let mut use_mascot = live_image.is_some()
        && !theme.monochrome
        && state.surface == crate::btw::SurfaceFocus::Main
        && state.history_search.is_none()
        && state.update.is_none()
        && state.overlay.is_none()
        && height >= 16
        && mascot_h >= 5
        && mascot_w >= 16;
    let mark_size = if inner >= 58 {
        BrandMarkSize::Master
    } else if inner >= 46 {
        BrandMarkSize::Compact
    } else {
        BrandMarkSize::Micro
    };
    let mark_rows = level_mark(mark_size);
    let mark_w = level_mark_width(mark_size);

    let mut show_mark = !use_mascot && bordered && inner >= mark_w + 28;
    let hero_for = |use_mascot: bool, show_mark: bool| {
        if use_mascot {
            mascot_h
        } else if show_mark {
            mark_rows.len().max(5)
        } else if height < 12 {
            3
        } else {
            4
        }
    };
    // Vertical budget. The card is sized to the viewport the caller published
    // (the conversation's content rect), and it must fit it: printing more rows
    // than that pushes the card's own notice below the fold, where a first
    // frame never shows it. Spacing is presentation, so it goes first;
    // the navigation and the untrusted-repository block
    // are content and are never dropped.
    let mut spacer_after_hero = bordered && height >= 16;
    let mut untrusted_blank = !state.untrusted_config.is_empty();
    let untrusted_content = if state.untrusted_config.is_empty() {
        0
    } else {
        1 + state.untrusted_config.len() + 1
    };
    let planned_rows = |hero_rows: usize, spacer: bool, blank: bool| {
        hero_rows
            + usize::from(spacer)
            + 1 // rule
            + lower_rows
            + untrusted_content
            + usize::from(blank)
            + usize::from(bordered) * 2
    };
    while planned_rows(
        hero_for(use_mascot, show_mark),
        spacer_after_hero,
        untrusted_blank,
    ) > height
    {
        if spacer_after_hero {
            spacer_after_hero = false;
        } else if untrusted_blank {
            untrusted_blank = false;
        } else if use_mascot {
            // The illustration is decoration; a slot that cannot be drawn
            // must not leave an empty column beside the text.
            use_mascot = false;
        } else if show_mark {
            // The illustration is decoration; the notice is not.
            show_mark = false;
        } else {
            // Nothing droppable is left: the content genuinely needs more room
            // than the viewport offers, and the reader scrolls.
            break;
        }
    }
    let art_w = if use_mascot {
        mascot_w
    } else if show_mark {
        mark_w
    } else {
        0
    };
    let gap = if art_w > 0 { 4 } else { 0 };
    let text_w = inner.saturating_sub(art_w + gap);
    let hero_rows = hero_for(use_mascot, show_mark);
    let text_top = hero_rows.saturating_sub(5) / 2;
    for row in 0..hero_rows {
        let mut spans = if show_mark {
            mark_rows
                .get(row)
                .map(|r| paint_mark_row(r, BrandStyles::from_theme(theme)))
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let used: usize = spans.iter().map(|s| disp_w(s.content.as_ref())).sum();
        if art_w > 0 {
            spans.push(Span::raw(" ".repeat(art_w.saturating_sub(used) + gap)));
        }
        let text_row = row.checked_sub(text_top);
        let text = match text_row {
            Some(0) => vec![
                Span::styled(truncate_w("CodeLeveler", text_w), strong),
                Span::styled(
                    truncate_w(
                        &format!("  v{}", state.version()),
                        text_w.saturating_sub(11),
                    ),
                    muted,
                ),
            ],
            Some(1) => vec![Span::styled(truncate_w(tagline, text_w), style)],
            Some(2) if hero_rows == 3 => vec![Span::styled(truncate_w(ready, text_w), muted)],
            Some(3) => vec![Span::styled(truncate_w(ready, text_w), muted)],
            _ => Vec::new(),
        };
        spans.extend(text);
        body.push(spans);
    }
    if spacer_after_hero {
        body.push(Vec::new());
    }
    body.push(vec![Span::styled(
        "─".repeat(inner),
        Style::default().fg(theme.border.subtle),
    )]);
    let (command, description) = tip(state);
    let tip_title = format!("{} {command}", if zh { "试试" } else { "Try" });
    let menu_spans = |index: usize| {
        let suffix = if right_w >= 20 {
            format!("Alt+{}", index + 1)
        } else {
            String::new()
        };
        let prefix = truncate_w("› ", right_w);
        let label_w = right_w.saturating_sub(disp_w(&suffix) + disp_w(&prefix));
        vec![
            Span::styled(prefix, accent),
            Span::styled(
                crate::render::pad_to_width(
                    &truncate_w(action_label(state.locale, index), label_w),
                    label_w,
                ),
                style,
            ),
            Span::styled(suffix, muted),
        ]
    };
    if columns {
        for row in 0..3 {
            let mut spans = match row {
                0 => vec![Span::styled(
                    truncate_w(&tip_title, left_w),
                    Style::default()
                        .fg(theme.status.warning)
                        .add_modifier(Modifier::BOLD),
                )],
                1 => vec![Span::styled(truncate_w(description, left_w), muted)],
                _ => Vec::new(),
            };
            let used: usize = spans.iter().map(|span| disp_w(&span.content)).sum();
            spans.push(Span::raw(" ".repeat(left_w.saturating_sub(used) + 2)));
            menu_rows.push((body.len(), row));
            spans.extend(menu_spans(row));
            body.push(spans);
        }
    } else {
        body.push(vec![Span::styled(
            truncate_w(&tip_title, left_w),
            Style::default()
                .fg(theme.status.warning)
                .add_modifier(Modifier::BOLD),
        )]);
        body.push(vec![Span::styled(truncate_w(description, left_w), muted)]);
        if height >= 16 {
            body.push(Vec::new());
        }
        for index in 0..3 {
            menu_rows.push((body.len(), index));
            body.push(menu_spans(index));
        }
    }
    if !state.untrusted_config.is_empty() {
        if untrusted_blank {
            body.push(Vec::new());
        }
        body.push(vec![Span::styled(
            truncate_w(&format!("⚠ {}", t.untrusted_config_title), inner),
            Style::default().fg(theme.status.warning),
        )]);
        for path in &state.untrusted_config {
            body.push(vec![Span::styled(truncate_w(path, inner), muted)]);
        }
        body.push(vec![Span::styled(
            truncate_w(t.untrusted_config_hint, inner),
            muted,
        )]);
    }
    let mut lines = Vec::new();
    let mut actions = Vec::new();
    if bordered {
        lines.push(Line::from(vec![
            Span::raw(" ".repeat(left)),
            Span::styled(
                format!("╭{}╮", "─".repeat(card_w - 2)),
                Style::default().fg(theme.border.subtle),
            ),
        ]));
    }
    let offset = lines.len();
    for (row, content) in body.into_iter().enumerate() {
        let used: usize = content.iter().map(|s| disp_w(s.content.as_ref())).sum();
        let mut spans = vec![Span::raw(" ".repeat(left))];
        if bordered {
            spans.push(Span::styled("│", Style::default().fg(theme.border.subtle)));
            spans.push(Span::raw("  "));
        }
        spans.extend(content);
        if bordered {
            spans.push(Span::raw(" ".repeat(inner.saturating_sub(used) + 2)));
            spans.push(Span::styled("│", Style::default().fg(theme.border.subtle)));
        }
        lines.push(Line::from(spans));
        if let Some((_, index)) = menu_rows.iter().find(|(menu_row, _)| *menu_row == row) {
            actions.push(MenuHit {
                row: row + offset,
                start: left + inset + menu_x,
                end: left + inset + menu_x + right_w,
                index: *index,
            });
        }
    }
    if bordered {
        lines.push(Line::from(vec![
            Span::raw(" ".repeat(left)),
            Span::styled(
                format!("╰{}╯", "─".repeat(card_w - 2)),
                Style::default().fg(theme.border.subtle),
            ),
        ]));
    }
    let image = use_mascot.then_some(ratatui::layout::Rect::new(
        (left + inset) as u16,
        offset as u16,
        mascot_w as u16,
        mascot_h as u16,
    ));
    WelcomeLayout {
        lines,
        actions,
        image,
    }
}

/// The exact PNG slot in viewport coordinates, shared with the native widget and preview.
pub(crate) fn native_image_rect(
    state: &AppState,
    content: ratatui::layout::Rect,
    scroll: usize,
) -> Option<ratatui::layout::Rect> {
    if !conversation_is_empty(state) {
        return None;
    }
    let layout = layout(
        state,
        content.width as usize,
        content.height as usize,
        &state.theme,
        state.t(),
    );
    let image = layout.image?;
    let pad = crate::conversation::geometry::painted_top_padding(
        state,
        layout.lines.len(),
        content.height as usize,
    );
    let top = usize::from(image.y) + pad;
    if top < scroll || top - scroll + usize::from(image.height) > usize::from(content.height) {
        return None;
    }
    Some(ratatui::layout::Rect::new(
        content.x + image.x,
        content.y + (top - scroll) as u16,
        image.width,
        image.height,
    ))
}

pub(crate) fn render_native_image(
    frame: &mut ratatui::Frame,
    state: &mut AppState,
    content: ratatui::layout::Rect,
    scroll: usize,
) {
    let Some(rect) = native_image_rect(state, content, scroll) else {
        return;
    };
    let Some(image) = &mut state.welcome_image else {
        return;
    };
    frame.render_stateful_widget(
        ratatui_image::StatefulImage::default().resize(ratatui_image::Resize::Fit(Some(
            image::imageops::FilterType::Lanczos3,
        ))),
        rect,
        &mut image.protocol,
    );
    if let Some(Err(error)) = image.protocol.last_encoding_result() {
        tracing::warn!(%error,"welcome PNG encoding failed; using terminal logo");
        image.mark_failed();
        // Clear skipped protocol cells as well as symbols, then immediately
        // render the logo fallback in the same frame.
        frame.render_widget(ratatui::widgets::Clear, content);
        let mut lines = splash_lines(
            state,
            content.width as usize,
            content.height as usize,
            &state.theme,
            state.t(),
        );
        let pad = crate::conversation::geometry::painted_top_padding(
            state,
            lines.len(),
            content.height as usize,
        );
        let mut padded = vec![Line::from(""); pad];
        padded.append(&mut lines);
        frame.render_widget(
            ratatui::widgets::Paragraph::new(padded).style(
                Style::default()
                    .bg(state.theme.surface.canvas)
                    .fg(state.theme.text.primary),
            ),
            content,
        );
    } else {
        image.mark_uploaded();
    }
}

fn disp_w(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}
fn truncate_w(s: &str, width: usize) -> String {
    if width == 0 {
        String::new()
    } else {
        crate::render::truncate_display(s, width)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Boot;
    use leveler_client_protocol::SessionId;
    fn state(locale: Locale) -> AppState {
        AppState::new(
            Theme::no_color(),
            Boot {
                session_id: SessionId::new("s1"),
                user: "u".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                show_welcome: true,
                draft_path: None,
                history_path: None,
                context_window: 0,
                locale,
                untrusted_config: Vec::new(),
                model_notice: None,
                thinking: None,
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

    #[test]
    fn welcome_menu_opens_existing_views_without_replacing_draft() {
        use crate::{action::Action, reducer::reduce, screen::Screen};
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut s = state(Locale::Zh);
        s.composer.replace("unfinished draft");
        let effects = reduce(
            &mut s,
            Action::Key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::ALT)),
        );
        assert_eq!(s.active_screen, Screen::Sessions);
        assert_eq!(s.composer.canonical_text(), "unfinished draft");
        assert!(effects.iter().any(|effect| matches!(
            effect,
            crate::action::Effect::Send(
                leveler_client_protocol::ClientCommand::RequestSessionListFor { .. }
            )
        )));
    }

    #[test]
    fn menu_copy_and_tip_are_supported_and_stable() {
        for locale in [Locale::Zh, Locale::En] {
            let mut s = state(locale);
            s.welcome_tip = 0;
            let text = paint(&s, 100, 30);
            for index in 0..3 {
                assert!(text.contains(action_label(locale, index)), "{text}");
                assert!(crate::screen::is_known_slash_token(
                    action_command(index).unwrap()
                ));
            }
            for seed in 0..16 {
                s.welcome_tip = tip_index(seed);
                let chosen = tip(&s);
                assert!(crate::screen::is_known_slash_token(
                    chosen.0.trim_start_matches('/')
                ));
                for (w, h) in [(100, 30), (80, 24), (40, 12)] {
                    let _ = paint(&s, w, h);
                    assert_eq!(tip(&s), chosen);
                }
            }
        }
    }

    #[test]
    fn first_real_content_hides_splash() {
        let mut s = state(Locale::Zh);
        assert!(conversation_is_empty(&s));
        s.transcript.push_user("hello".into());
        assert!(!conversation_is_empty(&s));
        assert!(action_at(&s, 80, 24, 0, 0).is_none());
    }

    #[test]
    fn layouts_stay_in_width_and_task_hits_follow_the_painted_rows() {
        for locale in [Locale::Zh, Locale::En] {
            for theme in [
                Theme::dark(),
                Theme::light(),
                Theme::high_contrast(),
                Theme::no_color(),
            ] {
                for native in [false, true] {
                    let mut s = state(locale);
                    s.theme = theme.clone();
                    if native {
                        s.welcome_image = Some(
                            NativeMascot::new(
                                ratatui_image::picker::ProtocolType::Iterm2,
                                (10, 20),
                            )
                            .unwrap(),
                        );
                    }
                    for (w, h) in [
                        (120, 36),
                        (100, 30),
                        (80, 24),
                        (72, 20),
                        (64, 16),
                        (48, 12),
                        (40, 10),
                        (20, 8),
                        (8, 4),
                        (1, 1),
                    ] {
                        let layout = layout(&s, w, h, &s.theme, s.t());
                        for line in &layout.lines {
                            assert!(
                                line.width() <= w,
                                "{locale:?} {w}x{h} native={native}: {:?}",
                                line_plain(line)
                            );
                        }
                        assert_eq!(layout.actions.len(), 3);
                        for hit in layout.actions {
                            assert_eq!(action_at(&s, w, h, hit.row, hit.start), Some(hit.index));
                            assert_eq!(action_at(&s, w, h, hit.row, hit.end), None);
                        }
                        if let Some(image) = layout.image {
                            assert!(usize::from(image.right()) <= w);
                            assert!(usize::from(image.bottom()) <= h);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn native_png_uses_image_widget_and_unsupported_uses_terminal_logo() {
        use ratatui::{Terminal, backend::TestBackend};
        use ratatui_image::picker::ProtocolType;
        let mut s = state(Locale::Zh);
        s.theme = Theme::dark();
        s.size = (110, 40);
        s.welcome_image = Some(NativeMascot::new(ProtocolType::Iterm2, (10, 20)).unwrap());
        let mut terminal = Terminal::new(TestBackend::new(110, 40)).unwrap();
        terminal.draw(|f| crate::render::render(f, &mut s)).unwrap();
        assert!(
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .any(|c| c.symbol().contains("1337;File=inline=1"))
        );
        assert!(!paint(&s, 100, 30).contains('▀'));
        s.welcome_image = None;
        let text = paint(&s, 100, 30);
        assert!(text.contains('█'));
        assert!(!text.contains('▀'));
        s.welcome_image = Some(NativeMascot::new(ProtocolType::Iterm2, (10, 20)).unwrap());
        s.theme = Theme::no_color();
        assert!(layout(&s, 100, 30, &s.theme, s.t()).image.is_none());
    }

    #[test]
    fn native_graphics_reset_after_resize_and_hide_under_other_surfaces() {
        use crate::action::Action;
        use ratatui::{Terminal, backend::TestBackend};
        use ratatui_image::picker::ProtocolType;
        let mut s = state(Locale::Zh);
        s.theme = Theme::dark();
        s.size = (110, 40);
        s.welcome_image = Some(NativeMascot::new(ProtocolType::Kitty, (10, 20)).unwrap());
        let mut terminal = Terminal::new(TestBackend::new(110, 40)).unwrap();
        let transmits = |buffer: &ratatui::buffer::Buffer| {
            buffer.content.iter().any(|c| c.symbol().contains("\x1b_G"))
        };
        assert!(transmits(
            terminal
                .draw(|f| crate::render::render(f, &mut s))
                .unwrap()
                .buffer
        ));
        assert!(!transmits(
            terminal
                .draw(|f| crate::render::render(f, &mut s))
                .unwrap()
                .buffer
        ));
        crate::reducer::reduce(&mut s, Action::Resize(120, 40));
        terminal
            .resize(ratatui::layout::Rect::new(0, 0, 120, 40))
            .unwrap();
        assert!(transmits(
            terminal
                .draw(|f| crate::render::render(f, &mut s))
                .unwrap()
                .buffer
        ));
        s.welcome_image.as_mut().unwrap().reset();
        assert!(transmits(
            terminal
                .draw(|f| crate::render::render(f, &mut s))
                .unwrap()
                .buffer
        ));
        s.history_search = Some(crate::history_search::HistorySearch::default());
        let content = s.conv.rect.unwrap();
        assert!(
            native_image_rect(
                &s,
                ratatui::layout::Rect::new(content.0, content.1, content.2, content.3),
                0
            )
            .is_none()
        );
        assert!(!transmits(
            terminal
                .draw(|f| crate::render::render(f, &mut s))
                .unwrap()
                .buffer
        ));
        s.history_search = None;
        s.overlay = Some(crate::overlay::Overlay::ModelPicker(Box::new(
            crate::overlay::SelectionModel::new("Models", Vec::new(), false),
        )));
        assert!(layout(&s, 100, 30, &s.theme, s.t()).image.is_none());
        s.overlay = None;
        s.transcript.push_user("hello".into());
        let rendered = terminal.draw(|f| crate::render::render(f, &mut s)).unwrap();
        assert!(
            rendered
                .buffer
                .content
                .iter()
                .all(|c| !c.symbol().contains('\u{1b}'))
        );
        assert!(rendered.buffer.content.iter().all(|c| !c.skip));
    }

    #[test]
    fn ignored_repo_warning_remains_available_in_compact_layout() {
        let mut s = state(Locale::Zh);
        s.untrusted_config = vec![".leveler/hooks.yaml".into()];
        for (w, h) in [(80, 24), (40, 8)] {
            let text = paint(&s, w, h);
            assert!(text.contains("hooks.yaml"), "{text}");
            assert!(text.contains("leveler trust"), "{text}");
        }
    }

    #[test]
    fn mouse_and_alt_shortcuts_open_existing_views_and_preserve_input() {
        use crate::{
            action::{Action, Effect},
            overlay::Overlay,
            reducer::reduce,
            screen::Screen,
        };
        use crossterm::event::{
            KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
        };
        use leveler_client_protocol::{AttachmentId, AttachmentKind, AttachmentRef, ClientCommand};
        for keyboard in [false, true] {
            for (w, h) in [(100, 30), (80, 20), (48, 14)] {
                for index in 0..3 {
                    let mut s = state(Locale::Zh);
                    s.size = (w + 4, h + 8);
                    s.conv.rect = Some((2, 3, w, h));
                    s.composer.replace("my draft");
                    s.available_models =
                        vec![leveler_client_protocol::ModelRef::parse("openai/gpt-test").unwrap()];
                    s.pending_attachments.push(AttachmentRef {
                        id: AttachmentId::new("draft"),
                        kind: AttachmentKind::Image,
                        name: "draft.png".into(),
                        mime_type: "image/png".into(),
                        size_bytes: 1,
                        sha256: "hash".into(),
                        width: Some(1),
                        height: Some(1),
                    });
                    s.composer.insert_image_token();
                    let draft = s.composer.canonical_text();
                    let attachment = s.pending_attachments.clone();
                    let layout = layout(&s, w as usize, h as usize, &s.theme, s.t());
                    let hit = &layout.actions[index];
                    let pad = crate::conversation::geometry::painted_top_padding(
                        &s,
                        layout.lines.len(),
                        h as usize,
                    );
                    let action = if keyboard {
                        Action::Key(KeyEvent::new(
                            KeyCode::Char(char::from(b'1' + index as u8)),
                            KeyModifiers::ALT,
                        ))
                    } else {
                        Action::Mouse(MouseEvent {
                            kind: MouseEventKind::Down(MouseButton::Left),
                            column: 2 + hit.start as u16,
                            row: 3 + (pad + hit.row) as u16,
                            modifiers: KeyModifiers::NONE,
                        })
                    };
                    let effects = reduce(&mut s, action);
                    assert_eq!(s.composer.canonical_text(), draft);
                    assert_eq!(s.pending_attachments, attachment);
                    assert!(conversation_is_empty(&s));
                    match index {
                        0 => {
                            assert_eq!(s.active_screen, Screen::Sessions);
                            assert!(effects.iter().any(|effect| matches!(
                                effect,
                                Effect::Send(ClientCommand::RequestSessionListFor { .. })
                            )));
                        }
                        1 => {
                            assert_eq!(s.active_screen, Screen::Help);
                            assert!(effects.is_empty());
                        }
                        2 => {
                            assert!(matches!(s.overlay, Some(Overlay::ModelPicker(_))));
                            assert!(effects.is_empty());
                        }
                        _ => unreachable!(),
                    }
                }
            }
        }
    }

    #[test]
    fn left_tip_and_side_surfaces_never_open_menu_actions() {
        use crate::{action::Action, reducer::reduce, screen::Screen};
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        for side in [false, true] {
            let mut s = state(Locale::Zh);
            s.size = (104, 38);
            s.conv.rect = Some((2, 3, 100, 30));
            if side {
                s.surface = crate::btw::SurfaceFocus::Btw;
            }
            let layout = layout(&s, 100, 30, &s.theme, s.t());
            let hit = &layout.actions[0];
            let pad =
                crate::conversation::geometry::painted_top_padding(&s, layout.lines.len(), 30);
            let col = if side { hit.start } else { hit.start - 2 };
            reduce(
                &mut s,
                Action::Mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 2 + col as u16,
                    row: 3 + (pad + hit.row) as u16,
                    modifiers: KeyModifiers::NONE,
                }),
            );
            assert_eq!(s.active_screen, Screen::Conversation);
            assert!(s.overlay.is_none());
            assert!(s.composer.is_empty());
        }
    }

    #[test]
    fn overflowing_welcome_hits_start_at_the_visible_top() {
        let mut s = state(Locale::Zh);
        s.conv.rect = Some((0, 0, 80, 8));
        s.conv.auto_scroll = true;
        s.untrusted_config = vec![".leveler/hooks.yaml".into()];
        let pos = crate::conversation::geometry::screen_to_content(&s, 0, 0).unwrap();
        assert_eq!(pos.row, 0);
    }

    #[test]
    fn a_failed_protocol_stops_reserving_the_image_slot() {
        use ratatui_image::picker::ProtocolType;
        let mut s = state(Locale::Zh);
        s.theme = Theme::dark();
        let mut image = NativeMascot::new(ProtocolType::Kitty, (10, 20)).unwrap();
        image.mark_failed();
        s.welcome_image = Some(image);
        let layout = layout(&s, 100, 30, &s.theme, s.t());
        assert!(layout.image.is_none());
        let text = joined(&layout.lines);
        assert!(text.contains("CodeLeveler"));
        for index in 0..3 {
            let label = action_label(Locale::Zh, index);
            assert!(text.contains(label), "{label}");
        }
    }

    #[test]
    fn overflowing_card_drops_the_image_instead_of_reserving_a_blank_column() {
        use ratatui_image::picker::ProtocolType;
        for (height, paths) in [(30, 14usize), (24, 8), (20, 6)] {
            let mut s = state(Locale::Zh);
            s.theme = Theme::dark();
            s.welcome_image = Some(NativeMascot::new(ProtocolType::Iterm2, (10, 20)).unwrap());
            s.untrusted_config = (0..paths)
                .map(|index| format!(".leveler/config-{index}.yaml"))
                .collect();
            let layout = layout(&s, 100, height, &s.theme, s.t());
            assert!(
                layout.image.is_none(),
                "{height}x100 with {paths} untrusted paths kept an off-screen image slot"
            );
            assert!(
                layout.lines.len() <= height,
                "{height}x100 overflowed: {}",
                layout.lines.len()
            );
            let first_body = line_plain(&layout.lines[1]);
            let expected_prefix = format!("{}│  CodeLeveler", " ".repeat(7));
            assert!(
                first_body.starts_with(&expected_prefix),
                "image column was still reserved: {first_body:?}"
            );
            assert!(
                joined(&layout.lines).contains("hooks.yaml")
                    || joined(&layout.lines).contains("config-")
            );
        }
    }

    #[test]
    #[ignore = "manual rendered welcome preview"]
    fn dump_welcome_preview() {
        use ratatui::style::Color;
        use ratatui::{Terminal, backend::TestBackend, widgets::Paragraph};
        let mut previews = Vec::new();
        for (name, theme, w, h) in [
            ("dark", Theme::dark(), 110, 36),
            ("light", Theme::light(), 110, 36),
            ("narrow", Theme::dark(), 48, 20),
            ("no-color", Theme::no_color(), 80, 24),
            ("full-native", Theme::dark(), 110, 40),
            ("full-dark", Theme::dark(), 110, 40),
            ("full-narrow", Theme::dark(), 60, 24),
        ] {
            let mut s = state(Locale::Zh);
            s.theme = theme;
            s.welcome_tip = 0;
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            let lines = splash_lines(&s, w as usize, h as usize, &s.theme, s.t());
            s.size = (w, h);
            if name == "full-native" {
                s.welcome_image = Some(
                    NativeMascot::new(ratatui_image::picker::ProtocolType::Iterm2, (12, 26))
                        .unwrap(),
                );
            }
            terminal
                .draw(|frame| {
                    if name.starts_with("full-") {
                        crate::render::render(frame, &mut s);
                        return;
                    }
                    frame.render_widget(
                        ratatui::widgets::Block::default().style(
                            Style::default()
                                .bg(s.theme.surface.canvas)
                                .fg(s.theme.text.primary),
                        ),
                        frame.area(),
                    );
                    let pad = (h as usize).saturating_sub(lines.len()) / 2;
                    let rect =
                        ratatui::layout::Rect::new(0, pad as u16, w, h.saturating_sub(pad as u16));
                    frame.render_widget(Paragraph::new(lines.clone()), rect);
                })
                .unwrap();
            let color = |c: Color| match c {
                Color::Rgb(r, g, b) => serde_json::json!([r, g, b]),
                Color::Indexed(i) => {
                    let (r, g, b) = crate::theme::xterm_256_rgb(i);
                    serde_json::json!([r, g, b])
                }
                _ => serde_json::Value::Null,
            };
            let cells:Vec<_>=terminal.backend().buffer().content.iter().map(|cell|serde_json::json!({"symbol":if cell.symbol().contains('\u{1b}') { " " } else { cell.symbol() },"fg":color(cell.fg),"bg":color(cell.bg),"bold":cell.modifier.contains(Modifier::BOLD)})).collect();
            let native_rect = s
                .conv
                .rect
                .and_then(|(x, y, w, h)| {
                    native_image_rect(&s, ratatui::layout::Rect::new(x, y, w, h), 0)
                })
                .map(|r| serde_json::json!({"x":r.x,"y":r.y,"width":r.width,"height":r.height}));
            previews.push(serde_json::json!({"name":name,"width":w,"height":h,"cells":cells,"image_rect":native_rect}));
        }
        std::fs::write(
            "/tmp/codeleveler-welcome-preview.json",
            serde_json::to_vec(&previews).unwrap(),
        )
        .unwrap();
    }
}
