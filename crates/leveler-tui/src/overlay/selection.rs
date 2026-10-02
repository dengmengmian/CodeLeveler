//! A reusable single-select list , the shared core behind the model
//! and mode pickers: arrow/Ctrl-P/N + Enter
//! navigation, number quick-select when not searchable, type-to-filter when
//! searchable, recommended/current markers, and disabled rows with a reason that
//! cannot be confirmed.
//!
//! A searchable picker's query is a [`Composer`], the same editor the composer
//! and the clarification field use: one grapheme-safe buffer with one keymap.
//! What stays here is only what makes a picker a picker — the rows, the filter,
//! the host's own keys.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::composer::Composer;

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
    /// Whether a digit confirms a row outright. Off for destructive
    /// confirmations, where a stray key must not pick the dangerous choice.
    quick_select: bool,
    /// The filter text: a real editor, so a query can be corrected in the
    /// middle instead of only backspaced from the end.
    query: Composer,
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
            quick_select: true,
            query: Composer::new(),
        };
        model.cursor = model.first_enabled_visible().unwrap_or(0);
        model
    }

    pub fn with_description(mut self, text: impl Into<String>) -> Self {
        self.description = Some(text.into());
        self
    }

    /// Turn off digit quick-select. A destructive confirmation must be a
    /// deliberate move-to-the-row then Enter, never a single stray digit.
    pub fn without_quick_select(mut self) -> Self {
        self.quick_select = false;
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
        self.query.text()
    }

    /// The query and the caret's display column — the same contract every other
    /// editor surface uses to place its insertion point.
    pub fn query_field(&self) -> (&str, (usize, usize)) {
        (self.query.text(), self.query.cursor_row_col_display())
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
        let query = self.query.text();
        if query.is_empty() {
            return (0..self.options.len()).collect();
        }
        let q = query.to_lowercase();
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

    /// Feed one key press.
    ///
    /// The picker's own keys are matched first: navigation, quick-select, and
    /// the deliberate-confirmation rules are this widget's contract, and
    /// sharing an editor with the composer must not quietly re-map them.
    /// Everything the picker does not claim goes to the query, which edits with
    /// the editor's own keymap — Ctrl+A/E/U/K/W, Home/End, Delete, Left/Right,
    /// word movement — so the same keystroke means the same thing here as it
    /// does in the composer.
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
            // Digits quick-select only when not searchable and not a
            // confirmation that opted out. In a searchable picker a digit is
            // filter text, so it falls through to the query.
            KeyCode::Char(d @ '1'..='9') if !self.searchable && self.quick_select && !ctrl => {
                self.quick_select(d)
            }
            _ if self.searchable => self.edit_query(key),
            _ => SelectionOutcome::None,
        }
    }

    /// Hand one key to the query, and re-anchor the highlighted row when the
    /// filter actually changed: moving the caret inside an unchanged query must
    /// not jump the list back to the first row.
    fn edit_query(&mut self, key: KeyEvent) -> SelectionOutcome {
        let before = self.query.text().to_owned();
        if !self.query.apply_editing_key(key) {
            return SelectionOutcome::None;
        }
        if self.query.text() != before {
            self.cursor = self.first_enabled_visible().unwrap_or(0);
        }
        self.clamp_cursor();
        SelectionOutcome::None
    }

    /// Insert text that arrived as a bracketed paste or a typing burst.
    ///
    /// A search box is one line by contract, so a line break becomes a space
    /// instead of a second row or a `[Pasted: N lines]` chip. A chip names text
    /// that is going to be submitted; a filter never is, and this text is
    /// inserted rather than staged, so nothing outlives the picker.
    pub fn insert_query_text(&mut self, text: &str) {
        if !self.searchable || text.is_empty() {
            return;
        }
        let one_line = text.replace("\r\n", "\n").replace(['\n', '\r'], " ");
        self.query.insert_str(&one_line);
        self.cursor = self.first_enabled_visible().unwrap_or(0);
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

    // ---- the query is an editor, and the picker is still a picker -----------

    fn search_model() -> SelectionModel {
        SelectionModel::new(
            "Models",
            vec![
                SelectionOption::new("deepseek/v3", "deepseek/v3"),
                SelectionOption::new("glm/5", "glm/5"),
            ],
            true,
        )
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn type_str(m: &mut SelectionModel, s: &str) {
        for ch in s.chars() {
            m.on_key(key(KeyCode::Char(ch)));
        }
    }

    #[test]
    fn the_query_edits_with_the_editor_keymap() {
        let mut m = search_model();
        type_str(&mut m, "glm/5 deepseek/v3");
        assert_eq!(m.query(), "glm/5 deepseek/v3");
        // Ctrl+W eats the last word, Ctrl+U the rest of the line.
        m.on_key(ctrl('w'));
        assert_eq!(m.query(), "glm/5 ");
        m.on_key(ctrl('u'));
        assert_eq!(m.query(), "");
        // A real caret, not "always at the end".
        type_str(&mut m, "abc");
        m.on_key(key(KeyCode::Left));
        m.on_key(key(KeyCode::Left));
        m.on_key(key(KeyCode::Char('X')));
        assert_eq!(m.query(), "aXbc");
        m.on_key(key(KeyCode::Home));
        m.on_key(key(KeyCode::Delete));
        assert_eq!(m.query(), "Xbc");
        m.on_key(key(KeyCode::End));
        m.on_key(key(KeyCode::Backspace));
        assert_eq!(m.query(), "Xb");
        // Ctrl+A / Ctrl+E move within the query.
        m.on_key(ctrl('a'));
        m.on_key(key(KeyCode::Char('Y')));
        assert_eq!(m.query(), "YXb");
        m.on_key(ctrl('e'));
        m.on_key(key(KeyCode::Char('Z')));
        assert_eq!(m.query(), "YXbZ");
        m.on_key(ctrl('k'));
        assert_eq!(m.query(), "YXbZ", "the caret is already at the line end");
        m.on_key(ctrl('a'));
        m.on_key(ctrl('k'));
        assert_eq!(m.query(), "");
    }

    #[test]
    fn the_query_edits_unicode_by_grapheme() {
        let mut m = search_model();
        type_str(&mut m, "中文");
        assert_eq!(m.query(), "中文");
        m.on_key(key(KeyCode::Backspace));
        assert_eq!(m.query(), "中");
        // A combining mark delivered as its own key event joins the cluster
        // before it; the next backspace takes the whole cluster, not a stray
        // mark left behind.
        type_str(&mut m, "e\u{301}");
        assert_eq!(m.query(), "中e\u{301}");
        m.on_key(key(KeyCode::Backspace));
        assert_eq!(m.query(), "中");
    }

    #[test]
    fn a_paste_into_the_query_is_one_line_and_is_never_folded() {
        let mut m = search_model();
        m.insert_query_text("glm/5\r\nline two\rline three\nline four");
        assert_eq!(m.query(), "glm/5 line two line three line four");
        assert!(!m.query().contains('\n'));
        // Even a huge paste stays filter text: no chip, and nothing staged —
        // a chip names text that will be submitted, and a filter never is.
        let big = (1..=500)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut m = search_model();
        m.insert_query_text(&big);
        assert!(!m.query().contains("[Pasted"));
        assert!(!m.query().contains('\n'));
        assert!(m.query().starts_with("line 1 line 2"));
        assert_eq!(
            m.query().matches(' ').count(),
            999,
            "500 lines joined by spaces"
        );
    }

    #[test]
    fn a_paste_does_not_enter_a_picker_with_no_query() {
        let mut m = model();
        m.insert_query_text("hello");
        assert_eq!(m.query(), "");
    }

    #[test]
    fn moving_the_caret_inside_the_query_does_not_jump_the_row() {
        let mut m = search_model();
        type_str(&mut m, "glm");
        assert_eq!(m.visible_rows().len(), 1);
        let highlighted = m.visible_rows()[0].2;
        assert!(highlighted);
        m.on_key(key(KeyCode::Left));
        m.on_key(key(KeyCode::Home));
        m.on_key(ctrl('e'));
        assert_eq!(m.visible_rows().len(), 1);
        assert!(m.visible_rows()[0].2, "the row stays highlighted");
    }

    #[test]
    fn the_picker_s_own_keys_keep_their_meaning_with_a_query() {
        // Ctrl+N / Ctrl+P walk the list; they do not edit the query.
        let mut m = search_model();
        m.on_key(ctrl('n'));
        assert_eq!(m.query(), "");
        assert_eq!(
            m.on_key(key(KeyCode::Enter)),
            SelectionOutcome::Confirm("glm/5".into())
        );
        let mut m = search_model();
        m.on_key(key(KeyCode::Down));
        m.on_key(ctrl('p'));
        assert_eq!(
            m.on_key(key(KeyCode::Enter)),
            SelectionOutcome::Confirm("deepseek/v3".into())
        );
        // A digit is filter text in a searchable picker, never a jump.
        let mut m = search_model();
        m.on_key(key(KeyCode::Char('5')));
        assert_eq!(m.query(), "5");
        assert_eq!(
            m.on_key(key(KeyCode::Enter)),
            SelectionOutcome::Confirm("glm/5".into())
        );
        // Esc still cancels.
        assert_eq!(m.on_key(key(KeyCode::Esc)), SelectionOutcome::Cancel);
    }
}
