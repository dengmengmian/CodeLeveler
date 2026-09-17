//! Neutral events the kernel emits as a run progresses. A host maps these
//! onto its own event vocabulary; the kernel never renders them.

use leveler_model::{ContextAccounting, TokenUsage};

/// What the loop observed. Deltas are token-level; tool events carry only a
/// bounded preview, never a full payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
    /// A model stream attempt is starting. Discard any in-flight deltas from
    /// the previous attempt before applying new ones.
    StreamAttemptStarted,
    /// A streamed chunk of assistant text.
    AssistantDelta(String),
    /// A streamed chunk of model reasoning, rendered separately from the
    /// final assistant answer.
    ReasoningDelta(String),
    /// Token usage the model reported for a request (may arrive mid-stream or
    /// at the end).
    Usage(TokenUsage),
    /// The context accounting of the request the loop is ABOUT to send: what
    /// the exact next model-visible payload is made of, category by category.
    /// Computed from the real `messages` + `tools`, never from a UI transcript.
    ContextUsage(ContextAccounting),
    /// A model round is about to retry the same request. Transient: a
    /// connectivity fact for a live status line, never a transcript item and
    /// never an executor outcome.
    ModelRetrying {
        /// Failures so far on this lane (1-based): the retry about to happen.
        attempt: u32,
        /// The bound for this lane, so a client can say `2/3`.
        max_attempts: u32,
        /// How long the loop will wait before the next attempt.
        delay_ms: u64,
    },
    /// The retry budget is spent on a `Safe` failure, so the round is waiting,
    /// low-frequency, for the network to come back instead of failing the task.
    /// Transient, like [`AgentEvent::ModelRetrying`]. `elapsed_ms` is how long
    /// this wait has lasted so far.
    ModelWaitingForNetwork { elapsed_ms: u64 },
    /// A tool call is about to execute. `id` pairs it with the matching
    /// [`AgentEvent::ToolCallFinished`].
    ToolCallStarted {
        id: String,
        name: String,
        arguments: String,
    },
    /// A tool finished. `id` matches its [`AgentEvent::ToolCallStarted`].
    ToolCallFinished {
        id: String,
        name: String,
        is_error: bool,
        preview: String,
    },
}

/// Bounded, terminal-safe preview of a payload.
pub(crate) fn preview(s: &str) -> String {
    const MAX: usize = 1200;
    if s.chars().count() <= MAX {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(MAX).collect();
        format!("{truncated}…")
    }
}
