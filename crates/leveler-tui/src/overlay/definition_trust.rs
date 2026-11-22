//! The mid-turn prompt that asks a human to admit one untrusted definition.
//!
//! The content comes from the runtime's structured request. Nothing here is
//! inferred from model text, and dismissal is a refusal: an unreadable prompt
//! never becomes an approval.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use leveler_client_protocol::{DefinitionTrustDecision, UiDefinitionTrustRequest};

/// The decisions in display order. The first is the narrowest grant.
pub const OPTIONS: [DefinitionTrustDecision; 3] = [
    DefinitionTrustDecision::AllowOnce,
    DefinitionTrustDecision::TrustCurrentVersion,
    DefinitionTrustDecision::Deny,
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DefinitionTrustOutcome {
    /// Consumed; stay open.
    None,
    /// The user decided; resolve the pending request.
    Decide(DefinitionTrustDecision),
}

#[derive(Debug, Clone)]
pub struct DefinitionTrustOverlay {
    pub request: UiDefinitionTrustRequest,
    cursor: usize,
    /// The content/diff pane is expanded below the decisions.
    pub detail_open: bool,
    pub detail_scroll: usize,
}

impl DefinitionTrustOverlay {
    pub fn new(request: UiDefinitionTrustRequest) -> Self {
        Self {
            request,
            cursor: 0,
            detail_open: false,
            detail_scroll: 0,
        }
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn on_key(&mut self, key: KeyEvent) -> DefinitionTrustOutcome {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return DefinitionTrustOutcome::None;
        }
        match key.code {
            KeyCode::Esc => {
                // Two levels: close the detail first, then refuse the request.
                if self.detail_open {
                    self.detail_open = false;
                    DefinitionTrustOutcome::None
                } else {
                    DefinitionTrustOutcome::Decide(DefinitionTrustDecision::Deny)
                }
            }
            KeyCode::Up => {
                self.cursor = self.cursor.saturating_sub(1);
                DefinitionTrustOutcome::None
            }
            KeyCode::Down => {
                self.cursor = (self.cursor + 1).min(OPTIONS.len() - 1);
                DefinitionTrustOutcome::None
            }
            KeyCode::Char('y') => {
                DefinitionTrustOutcome::Decide(DefinitionTrustDecision::AllowOnce)
            }
            KeyCode::Char('t') => {
                DefinitionTrustOutcome::Decide(DefinitionTrustDecision::TrustCurrentVersion)
            }
            KeyCode::Char('d') | KeyCode::Char('n') => {
                DefinitionTrustOutcome::Decide(DefinitionTrustDecision::Deny)
            }
            KeyCode::Char('v') => {
                self.detail_open = !self.detail_open;
                self.detail_scroll = 0;
                DefinitionTrustOutcome::None
            }
            KeyCode::PageUp if self.detail_open => {
                self.detail_scroll = self.detail_scroll.saturating_sub(10);
                DefinitionTrustOutcome::None
            }
            KeyCode::PageDown if self.detail_open => {
                self.detail_scroll = self.detail_scroll.saturating_add(10);
                DefinitionTrustOutcome::None
            }
            KeyCode::Enter => DefinitionTrustOutcome::Decide(OPTIONS[self.cursor]),
            KeyCode::Char(c @ '1'..='3') => match c.to_digit(10).map(|d| d as usize - 1) {
                Some(index) if index < OPTIONS.len() => {
                    DefinitionTrustOutcome::Decide(OPTIONS[index])
                }
                _ => DefinitionTrustOutcome::None,
            },
            _ => DefinitionTrustOutcome::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_client_protocol::{UiDefinitionTrustRequest, UiDefinitionTrustState};
    use leveler_core::{DefinitionTrustRequestId, SessionId};

    fn request() -> UiDefinitionTrustRequest {
        UiDefinitionTrustRequest {
            id: DefinitionTrustRequestId::new("d1"),
            session_id: SessionId::new("s1"),
            call_id: "call-1".into(),
            identity: "skill:.leveler/skills/demo".into(),
            display: ".leveler/skills/demo".into(),
            kind: "skill".into(),
            scope: "project".into(),
            source: "CodeLeveler".into(),
            location: "/repo/.leveler/skills/demo".into(),
            state: UiDefinitionTrustState::Unapproved,
            approved_digest: None,
            current_digest: "sha256:new".into(),
            current_content: "BODY\n".into(),
            approved_content: None,
            content_truncated: false,
        }
    }

    #[test]
    fn esc_without_detail_is_a_refusal_not_an_approval() {
        let mut overlay = DefinitionTrustOverlay::new(request());
        assert_eq!(
            overlay.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty())),
            DefinitionTrustOutcome::Decide(DefinitionTrustDecision::Deny)
        );
    }

    #[test]
    fn the_first_esc_closes_detail_and_a_second_refuses() {
        let mut overlay = DefinitionTrustOverlay::new(request());
        overlay.on_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::empty()));
        assert!(overlay.detail_open);
        assert_eq!(
            overlay.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty())),
            DefinitionTrustOutcome::None
        );
        assert!(!overlay.detail_open);
        assert_eq!(
            overlay.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty())),
            DefinitionTrustOutcome::Decide(DefinitionTrustDecision::Deny)
        );
    }

    #[test]
    fn enter_picks_the_focused_option_and_never_a_permission_decision() {
        let mut overlay = DefinitionTrustOverlay::new(request());
        assert_eq!(
            overlay.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty())),
            DefinitionTrustOutcome::Decide(DefinitionTrustDecision::AllowOnce)
        );
        let mut overlay = DefinitionTrustOverlay::new(request());
        overlay.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::empty()));
        assert_eq!(
            overlay.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty())),
            DefinitionTrustOutcome::Decide(DefinitionTrustDecision::TrustCurrentVersion)
        );
    }
}
