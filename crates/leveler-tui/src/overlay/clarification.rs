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

    /// The option a lone digit stands for, if any. What `1-9 选项` means: the
    /// digit is typed like any other character and only Enter submits, so an
    /// answer that merely STARTS with a digit stays one answer instead of
    /// being cut in half — the first key sent, the rest left behind in the
    /// composer.
    pub fn selected_option(&self) -> Option<usize> {
        let trimmed = self.input.trim();
        let mut chars = trimmed.chars();
        let digit = chars.next().filter(|c| c.is_ascii_digit())?;
        if chars.next().is_some() {
            return None;
        }
        let index = digit.to_digit(10)? as usize;
        let index = index.checked_sub(1)?;
        (index < self.request.options.len()).then_some(index)
    }

    fn answer(&self) -> String {
        match self.selected_option() {
            Some(index) => self.request.options[index].clone(),
            None => self.input.trim().to_string(),
        }
    }

    /// Pasted / burst text goes into the answer field. The prompt is a
    /// single-line input, so newlines become spaces — but the content reaches
    /// the question instead of being swallowed by the composer hidden behind
    /// the overlay (R004 F2).
    pub fn insert_text(&mut self, s: &str) {
        let normalized = s
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .replace('\n', " ");
        self.input.push_str(&normalized);
    }

    pub fn on_key(&mut self, key: KeyEvent) -> ClarificationOutcome {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return ClarificationOutcome::None;
        }
        match key.code {
            KeyCode::Esc => ClarificationOutcome::Answer(String::new()),
            KeyCode::Enter => ClarificationOutcome::Answer(self.answer()),
            KeyCode::Backspace => {
                pop_grapheme(&mut self.input);
                ClarificationOutcome::None
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
    fn a_digit_then_enter_picks_that_option() {
        let mut ov = ClarificationOverlay::new(req(vec!["A", "B"]));
        assert_eq!(
            ov.on_key(key(KeyCode::Char('2'))),
            ClarificationOutcome::None
        );
        assert_eq!(
            ov.on_key(key(KeyCode::Enter)),
            ClarificationOutcome::Answer("B".into())
        );
    }

    /// A person answering "3，其余不变" starts with a digit. Submitting on the
    /// digit alone sent option 3 and dropped the rest of the sentence into the
    /// composer behind the overlay, unasked.
    #[test]
    fn an_answer_that_starts_with_a_digit_stays_one_answer() {
        let mut ov = ClarificationOverlay::new(req(vec!["A", "B", "C"]));
        for c in "3，其余不变".chars() {
            assert_eq!(ov.on_key(key(KeyCode::Char(c))), ClarificationOutcome::None);
        }
        assert_eq!(
            ov.on_key(key(KeyCode::Enter)),
            ClarificationOutcome::Answer("3，其余不变".into())
        );
    }

    /// A digit no option answers to is the answer itself.
    #[test]
    fn a_digit_outside_the_options_is_free_text() {
        let mut ov = ClarificationOverlay::new(req(vec!["A"]));
        ov.on_key(key(KeyCode::Char('7')));
        assert_eq!(
            ov.on_key(key(KeyCode::Enter)),
            ClarificationOutcome::Answer("7".into())
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
