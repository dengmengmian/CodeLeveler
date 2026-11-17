//! `/help` — the command browser.
//!
//! The list is generated from [`crate::screen::SLASH_DEFS`], the same registry
//! the parser, completion popup and `/` menu use. There is no second command
//! list here: a command that cannot run cannot appear, and a command that
//! appears is dispatched by the same canonical name.
//!
//! Interaction follows the same pattern as the other secondary pages: a
//! searchable grouped list, Enter for a detail page, Esc one level back, and a
//! Tab cycle over the registry's categories. Help never executes a command —
//! the detail page tells the reader what to type.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};

use crate::i18n::UiText;
use crate::screen::{BusyPolicy, SlashCategory};

#[derive(Debug, Default)]
pub struct HelpSearch {
    pub query: String,
    pub selected: usize,
    /// `None` means every category.
    pub category: Option<SlashCategory>,
    /// The canonical command whose detail page is open.
    pub detail: Option<&'static str>,
    pub detail_scroll: usize,
}

#[derive(Debug)]
pub struct HelpEntry {
    pub binding: String,
    pub description: String,
    pub aliases: Vec<&'static str>,
    pub usage: &'static str,
    pub category: Option<SlashCategory>,
    pub busy: Option<BusyPolicy>,
    /// The canonical command, for opening the detail page. `None` for a
    /// keyboard row.
    pub command: Option<&'static str>,
    pub is_key: bool,
}

/// Every registry command (in registry order) followed by the keyboard
/// cheatsheet. The caller groups commands by [`HelpEntry::category`].
pub fn entries(t: &UiText) -> Vec<HelpEntry> {
    let mut entries = Vec::new();
    for def in crate::screen::SLASH_DEFS.iter().filter(|def| def.listed) {
        entries.push(HelpEntry {
            binding: def.name.to_string(),
            description: crate::screen::slash_description(def.name, t).to_string(),
            aliases: def.aliases.to_vec(),
            usage: crate::screen::slash_usage(def.name),
            category: Some(def.category),
            busy: Some(def.busy),
            command: Some(def.name),
            is_key: false,
        });
    }
    for (binding, description) in key_bindings(t) {
        entries.push(HelpEntry {
            binding,
            description,
            aliases: Vec::new(),
            usage: "",
            category: None,
            busy: None,
            command: None,
            is_key: true,
        });
    }
    entries
}

fn key_bindings(t: &UiText) -> Vec<(String, String)> {
    let keys: Vec<(String, String)> = vec![
        ("Enter".into(), t.key_submit.into()),
        ("Ctrl+R".into(), t.key_history_search.into()),
        ("Ctrl+Z".into(), t.key_undo.into()),
        ("Ctrl+Shift+Z / Alt+Z".into(), t.key_redo.into()),
        ("Ctrl+Y".into(), t.key_yank.into()),
        ("Ctrl+J / Alt+Enter".into(), t.key_newline.into()),
        ("Ctrl+X Ctrl+E".into(), t.key_editor.into()),
        ("Shift+Tab".into(), t.key_permission.into()),
        ("Tab".into(), t.key_tab_focus.into()),
        (
            format!("↑/↓ ({})", t.help_scope_input),
            t.key_history.into(),
        ),
        (
            format!("↑/↓ ({})", t.help_scope_conversation),
            t.key_scroll_messages.into(),
        ),
        ("PageUp/PageDown".into(), t.key_scroll_page.into()),
        ("Ctrl+C".into(), t.key_cancel_quit.into()),
        ("Ctrl+V".into(), t.key_paste_image.into()),
        ("Ctrl+O".into(), t.key_expand.into()),
        ("Ctrl+?".into(), t.help_title.into()),
        ("Ctrl+D/T/S".into(), t.key_screens.into()),
        ("Ctrl+End / Ctrl+↓".into(), t.key_jump.into()),
        ("Ctrl+G".into(), t.key_final_nav.into()),
        ("End".into(), t.key_end.into()),
        ("Esc".into(), t.key_esc.into()),
    ];
    keys
}

