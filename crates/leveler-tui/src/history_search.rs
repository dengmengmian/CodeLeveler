//! A transient view over the composer's existing submission history.
use crate::composer::Composer;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Default)]
pub struct HistorySearch {
    pub query: String,
    pub selected: usize,
}
impl HistorySearch {
    pub fn matches<'a>(&self, history: &'a [String]) -> Vec<&'a str> {
        let query = self.query.to_lowercase();
        history
            .iter()
            .rev()
            .filter(|text| text.to_lowercase().contains(&query))
            .take(100)
            .map(String::as_str)
            .collect()
    }
    pub fn handle_key(&mut self, key: KeyEvent, composer: &mut Composer) -> bool {
        let matches = self.matches(composer.history());
        match key.code {
            KeyCode::Esc => return true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return true,
            KeyCode::Enter => {
                if let Some(text) = matches.get(self.selected) {
                    let text = text.to_string();
                    composer.replace(text);
                    return true;
                }
            }
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => {
                self.selected = self
                    .selected
                    .saturating_add(1)
                    .min(matches.len().saturating_sub(1))
            }
            KeyCode::Backspace => {
                crate::help::pop_grapheme(&mut self.query);
                self.selected = 0;
            }
            KeyCode::Char(c)
                if !key.modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) =>
            {
                self.query.push(c);
                self.selected = 0;
            }
            _ => {}
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    #[test]
    fn bounded_results_and_control_keys_leave_live_draft_untouched() {
        let mut c = Composer::new();
        c.set_history((0..200).map(|i| format!("entry {i}")).collect());
        c.replace("draft");
        let mut search = HistorySearch::default();
        assert_eq!(search.matches(c.history()).len(), 100);
        assert_eq!(search.matches(c.history())[0], "entry 199");
        search.query = "e\u{301}".into();
        search.handle_key(key(KeyCode::Backspace), &mut c);
        assert!(search.query.is_empty());
        assert!(search.handle_key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &mut c
        ));
        assert_eq!(c.text(), "draft");
    }
    #[test]
    fn searches_chinese_and_newest_first_without_changing_draft() {
        let mut c = Composer::new();
        c.set_history(vec!["修复登录".into(), "其他".into(), "修复测试".into()]);
        c.replace("草稿");
        let mut s = HistorySearch::default();
        s.handle_key(key(KeyCode::Char('修')), &mut c);
        assert_eq!(s.matches(c.history()), vec!["修复测试", "修复登录"]);
        s.handle_key(key(KeyCode::Down), &mut c);
        assert_eq!(s.selected, 1);
        assert!(s.handle_key(key(KeyCode::Esc), &mut c));
        assert_eq!(c.text(), "草稿");
    }
    #[test]
    fn accept_restores_for_editing_without_recording_or_submitting() {
        let mut c = Composer::new();
        c.set_history(vec!["hello".into()]);
        c.replace("draft");
        let mut s = HistorySearch::default();
        assert!(s.handle_key(key(KeyCode::Enter), &mut c));
        assert_eq!(c.text(), "hello");
        assert_eq!(c.history(), &["hello"]);
    }
    #[test]
    fn empty_and_no_matches_preserve_draft_and_backspace_handles_unicode() {
        let mut c = Composer::new();
        c.replace("draft");
        let mut s = HistorySearch::default();
        assert!(!s.handle_key(key(KeyCode::Enter), &mut c));
        s.handle_key(key(KeyCode::Down), &mut c);
        assert_eq!(s.selected, 0);
        s.handle_key(key(KeyCode::Char('界')), &mut c);
        s.handle_key(key(KeyCode::Backspace), &mut c);
        assert!(s.query.is_empty());
        c.set_history(vec!["hello".into()]);
        s.query = "absent".into();
        assert!(s.matches(c.history()).is_empty());
        assert!(!s.handle_key(key(KeyCode::Enter), &mut c));
        assert_eq!(c.text(), "draft");
    }
}
