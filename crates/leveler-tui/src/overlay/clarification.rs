//! The clarification interaction (spec §35): the agent asked the user to
//! settle something mid-task.
//!
//! A clarification can carry several questions. They are answered as tabs —
//! one question on screen at a time, so the screen never becomes a wall of
//! prompts:
//!
//! - `Tab` / `Shift+Tab` switch question; `↑`/`↓` walk the current question's
//!   options; `Space` toggles a multi-choice pick; `Enter` confirms the
//!   question and moves to the next unanswered one.
//! - `Esc` ends the whole interaction as a skip. It never cancels the main
//!   task: the runtime reads an empty answer as "the user chose not to answer",
//!   which is a different fact from "the task was stopped".
//!
//! The overlay owns every key while it is open (see `reducer::overlay_keys`),
//! so nothing here has to defend against the composer or transcript stealing a
//! keystroke.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use leveler_client_protocol::{
    ClarificationQuestionKind, UiClarificationQuestion, UiClarificationRequest,
};

use crate::composer::Composer;

/// Result of a key press on the clarification interaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClarificationOutcome {
    None,
    /// The interaction resolved. An empty string is an explicit skip.
    Answer(String),
}

/// Why `Enter` did not confirm a multi-choice question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClarificationNotice {
    MinChoices(u32),
    MaxChoices(u32),
}

/// What the user settled one question to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Answer {
    /// Option indices. One for a single-choice question, N for a multi.
    Picks(Vec<usize>),
    /// Typed text: a text question, or the free-text "其他…" row.
    Text(String),
}

/// One question of the interaction, with the state the user builds up in it.
#[derive(Debug, Clone)]
pub(crate) struct Question {
    pub(crate) header: String,
    pub(crate) prompt: String,
    pub(crate) kind: ClarificationQuestionKind,
    pub(crate) options: Vec<String>,
    pub(crate) allow_other: bool,
    pub(crate) min_choices: u32,
    pub(crate) max_choices: Option<u32>,
    /// Index into the rendered rows: the options, then the "其他…" row.
    pub(crate) cursor: usize,
    pub(crate) selected: Vec<bool>,
    /// The field this question is answered in. The composer's editor, not a
    /// second one: what edits a draft has to edit an answer the same way.
    pub(crate) text: Composer,
    pub(crate) answer: Option<Answer>,
}

impl Question {
    fn new(q: &UiClarificationQuestion) -> Self {
        // A choice question with nothing to choose from can only be answered
        // by typing. Reading the kind literally here would render an empty
        // list the user can never confirm.
        let kind = match (q.kind, q.options.is_empty()) {
            (ClarificationQuestionKind::Single | ClarificationQuestionKind::Multi, true) => {
                ClarificationQuestionKind::Text
            }
            (kind, _) => kind,
        };
        Self {
            header: q.header.clone(),
            prompt: q.question.clone(),
            kind,
            selected: vec![false; q.options.len()],
            options: q.options.clone(),
            allow_other: q.allow_other && kind == ClarificationQuestionKind::Single,
            min_choices: q.min_choices,
            max_choices: q.max_choices,
            cursor: 0,
            text: Composer::new(),
            answer: None,
        }
    }

    /// The legacy single-question shape, read as one question.
    ///
    /// `allow_other` is on for a choice: the old overlay always accepted a
    /// typed answer next to the options, and dropping that would remove a way
    /// to answer without giving the user anything in its place.
    fn legacy(request: &UiClarificationRequest) -> Self {
        let (kind, allow_other) = if request.options.is_empty() {
            (ClarificationQuestionKind::Text, false)
        } else {
            (ClarificationQuestionKind::Single, true)
        };
        Self {
            header: String::new(),
            prompt: request.question.clone(),
            kind,
            selected: vec![false; request.options.len()],
            options: request.options.clone(),
            allow_other,
            min_choices: 0,
            max_choices: None,
            cursor: 0,
            text: Composer::new(),
            answer: None,
        }
    }

    /// Rows in the option list, including the free-text row when offered.
    pub(crate) fn rows(&self) -> usize {
        self.options.len() + usize::from(self.allow_other)
    }

    /// Whether the cursor currently sits on the free-text "其他…" row.
    pub(crate) fn on_other_row(&self) -> bool {
        self.allow_other && self.cursor >= self.options.len()
    }