impl HelpSearch {
    /// The entries this page shows under the current query and category.
    pub fn matches(&self, t: &UiText) -> Vec<HelpEntry> {
        let query = self.query.to_lowercase();
        entries(t)
            .into_iter()
            .filter(|entry| {
                if let Some(category) = self.category {
                    if entry.is_key {
                        // The keyboard section is not part of a command
                        // category. A command-category view hides it rather
                        // than pretending every key belongs to e.g. Agent.
                        return false;
                    }
                    if entry.category != Some(category) {
                        return false;
                    }
                }
                query.is_empty()
                    || entry.binding.to_lowercase().contains(&query)
                    || entry.description.to_lowercase().contains(&query)
                    || entry.usage.to_lowercase().contains(&query)
                    || entry
                        .aliases
                        .iter()
                        .any(|alias| alias.to_lowercase().contains(&query))
            })
            .collect()
    }

    pub fn detail_entry(&self, t: &UiText) -> Option<HelpEntry> {
        let name = self.detail?;
        entries(t)
            .into_iter()
            .find(|entry| entry.command == Some(name))
    }

    /// Point the selection at a row the page painted. Clamped to the current
    /// result set, so a click can never leave the cursor past the end of a
    /// list that shrank.
    pub fn select(&mut self, index: usize, t: &UiText) {
        self.selected = index.min(self.matches(t).len().saturating_sub(1));
    }

    /// Move the selection by `delta` rows over the current result set. The
    /// keyboard's ↑↓/PageUp/PageDown and the wheel all run this one move, so
    /// the two inputs cannot disagree about where the list ends.
    pub fn move_selection(&mut self, delta: isize, t: &UiText) {
        let max = self.matches(t).len().saturating_sub(1);
        self.selected = if delta < 0 {
            self.selected.saturating_sub(delta.unsigned_abs())
        } else {
            self.selected.saturating_add(delta as usize).min(max)
        };
    }

    /// Cycle the category filter in the registry's display order.
    pub fn cycle_category(&mut self, forward: bool) {
        let order = [
            None,
            Some(SlashCategory::Agent),
            Some(SlashCategory::View),
            Some(SlashCategory::Session),
            Some(SlashCategory::Input),
            Some(SlashCategory::System),
        ];
        let index = order
            .iter()
            .position(|category| *category == self.category)
            .unwrap_or(0);
        let next = if forward {
            (index + 1) % order.len()
        } else {
            (index + order.len() - 1) % order.len()
        };
        self.category = order[next];
        self.selected = 0;
    }

