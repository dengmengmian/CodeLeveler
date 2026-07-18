//! Empty-session splash: brand mark + short product cue.
//!
//! Shown only when Conversation has no real work yet. Terminal mark is plain
//! readable **CL** block letters (not an abstract monogram).

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::i18n::UiText;
use crate::state::AppState;
use crate::theme::Theme;
use crate::transcript::TranscriptItem;

/// Readable **C** + **L** block letters (not a geometric mono-glyph).
const LOGO: &[&str] = &[
    " ██████╗ ██      ",
    "██╔════╝ ██      ",
    "██║      ██      ",
    "██║      ██      ",
    "╚██████╗ ███████╗",
    " ╚═════╝ ╚══════╝",
];

/// Whether Conversation is still empty of real turns (only welcome/btw ignored).
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
                | TranscriptItem::Recap(_)
        )
    })
}

/// Splash lines for the empty Conversation viewport.
pub(crate) fn splash_lines(
    state: &AppState,
    width: usize,
    theme: &Theme,
    t: &UiText,
) -> Vec<Line<'static>> {
    let width = width.max(24);
    let accent = Style::default()
        .fg(theme.accent)
        .add_modifier(Modifier::BOLD);
    let muted = Style::default().fg(theme.muted);
    let text = Style::default().fg(theme.text);

    let mut out = Vec::new();
    // Vertical breathing room so the mark sits mid-upper, not glued to Header.
    out.push(Line::from(""));
    out.push(Line::from(""));

    // Centered monogram.
    for row in LOGO {
        out.push(center_line(row, width, accent));
    }
    out.push(Line::from(""));

    // Brand + version.
    let brand = format!("CodeLeveler  v{}", state.version());
    out.push(center_line(&brand, width, accent));

    // Tagline.
    out.push(center_line(t.splash_tagline, width, muted));
    out.push(Line::from(""));

    // Repo / branch cue (compact).
    let repo = short_repo(state);
    let branch = state.branch.as_deref().unwrap_or("—");
    let meta = format!("{repo}  ·   {branch}");
    out.push(center_line(&meta, width, muted));
    out.push(Line::from(""));

    // One-line how-to.
    out.push(center_line(t.splash_hint, width, text));
    out.push(Line::from(""));

    out
}

fn center_line(s: &str, width: usize, style: Style) -> Line<'static> {
    let w = unicode_width::UnicodeWidthStr::width(s);
    let pad = width.saturating_sub(w) / 2;
    let mut spans = Vec::new();
    if pad > 0 {
        spans.push(Span::raw(" ".repeat(pad)));
    }
    spans.push(Span::styled(s.to_string(), style));
    Line::from(spans)
}

fn short_repo(state: &AppState) -> String {
    let raw = if state.repository.is_empty() {
        return "—".into();
    } else {
        state.repository.as_str()
    };
    let home = leveler_core::environment()
        .var_os("HOME")
        .map(|h| h.to_string_lossy().into_owned());
    let display = match &home {
        Some(h) if raw.starts_with(h.as_str()) => format!("~{}", &raw[h.len()..]),
        _ => raw.to_string(),
    };
    let base = std::path::Path::new(&display)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(display.as_str());
    if display.starts_with("~/") && display != base {
        format!("~/.../{base}")
    } else {
        base.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Boot;
    use leveler_client_protocol::SessionId;

    fn empty_state() -> AppState {
        AppState::new(
            Theme::no_color(),
            Boot {
                session_id: SessionId::new("s1"),
                user: "u".into(),
                version: "0.1.0".into(),
                show_welcome: true,
                draft_path: None,
                history_path: None,
                context_window: 0,
                locale: crate::i18n::Locale::Zh,
            },
        )
    }

    #[test]
    fn splash_shows_brand_and_logo() {
        let s = empty_state();
        let lines = splash_lines(&s, 60, &s.theme, s.t());
        let text: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|sp| sp.content.as_ref()).collect())
            .collect();
        let joined = text.join("\n");
        assert!(joined.contains("CodeLeveler"), "{joined}");
        assert!(
            joined.contains('█') || joined.contains('╔'),
            "logo cells: {joined}"
        );
        // Two separate letter columns (C left, L right).
        assert!(
            joined.contains("██║      ██"),
            "CL letters missing: {joined}"
        );
        assert!(conversation_is_empty(&s));
    }

    #[test]
    fn non_empty_conversation_is_not_splash() {
        let mut s = empty_state();
        s.transcript.push_user("hello".into());
        assert!(!conversation_is_empty(&s));
    }
}
