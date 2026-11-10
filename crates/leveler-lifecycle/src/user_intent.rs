//! The structured user-intent contract.
//!
//! The harness never classifies a user's natural language to decide whether a
//! goal continues, pauses or is replaced. The model reads the message (in any
//! language, at any length) and states its understanding through the
//! constrained [`UserIntentDeclaration`] below. The runtime then validates
//! three mechanical facts and nothing else:
//!
//! 1. the declaration quotes a span that really exists in the newest user
//!    input ([`quote_is_in_input`]) — a model cannot invent an instruction;
//! 2. every referenced plan step id exists in the model's own declared plan —
//!    a boundary is a structural reference, never a text match;
//! 3. the operation sequence is a legal transition over the durable goal state
//!    ([`project_intent`]).
//!
//! Composition is first-class: `继续执行，但第三阶段完成后暂停` is one
//! declaration whose ordered operations release the old pause, continue the
//! existing goal, and set the next boundary. Applying them in order is the
//! only meaning the runtime assigns; it performs no semantic interpretation of
//! the quote.
//!
//! This module is pure: storage writes the projected state and the ordered log,
//! the model layer decides *what* the user meant, and the runtime only accepts
//! or refuses the structured statement.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::plan::PlanState;

/// Longest user span a declaration may quote. Generous: the whole message is
/// legitimate. The cap exists so a malformed call cannot copy an unbounded
/// transcript into the durable intent log.
pub const INTENT_SOURCE_QUOTE_MAX_CHARS: usize = 8_000;

/// Most operations one declaration may carry. The compositional cases in the
/// product contract use at most three (release, continue, set boundary).
pub const INTENT_OPERATIONS_MAX: usize = 8;

/// Longest plan-step identity.
pub const INTENT_STEP_ID_MAX_CHARS: usize = 120;

/// Longest display label kept beside a step identity.
pub const INTENT_STEP_LABEL_MAX_CHARS: usize = 200;

/// A structural reference to one step of the model's declared plan.
///
/// `step_id` is the identity the plan declared. `label` is a display
/// convenience captured at declaration time; it never participates in
/// matching, so renaming a step cannot silently move the boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageRef {
    /// The `PlanStep::id` the boundary refers to.
    pub step_id: String,
    /// Human-readable text at declaration time. Display only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// When a paused goal resumes withholding authorization.
///
/// The harness evaluates this only at a closeout, never mid-tool-call: a tool
/// batch already dispatched is atomic, and stopping between its calls would
/// leave a half-applied change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PauseBoundary {
    /// Stop at the next closeout. The current round's work completes; no new
    /// round starts without a new user authorization.
    Now,
    /// Stop at the next closeout, after whatever step the model is currently
    /// working on. Mechanically identical to [`Self::Now`]; kept because the
    /// model's declaration should preserve the distinction the user made.
    AfterAnyStep,
    /// Stop once the referenced plan step is declared `completed`.
    ///
    /// A step that disappears from a later plan is NOT completed: the model
    /// may rename, replace, split or merge steps while the work is still
    /// outstanding, and none of those transitions is a completion. Only an
    /// explicit `completed` status on the same stable id reaches the boundary.
    AfterStep { step: StageRef },
}

impl PauseBoundary {
    /// The step identity this boundary names, if any.
    pub fn step_id(&self) -> Option<&str> {
        match self {
            PauseBoundary::AfterStep { step } => Some(step.step_id.as_str()),
            PauseBoundary::Now | PauseBoundary::AfterAnyStep => None,
        }
    }

    /// Whether the boundary is in force at a closeout of `plan`.
    ///
    /// Only an explicit `completed` status on the named step reaches the
    /// boundary. A step that was deleted, renamed without keeping its id,
    /// replaced, split or merged is still outstanding: those transitions move
    /// the plan, they do not finish the work the user asked to see finished.
    /// The model may adjust the plan freely and can re-declare the boundary
    /// when the stage really moved.
    pub fn reached(&self, plan: &PlanState) -> bool {
        match self {
            PauseBoundary::Now | PauseBoundary::AfterAnyStep => true,
            PauseBoundary::AfterStep { step } => plan
                .steps
                .iter()
                .find(|candidate| candidate.id.as_deref() == Some(step.step_id.as_str()))
                .is_some_and(|candidate| candidate.status == "completed"),
        }
    }