    /// Open the selected command's detail page, when the row is a command.
    pub fn open_detail(&mut self, t: &UiText) {
        if self.detail.is_some() {
            return;
        }
        let entries = self.matches(t);
        if let Some(entry) = entries.get(self.selected)
            && let Some(command) = entry.command
        {
            self.detail = Some(command);
            self.detail_scroll = 0;
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent, t: &UiText, page: usize) {
        if self.detail.is_some() {
            match key.code {
                KeyCode::Esc => {
                    self.detail = None;
                    self.detail_scroll = 0;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.detail_scroll = self.detail_scroll.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.detail_scroll = self.detail_scroll.saturating_add(1);
                }
                KeyCode::PageUp => {
                    self.detail_scroll = self.detail_scroll.saturating_sub(10);
                }
                KeyCode::PageDown => {
                    self.detail_scroll = self.detail_scroll.saturating_add(10);
                }
                KeyCode::Home => self.detail_scroll = 0,
                _ => {}
            }
            return;
        }
        let max = self.matches(t).len().saturating_sub(1);
        match key.code {
            KeyCode::Up => self.move_selection(-1, t),
            KeyCode::Down => self.move_selection(1, t),
            KeyCode::PageUp => self.move_selection(-(page as isize), t),
            KeyCode::PageDown => self.move_selection(page as isize, t),
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = max,
            KeyCode::Enter => self.open_detail(t),
            KeyCode::Tab => self.cycle_category(true),
            KeyCode::BackTab => self.cycle_category(false),
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

/// Wheel and click share the keyboard's selection model. A row click selects;
/// Enter (or a second click is deliberately not a special case) opens detail.
pub fn handle_mouse(
    state: &mut crate::state::AppState,
    mouse: MouseEvent,
) -> Vec<crate::action::Effect> {
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            if state.help.detail.is_some() {
                state.help.detail_scroll = state.help.detail_scroll.saturating_sub(3);
            } else {
                let t = state.t();
                state.help.move_selection(-1, t);
            }
        }
        MouseEventKind::ScrollDown => {
            if state.help.detail.is_some() {
                state.help.detail_scroll = state.help.detail_scroll.saturating_add(3);
            } else {
                let t = state.t();
                state.help.move_selection(1, t);
            }
        }
        MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
            let point = (mouse.column, mouse.row).into();
            if let Some((_, key)) = state
                .screen_rows
                .iter()
                .find(|(rect, _)| rect.contains(point))
                && let Ok(index) = key.parse::<usize>()
            {
                let t = state.t();
                state.help.select(index, t);
            }
        }
        _ => {}
    }
    Vec::new()
}

/// Open Help directly on one command's detail page (`/help <command>`).
pub fn open_detail_for(state: &mut crate::state::AppState, command: &'static str) {
    state.active_screen = crate::screen::Screen::Help;
    state.screen_scroll = 0;
    state.detail_return = None;
    state.help.query.clear();
    state.help.category = None;
    state.help.selected = 0;
    state.help.detail = Some(command);
    state.help.detail_scroll = 0;
}

/// Open Help with the list filtered by `query`, when no command matched.
pub fn open_filtered(state: &mut crate::state::AppState, query: &str) {
    state.active_screen = crate::screen::Screen::Help;
    state.screen_scroll = 0;
    state.detail_return = None;
    state.help.query = query.to_string();
    state.help.category = None;
    state.help.selected = 0;
    state.help.detail = None;
    state.help.detail_scroll = 0;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_registered_listed_command_appears_exactly_once() {
        let t = crate::i18n::Locale::Zh.text();
        let entries = entries(t);
        for def in crate::screen::SLASH_DEFS.iter().filter(|def| def.listed) {
            let matches = entries
                .iter()
                .filter(|entry| entry.command == Some(def.name))
                .count();
            assert_eq!(matches, 1, "{} is not listed exactly once", def.name);
        }
        assert!(entries.iter().any(|entry| entry.is_key));
    }

    #[test]
    fn every_usage_line_names_its_command() {
        for def in crate::screen::SLASH_DEFS.iter().filter(|def| def.listed) {
            let usage = crate::screen::slash_usage(def.name);
            assert!(
                usage.starts_with(def.name),
                "{} usage starts with another command: {usage}",
                def.name
            );
        }
    }

    #[test]
    fn aliases_are_searchable_and_shown_on_the_detail_row() {
        let t = crate::i18n::Locale::En.text();
        let help = HelpSearch {
            query: "/mode".into(),
            ..HelpSearch::default()
        };
        let rows = help.matches(t);
        assert!(
            rows.iter()
                .any(|entry| entry.command == Some("/permission"))
        );
        let detail = rows
            .iter()
            .find(|entry| entry.command == Some("/permission"))
            .unwrap();
        assert!(detail.aliases.contains(&"/mode"));
    }

    #[test]
    fn tab_cycles_categories_and_enter_opens_a_detail_level() {
        let t = crate::i18n::Locale::Zh.text();
        let mut help = HelpSearch::default();
        help.cycle_category(true);
        assert_eq!(help.category, Some(SlashCategory::Agent));
        assert!(help.matches(t).iter().all(|entry| !entry.is_key));
        help.selected = 0;
        help.open_detail(t);
        assert!(help.detail.is_some());
        help.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), t, 10);
        assert!(help.detail.is_none());
    }

    #[test]
    fn editing_the_filter_resets_selection_and_navigation_clamps() {
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
