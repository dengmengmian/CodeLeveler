//! Neutral events the kernel emits as a run progresses. A host maps these
//! onto its own event vocabulary; the kernel never renders them.

use leveler_model::TokenUsage;

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