    /// One-line description for a model-visible or user-visible notice.
    pub fn describe(&self) -> String {
        match self {
            PauseBoundary::Now => "pause at the next boundary".to_string(),
            PauseBoundary::AfterAnyStep => "pause after the current step".to_string(),
            PauseBoundary::AfterStep { step } => format!(
                "pause after step {} ({})",
                step.step_id,
                step.label.as_deref().unwrap_or("unnamed")
            ),
        }
    }
}

/// One structured operation the model declares over the goal lifecycle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UserIntentOperation {
    /// The user authorizes the goal to continue. Releases a durable pause and
    /// the boundary it was waiting on.
    ContinueTask,
    /// The user wants the goal to stop at `boundary`.
    PauseTask { boundary: PauseBoundary },
    /// Set (or replace) a future pause boundary without changing the current
    /// pause authorization.
    SetPauseBoundary { boundary: PauseBoundary },
    /// Explicit replacement spelling; same transition as
    /// [`Self::SetPauseBoundary`].
    ReplacePauseBoundary { boundary: PauseBoundary },
    /// The user refined the current task. Scope changes travel through the
    /// requirement contract; this operation records the reading of the input.
    AmendCurrentTask,
    /// The user asked for a task unrelated to the running goal.
    StartIndependentTask,
    /// The user cancelled the goal.
    CancelTask,
    /// The user asked a question; answer it without driving the goal.
    AnswerOnly,
}

/// The model's structured reading of one user input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserIntentDeclaration {
    /// A verbatim (or whitespace-normalized) span of the newest user input.
    pub source_quote: String,
    /// Ordered operations. Applied in array order.
    pub operations: Vec<UserIntentOperation>,
}

/// Why a declaration was refused. Every variant names a mechanical fact, not
/// a disagreement with the model's interpretation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntentError {
    EmptyQuote,
    QuoteTooLong { chars: usize },
    QuoteNotInInput,
    NoOperations,
    TooManyOperations { count: usize },
    EmptyStepId,
    StepIdTooLong { chars: usize },
    StepLabelTooLong { chars: usize },
    BoundaryStepUnknown { step_id: String },
    BoundaryWithoutPlan { step_id: String },
    TerminalOperationCombined { operation: &'static str },
}

impl fmt::Display for IntentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IntentError::EmptyQuote => write!(
                f,
                "declare_intent requires a non-empty `source_quote` copied from the user's newest \
                 message"
            ),
            IntentError::QuoteTooLong { chars } => write!(
                f,
                "`source_quote` is {chars} characters; the limit is \
                 {INTENT_SOURCE_QUOTE_MAX_CHARS}"
            ),
            IntentError::QuoteNotInInput => write!(
                f,
                "`source_quote` does not occur in the user's newest message. Quote the user's own \
                 words exactly (whitespace differences are tolerated); do not paraphrase and do \
                 not invent an instruction."
            ),
            IntentError::NoOperations => write!(
                f,
                "declare_intent requires at least one operation from the documented list"
            ),
            IntentError::TooManyOperations { count } => write!(
                f,
                "declare_intent received {count} operations; the limit is {INTENT_OPERATIONS_MAX}"
            ),
            IntentError::EmptyStepId => write!(
                f,
                "a pause boundary must name the `step_id` of a step in your declared plan"
            ),
            IntentError::StepIdTooLong { chars } => write!(
                f,
                "step id is {chars} characters; the limit is {INTENT_STEP_ID_MAX_CHARS}"
            ),
            IntentError::StepLabelTooLong { chars } => write!(
                f,
                "step label is {chars} characters; the limit is \
                 {INTENT_STEP_LABEL_MAX_CHARS}"
            ),
            IntentError::BoundaryWithoutPlan { step_id } => write!(
                f,
                "pause boundary names step `{step_id}`, but this turn has no model-declared plan. \
                 Call update_plan first (each step with a stable `id`), then declare the boundary."
            ),
            IntentError::BoundaryStepUnknown { step_id } => write!(
                f,
                "pause boundary names step `{step_id}`, which is not in the current plan. A \
                 boundary is a structural reference: add the step, keep the id of a renamed \
                 step, or name the step you really mean."
            ),
            IntentError::TerminalOperationCombined { operation } => write!(
                f,
                "`{operation}` must be the only operation in its declaration; it cannot be \
                 combined with a state transition."
            ),
        }
    }
}

