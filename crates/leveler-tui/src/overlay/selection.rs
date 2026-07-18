//! A reusable single-select list , the shared core behind the model
//! and mode pickers: arrow/Ctrl-P/N + Enter
//! navigation, number quick-select when not searchable, type-to-filter when
//! searchable, recommended/current markers, and disabled rows with a reason that
//! cannot be confirmed.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_segmentation::UnicodeSegmentation;

/// One selectable row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionOption {
    /// Stable identifier returned on confirm.
    pub key: String,
    pub label: String,
    pub description: Option<String>,
    /// Marks the recommended row (shown with a `Recommended` badge, ).
    pub recommended: bool,
    /// Marks the row that is currently active.
    pub current: bool,
    /// `Some(reason)` makes the row un-confirmable and shows why .
    pub disabled_reason: Option<String>,
}

impl SelectionOption {
    pub fn new(key: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            label: label.into(),
            description: None,
            recommended: false,
            current: false,
            disabled_reason: None,
        }
    }

    pub fn description(mut self, text: impl Into<String>) -> Self {
        self.description = Some(text.into());
        self
    }

    pub fn recommended(mut self, yes: bool) -> Self {
        self.recommended = yes;
        self
    }

    pub fn current(mut self, yes: bool) -> Self {
        self.current = yes;
        self
    }

    pub fn disabled(mut self, reason: impl Into<String>) -> Self {
        self.disabled_reason = Some(reason.into());
        self
    }

    pub fn is_enabled(&self) -> bool {
        self.disabled_reason.is_none()
    }
}

/// What a key press produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionOutcome {
    /// Consumed; the overlay stays open.
    None,
    /// The user confirmed the option with this key.
    Confirm(String),
    /// The user dismissed the overlay (Esc).
    Cancel,
}

/// A single-select list model.
#[derive(Debug, Clone)]
pub struct SelectionModel {
    pub title: String,
    pub description: Option<String>,
    options: Vec<SelectionOption>,
    /// Cursor as an index into the currently visible (filtered) rows.
    cursor: usize,
    searchable: bool,
    query: String,
}

impl SelectionModel {
    /// Build a picker. If `searchable`, typing filters and number quick-select
    /// is disabled (digits go into the query).
    pub fn new(title: impl Into<String>, options: Vec<SelectionOption>, searchable: bool) -> Self {
        let mut model = Self {
            title: title.into(),
            description: None,
            options,
            cursor: 0,
            searchable,
            query: String::new(),
        };
        model.cursor = model.first_enabled_visible().unwrap_or(0);
        model
    }

    pub fn with_description(mut self, text: impl Into<String>) -> Self {
        self.description = Some(text.into());
        self
    }

    /// Force the initial cursor onto a specific option key (used for a
    /// safe-by-default focus). Falls back to the first enabled row.
    pub fn focus_key(mut self, key: &str) -> Self {
        if let Some(pos) = self
            .visible()
            .iter()
            .position(|&i| self.options[i].key == key)
        {
            self.cursor = pos;
        }
        self
    }

    // ---- inspection (for rendering) ----------------------------------------

    pub fn is_searchable(&self) -> bool {
        self.searchable
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    /// The visible rows with their absolute index and whether each is the cursor.
    pub fn visible_rows(&self) -> Vec<(usize, &SelectionOption, bool)> {
        self.visible()
            .into_iter()
            .enumerate()
            .map(|(vis, abs)| (abs, &self.options[abs], vis == self.cursor))
            .collect()
    }

    /// Absolute option indices currently visible under the query filter.
    fn visible(&self) -> Vec<usize> {
        if self.query.is_empty() {
            return (0..self.options.len()).collect();
        }
        let q = self.query.to_lowercase();
        self.options
            .iter()
            .enumerate()
            .filter(|(_, o)| {
                o.label.to_lowercase().contains(&q)
                    || o.key.to_lowercase().contains(&q)
                    || o.description
                        .as_deref()
                        .map(|d| d.to_lowercase().contains(&q))
                        .unwrap_or(false)
            })
            .map(|(i, _)| i)
            .collect()
    }

    fn first_enabled_visible(&self) -> Option<usize> {
        let vis = self.visible();
        vis.iter().position(|&abs| self.options[abs].is_enabled())
    }

    // ---- key handling -------------------------------------------------------

    pub fn on_key(&mut self, key: KeyEvent) -> SelectionOutcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => SelectionOutcome::Cancel,
            KeyCode::Up => {
                self.move_up();
                SelectionOutcome::None
            }
            KeyCode::Down => {
                self.move_down();
                SelectionOutcome::None
            }
            KeyCode::Char('p') if ctrl => {
                self.move_up();
                SelectionOutcome::None
            }
            KeyCode::Char('n') if ctrl => {
                self.move_down();
                SelectionOutcome::None
            }
            KeyCode::Enter => self.confirm_cursor(),
            KeyCode::Backspace if self.searchable => {
                pop_grapheme(&mut self.query);
                self.clamp_cursor();
                SelectionOutcome::None
            }
            // Digits quick-select only when not searchable .
            KeyCode::Char(d @ '1'..='9') if !self.searchable && !ctrl => self.quick_select(d),
            KeyCode::Char(c) if self.searchable && !ctrl => {
                self.query.push(c);
                self.cursor = self.first_enabled_visible().unwrap_or(0);
                SelectionOutcome::None
            }
            _ => SelectionOutcome::None,
        }
    }

    fn confirm_cursor(&mut self) -> SelectionOutcome {
        let vis = self.visible();
        match vis.get(self.cursor) {
            Some(&abs) if self.options[abs].is_enabled() => {
                SelectionOutcome::Confirm(self.options[abs].key.clone())
            }
            _ => SelectionOutcome::None,
        }
    }

    fn quick_select(&mut self, digit: char) -> SelectionOutcome {
        let n = (digit as u8 - b'1') as usize;
        let vis = self.visible();
        match vis.get(n) {
            Some(&abs) if self.options[abs].is_enabled() => {
                SelectionOutcome::Confirm(self.options[abs].key.clone())
            }
            _ => SelectionOutcome::None,
        }
    }

    fn move_up(&mut self) {
        let len = self.visible().len();
        if len == 0 {
            return;
        }
        // Step to the previous enabled row, wrapping.
        for _ in 0..len {
            self.cursor = if self.cursor == 0 {
                len - 1
            } else {
                self.cursor - 1
            };
            if self.cursor_enabled() {
                break;
            }
        }
    }

    fn move_down(&mut self) {
        let len = self.visible().len();
        if len == 0 {
            return;
        }
        for _ in 0..len {
            self.cursor = (self.cursor + 1) % len;
            if self.cursor_enabled() {
                break;
            }
        }
    }

    fn cursor_enabled(&self) -> bool {
        let vis = self.visible();
        vis.get(self.cursor)
            .map(|&abs| self.options[abs].is_enabled())
            .unwrap_or(false)
    }

    fn clamp_cursor(&mut self) {
        let len = self.visible().len();
        if len == 0 {
            self.cursor = 0;
        } else if self.cursor >= len {
            self.cursor = len - 1;
        }
    }
}