    /// Whether typing goes into this question's text field.
    pub(crate) fn typing(&self) -> bool {
        self.kind == ClarificationQuestionKind::Text || self.on_other_row()
    }

    pub(crate) fn is_multi(&self) -> bool {
        self.kind == ClarificationQuestionKind::Multi
    }

    /// The field's text and the caret's `(row, display column)` within it, in
    /// the shape the shared wrapping and caret rule takes.
    pub(crate) fn field_cursor(&self) -> (&str, (usize, usize)) {
        (self.text.text(), self.text.cursor_row_col_display())
    }

    /// The tab label: the model's, or a short read of the prompt when it sent
    /// none. Derived for display only — never written back to the request.
    pub(crate) fn display_header(&self) -> String {
        let header = self.header.trim();
        if !header.is_empty() {
            return header.to_string();
        }
        let first = self
            .prompt
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("");
        crate::render::text::truncate_display(first, HEADER_MAX_DISPLAY)
    }

    fn move_cursor(&mut self, delta: isize) {
        let rows = self.rows();
        if rows == 0 || self.typing() {
            return;
        }
        let rows = rows as isize;
        self.cursor = ((self.cursor as isize + delta).rem_euclid(rows)) as usize;
    }

    /// Space: a multi-choice toggle, or a literal space in a text field.
    fn space(&mut self) -> Option<ClarificationNotice> {
        if self.typing() {
            self.text.insert_char(' ');
            return None;
        }
        if !self.is_multi() || self.cursor >= self.options.len() {
            return None;
        }
        if self.selected[self.cursor] {
            self.selected[self.cursor] = false;
            return None;
        }
        // Refuse at the ceiling rather than accepting the pick and rejecting
        // the whole answer later: the user sees which pick was refused.
        if let Some(max) = self.max_choices
            && self.selected.iter().filter(|s| **s).count() as u32 >= max
        {
            return Some(ClarificationNotice::MaxChoices(max));
        }
        self.selected[self.cursor] = true;
        None
    }
}

/// How many display columns a derived tab label may take.
const HEADER_MAX_DISPLAY: usize = 8;

/// The clarification interaction.
#[derive(Debug, Clone)]
pub struct ClarificationOverlay {
    pub request: UiClarificationRequest,
    /// True when the request carried no `questions`, i.e. the client is
    /// showing the runtime's single-question projection.
    legacy: bool,
    questions: Vec<Question>,
    active: usize,
    notice: Option<ClarificationNotice>,
}

impl ClarificationOverlay {
    pub fn new(request: UiClarificationRequest) -> Self {
        let legacy = request.questions.is_empty();
        let questions = if legacy {
            vec![Question::legacy(&request)]
        } else {
            request.questions.iter().map(Question::new).collect()
        };
        Self {
            request,
            legacy,
            questions,
            active: 0,
            notice: None,
        }
    }

    // ---- inspection (rendering + tests) ------------------------------------

    pub fn active(&self) -> usize {
        self.active
    }

    pub fn len(&self) -> usize {
        self.questions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.questions.is_empty()
    }

    /// A request with no structured questions renders as one question with no
    /// tab strip: a strip of one is chrome, not information.
    pub fn legacy(&self) -> bool {
        self.legacy
    }

    pub fn answered_count(&self) -> usize {
        self.questions.iter().filter(|q| q.answer.is_some()).count()
    }

    pub fn is_answered(&self, index: usize) -> bool {
        self.questions
            .get(index)
            .is_some_and(|q| q.answer.is_some())
    }

    pub(crate) fn questions(&self) -> &[Question] {
        &self.questions
    }

    pub(crate) fn notice(&self) -> Option<ClarificationNotice> {
        self.notice
    }

    /// The active question's text field, empty when it has none.
    pub fn active_text(&self) -> &str {
        self.questions
            .get(self.active)
            .map(|q| q.text.text())
            .unwrap_or("")
    }