impl std::error::Error for IntentError {}

/// Whether `quote` occurs in `input`.
///
/// Exact containment first. If that fails, both sides are compared with runs
/// of whitespace collapsed, which tolerates a model that joined lines or
/// normalized spacing. Word content itself must still match: this is a
/// provenance check, not paraphrase detection.
pub fn quote_is_in_input(quote: &str, input: &str) -> bool {
    let quote = quote.trim();
    if quote.is_empty() {
        return false;
    }
    if input.contains(quote) {
        return true;
    }
    let collapsed_quote = collapse_whitespace(quote);
    let collapsed_input = collapse_whitespace(input);
    collapsed_input.contains(&collapsed_quote)
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A structural reference to lines of a user-authored message.
///
/// Line numbers are 1-based and inclusive, counted over the authoritative
/// message text the runtime holds (the newest user-authored input). A line is
/// the text between `\n` separators; a `\r` immediately before the separator
/// is not part of the line, and a final trailing newline creates no extra
/// line. The resolved quote is the exact byte range from the start of
/// `start_line` through the last character of `end_line`, so it is always a
/// verbatim substring of the message.
///
/// The model names a position instead of reproducing the characters. That
/// keeps provenance strict — the quote still comes from the original message —
/// while removing byte-for-byte copying (punctuation width, Unicode
/// normalization, accidental rewrapping) as a failure mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceSpan {
    pub start_line: u32,
    pub end_line: u32,
}

/// Why a [`SourceSpan`] cannot be resolved.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SourceSpanError {
    #[error("`start_line` and `end_line` are 1-based; line 0 does not exist")]
    ZeroLine,
    #[error("`end_line` ({end_line}) is before `start_line` ({start_line})")]
    Reversed { start_line: u32, end_line: u32 },
    #[error(
        "lines {start_line}-{end_line} are outside the user's message, which has {lines} line(s)"
    )]
    OutOfRange {
        start_line: u32,
        end_line: u32,
        lines: usize,
    },
    #[error("the span selects only blank text; name the line(s) that state the requirement")]
    Empty,
    #[error(
        "the span selects {chars} characters; the limit is {max_chars}. Name fewer lines or use \
         `source_quote` with a shorter verbatim fragment"
    )]
    TooLong { chars: usize, max_chars: usize },
}

/// Resolve `span` against the authoritative message text.
///
/// The returned string is cut from `input` by byte offsets derived from the
/// line boundaries; it is never reconstructed from the model's tokens.
pub fn resolve_source_span(
    span: SourceSpan,
    input: &str,
    max_chars: usize,
) -> Result<String, SourceSpanError> {
    if span.start_line == 0 || span.end_line == 0 {
        return Err(SourceSpanError::ZeroLine);
    }
    if span.end_line < span.start_line {
        return Err(SourceSpanError::Reversed {
            start_line: span.start_line,
            end_line: span.end_line,
        });
    }
    let lines = line_ranges(input);
    let (Ok(start_index), Ok(end_index)) = (
        usize::try_from(span.start_line),
        usize::try_from(span.end_line),
    ) else {
        return Err(SourceSpanError::OutOfRange {
            start_line: span.start_line,
            end_line: span.end_line,
            lines: lines.len(),
        });
    };
    let Some((start, _)) = lines.get(start_index - 1) else {
        return Err(SourceSpanError::OutOfRange {
            start_line: span.start_line,
            end_line: span.end_line,
            lines: lines.len(),
        });
    };
    let Some((_, end)) = lines.get(end_index - 1) else {
        return Err(SourceSpanError::OutOfRange {
            start_line: span.start_line,
            end_line: span.end_line,
            lines: lines.len(),
        });
    };
    let quote = &input[*start..*end];
    if quote.trim().is_empty() {
        return Err(SourceSpanError::Empty);
    }
    let chars = quote.chars().count();
    if chars > max_chars {
        return Err(SourceSpanError::TooLong { chars, max_chars });
    }
    Ok(quote.to_string())
}

