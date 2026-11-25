//! Two credential surfaces: the review that opens when a credential is
//! detected in the composer, and the explicit secure-paste field.
//!
//! Neither model holds the value in a rendered line. The review shows a field
//! name and a length; the paste field shows a fixed mask. Everything the value
//! touches stays in memory until the runtime confirms custody.

use crossterm::event::{KeyCode, KeyEvent};
use leveler_core::{SecretFinding, SecretLevel, masked_secret};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::i18n::Locale;
use crate::theme::Theme;

use super::{RawContent, decision_rule};

/// What the person chose in the credential review.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretReviewChoice {
    /// Hand the values to the runtime vault and send references.
    Capture,
    /// Send the text exactly as typed, with the value visible to the model.
    Plaintext,
    /// Close the review and leave the composer untouched.
    Back,
}

/// Review a detected credential before anything leaves the client.
#[derive(Debug, Clone)]
pub struct SecretReviewOverlay {
    /// The exact text the person submitted. It is kept so the chosen action
    /// describes the same bytes the detector saw.
    pub draft: String,
    /// Non-overlapping findings, sorted by position.
    pub findings: Vec<SecretFinding>,
    choice: usize,
}

impl SecretReviewOverlay {
    /// Open the review for `draft`.
    pub fn new(draft: String, findings: Vec<SecretFinding>) -> Self {
        Self {
            draft,
            findings,
            choice: 0,
        }
    }

    /// The focused choice.
    pub fn choice(&self) -> SecretReviewChoice {
        match self.choice {
            1 => SecretReviewChoice::Plaintext,
            2 => SecretReviewChoice::Back,
            _ => SecretReviewChoice::Capture,
        }
    }

    /// Handle one key. `None` keeps the review open.
    pub fn on_key(&mut self, key: KeyEvent) -> Option<SecretReviewChoice> {
        match key.code {
            KeyCode::Esc => Some(SecretReviewChoice::Back),
            KeyCode::Up | KeyCode::BackTab => {
                self.choice = (self.choice + 2) % 3;
                None
            }
            KeyCode::Down | KeyCode::Tab => {
                self.choice = (self.choice + 1) % 3;
                None
            }
            KeyCode::Char('1') => Some(SecretReviewChoice::Capture),
            KeyCode::Char('2') => Some(SecretReviewChoice::Plaintext),
            KeyCode::Char('3') => Some(SecretReviewChoice::Back),
            KeyCode::Enter => Some(self.choice()),
            _ => None,
        }
    }

    /// Whether any finding is high confidence. The UI uses it to lead with the
    /// stronger warning; the decision itself is the person's.
    pub fn has_high_confidence(&self) -> bool {
        self.findings
            .iter()
            .any(|finding| finding.level == SecretLevel::HighConfidence)
    }
}

/// What the person did with the secure-paste field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecurePasteOutcome {
    /// Keep typing.
    None,
    /// Capture the buffer's contents.
    Submit(String),
    /// Discard the buffer.
    Cancel,
}

/// A masked, single-line field for a credential of any shape.
#[derive(Debug, Clone, Default)]
pub struct SecurePasteOverlay {
    buffer: String,
}

impl SecurePasteOverlay {
    /// An empty field.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append pasted or typed text.
    pub fn insert_text(&mut self, text: &str) {
        self.buffer.push_str(text);
    }

    /// Handle one key.
    pub fn on_key(&mut self, key: KeyEvent) -> SecurePasteOutcome {
        match key.code {
            KeyCode::Esc => SecurePasteOutcome::Cancel,
            KeyCode::Enter => {
                if self.buffer.trim().is_empty() {
                    SecurePasteOutcome::None
                } else {
                    SecurePasteOutcome::Submit(std::mem::take(&mut self.buffer))
                }
            }
            KeyCode::Backspace => {
                self.buffer.pop();
                SecurePasteOutcome::None
            }
            KeyCode::Char(c) => {
                self.buffer.push(c);
                SecurePasteOutcome::None
            }
            _ => SecurePasteOutcome::None,
        }
    }

    /// A representation safe to render: the mask, never the value.
    pub fn mask(&self) -> String {
        let chars = self.buffer.chars().count();
        if chars == 0 {
            return String::new();
        }
        "•".repeat(chars.min(32))
    }

    /// Character count, for the hint line.
    pub fn len(&self) -> usize {
        self.buffer.chars().count()
    }

    /// Whether nothing has been entered.
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }
}