    /// Pasted / burst text goes into the active question's text field. A
    /// question with no field (a multi-choice list, a single choice without
    /// "其他…") has nowhere to put it, and the text is not an answer — it is
    /// left alone rather than smuggled to the composer hidden behind the
    /// overlay.
    ///
    /// The paste goes in exactly as the composer takes one: a large paste is
    /// shown as a chip and kept whole underneath it, so the answer that is sent
    /// is the text that was pasted. Nothing is flattened here — a pasted log has
    /// to arrive with its line breaks.
    pub fn insert_text(&mut self, s: &str) {
        let Some(question) = self.questions.get_mut(self.active) else {
            return;
        };
        if question.kind == ClarificationQuestionKind::Text {
            question.text.insert_paste(s);
            return;
        }
        if question.allow_other {
            // Move the cursor onto the row the text lands in, so a paste is
            // visible instead of appearing to do nothing.
            question.cursor = question.options.len();
            question.text.insert_paste(s);
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> ClarificationOutcome {
        // Editing first. While a field is focused, the keys that edit a draft
        // belong to the editor; the interaction's own keys — Enter, Esc, Tab,
        // the arrows, Space on a choice row — are matched below and are
        // untouched. A Ctrl key the editor does not claim falls through to
        // this match, which ignores it, rather than typing its letter.
        if let Some(question) = self.questions.get_mut(self.active)
            && question.typing()
            && question.text.apply_editing_key(key)
        {
            return ClarificationOutcome::None;
        }
        match key.code {
            KeyCode::Esc => ClarificationOutcome::Answer(String::new()),
            // Shift+Tab arrives as `BackTab` on most terminals and as
            // Tab+SHIFT on some; both mean "previous question".
            KeyCode::BackTab => {
                self.step(-1);
                ClarificationOutcome::None
            }
            KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.step(-1);
                ClarificationOutcome::None
            }
            KeyCode::Tab => {
                self.step(1);
                ClarificationOutcome::None
            }
            KeyCode::Up => {
                self.mutate_active(|q| q.move_cursor(-1));
                ClarificationOutcome::None
            }
            KeyCode::Down => {
                self.mutate_active(|q| q.move_cursor(1));
                ClarificationOutcome::None
            }
            KeyCode::Char(' ') => {
                let notice = self.questions.get_mut(self.active).and_then(|q| q.space());
                // A successful toggle clears the previous refusal: leaving it
                // on screen after the user fixed the pick would keep claiming
                // a constraint that is no longer being broken.
                self.notice = notice;
                ClarificationOutcome::None
            }
            KeyCode::Enter => self.confirm_active(),
            _ => ClarificationOutcome::None,
        }
    }

    fn mutate_active(&mut self, f: impl FnOnce(&mut Question)) {
        if let Some(q) = self.questions.get_mut(self.active) {
            f(q);
        }
        self.notice = None;
    }

    /// `Tab` switching. Free in both directions, answered or not: revisiting a
    /// settled question to change the answer is the point of the tabs.
    fn step(&mut self, delta: isize) {
        if self.questions.is_empty() {
            return;
        }
        let n = self.questions.len() as isize;
        self.active = ((self.active as isize + delta).rem_euclid(n)) as usize;
        self.notice = None;
    }

    fn confirm_active(&mut self) -> ClarificationOutcome {
        let Some(q) = self.questions.get_mut(self.active) else {
            return ClarificationOutcome::Answer(String::new());
        };
        if q.typing() {
            // The editor may be holding a paste chip; the answer is the text it
            // stands for, never the chip.
            let text = q.text.canonical_text();
            let text = text.trim().to_string();
            // An empty "其他…" entry is not an answer — there is nothing to
            // record. Leaving the question unanswered is the honest outcome;
            // Esc leaves the field, Enter stays until something is typed.
            if q.kind == ClarificationQuestionKind::Text || !text.is_empty() {
                q.answer = Some(Answer::Text(text));
            } else {
                return ClarificationOutcome::None;
            }
        } else if q.is_multi() {
            let picks: Vec<usize> = q
                .selected
                .iter()
                .enumerate()
                .filter_map(|(i, on)| on.then_some(i))
                .collect();
            if (picks.len() as u32) < q.min_choices {
                self.notice = Some(ClarificationNotice::MinChoices(q.min_choices));
                return ClarificationOutcome::None;
            }
            q.answer = Some(Answer::Picks(picks));
        } else {
            if q.options.is_empty() {
                return ClarificationOutcome::None;
            }
            q.answer = Some(Answer::Picks(vec![q.cursor.min(q.options.len() - 1)]));
        }
        self.notice = None;
        // Prefer the next question still waiting for an answer; when every
        // question is settled, the interaction is done and submits.
        if let Some(next) = self.next_unanswered() {
            self.active = next;
            return ClarificationOutcome::None;
        }
        ClarificationOutcome::Answer(self.compose())
    }

    fn next_unanswered(&self) -> Option<usize> {
        let n = self.questions.len();
        (1..=n)
            .map(|offset| (self.active + offset) % n)
            .find(|i| self.questions[*i].answer.is_none())
    }

    /// The answer the runtime sends to the model.
    ///
    /// One labelled line per question, so the model can tell which answer
    /// belongs to which question. The legacy single-question shape keeps its
    /// bare answer: there is only one question, and a label would be noise the
    /// old protocol never carried.
    pub fn compose(&self) -> String {
        if self.legacy {
            return self
                .questions
                .first()
                .and_then(|q| q.answer.as_ref())
                .map(|answer| answer_text(answer, self.questions[0].options.as_slice()))
                .unwrap_or_default();
        }
        self.questions
            .iter()
            .map(|q| {
                let value = q
                    .answer
                    .as_ref()
                    .map(|answer| answer_text(answer, &q.options))
                    .unwrap_or_else(|| "(none)".to_string());
                format!("{}: {value}", q.display_header())
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// One recorded answer as the model reads it: the option LABELS, never their
/// indices — the model never saw the numbers, and a list position is not an
/// answer.
fn answer_text(answer: &Answer, options: &[String]) -> String {
    match answer {
        Answer::Text(text) => text.clone(),
        Answer::Picks(picks) => {
            if picks.is_empty() {
                return "(none)".to_string();
            }
            picks
                .iter()
                .map(|i| options.get(*i).cloned().unwrap_or_default())
                .filter(|label| !label.is_empty())
                .collect::<Vec<_>>()
                .join(", ")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_client_protocol::ClarificationId;
    use unicode_width::UnicodeWidthStr;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    fn shift_tab() -> KeyEvent {
        KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT)
    }

    fn legacy(options: Vec<&str>) -> ClarificationOverlay {
        ClarificationOverlay::new(UiClarificationRequest::single(
            ClarificationId::new("c1"),
            "选哪个方案？",
            options.into_iter().map(String::from).collect(),
        ))
    }

    fn question(
        header: &str,
        kind: ClarificationQuestionKind,
        options: &[&str],
    ) -> UiClarificationQuestion {
        UiClarificationQuestion {
            header: header.into(),
            question: format!("{header}怎么定？"),
            kind,
            options: options.iter().map(|s| (*s).to_string()).collect(),
            allow_other: false,
            min_choices: 0,
            max_choices: None,
        }
    }

    fn multi_question(questions: Vec<UiClarificationQuestion>) -> ClarificationOverlay {
        ClarificationOverlay::new(UiClarificationRequest {
            id: ClarificationId::new("c1"),
            question: "需要你的选择".into(),
            options: Vec::new(),
            questions,
        })
    }

    fn three() -> ClarificationOverlay {
        multi_question(vec![
            question(
                "数据策略",
                ClarificationQuestionKind::Single,
                &["保留 demo fallback", "删除全部 mock"],
            ),
            question(
                "验证范围",
                ClarificationQuestionKind::Multi,
                &["单元测试", "TUI 测试", "workspace test"],
            ),
            question("补充要求", ClarificationQuestionKind::Text, &[]),
        ])
    }

    #[test]
    fn a_legacy_request_is_one_question_with_no_tab_strip() {
        let ov = legacy(vec!["A", "B"]);
        assert!(ov.legacy());
        assert_eq!(ov.len(), 1);
    }

    /// The old overlay's contract: Enter submits the focused option's label.
    #[test]
    fn legacy_option_confirm_submits_the_bare_option_text() {
        let mut ov = legacy(vec!["A", "B"]);
        assert_eq!(ov.on_key(key(KeyCode::Down)), ClarificationOutcome::None);
        assert_eq!(
            ov.on_key(key(KeyCode::Enter)),
            ClarificationOutcome::Answer("B".into())
        );
    }

    /// A legacy question with no options is a text field, as before.
    #[test]
    fn legacy_free_text_question_submits_what_was_typed() {
        let mut ov = ClarificationOverlay::new(UiClarificationRequest::single(
            ClarificationId::new("c1"),
            "名字？",
            Vec::new(),
        ));
        for c in "保留旧字段".chars() {
            assert_eq!(ov.on_key(key(KeyCode::Char(c))), ClarificationOutcome::None);
        }
        assert_eq!(
            ov.on_key(key(KeyCode::Enter)),
            ClarificationOutcome::Answer("保留旧字段".into())
        );
    }

    /// A legacy choice still accepts a typed answer through "其他…", and it is
    /// never cut in half: only Enter submits.
    #[test]
    fn legacy_other_entry_keeps_a_whole_sentence() {
        let mut ov = legacy(vec!["A", "B"]);
        ov.on_key(key(KeyCode::Down));
        ov.on_key(key(KeyCode::Down));
        for c in "3，其余不变".chars() {
            assert_eq!(ov.on_key(key(KeyCode::Char(c))), ClarificationOutcome::None);
        }
        assert_eq!(
            ov.on_key(key(KeyCode::Enter)),
            ClarificationOutcome::Answer("3，其余不变".into())
        );
    }

    #[test]
    fn esc_skips_with_an_empty_answer() {
        let mut ov = legacy(vec!["A"]);
        assert_eq!(
            ov.on_key(key(KeyCode::Esc)),
            ClarificationOutcome::Answer(String::new())
        );
    }

    #[test]
    fn tab_and_shift_tab_step_through_questions_both_ways() {
        let mut ov = three();
        assert_eq!(ov.active(), 0);
        ov.on_key(key(KeyCode::Tab));
        assert_eq!(ov.active(), 1);
        ov.on_key(shift_tab());
        assert_eq!(ov.active(), 0);
        // Backwards from the first wraps to the last.
        ov.on_key(key(KeyCode::BackTab));
        assert_eq!(ov.active(), 2);
        ov.on_key(key(KeyCode::Tab));
        assert_eq!(ov.active(), 0);
    }

    /// Arrows move inside the current question, never between questions and
    /// never past the ends.
    #[test]
    fn arrows_move_the_current_questions_cursor_only() {
        let mut ov = three();
        assert_eq!(ov.questions()[0].cursor, 0);
        ov.on_key(key(KeyCode::Up));
        assert_eq!(ov.questions()[0].cursor, 1, "up wraps to the last row");
        ov.on_key(key(KeyCode::Down));
        assert_eq!(ov.questions()[0].cursor, 0);
        // The active question is the one that moved; the others did not.
        assert_eq!(ov.questions()[1].cursor, 0);
        assert_eq!(ov.active(), 0);
    }

    #[test]
    fn confirming_a_single_choice_advances_to_the_next_unanswered_question() {
        let mut ov = three();
        ov.on_key(key(KeyCode::Enter));
        assert!(ov.is_answered(0));
        assert_eq!(
            ov.active(),
            1,
            "Enter moves to the next unanswered question"
        );
    }

    #[test]
    fn confirming_the_last_unanswered_submits_every_answer() {
        let mut ov = three();
        ov.on_key(key(KeyCode::Enter)); // data strategy = option 0
        ov.on_key(key(KeyCode::Char(' '))); // multi: TUI 测试
        ov.on_key(key(KeyCode::Enter)); // confirm multi
        assert_eq!(ov.active(), 2, "the text question is last");
        for c in "登录后绝不能再读取 mock 数据".chars() {
            ov.on_key(key(KeyCode::Char(c)));
        }
        let outcome = ov.on_key(key(KeyCode::Enter));
        match outcome {
            ClarificationOutcome::Answer(text) => {
                assert_eq!(
                    text,
                    "数据策略: 保留 demo fallback\n验证范围: 单元测试\n补充要求: 登录后绝不能再读取 mock 数据"
                );
            }
            other => panic!("expected a submitted answer, got {other:?}"),
        }
    }

    /// Answering questions out of order still submits in question order, and
    /// confirming a settled question does not re-open it.
    #[test]
    fn an_answered_question_can_be_reopened_and_changed() {
        let mut ov = legacy(vec!["A", "B", "C"]);
        ov.on_key(key(KeyCode::Down));
        ov.on_key(key(KeyCode::Enter));
        assert_eq!(ov.answered_count(), 1);
        // Reopening does not clear the recorded answer until a new Enter.
        ov.on_key(key(KeyCode::Up));
        assert_eq!(ov.answered_count(), 1);
        assert_eq!(
            ov.on_key(key(KeyCode::Enter)),
            ClarificationOutcome::Answer("A".into())
        );
    }

    /// Space is a multi-choice toggle and nothing else. On a single choice it
    /// must not answer or move anything.
    #[test]
    fn space_toggles_only_multi_choice_rows() {
        let mut ov = three();
        ov.on_key(key(KeyCode::Tab));
        ov.on_key(key(KeyCode::Char(' ')));
        assert!(ov.questions()[1].selected[0]);
        ov.on_key(key(KeyCode::Char(' ')));
        assert!(!ov.questions()[1].selected[0]);
        // On the single choice, Space changes nothing.
        ov.on_key(key(KeyCode::BackTab));
        ov.on_key(key(KeyCode::Char(' ')));
        assert!(ov.questions()[0].answer.is_none());
    }

    #[test]
    fn a_multi_choice_submits_the_whole_selected_set() {
        let mut ov = three();
        ov.on_key(key(KeyCode::Tab));
        ov.on_key(key(KeyCode::Char(' '))); // 单元测试
        ov.on_key(key(KeyCode::Down));
        ov.on_key(key(KeyCode::Char(' '))); // TUI 测试
        ov.on_key(key(KeyCode::Enter));
        assert_eq!(
            ov.questions()[1].answer,
            Some(Answer::Picks(vec![0, 1])),
            "the submitted set is what was toggled"
        );
    }

    #[test]
    fn a_min_choice_constraint_blocks_submission_and_says_why() {
        let mut ov = three();
        ov.questions[1].min_choices = 1;
        ov.on_key(key(KeyCode::Tab));
        assert_eq!(ov.on_key(key(KeyCode::Enter)), ClarificationOutcome::None);
        assert_eq!(ov.notice(), Some(ClarificationNotice::MinChoices(1)));
        assert!(ov.questions()[1].answer.is_none());
        // Once satisfied, Enter confirms.
        ov.on_key(key(KeyCode::Char(' ')));
        assert_eq!(ov.notice(), None);
    }

    #[test]
    fn a_max_choice_constraint_refuses_the_pick_that_would_exceed_it() {
        let mut ov = three();
        ov.questions[1].max_choices = Some(1);
        ov.on_key(key(KeyCode::Tab));
        ov.on_key(key(KeyCode::Char(' ')));
        ov.on_key(key(KeyCode::Down));
        ov.on_key(key(KeyCode::Char(' ')));
        assert_eq!(ov.questions()[1].selected, vec![true, false, false]);
        assert_eq!(ov.notice(), Some(ClarificationNotice::MaxChoices(1)));
    }

    #[test]
    fn a_text_question_takes_typing_and_backspace() {
        let mut ov = three();
        ov.on_key(key(KeyCode::Tab));
        ov.on_key(key(KeyCode::Tab));
        for c in "abc".chars() {
            ov.on_key(key(KeyCode::Char(c)));
        }
        ov.on_key(key(KeyCode::Backspace));
        assert_eq!(ov.questions()[2].text.text(), "ab");
        // Space is a literal space in a text field.
        ov.on_key(key(KeyCode::Char(' ')));
        assert_eq!(ov.questions()[2].text.text(), "ab ");
    }

    /// An empty "其他…" entry is not an answer; Enter does not record one.
    #[test]
    fn an_empty_other_entry_does_not_confirm() {
        let mut ov = legacy(vec!["A"]);
        ov.on_key(key(KeyCode::Down));
        assert_eq!(ov.on_key(key(KeyCode::Enter)), ClarificationOutcome::None);
        assert_eq!(ov.answered_count(), 0);
    }

    #[test]
    fn backspace_edits_a_text_question() {
        let mut ov = ClarificationOverlay::new(UiClarificationRequest::single(
            ClarificationId::new("c1"),
            "名字？",
            Vec::new(),
        ));
        ov.on_key(key(KeyCode::Char('a')));
        ov.on_key(key(KeyCode::Char('b')));
        ov.on_key(key(KeyCode::Backspace));
        assert_eq!(ov.questions()[0].text.text(), "a");
    }

    /// A paste lands in the active question's text field, and on a choice it
    /// brings the cursor to the row it filled so the text is visible.
    #[test]
    fn pasted_text_lands_in_the_active_questions_field() {
        let mut ov = legacy(vec!["A", "B"]);
        ov.insert_text("需要保留旧字段");
        assert_eq!(ov.questions()[0].text.text(), "需要保留旧字段");
        assert!(ov.questions()[0].on_other_row());
    }

    /// A question with no text field leaves the paste alone instead of
    /// answering with text the question never offered.
    #[test]
    fn a_paste_with_nowhere_to_go_does_not_answer() {
        let mut ov = three();
        ov.on_key(key(KeyCode::Tab)); // multi, allow_other = false
        ov.insert_text("不该出现在这里");
        assert!(ov.questions()[1].text.is_empty());
    }

    #[test]
    fn ctrl_keys_are_not_consumed_as_answers() {
        let mut ov = three();
        let ctrl_d = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert_eq!(ov.on_key(ctrl_d), ClarificationOutcome::None);
        assert_eq!(ov.questions()[0].text.text(), "");
    }

    /// A choice question with no options can only be answered by typing.
    /// Reading `kind` literally would render an empty unconfirmable list.
    #[test]
    fn a_choice_without_options_reads_as_text() {
        for kind in [
            ClarificationQuestionKind::Single,
            ClarificationQuestionKind::Multi,
        ] {
            let mut ov = multi_question(vec![question("数据策略", kind, &[])]);
            for c in "typed".chars() {
                ov.on_key(key(KeyCode::Char(c)));
            }
            assert_eq!(
                ov.on_key(key(KeyCode::Enter)),
                ClarificationOutcome::Answer("数据策略: typed".into()),
                "kind {kind:?}"
            );
        }
    }

    #[test]
    fn a_header_is_derived_when_the_model_sent_none() {
        let ov = multi_question(vec![UiClarificationQuestion {
            header: String::new(),
            question: "数据策略怎么定？".into(),
            kind: ClarificationQuestionKind::Single,
            options: vec!["A".into()],
            allow_other: false,
            min_choices: 0,
            max_choices: None,
        }]);
        assert!(
            !ov.questions()[0].display_header().is_empty(),
            "a missing header still yields a label"
        );
        assert!(
            UnicodeWidthStr::width(ov.questions()[0].display_header().as_str()) <= 8,
            "a derived tab label stays short"
        );
    }

    // ── The field is the composer's editor ──────────────────────────────────

    fn single_text() -> ClarificationOverlay {
        ClarificationOverlay::new(UiClarificationRequest::single(
            ClarificationId::new("c1"),
            "把日志发我",
            Vec::new(),
        ))
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn type_str(ov: &mut ClarificationOverlay, s: &str) {
        for c in s.chars() {
            ov.on_key(key(KeyCode::Char(c)));
        }
    }

    fn submit(mut ov: ClarificationOverlay) -> String {
        match ov.on_key(key(KeyCode::Enter)) {
            ClarificationOutcome::Answer(answer) => answer,
            other => panic!("expected a submitted answer, got {other:?}"),
        }
    }

    /// Typing, deleting and moving use the composer's own editing commands —
    /// readline's kill pair included — because the field is the composer's
    /// editor and not a second, smaller one.
    #[test]
    fn the_field_edits_with_the_composer_s_editing_keys() {
        let mut ov = single_text();
        type_str(&mut ov, "hello world");
        ov.on_key(ctrl('w'));
        assert_eq!(ov.active_text(), "hello ");

        ov.on_key(ctrl('a'));
        ov.on_key(ctrl('k'));
        assert_eq!(ov.active_text(), "");

        type_str(&mut ov, "abc");
        ov.on_key(key(KeyCode::Left));
        ov.on_key(ctrl('u'));
        assert_eq!(ov.active_text(), "c");

        ov.on_key(ctrl('e'));
        ov.on_key(key(KeyCode::Delete));
        assert_eq!(ov.active_text(), "c");

        ov.on_key(key(KeyCode::Home));
        ov.on_key(key(KeyCode::Delete));
        assert_eq!(ov.active_text(), "");
    }

    /// Backspace removes a whole grapheme: a combining mark goes with the
    /// letter it belongs to, never on its own.
    #[test]
    fn the_field_deletes_a_whole_grapheme() {
        let mut ov = single_text();
        type_str(&mut ov, "中文");
        ov.on_key(key(KeyCode::Backspace));
        assert_eq!(ov.active_text(), "中");

        // A combining sequence arrives as one burst and leaves as one
        // grapheme: the accent goes with the letter it belongs to.
        let mut combining = single_text();
        combining.insert_text("e\u{301}x");
        combining.on_key(key(KeyCode::Backspace));
        assert_eq!(combining.active_text(), "e\u{301}");
        combining.on_key(key(KeyCode::Backspace));
        assert_eq!(combining.active_text(), "");
    }

    /// A large paste is folded for display and delivered whole: the chip is
    /// presentation, and the payload behind it is what the runtime receives.
    #[test]
    fn a_large_paste_is_folded_for_display_and_delivered_whole() {
        let mut ov = single_text();
        let payload: String = (1..=977).map(|line| format!("log line {line}\n")).collect();
        ov.insert_text(&payload);
        assert_eq!(ov.active_text(), "[Pasted: 977 lines]");
        let answer = submit(ov);
        assert_eq!(answer, payload.trim_end());
    }

    /// Normal text and a paste are delivered together, in the order they were
    /// put into the sentence.
    #[test]
    fn normal_text_and_a_paste_are_both_delivered() {
        let mut ov = single_text();
        type_str(&mut ov, "请分析：");
        let payload: String = (0..300).map(|line| format!("line {line}\n")).collect();
        ov.insert_text(&payload);
        assert_eq!(ov.active_text(), "请分析：[Pasted: 300 lines]");
        type_str(&mut ov, "重点看 runtime lifecycle");
        let answer = submit(ov);
        assert!(answer.starts_with("请分析：line 0\n"), "{answer}");
        assert!(answer.contains("line 299"), "{answer}");
        assert!(answer.ends_with("重点看 runtime lifecycle"), "{answer}");
    }

    /// Below the fold threshold the paste is inserted as written, line breaks
    /// and all — nothing is flattened on the way in.
    #[test]
    fn a_small_paste_keeps_its_own_text() {
        let mut ov = single_text();
        ov.insert_text("第一行\n第二行");
        assert_eq!(ov.active_text(), "第一行\n第二行");
        assert_eq!(submit(ov), "第一行\n第二行");
    }

    /// The chip counts pasted lines, not bytes: the same script counts the same
    /// whether it is ASCII or CJK.
    #[test]
    fn a_paste_is_counted_in_lines_not_bytes() {
        let mut ov = single_text();
        ov.insert_text("中文\n第二行\n第三行\n第四行\n第五行");
        assert_eq!(ov.active_text(), "[Pasted: 5 lines]");
    }

    /// The field inherits the editor's grapheme accounting, including the case
    /// that used to drift: a combining mark arriving as its own key event joins
    /// the character before it, and the caret counts the text, not the keys.
    #[test]
    fn a_combining_mark_is_one_cluster_in_the_field() {
        // Backspace takes the whole cluster, not a mark left standing alone.
        let mut ov = single_text();
        type_str(&mut ov, "e");
        type_str(&mut ov, "\u{301}");
        assert_eq!(ov.active_text(), "e\u{301}");
        ov.on_key(key(KeyCode::Backspace));
        assert_eq!(ov.active_text(), "");
        // Moving over it moves over the cluster: two Lefts from the end land
        // before `e`, so the next character goes in front of both halves.
        let mut ov = single_text();
        type_str(&mut ov, "e\u{301}x");
        ov.on_key(key(KeyCode::Left));
        ov.on_key(key(KeyCode::Left));
        type_str(&mut ov, "A");
        assert_eq!(ov.active_text(), "Ae\u{301}x");
        ov.on_key(ctrl('e'));
        assert_eq!(ov.active_text(), "Ae\u{301}x");
    }

    /// A folded paste comes back exactly as it went in — combining marks, CJK,
    /// emoji, a ZWJ sequence and all. The grapheme-cursor work touched the same
    /// path that stores a paste payload, so this is the regression that would
    /// matter most: silently rewriting what the model is about to read.
    #[test]
    fn a_unicode_paste_is_delivered_byte_for_byte() {
        let payload = "修复 e\u{301} 的重入\n\u{1f44d}\u{1f3fd} emoji\n\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466} family\n中文行\n\u{1f1e8}\u{1f1f3} flag";
        let mut ov = single_text();
        type_str(&mut ov, "请分析：");
        ov.insert_text(payload);
        assert_eq!(ov.active_text(), "请分析：[Pasted: 5 lines]");
        let answer = submit(ov);
        assert_eq!(answer, format!("请分析：{payload}"));
        // Nothing was normalized away, and nothing was flattened into the chip.
        assert!(answer.contains("e\u{301}"));
        assert!(answer.contains("\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466}"));
        assert!(answer.contains('\n'));
        assert!(!answer.contains("[Pasted:"), "{answer}");
    }
}