/// Byte ranges `(content_start, content_end)` of every line in `input`.
///
/// Mirrors the line model documented on [`SourceSpan`]: `\n` separates lines,
/// a `\r` directly before it is excluded from the line, and a final trailing
/// newline adds no empty line. The empty string has no lines.
fn line_ranges(input: &str) -> Vec<(usize, usize)> {
    if input.is_empty() {
        return Vec::new();
    }
    let mut ranges = Vec::new();
    let mut start = 0;
    for segment in input.split_inclusive('\n') {
        let content = match segment.strip_suffix('\n') {
            Some(rest) => rest.strip_suffix('\r').unwrap_or(rest),
            None => segment,
        };
        ranges.push((start, start + content.len()));
        start += segment.len();
    }
    ranges
}

fn validate_boundary(boundary: &PauseBoundary, plan: &PlanState) -> Result<(), IntentError> {
    let PauseBoundary::AfterStep { step } = boundary else {
        return Ok(());
    };
    let step_id = step.step_id.trim();
    if step_id.is_empty() {
        return Err(IntentError::EmptyStepId);
    }
    let id_chars = step_id.chars().count();
    if id_chars > INTENT_STEP_ID_MAX_CHARS {
        return Err(IntentError::StepIdTooLong { chars: id_chars });
    }
    if let Some(label) = step.label.as_deref() {
        let label_chars = label.chars().count();
        if label_chars > INTENT_STEP_LABEL_MAX_CHARS {
            return Err(IntentError::StepLabelTooLong { chars: label_chars });
        }
    }
    if !plan.is_model_explicit() {
        return Err(IntentError::BoundaryWithoutPlan {
            step_id: step_id.to_string(),
        });
    }
    if !plan
        .steps
        .iter()
        .any(|candidate| candidate.id.as_deref() == Some(step_id))
    {
        return Err(IntentError::BoundaryStepUnknown {
            step_id: step_id.to_string(),
        });
    }
    Ok(())
}

/// The state a declaration produces over the durable goal row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntentProjection {
    /// Whether the durable pause is in force. The caller supplies the
    /// timestamp it already has; this module has no clock and no storage.
    pub pause_in_force: bool,
    /// The boundary to persist. `None` releases it.
    pub pause_boundary: Option<PauseBoundary>,
    /// The declaration settles the goal (user cancellation).
    pub cancel: bool,
    /// The user asked a question; no goal drive is authorized by this input.
    pub answer_only: bool,
    /// The input asks for a task independent of the running goal.
    pub independent_task: bool,
}

