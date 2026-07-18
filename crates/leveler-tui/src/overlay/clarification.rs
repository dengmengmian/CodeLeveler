//! The clarification overlay (spec §35): the agent asked a question mid-task.
//!
//! The user can pick a numbered option or type a free-text answer. Esc (or an
//! empty Enter) skips — the model then proceeds on its own.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_segmentation::UnicodeSegmentation;

use leveler_client_protocol::UiClarificationRequest;

/// Result of a key press on the clarification overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClarificationOutcome {
    None,
    /// The user answered (empty string = skip).
    Answer(String),
}

/// The clarification overlay state.
#[derive(Debug, Clone)]
pub struct ClarificationOverlay {
    pub request: UiClarificationRequest,
    input: String,
}

impl ClarificationOverlay {
    pub fn new(request: UiClarificationRequest) -> Self {
        Self {
            request,
            input: String::new(),
        }
    }

    pub fn input(&self) -> &str {
        &self.input
    }

    pub fn on_key(&mut self, key: KeyEvent) -> ClarificationOutcome {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return ClarificationOutcome::None;
        }
        match key.code {
            KeyCode::Esc => ClarificationOutcome::Answer(String::new()),
            KeyCode::Enter => ClarificationOutcome::Answer(self.input.trim().to_string()),
            KeyCode::Backspace => {
                pop_grapheme(&mut self.input);
                ClarificationOutcome::None
            }
            // A digit quick-selects an option, but only when no free text is
            // being typed (so numeric answers still work).
            KeyCode::Char(d @ '1'..='9') if self.input.is_empty() => {
                let idx = (d as u8 - b'1') as usize;
                match self.request.options.get(idx) {
                    Some(option) => ClarificationOutcome::Answer(option.clone()),
                    None => {
                        self.input.push(d);
                        ClarificationOutcome::None
                    }
                }
            }
            KeyCode::Char(c) => {
                self.input.push(c);
                ClarificationOutcome::None
            }
            _ => ClarificationOutcome::None,
        }
    }
}

fn pop_grapheme(s: &mut String) {
    if let Some((idx, _)) = s.grapheme_indices(true).next_back() {
        s.truncate(idx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_client_protocol::ClarificationId;

    fn req(options: Vec<&str>) -> UiClarificationRequest {
        UiClarificationRequest {
            id: ClarificationId::new("c1"),
            question: "选哪个方案？".into(),
            options: options.into_iter().map(String::from).collect(),
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    #[test]
    fn digit_selects_option() {
        let mut ov = ClarificationOverlay::new(req(vec!["A", "B"]));
        assert_eq!(
            ov.on_key(key(KeyCode::Char('2'))),
            ClarificationOutcome::Answer("B".into())
        );
    }

    #[test]
    fn free_text_then_enter() {
        let mut ov = ClarificationOverlay::new(req(vec![]));
        for c in "保留旧字段".chars() {
            ov.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(
            ov.on_key(key(KeyCode::Enter)),
            ClarificationOutcome::Answer("保留旧字段".into())
        );
    }

    #[test]
    fn esc_skips_with_empty_answer() {
        let mut ov = ClarificationOverlay::new(req(vec!["A"]));
        assert_eq!(
            ov.on_key(key(KeyCode::Esc)),
            ClarificationOutcome::Answer(String::new())
        );
    }

    #[test]
    fn backspace_edits_free_text() {
        let mut ov = ClarificationOverlay::new(req(vec![]));
        ov.on_key(key(KeyCode::Char('a')));
        ov.on_key(key(KeyCode::Char('b')));
        ov.on_key(key(KeyCode::Backspace));
        assert_eq!(ov.input(), "a");
    }
}
