//! Bare http(s) URL detection for click-to-open and visual link styling.
//!
//! Only `http` / `https` are recognized so a click can never launch another scheme.
//! Hit-testing uses **display columns** (same coordinate space as mouse / selection).

use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use unicode_width::UnicodeWidthChar;

/// Extract the http(s) URL spanning display column `col` in `line`, or `None`.
pub(crate) fn url_at(line: &str, col: usize) -> Option<String> {
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    let mut i = 0;
    let mut disp = 0usize;
    while i < n {
        let scheme = if chars_start_with(&chars, i, "https://") {
            8
        } else if chars_start_with(&chars, i, "http://") {
            7
        } else {
            disp += char_disp_width(chars[i]);
            i += 1;
            continue;
        };
        let start_disp = disp;
        let mut j = i;
        while j < n && is_url_char(chars[j]) {
            disp += char_disp_width(chars[j]);
            j += 1;
        }
        // Drop trailing sentence punctuation that is not really part of the URL.
        let mut end = j;
        let mut end_disp = disp;
        while end > i + scheme && is_trailing_punct(chars[end - 1]) {
            end_disp = end_disp.saturating_sub(char_disp_width(chars[end - 1]));
            end -= 1;
        }
        if col >= start_disp && col < end_disp {
            return Some(chars[i..end].iter().collect());
        }
        i = j;
    }
    None
}

/// Split `text` into spans; bare http(s) runs use `link`, everything else `base`.
pub(crate) fn spans_with_bare_urls(text: &str, base: Style, link: Style) -> Vec<Span<'static>> {
    if text.is_empty() {
        return vec![Span::styled(String::new(), base)];
    }
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut i = 0;
    let mut plain_start = 0usize;
    while i < n {
        let scheme = if chars_start_with(&chars, i, "https://") {
            8
        } else if chars_start_with(&chars, i, "http://") {
            7
        } else {
            i += 1;
            continue;
        };
        let mut j = i;
        while j < n && is_url_char(chars[j]) {
            j += 1;
        }
        let mut end = j;
        while end > i + scheme && is_trailing_punct(chars[end - 1]) {
            end -= 1;
        }
        if plain_start < i {
            out.push(Span::styled(
                chars[plain_start..i].iter().collect::<String>(),
                base,
            ));
        }
        out.push(Span::styled(chars[i..end].iter().collect::<String>(), link));
        // Continue from `end` so trimmed trailing punctuation is painted plain.
        plain_start = end;
        i = end;
    }
    if plain_start < n {
        out.push(Span::styled(
            chars[plain_start..].iter().collect::<String>(),
            base,
        ));
    }
    if out.is_empty() {
        out.push(Span::styled(text.to_string(), base));
    }
    out
}

/// Accent + underline style used for bare and markdown links.
pub(crate) fn link_style(accent: ratatui::style::Color) -> Style {
    Style::default()
        .fg(accent)
        .add_modifier(Modifier::UNDERLINED)
}

fn chars_start_with(chars: &[char], i: usize, pat: &str) -> bool {
    pat.chars()
        .enumerate()
        .all(|(k, pc)| chars.get(i + k) == Some(&pc))
}

fn is_url_char(c: char) -> bool {
    !c.is_whitespace() && !matches!(c, '<' | '>' | '"' | '`' | '\'' | '|')
}

fn is_trailing_punct(c: char) -> bool {
    matches!(
        c,
        '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '"' | '\''
    )
}

fn char_disp_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    #[test]
    fn detects_http_url_only_under_the_clicked_column() {
        let line = "see https://example.com/path please";
        // "see " = 4 display cols; URL starts at 4.
        assert_eq!(
            url_at(line, 10).as_deref(),
            Some("https://example.com/path")
        );
        assert_eq!(url_at(line, 0), None);
        assert_eq!(url_at(line, 40), None);
    }

    #[test]
    fn trims_trailing_punctuation_and_ignores_other_schemes() {
        assert_eq!(url_at("(http://a.io).", 3).as_deref(), Some("http://a.io"));
        assert_eq!(url_at("file:///etc/passwd", 3), None);
        assert_eq!(url_at("run ssh://x", 6), None);
    }

    #[test]
    fn detects_localhost_and_lan_urls() {
        let line = "  - Local:   http://localhost:3000";
        let start = line.find("http://").expect("url");
        // ASCII line: char index == display col.
        assert_eq!(
            url_at(line, start + 5).as_deref(),
            Some("http://localhost:3000")
        );
        let lan = "Network: http://10.9.9.199:3000";
        let s = lan.find("http://").unwrap();
        assert_eq!(
            url_at(lan, s + 2).as_deref(),
            Some("http://10.9.9.199:3000")
        );
    }

    #[test]
    fn display_column_accounts_for_wide_prefix() {
        // CJK "见" is typically 2 display cells.
        let line = "见 http://localhost:3000";
        let url_start = UnicodeWidthChar::width('见').unwrap_or(1) + 1; // 见 + space
        assert_eq!(
            url_at(line, url_start).as_deref(),
            Some("http://localhost:3000")
        );
        // Column on the CJK char is not a URL.
        assert_eq!(url_at(line, 0), None);
    }

    #[test]
    fn spans_with_bare_urls_styles_only_the_url_run() {
        let base = Style::default().fg(Color::Gray);
        let link = link_style(Color::Cyan);
        let spans = spans_with_bare_urls("Local: http://localhost:3000 done", base, link);
        let joined: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, "Local: http://localhost:3000 done");
        let url_span = spans
            .iter()
            .find(|s| s.content.contains("localhost"))
            .expect("url span");
        assert!(url_span.style.add_modifier.contains(Modifier::UNDERLINED));
        assert_eq!(url_span.style.fg, Some(Color::Cyan));
        let plain = spans
            .iter()
            .find(|s| s.content.contains("Local"))
            .expect("plain");
        assert!(!plain.style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn spans_without_url_stay_single_base_span() {
        let base = Style::default().fg(Color::Gray);
        let link = link_style(Color::Cyan);
        let spans = spans_with_bare_urls("no links here", base, link);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content.as_ref(), "no links here");
        assert_eq!(spans[0].style.fg, Some(Color::Gray));
    }
}