/// Validate and apply one declaration in order.
///
/// `current` is the durable authorization state; `plan` is the model's own
/// latest declared plan. No semantic reading of `source_quote` happens here —
/// the caller validates provenance against the newest user input.
pub fn project_intent(
    declaration: &UserIntentDeclaration,
    current: &IntentProjection,
    plan: &PlanState,
) -> Result<IntentProjection, IntentError> {
    if declaration.operations.is_empty() {
        return Err(IntentError::NoOperations);
    }
    if declaration.operations.len() > INTENT_OPERATIONS_MAX {
        return Err(IntentError::TooManyOperations {
            count: declaration.operations.len(),
        });
    }
    let quote_chars = declaration.source_quote.trim().chars().count();
    if quote_chars > INTENT_SOURCE_QUOTE_MAX_CHARS {
        return Err(IntentError::QuoteTooLong { chars: quote_chars });
    }
    let mut state = IntentProjection {
        pause_in_force: current.pause_in_force,
        pause_boundary: current.pause_boundary.clone(),
        cancel: false,
        answer_only: false,
        independent_task: false,
    };
    let terminal_count = declaration
        .operations
        .iter()
        .filter(|operation| {
            matches!(
                operation,
                UserIntentOperation::CancelTask
                    | UserIntentOperation::AnswerOnly
                    | UserIntentOperation::StartIndependentTask
            )
        })
        .count();
    if terminal_count > 1 {
        return Err(IntentError::TerminalOperationCombined {
            operation: "the terminal operations in this declaration",
        });
    }
    for operation in &declaration.operations {
        match operation {
            UserIntentOperation::ContinueTask => {
                state.pause_in_force = false;
                state.pause_boundary = None;
                state.cancel = false;
                state.answer_only = false;
                state.independent_task = false;
            }
            UserIntentOperation::PauseTask { boundary }
            | UserIntentOperation::SetPauseBoundary { boundary }
            | UserIntentOperation::ReplacePauseBoundary { boundary } => {
                validate_boundary(boundary, plan)?;
                state.pause_in_force |= boundary.reached(plan);
                state.pause_boundary = Some(boundary.clone());
            }
            UserIntentOperation::AmendCurrentTask => {}
            UserIntentOperation::StartIndependentTask => {
                if declaration.operations.len() != 1 {
                    return Err(IntentError::TerminalOperationCombined {
                        operation: "start_independent_task",
                    });
                }
                // The running goal's pause and boundary are untouched: the new
                // task is not its continuation, and cancelling its authorization
                // state would be a second decision this operation does not make.
                state.independent_task = true;
            }
            UserIntentOperation::CancelTask => {
                if declaration.operations.len() != 1 {
                    return Err(IntentError::TerminalOperationCombined {
                        operation: "cancel_task",
                    });
                }
                state.cancel = true;
                state.pause_in_force = false;
                state.pause_boundary = None;
            }
            UserIntentOperation::AnswerOnly => {
                if declaration.operations.len() != 1 {
                    return Err(IntentError::TerminalOperationCombined {
                        operation: "answer_only",
                    });
                }
                state.answer_only = true;
            }
        }
    }
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PlanOrigin, PlanStep};

    fn plan(steps: &[(&str, &str)]) -> PlanState {
        PlanState {
            steps: steps
                .iter()
                .map(|(id, status)| PlanStep {
                    step: format!("step {id}"),
                    status: status.to_string(),
                    id: Some(id.to_string()),
                    origin: PlanOrigin::ModelExplicit,
                })
                .collect(),
        }
    }

    fn declaration(quote: &str, operations: Vec<UserIntentOperation>) -> UserIntentDeclaration {
        UserIntentDeclaration {
            source_quote: quote.to_string(),
            operations,
        }
    }

    #[test]
    fn multilingual_quotes_are_checked_by_occurrence_not_by_vocabulary() {
        let inputs = [
            "继续执行，但第三阶段完成后暂停。",
            "Keep going, but pause after stage three.",
            "続けてください。ただし第3段階の後に止めてください。",
            "Mach weiter, aber halte nach Phase drei an.",
            "Sigue, pero pausa después de la fase tres.",
            "ok — continue, then wait for me before the final pass",
        ];
        for input in inputs {
            assert!(quote_is_in_input(input, input), "{input}");
        }
        assert!(quote_is_in_input(
            "pause after stage three",
            "Keep going, but pause after stage three."
        ));
        assert!(quote_is_in_input(
            "第三阶段完成后暂停",
            "继续执行，但第三阶段完成后暂停。"
        ));
        assert!(!quote_is_in_input(
            "stop after stage two",
            "Keep going, but pause after stage three."
        ));
        assert!(!quote_is_in_input(
            "停止",
            "继续执行，但第三阶段完成后暂停。"
        ));
    }

    #[test]
    fn whitespace_differences_are_tolerated_but_words_are_not_invented() {
        assert!(quote_is_in_input(
            "pause   after\nstage three",
            "Keep going, but pause after stage three."
        ));
        assert!(!quote_is_in_input(
            "pause after stage four",
            "Keep going, but pause after stage three."
        ));
    }

    fn span(start_line: u32, end_line: u32) -> SourceSpan {
        SourceSpan {
            start_line,
            end_line,
        }
    }

    #[test]
    fn a_line_span_resolves_to_the_exact_message_bytes() {
        let input = "第一行\n第二行：创建 stage-2.txt！\n第三行";
        let quote = resolve_source_span(span(2, 2), input, 8000).expect("line two resolves");
        assert_eq!(quote, "第二行：创建 stage-2.txt！");
        assert!(input.contains(&quote));
    }

    #[test]
    fn punctuation_width_is_never_compared_because_the_runtime_cuts_the_text() {
        // The user wrote both widths on purpose; no model copy participates.
        let input = "run `echo hi!`\nThen verify it worked！\nDone.";
        let quote = resolve_source_span(span(2, 2), input, 8000).expect("span resolves");
        assert_eq!(quote, "Then verify it worked！");
        assert!(quote.contains('！'));
        assert!(!quote.contains("worked!"));
    }

    #[test]
    fn a_multi_line_span_keeps_every_character_between_the_boundaries() {
        let input = "one\ntwo\t  and spaces\nthree";
        let quote = resolve_source_span(span(1, 2), input, 8000).expect("span resolves");
        assert_eq!(quote, "one\ntwo\t  and spaces");
        assert!(input.contains(&quote));
    }

    #[test]
    fn crlf_and_a_trailing_newline_do_not_shift_line_numbers() {
        let input = "first\r\nsecond\r\nthird\r\n";
        let quote = resolve_source_span(span(2, 2), input, 8000).expect("span resolves");
        assert_eq!(quote, "second");
        assert!(input.contains(&quote));

        // A final newline does not create a fourth, empty line.
        let error = resolve_source_span(span(4, 4), input, 8000).expect_err("line 4 is absent");
        assert_eq!(
            error,
            SourceSpanError::OutOfRange {
                start_line: 4,
                end_line: 4,
                lines: 3
            }
        );
    }

    #[test]
    fn combining_marks_and_emoji_are_addressed_by_line_not_character_count() {
        let input = "e\u{301} combining, 🚀 emoji\n第二行 ✅";
        let quote = resolve_source_span(span(1, 1), input, 8000).expect("span resolves");
        assert_eq!(quote, "e\u{301} combining, 🚀 emoji");
        assert_eq!(
            quote.chars().count(),
            "e\u{301} combining, 🚀 emoji".chars().count()
        );
    }

    #[test]
    fn repeated_text_resolves_positionally_instead_of_by_first_match() {
        let input = "same\ndifferent\nsame";
        let quote = resolve_source_span(span(3, 3), input, 8000).expect("span resolves");
        assert_eq!(quote, "same");
        let earlier = resolve_source_span(span(1, 1), input, 8000).expect("span resolves");
        assert_eq!(earlier, "same");
        // Both are the same text, but the span is still an unambiguous position.
        let middle = resolve_source_span(span(2, 2), input, 8000).expect("span resolves");
        assert_eq!(middle, "different");
    }

    #[test]
    fn blank_reversed_and_out_of_range_spans_are_refused() {
        let input = "real requirement\n\nlast";
        assert_eq!(
            resolve_source_span(span(2, 2), input, 8000).expect_err("blank line"),
            SourceSpanError::Empty
        );
        assert_eq!(
            resolve_source_span(span(0, 1), input, 8000).expect_err("zero line"),
            SourceSpanError::ZeroLine
        );
        assert_eq!(
            resolve_source_span(span(3, 2), input, 8000).expect_err("reversed"),
            SourceSpanError::Reversed {
                start_line: 3,
                end_line: 2
            }
        );
        assert_eq!(
            resolve_source_span(span(1, 4), input, 8000).expect_err("past the end"),
            SourceSpanError::OutOfRange {
                start_line: 1,
                end_line: 4,
                lines: 3
            }
        );
        assert_eq!(
            resolve_source_span(span(1, 1), "", 8000).expect_err("empty message"),
            SourceSpanError::OutOfRange {
                start_line: 1,
                end_line: 1,
                lines: 0
            }
        );
    }

    #[test]
    fn an_over_long_span_is_refused_instead_of_truncated() {
        let input = "aaaaaaaaaa\nbbbbbbbbbb";
        let error = resolve_source_span(span(1, 2), input, 5).expect_err("too long");
        assert_eq!(
            error,
            SourceSpanError::TooLong {
                chars: 21,
                max_chars: 5
            }
        );
    }

    #[test]
    fn continue_then_set_boundary_is_ordered_and_composable() {
        let plan = plan(&[
            ("s1", "completed"),
            ("s2", "in_progress"),
            ("s3", "pending"),
        ]);
        let current = IntentProjection {
            pause_in_force: true,
            pause_boundary: Some(PauseBoundary::Now),
            cancel: false,
            answer_only: false,
            independent_task: false,
        };
        let declaration = declaration(
            "继续执行，但第三阶段完成后暂停",
            vec![
                UserIntentOperation::ContinueTask,
                UserIntentOperation::SetPauseBoundary {
                    boundary: PauseBoundary::AfterStep {
                        step: StageRef {
                            step_id: "s3".into(),
                            label: Some("stage three".into()),
                        },
                    },
                },
            ],
        );
        let projected = project_intent(&declaration, &current, &plan).unwrap();
        assert!(!projected.pause_in_force, "the old pause is released");
        assert_eq!(
            projected.pause_boundary,
            Some(PauseBoundary::AfterStep {
                step: StageRef {
                    step_id: "s3".into(),
                    label: Some("stage three".into()),
                }
            })
        );
        assert!(!projected.pause_boundary.unwrap().reached(&plan));
    }

    #[test]
    fn a_completed_boundary_step_pauses_at_the_next_closeout() {
        let plan = plan(&[("s1", "completed"), ("s2", "completed")]);
        let current = IntentProjection {
            pause_in_force: false,
            pause_boundary: None,
            cancel: false,
            answer_only: false,
            independent_task: false,
        };
        let projected = project_intent(
            &declaration(
                "stop after stage two",
                vec![UserIntentOperation::PauseTask {
                    boundary: PauseBoundary::AfterStep {
                        step: StageRef {
                            step_id: "s2".into(),
                            label: None,
                        },
                    },
                }],
            ),
            &current,
            &plan,
        )
        .unwrap();
        assert!(projected.pause_in_force);
    }

    #[test]
    fn a_boundary_cannot_name_a_step_the_model_never_declared() {
        let plan = plan(&[("s1", "pending")]);
        let current = IntentProjection {
            pause_in_force: false,
            pause_boundary: None,
            cancel: false,
            answer_only: false,
            independent_task: false,
        };
        let error = project_intent(
            &declaration(
                "pause after stage three",
                vec![UserIntentOperation::PauseTask {
                    boundary: PauseBoundary::AfterStep {
                        step: StageRef {
                            step_id: "s3".into(),
                            label: None,
                        },
                    },
                }],
            ),
            &current,
            &plan,
        )
        .unwrap_err();
        assert_eq!(
            error,
            IntentError::BoundaryStepUnknown {
                step_id: "s3".into()
            }
        );
    }

    #[test]
    fn a_boundary_without_a_plan_is_refused_with_the_fix() {
        let current = IntentProjection {
            pause_in_force: false,
            pause_boundary: None,
            cancel: false,
            answer_only: false,
            independent_task: false,
        };
        let error = project_intent(
            &declaration(
                "pause after stage three",
                vec![UserIntentOperation::PauseTask {
                    boundary: PauseBoundary::AfterStep {
                        step: StageRef {
                            step_id: "s3".into(),
                            label: None,
                        },
                    },
                }],
            ),
            &current,
            &PlanState::default(),
        )
        .unwrap_err();
        assert!(matches!(error, IntentError::BoundaryWithoutPlan { .. }));
        assert!(error.to_string().contains("update_plan"));
    }

    #[test]
    fn a_step_that_vanished_from_the_plan_is_not_reached() {
        let boundary = PauseBoundary::AfterStep {
            step: StageRef {
                step_id: "s2".into(),
                label: None,
            },
        };
        // Deleted outright.
        assert!(!boundary.reached(&plan(&[("s1", "completed")])));
        // Renamed without keeping the stable id.
        assert!(!boundary.reached(&plan(&[("s1", "completed"), ("renamed", "completed"),])));
        // Replaced by a different step.
        assert!(!boundary.reached(&plan(&[
            ("s1", "completed"),
            ("replacement", "in_progress"),
        ])));
        // Split into two steps.
        assert!(!boundary.reached(&plan(&[
            ("s1", "completed"),
            ("s2a", "completed"),
            ("s2b", "pending"),
        ])));
        // Merged into a later step.
        assert!(!boundary.reached(&plan(&[("s1", "completed"), ("s2-and-s3", "in_progress"),])));
    }

    #[test]
    fn a_boundary_needs_an_explicit_completed_step() {
        let boundary = PauseBoundary::AfterStep {
            step: StageRef {
                step_id: "s2".into(),
                label: None,
            },
        };
        assert!(!boundary.reached(&plan(&[("s2", "pending")])));
        assert!(!boundary.reached(&plan(&[("s2", "in_progress")])));
        assert!(!boundary.reached(&plan(&[("s2", "abandoned")])));
        assert!(boundary.reached(&plan(&[("s2", "completed")])));
        // A rename that keeps the stable id follows the same step.
        let renamed = PlanState {
            steps: vec![PlanStep {
                step: "the renamed stage".into(),
                status: "completed".into(),
                id: Some("s2".into()),
                origin: PlanOrigin::ModelExplicit,
            }],
        };
        assert!(boundary.reached(&renamed));
    }

    #[test]
    fn terminal_operations_cannot_be_combined_with_transitions() {
        let plan = PlanState::default();
        let current = IntentProjection {
            pause_in_force: false,
            pause_boundary: None,
            cancel: false,
            answer_only: false,
            independent_task: false,
        };
        for operations in [
            vec![
                UserIntentOperation::CancelTask,
                UserIntentOperation::ContinueTask,
            ],
            vec![
                UserIntentOperation::AnswerOnly,
                UserIntentOperation::PauseTask {
                    boundary: PauseBoundary::Now,
                },
            ],
        ] {
            let error = project_intent(&declaration("cancel that", operations), &current, &plan)
                .unwrap_err();
            assert!(matches!(
                error,
                IntentError::TerminalOperationCombined { .. }
            ));
        }
    }

    #[test]
    fn an_empty_or_oversized_declaration_is_refused() {
        let plan = PlanState::default();
        let current = IntentProjection {
            pause_in_force: false,
            pause_boundary: None,
            cancel: false,
            answer_only: false,
            independent_task: false,
        };
        assert_eq!(
            project_intent(&declaration("go on", vec![]), &current, &plan,).unwrap_err(),
            IntentError::NoOperations
        );
        let too_many = vec![UserIntentOperation::AmendCurrentTask; INTENT_OPERATIONS_MAX + 1];
        assert!(matches!(
            project_intent(&declaration("go on", too_many), &current, &plan,).unwrap_err(),
            IntentError::TooManyOperations { .. }
        ));
        let oversized = "x".repeat(INTENT_SOURCE_QUOTE_MAX_CHARS + 1);
        assert_eq!(
            project_intent(
                &declaration(&oversized, vec![UserIntentOperation::AmendCurrentTask]),
                &current,
                &plan,
            )
            .unwrap_err(),
            IntentError::QuoteTooLong {
                chars: INTENT_SOURCE_QUOTE_MAX_CHARS + 1
            }
        );
    }

    #[test]
    fn serde_shape_is_the_model_facing_contract() {
        let value = serde_json::json!({
            "source_quote": "继续执行，但第三阶段完成后暂停。",
            "operations": [
                {"kind": "continue_task"},
                {"kind": "set_pause_boundary", "boundary": {"kind": "after_step", "step": {"step_id": "s3"}}}
            ]
        });
        let declaration: UserIntentDeclaration = serde_json::from_value(value).unwrap();
        assert_eq!(declaration.operations.len(), 2);
        assert!(matches!(
            declaration.operations[1],
            UserIntentOperation::SetPauseBoundary {
                boundary: PauseBoundary::AfterStep { .. }
            }
        ));
    }
}
