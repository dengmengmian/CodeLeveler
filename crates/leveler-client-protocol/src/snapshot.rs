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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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

    #[test]
    fn old_snapshot_json_without_reasoning_stays_compatible() {
        let json = serde_json::json!({
            "id": "s1",
            "repository": "/repo",
            "goal": "g",
            "model": null,
            "mode": "assisted",
            "branch": null,
            "status": "idle",
            "messages": []
        });
        let snap: UiSessionSnapshot = serde_json::from_value(json).unwrap();
        assert_eq!(snap.reasoning, None);
    }

    #[test]
    fn old_snapshot_json_without_axes_stays_compatible() {
        let json = serde_json::json!({
            "id": "s1",
            "repository": "/repo",
            "goal": "g",
            "model": null,
            "mode": "assisted",
            "branch": null,
            "status": "idle",
            "messages": []
        });
        let snap: UiSessionSnapshot = serde_json::from_value(json).unwrap();
        assert_eq!(snap.work_profile, None);
        assert_eq!(snap.collaboration, None);
    }

    #[test]
    fn snapshot_carries_product_axes_when_set() {
        let mut snap = UiSessionSnapshot {
            id: crate::SessionId::new("s1"),
            repository: "/repo".into(),
            goal: "g".into(),
            model: None,
            mode: crate::PermissionProfile::Assisted,
            branch: None,
            status: "idle".into(),
            messages: Vec::new(),
            pending_interactions: Vec::new(),
            available_models: Vec::new(),
            vision: false,
            last_sequence: None,
            active_tools: Vec::new(),
            plan: None,
            verification: None,
            diff: None,
            checkpoints: Vec::new(),
            user_shells: Vec::new(),
            completion_report: None,
            reasoning: None,
            work_profile: None,
            collaboration: None,
        };
        // Unset axes stay off the wire (old-client compatibility).
        let value = serde_json::to_value(&snap).unwrap();
        assert!(value.get("work_profile").is_none(), "{value}");
        assert!(value.get("collaboration").is_none(), "{value}");

        snap.work_profile = Some("delivery".into());
        snap.collaboration = Some("goal".into());
        let value = serde_json::to_value(&snap).unwrap();
        assert_eq!(value["work_profile"], "delivery");
        assert_eq!(value["collaboration"], "goal");
        let back: UiSessionSnapshot = serde_json::from_value(value).unwrap();
        assert_eq!(back, snap);
    }

    #[test]
    fn snapshot_omits_reasoning_when_unset() {
        let snap = UiSessionSnapshot {
            id: crate::SessionId::new("s1"),
            repository: "/repo".into(),
            goal: "g".into(),
            model: None,
            mode: crate::PermissionProfile::Assisted,
            branch: None,
            status: "idle".into(),
            messages: Vec::new(),
            pending_interactions: Vec::new(),
            available_models: Vec::new(),
            vision: false,
            last_sequence: None,
            active_tools: Vec::new(),
            plan: None,
            verification: None,
            diff: None,
            checkpoints: Vec::new(),
            user_shells: Vec::new(),
            completion_report: None,
            reasoning: None,
            work_profile: None,
            collaboration: None,
        };
        let value = serde_json::to_value(&snap).unwrap();
        assert!(value.get("reasoning").is_none(), "{value}");
    }
}

/// Who authored a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum UiRole {
    User,
    Assistant,
    System,
    Tool,
}

/// A rendered message in the transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiMessage {
    pub id: MessageId,
    pub role: UiRole,
    pub text: String,
}