/// Remove the last grapheme from a string (Unicode-correct backspace).
fn pop_grapheme(s: &mut String) {
    if let Some((idx, _)) = s.grapheme_indices(true).next_back() {
        s.truncate(idx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    fn model() -> SelectionModel {
        SelectionModel::new(
            "Pick",
            vec![
                SelectionOption::new("a", "Alpha").recommended(true),
                SelectionOption::new("b", "Beta"),
                SelectionOption::new("c", "Gamma").disabled("unavailable"),
            ],
            false,
        )
    }

    #[test]
    fn enter_confirms_cursor() {
        let mut m = model();
        assert_eq!(
            m.on_key(key(KeyCode::Enter)),
            SelectionOutcome::Confirm("a".into())
        );
    }

    #[test]
    fn down_skips_disabled_and_wraps_to_enabled() {
        let mut m = model();
        m.on_key(key(KeyCode::Down)); // a -> b
        assert_eq!(
            m.on_key(key(KeyCode::Enter)),
            SelectionOutcome::Confirm("b".into())
        );
        m.on_key(key(KeyCode::Down)); // b -> (skip c) -> a
        assert_eq!(
            m.on_key(key(KeyCode::Enter)),
            SelectionOutcome::Confirm("a".into())
        );
    }

    #[test]
    fn number_quick_selects_when_not_searchable() {
        let mut m = model();
        assert_eq!(
            m.on_key(key(KeyCode::Char('2'))),
            SelectionOutcome::Confirm("b".into())
        );
    }

    #[test]
    fn disabled_row_cannot_be_confirmed_by_number() {
        let mut m = model();
        // 3rd row is disabled.
        assert_eq!(m.on_key(key(KeyCode::Char('3'))), SelectionOutcome::None);
    }

    #[test]
    fn esc_cancels() {
        let mut m = model();
        assert_eq!(m.on_key(key(KeyCode::Esc)), SelectionOutcome::Cancel);
    }

    #[test]
    fn search_filters_and_digits_are_text() {
        let mut m = SelectionModel::new(
            "Models",
            vec![
                SelectionOption::new("deepseek/v3", "deepseek/v3"),
                SelectionOption::new("glm/5", "glm/5"),
            ],
            true,
        );
        for c in "glm".chars() {
            m.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(m.query(), "glm");
        assert_eq!(m.visible_rows().len(), 1);
        assert_eq!(
            m.on_key(key(KeyCode::Enter)),
            SelectionOutcome::Confirm("glm/5".into())
        );
    }

    #[test]
    fn search_backspace_edits_query() {
        let mut m =
            SelectionModel::new("Models", vec![SelectionOption::new("glm/5", "glm/5")], true);
        m.on_key(key(KeyCode::Char('x')));
        assert_eq!(m.visible_rows().len(), 0);
        m.on_key(key(KeyCode::Backspace));
        assert_eq!(m.query(), "");
        assert_eq!(m.visible_rows().len(), 1);
    }

    #[test]
    fn focus_key_sets_initial_cursor() {
        let m = model().focus_key("b");
        let cursor_row = m.visible_rows().into_iter().find(|(_, _, is)| *is).unwrap();
        assert_eq!(cursor_row.1.key, "b");
    }
}