pub(super) fn review_content(
    ov: &SecretReviewOverlay,
    theme: &Theme,
    width: usize,
    locale: Locale,
) -> RawContent {
    let t = locale.text();
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(decision_rule(theme, width));
    let heading = if ov.findings.len() == 1 {
        t.secret.review_title.to_string()
    } else {
        t.secret
            .review_multiple
            .replace("{}", &ov.findings.len().to_string())
    };
    lines.push(Line::from(Span::styled(
        heading,
        Style::default()
            .fg(theme.text.primary)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));
    for finding in &ov.findings {
        let value = finding.value(&ov.draft);
        let label = finding.key.clone().unwrap_or_else(|| match finding.kind {
            leveler_core::SecretKind::ProviderKey => t.secret.kind_provider_key.to_string(),
            leveler_core::SecretKind::JsonWebToken => t.secret.kind_jwt.to_string(),
            leveler_core::SecretKind::PemPrivateKey => t.secret.kind_private_key.to_string(),
            leveler_core::SecretKind::UrlCredentials => t.secret.kind_url.to_string(),
            leveler_core::SecretKind::KeyValue => t.secret.kind_value.to_string(),
            leveler_core::SecretKind::ProseCredential => t.secret.kind_prose.to_string(),
        });
        let strength = match finding.level {
            SecretLevel::HighConfidence => t.secret.level_high,
            SecretLevel::Ambiguous => t.secret.level_ambiguous,
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("{label}  "),
                Style::default().fg(theme.text.secondary),
            ),
            Span::styled(masked_secret(value), Style::default().fg(theme.text.muted)),
            Span::styled(
                format!("  · {strength}"),
                Style::default().fg(align_level(theme, finding.level)),
            ),
        ]));
    }
    lines.push(Line::from(""));
    let options = [
        (t.secret.review_capture, SecretReviewChoice::Capture),
        (t.secret.review_plaintext, SecretReviewChoice::Plaintext),
        (t.secret.review_back, SecretReviewChoice::Back),
    ];
    let mut cursor_row = None;
    for (index, (label, _)) in options.iter().enumerate() {
        let focused = ov.choice == index;
        if focused {
            cursor_row = Some(lines.len());
        }
        lines.push(Line::from(vec![
            Span::styled(
                format!("{} ", if focused { "●" } else { "○" }),
                Style::default().fg(if focused {
                    theme.status.info
                } else {
                    theme.text.muted
                }),
            ),
            Span::styled(
                format!("{}. {label}", index + 1),
                Style::default().fg(if focused {
                    theme.text.primary
                } else {
                    theme.text.secondary
                }),
            ),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        t.secret.review_hint,
        Style::default().fg(theme.text.muted),
    )));
    RawContent {
        title: t.secret.review_title.to_string(),
        lines,
        cursor: None,
        cursor_row,
        focus_block: None,
        tail_from: None,
    }
}

pub(super) fn paste_content(
    ov: &SecurePasteOverlay,
    theme: &Theme,
    width: usize,
    locale: Locale,
) -> RawContent {
    let t = locale.text();
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(decision_rule(theme, width));
    let mask = ov.mask();
    let shown = if mask.is_empty() {
        t.secret.paste_empty.to_string()
    } else {
        format!("{mask}  ({})", ov.len())
    };
    lines.push(Line::from(Span::styled(
        shown,
        Style::default().fg(if ov.is_empty() {
            theme.text.muted
        } else {
            theme.text.primary
        }),
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        t.secret.paste_hint,
        Style::default().fg(theme.text.muted),
    )));
    lines.push(Line::from(Span::styled(
        t.secret.paste_scope,
        Style::default().fg(theme.text.muted),
    )));
    RawContent {
        title: t.secret.paste_title.to_string(),
        lines,
        cursor: None,
        cursor_row: None,
        focus_block: None,
        tail_from: None,
    }
}

fn align_level(theme: &Theme, level: SecretLevel) -> ratatui::style::Color {
    match level {
        SecretLevel::HighConfidence => theme.status.warning,
        SecretLevel::Ambiguous => theme.text.muted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    fn review() -> SecretReviewOverlay {
        let draft = "API_KEY=abc123def".to_string();
        let findings = leveler_core::detect_secrets(&draft);
        SecretReviewOverlay::new(draft, findings)
    }

    #[test]
    fn the_safe_choice_is_focused_first() {
        assert_eq!(review().choice(), SecretReviewChoice::Capture);
    }

    #[test]
    fn arrows_move_and_escape_returns_to_editing() {
        let mut ov = review();
        assert_eq!(ov.on_key(key(KeyCode::Down)), None);
        assert_eq!(ov.choice(), SecretReviewChoice::Plaintext);
        assert_eq!(ov.on_key(key(KeyCode::Down)), None);
        assert_eq!(ov.choice(), SecretReviewChoice::Back);
        assert_eq!(ov.on_key(key(KeyCode::Down)), None);
        assert_eq!(ov.choice(), SecretReviewChoice::Capture);
        assert_eq!(ov.on_key(key(KeyCode::Esc)), Some(SecretReviewChoice::Back));
    }

    #[test]
    fn enter_and_digits_choose() {
        let mut ov = review();
        assert_eq!(
            ov.on_key(key(KeyCode::Enter)),
            Some(SecretReviewChoice::Capture)
        );
        assert_eq!(
            ov.on_key(key(KeyCode::Char('2'))),
            Some(SecretReviewChoice::Plaintext)
        );
        assert_eq!(
            ov.on_key(key(KeyCode::Char('3'))),
            Some(SecretReviewChoice::Back)
        );
        assert_eq!(ov.on_key(key(KeyCode::Char('x'))), None);
    }

    #[test]
    fn the_review_never_renders_the_value() {
        let ov = review();
        let theme = Theme::default();
        let content = review_content(&ov, &theme, 80, Locale::En);
        let rendered: String = content
            .lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.clone()))
            .collect();
        assert!(!rendered.contains("abc123def"), "{rendered}");
        assert!(rendered.contains("API_KEY"), "{rendered}");
    }

    #[test]
    fn the_paste_field_masks_everything_it_received() {
        let mut ov = SecurePasteOverlay::new();
        ov.insert_text("hunter2-abc123");
        assert_eq!(ov.len(), 14);
        assert!(!ov.mask().contains("hunter"));
        assert_eq!(ov.mask().chars().count(), 14);
        ov.on_key(key(KeyCode::Backspace));
        assert_eq!(ov.len(), 13);
    }

    #[test]
    fn paste_enter_captures_and_escape_cancels() {
        let mut ov = SecurePasteOverlay::new();
        assert_eq!(ov.on_key(key(KeyCode::Enter)), SecurePasteOutcome::None);
        ov.insert_text("value-123");
        assert_eq!(
            ov.on_key(key(KeyCode::Enter)),
            SecurePasteOutcome::Submit("value-123".into())
        );
        assert!(ov.is_empty());
        assert_eq!(ov.on_key(key(KeyCode::Esc)), SecurePasteOutcome::Cancel);
    }
}
