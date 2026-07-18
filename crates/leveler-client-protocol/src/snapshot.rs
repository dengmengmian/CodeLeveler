//! UI-facing snapshot types: the runtime's state rendered for a client.
//!
//! These are deliberately lossy projections of the real domain (they carry
//! display strings, not live handles) so a client can render without reaching
//! into the runtime. Large payloads (full tool output, image bytes) never live
//! here — they stay in the artifact store and are referenced by id .

use serde::{Deserialize, Serialize};

use crate::PermissionProfile;
use leveler_core::{SessionId, ToolCallId};
use leveler_model::ModelRef;

use crate::{UiCompletionReport, UiDiff, UiPlan, UiVerification};

/// Identifies a single assistant/user message in the transcript.
///
/// A protocol-level id (the runtime persists messages as an ordered log, not by
/// id); it lets streaming deltas target the right in-flight message.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MessageId(String);

impl MessageId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for MessageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_id_roundtrips_as_str() {
        let id = MessageId::new("msg-42");
        assert_eq!(id.as_str(), "msg-42");
    }

    #[test]
    fn message_id_display_writes_value() {
        let id = MessageId::new("msg-42");
        assert_eq!(id.to_string(), "msg-42");
    }
}

/// Who authored a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiRole {
    User,
    Assistant,
    System,
    Tool,
}

/// A rendered message in the transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiMessage {
    pub id: MessageId,
    pub role: UiRole,
    pub text: String,
}

/// Coarse runtime state, surfaced in the status line .
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeStatus {
    /// Ready for input.
    Idle,
    /// A turn is running.
    Busy,
    /// A turn ended in error.
    Error,
}

/// A conversation restore point (spec §68). Restoring truncates the transcript
/// back to `ordinal` messages; working-tree files are left to the user's git.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiCheckpoint {
    pub id: leveler_core::CheckpointId,
    pub label: String,
    /// The persisted-message count to truncate back to.
    pub ordinal: u32,
}

/// A tool invocation that was still running when a client took its snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiActiveToolCall {
    pub id: ToolCallId,
    pub name: String,
    pub arguments: String,
}

/// A one-line session summary for the Sessions screen (spec §52).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiSessionSummary {
    pub id: SessionId,
    pub goal: String,
    pub status: String,
    pub model: String,
    pub updated_at: String,
}

/// Everything a client needs to render a session's header and transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiSessionSnapshot {
    pub id: SessionId,
    pub repository: String,
    pub goal: String,
    pub model: Option<ModelRef>,
    pub mode: PermissionProfile,
    /// VCS branch, if the repository is a git repo.
    pub branch: Option<String>,
    /// Persisted status string (e.g. "running", "completed").
    pub status: String,
    pub messages: Vec<UiMessage>,
    /// Live approval/clarification waiters for reconnect/resync.
    #[serde(default)]
    pub pending_interactions: Vec<crate::UiPendingInteraction>,
    /// Models the user can switch to (for the model picker, ).
    #[serde(default)]
    pub available_models: Vec<ModelRef>,
    /// Whether the current model accepts image input (spec §42).
    #[serde(default)]
    pub vision: bool,
    /// The event-log sequence this snapshot reflects — the resync anchor. A
    /// client that fell behind (broadcast lag, reconnect) takes a fresh snapshot
    /// and resumes the event stream *after* this sequence, so it neither
    /// double-applies nor misses a canonical event. `None` when unknown (e.g. a
    /// brand-new session with no events yet).
    #[serde(default)]
    pub last_sequence: Option<i64>,
    /// Live render state needed to reconnect while a long turn is still
    /// running. All fields are additive/defaulted for protocol compatibility.
    #[serde(default)]
    pub active_tools: Vec<UiActiveToolCall>,
    #[serde(default)]
    pub plan: Option<UiPlan>,
    #[serde(default)]
    pub verification: Option<UiVerification>,
    #[serde(default)]
    pub diff: Option<UiDiff>,
    #[serde(default)]
    pub checkpoints: Vec<UiCheckpoint>,
    #[serde(default)]
    pub completion_report: Option<UiCompletionReport>,
}
