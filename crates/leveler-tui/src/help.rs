//! Searchable help uses the same slash-command descriptions as completion.
use crate::i18n::UiText;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Default)]
pub struct HelpSearch {
    pub query: String,
    pub selected: usize,
}
#[derive(Debug)]
pub struct HelpEntry {
    pub binding: String,
    pub description: String,
}

pub fn entries(t: &UiText) -> Vec<HelpEntry> {
    let mut entries = Vec::new();
    for (_, commands) in crate::screen::slash_commands_grouped(t) {
        for (name, desc) in commands {
            entries.push(HelpEntry {
                binding: name.to_string(),
                description: desc.to_string(),
            });
        }
    }
    let keys = [
        ("Enter", t.key_submit),
        ("Ctrl+R", t.key_history_search),
        ("Ctrl+Z", t.key_undo),
        ("Ctrl+Shift+Z / Alt+Z", t.key_redo),
        ("Ctrl+Y", t.key_yank),
        ("Ctrl+J / Alt+Enter", t.key_newline),
        ("Ctrl+X Ctrl+E", t.key_editor),
        ("Shift+Tab", t.key_permission),
        ("Tab", t.key_tab_focus),
        (&format!("↑/↓ ({})", t.help_scope_input), t.key_history),
        (
            &format!("↑/↓ ({})", t.help_scope_conversation),
            t.key_scroll_messages,
        ),
        ("PageUp/PageDown", t.key_scroll_page),
        ("Ctrl+C", t.key_cancel_quit),
        ("Ctrl+M", t.key_model),
        ("Ctrl+V", t.key_paste_image),
        ("Ctrl+O", t.key_expand),
        ("Ctrl+?", t.help_title),
        ("Ctrl+D/T/S", t.key_screens),
        ("Ctrl+End / Ctrl+↓", t.key_jump),
        ("Ctrl+G", t.key_final_nav),
        ("End", t.key_end),
        ("Esc", t.key_esc),
    ];

    entries.extend(keys.into_iter().map(|(binding, description)| HelpEntry {
        binding: binding.to_string(),
        description: description.to_string(),
    }));
    entries
}
impl HelpSearch {
    pub fn matches(&self, t: &UiText) -> Vec<HelpEntry> {
        let query = self.query.to_lowercase();
        entries(t)
            .into_iter()
            .filter(|entry| {
                entry.binding.to_lowercase().contains(&query)
                    || entry.description.to_lowercase().contains(&query)
            })
            .collect()
    }
    pub fn handle_key(&mut self, key: KeyEvent, t: &UiText, page: usize) {
        let max = self.matches(t).len().saturating_sub(1);
        match key.code {
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => self.selected = self.selected.saturating_add(1).min(max),
            KeyCode::PageUp => self.selected = self.selected.saturating_sub(page),
            KeyCode::PageDown => self.selected = self.selected.saturating_add(page).min(max),
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = max,
            KeyCode::Backspace => {
                pop_grapheme(&mut self.query);
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
    }
}
pub(crate) fn pop_grapheme(text: &mut String) {
    use unicode_segmentation::UnicodeSegmentation;
    if let Some((start, _)) = text.grapheme_indices(true).next_back() {
        text.truncate(start);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn matches_bindings_and_localized_descriptions() {
        let t = crate::i18n::Locale::Zh.text();
        let mut help = HelpSearch::default();
        assert!(help.matches(t).iter().any(|e| e.binding == "/goal"));
        help.query = "ctrl+j".into();
        assert!(help.matches(t).iter().any(|e| e.binding.contains("Ctrl+J")));
        help.query = t.key_editor.into();
        assert!(help.matches(t).iter().any(|e| e.binding == "Ctrl+X Ctrl+E"));
    }
    #[test]
    fn editing_filter_resets_selection_and_navigation_clamps() {
        let t = crate::i18n::Locale::En.text();
        let mut help = HelpSearch::default();
        help.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE), t, 5);
        assert_eq!(help.selected, help.matches(t).len() - 1);
        help.handle_key(KeyEvent::new(KeyCode::Char('界'), KeyModifiers::NONE), t, 5);
        assert_eq!(help.selected, 0);
        assert!(help.matches(t).is_empty());
        help.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), t, 5);
        assert_eq!(help.selected, 0);
        help.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE), t, 5);
        assert!(help.query.is_empty());
    }
}
