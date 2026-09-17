//! Mid-task clarification: the counterpart of [`crate::approval`] for
//! questions rather than permissions.
//!
//! The executor asks a [`Clarifier`] when the model needs the user to resolve
//! something it cannot decide alone. Like [`crate::Approver`], the port lives
//! here so the runtime that records the request and the harness that raises it
//! share one vocabulary without either depending on the other.

use async_trait::async_trait;

use leveler_core::{ClarificationId, TurnId};

/// How one question of a clarification is answered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ClarificationQuestionKind {
    /// Exactly one option, or a free-text answer when `allow_other` is set.
    #[default]
    Single,
    /// Zero or more options.
    Multi,
    /// A free-text answer.
    Text,
}

/// One question of a clarification interaction (spec §35).
///
/// A clarification can carry several questions the user answers in one
/// sitting. The kind is explicit so the UI never has to guess a text prompt
/// out of an empty option list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClarificationQuestion {
    /// Short label for the question's tab (empty = derive from `question`).
    pub header: String,
    pub question: String,
    pub kind: ClarificationQuestionKind,
    pub options: Vec<String>,
    /// Offer a trailing free-text entry ("其他…") next to the options.
    pub allow_other: bool,
    pub min_choices: u32,
    pub max_choices: Option<u32>,
}

/// A request for the user to clarify something mid-task (spec §35): the model
/// calls `request_user_input` (or legacy `ask_user`), which blocks until the UI answers.
#[derive(Debug, Clone)]
pub struct ClarificationRequest {
    pub id: ClarificationId,
    /// Filled by the engine recorder once the persisted turn exists.
    pub turn_id: Option<TurnId>,
    pub tool: String,
    pub call_id: String,
    pub action_fingerprint: String,
    /// The headline: what the interaction is about. For a legacy
    /// single-question request this is also the whole prompt.
    pub question: String,
    pub options: Vec<String>,
    /// The questions to answer together. Empty means the legacy
    /// single-question shape (`question`/`options`).
    pub questions: Vec<ClarificationQuestion>,
}

/// How a clarification request ended. `Answered` is the ONLY variant that may
/// be presented to the model as the user speaking; every other variant must
/// surface as "no user reply", never as an (empty) answer. This is the
/// clarification-side counterpart of `Approver::has_human` (R004 F2: an empty
/// string silently impersonated the user).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClarifyOutcome {
    /// A human answered with this text (may still be empty = explicit skip).
    Answered(String),
    /// A human saw the question and explicitly skipped it.
    Skipped,
    /// No human is attached to this run (headless run, sub-agent, or the
    /// question could not be delivered to any client).
    Unattended,
    /// The question was delivered but nobody responded before the deadline.
    TimedOut,
    /// The turn is being cancelled; the answer no longer matters.
    Cancelled,
}

impl ClarifyOutcome {
    /// Short machine label for recording/observability.
    pub fn label(&self) -> &'static str {
        match self {
            ClarifyOutcome::Answered(_) => "answered",
            ClarifyOutcome::Skipped => "skipped",
            ClarifyOutcome::Unattended => "unattended",
            ClarifyOutcome::TimedOut => "timed_out",
            ClarifyOutcome::Cancelled => "cancelled",
        }
    }
}

/// Something that can answer clarification requests.
#[async_trait]
pub trait Clarifier: Send + Sync {
    async fn clarify(&self, request: &ClarificationRequest) -> ClarifyOutcome;
}

/// Non-interactive default: no human is attached, and the model must be told
/// so instead of receiving a fabricated empty "answer".
pub struct AutoClarify;

#[async_trait]
impl Clarifier for AutoClarify {
    async fn clarify(&self, _request: &ClarificationRequest) -> ClarifyOutcome {
        ClarifyOutcome::Unattended
    }
}