/// The runtime's self-description, served to clients for identity
/// verification, diagnostics, and reconnect checks.
///
/// `runtime_id` is the durable identity (stable across daemon restarts);
/// `pid` and `version` are diagnostics about the *current process* serving
/// that identity — never identity themselves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RuntimeInfo {
    pub runtime_id: leveler_core::RuntimeId,
    /// The serving binary's version (diagnostics).
    #[serde(default)]
    pub version: String,
    /// The serving process id (diagnostics; changes on restart).
    #[serde(default)]
    pub pid: u32,
    /// Minimal health (all additive; old daemons deserialize as defaults).
    ///
    /// Health is NOT ownership: a reachable, accepting runtime still proves
    /// task authority only through its current OwnershipToken — this block
    /// never bypasses fencing. Reachability itself is the request having
    /// succeeded; a timeout/decode failure means "unreachable", never a
    /// healthy-shaped default.
    #[serde(default)]
    pub health: RuntimeHealth,
}

/// The runtime's admission/lifecycle health at answer time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RuntimeHealth {
    /// Whether the runtime will admit new main turns right now (false while
    /// shutting down or at capacity). Old daemons report the default
    /// `false` — callers treat "unknown" conservatively.
    #[serde(default)]
    pub accepting_work: bool,
    /// Main turns currently executing.
    #[serde(default)]
    pub active_turns: u32,
    /// The concurrent main-turn admission limit, when one exists. This is
    /// the real ActiveTurns capacity, not an invented number.
    #[serde(default)]
    pub turn_capacity: Option<u32>,
    /// The runtime has begun an explicit shutdown.
    #[serde(default)]
    pub shutting_down: bool,
}

/// Coarse runtime state, surfaced in the status line .
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiCheckpoint {
    pub id: leveler_core::CheckpointId,
    pub label: String,
    /// The persisted-message count to truncate back to.
    pub ordinal: u32,
}

/// A tool invocation that was still running when a client took its snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiActiveToolCall {
    pub id: ToolCallId,
    pub name: String,
    pub arguments: String,
}

/// One user shell execution (`!command`) as the reconnect snapshot carries
/// it: the active one plus a bounded recent history. `output_tail` is the
/// bounded end of the combined output (never the full log).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiUserShell {
    pub id: leveler_core::UserShellId,
    pub command: String,
    pub cwd: String,
    /// `running | success | failed | cancelled`.
    pub status: String,
    /// Seconds elapsed at snapshot time (running) or total runtime (done).
    pub elapsed_secs: u64,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub output_tail: String,
    /// True when `output_tail` dropped earlier output.
    #[serde(default)]
    pub output_truncated: bool,
}

/// A one-line session summary for the Sessions screen (spec §52).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiSessionSummary {
    pub id: SessionId,
    pub goal: String,
    pub status: String,
    pub model: String,
    pub updated_at: String,
    /// Repository root the session belongs to. Filled by the runtime that
    /// owns the session and by the WebUI aggregation router (multi-project
    /// grouping); omitted on the wire when unknown so old fixtures and
    /// clients keep parsing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
}

/// Everything a client needs to render a session's header and transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
    /// User shell executions: the active one (if any) plus a bounded recent
    /// history, newest last. Additive/defaulted like the rest of this block.
    #[serde(default)]
    pub user_shells: Vec<UiUserShell>,
    #[serde(default)]
    pub completion_report: Option<UiCompletionReport>,
    /// Runtime-projected reasoning state. Absent on old runtimes so a new
    /// client keeps its boot-time value. Present with `effective: None` means
    /// the model has no controllable effort knob (do not invent one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<UiReasoningState>,
    /// Product work-profile axis (`economy | balanced | delivery`). The source
    /// of truth is the session record (`SetProductAxes`); carried here so a
    /// reconnecting client shows the axis the runtime will actually use
    /// instead of a stale local guess. Absent on old runtimes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_profile: Option<String>,
    /// Product collaboration axis (`chat | plan | goal`). Same contract as
    /// `work_profile` — the runtime routes submits (goal) and restricts tools
    /// (plan) from this value, so clients must not invent it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collaboration: Option<String>,
}

/// Additive reasoning projection. The client must display `effective` and
/// must not infer an effort from the model name or provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiReasoningState {
    /// Wire value the runtime will send (`max`, `high`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective: Option<String>,
}
