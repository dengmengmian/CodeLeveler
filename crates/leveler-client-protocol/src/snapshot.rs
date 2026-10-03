//! UI-facing snapshot types: the runtime's state rendered for a client.
//!
//! These are deliberately lossy projections of the real domain (they carry
//! display strings, not live handles) so a client can render without reaching
//! into the runtime. Large payloads (full tool output, image bytes) never live
//! here — they stay in the artifact store and are referenced by id .

use serde::{Deserialize, Serialize};

use crate::PermissionProfile;
use leveler_core::{SessionId, ToolCallId};
use leveler_model::{ModelRef, ThinkingLevel};

use crate::{UiCompletionReport, UiDiff, UiPlan};

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
    fn session_summary_preserves_declaration_and_accepts_legacy_rows() {
        let legacy = serde_json::json!({"id":"s", "goal":"g", "status":"completed", "model":"m", "updated_at":"now"});
        let row: UiSessionSummary = serde_json::from_value(legacy.clone()).unwrap();
        assert!(
            serde_json::to_value(row)
                .unwrap()
                .get("declaration")
                .is_none()
        );
        for declaration in ["answered", "completed"] {
            let mut wire = legacy.clone();
            wire["declaration"] = declaration.into();
            let row: UiSessionSummary = serde_json::from_value(wire).unwrap();
            assert_eq!(
                serde_json::to_value(row).unwrap()["declaration"],
                declaration
            );
        }
    }

    #[test]
    fn workspace_snapshot_distinguishes_none_from_legacy_repository() {
        let legacy = serde_json::json!({"id":"s", "repository":"/repo", "goal":"g", "model":null, "mode":"assisted", "branch":null, "status":"idle", "messages":[]});
        let snapshot: UiSessionSnapshot = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(
            serde_json::to_value(snapshot).unwrap()["repository"],
            "/repo"
        );
        let mut absent = legacy;
        absent["repository"] = serde_json::Value::Null;
        let snapshot: UiSessionSnapshot = serde_json::from_value(absent)
            .expect("No Workspace must decode without a fake repository");
        assert!(serde_json::to_value(snapshot).unwrap()["repository"].is_null());
    }

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
    fn old_snapshot_json_without_thinking_stays_compatible() {
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
        assert_eq!(snap.thinking, None);
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
            repository: Some("/repo".into()),
            task_status: None,
            task_terminal: None,
            goal: "g".into(),
            model: None,
            mode: crate::PermissionProfile::Assisted,
            branch: None,
            status: "idle".into(),
            finalization_stage: None,
            messages: Vec::new(),
            pending_interactions: Vec::new(),
            available_models: Vec::new(),
            vision: false,
            last_sequence: None,
            active_tools: Vec::new(),
            active_background_tasks: Vec::new(),
            plan: None,
            diff: None,
            checkpoints: Vec::new(),
            recaps: Vec::new(),
            user_shells: Vec::new(),
            completion_report: None,
            thinking: None,
            work_profile: None,
            collaboration: None,
            children: Vec::new(),
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
    fn the_thinking_state_is_canonical_and_carries_no_provider_parameter() {
        let state = crate::UiThinkingState {
            configured: leveler_model::ThinkingLevel::High,
            session_override: Some(leveler_model::ThinkingLevel::Max),
            current: leveler_model::ThinkingLevel::Max,
            effective: leveler_model::ThinkingLevel::Max,
            access: crate::UiThinkingAccess::Adjustable,
            choices: vec![
                leveler_model::ThinkingLevel::Auto,
                leveler_model::ThinkingLevel::Low,
                leveler_model::ThinkingLevel::High,
                leveler_model::ThinkingLevel::Max,
            ],
        };
        let json = serde_json::to_value(&state).unwrap();
        let text = json.to_string();
        assert!(text.contains("\"max\""), "{text}");
        // The provider's own vocabulary has no way in.
        for native in ["xhigh", "reasoning_effort", "budget_tokens", "enabled"] {
            assert!(!text.contains(native), "{native} in {text}");
        }
        // ...and it survives a round trip unchanged.
        let back: crate::UiThinkingState = serde_json::from_value(json).unwrap();
        assert_eq!(back, state);
    }

    #[test]
    fn snapshot_omits_thinking_when_unset() {
        let snap = UiSessionSnapshot {
            id: crate::SessionId::new("s1"),
            repository: Some("/repo".into()),
            task_status: None,
            task_terminal: None,
            goal: "g".into(),
            model: None,
            mode: crate::PermissionProfile::Assisted,
            branch: None,
            status: "idle".into(),
            finalization_stage: None,
            messages: Vec::new(),
            pending_interactions: Vec::new(),
            available_models: Vec::new(),
            vision: false,
            last_sequence: None,
            active_tools: Vec::new(),
            active_background_tasks: Vec::new(),
            plan: None,
            diff: None,
            checkpoints: Vec::new(),
            recaps: Vec::new(),
            user_shells: Vec::new(),
            completion_report: None,
            thinking: None,
            work_profile: None,
            collaboration: None,
            children: Vec::new(),
        };
        let value = serde_json::to_value(&snap).unwrap();
        assert!(value.get("thinking").is_none(), "{value}");
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
    /// Persisted transcript ordinal (append order in the message log), when
    /// known. Lets a client interleave durable goal recaps at the position
    /// their checkpoint represents. Additive: absent on live-stream messages
    /// and old runtimes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ordinal: Option<u64>,
    /// What kind of message this is when its role alone would mislead.
    /// `None` is an ordinary message of its role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<UiMessageKind>,
    /// How many images this message carried. A client renders them its own
    /// way — the text never holds a file path, and a message that was only a
    /// picture would otherwise read as an empty one. Additive: 0 on old
    /// runtimes and on every message that carried none.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub images: usize,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// A message whose role does not say who wrote it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum UiMessageKind {
    /// Written by the runtime into the model's context as a user-role turn —
    /// a child's settlement, a lost or resumed delegation — not typed by the
    /// user. Render it as a runtime notice, never as user input.
    RuntimeNotice,
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
    /// WHICH build is serving — version plus the revision it came from.
    ///
    /// `version` alone cannot answer that: a replaced binary on disk leaves
    /// the running daemon on its original image, and both report the same
    /// version while behaving differently. A runtime built before this field
    /// existed reports the default, which `BuildIdentity::is_known` rejects
    /// rather than reading as agreement.
    #[serde(default)]
    pub build: leveler_core::BuildIdentity,
    /// Digest of the global, project and repository model/provider sources
    /// loaded by this process. Clients compare it with their current files so
    /// a same-build daemon never serves a stale configuration generation.
    #[serde(default)]
    pub config_fingerprint: Option<String>,
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
    /// Background tasks (dev servers, builds) still alive across the whole
    /// runtime. `active_turns == 0` alone is NOT idle: a `cargo check` can
    /// outlive its turn, and a retire that ignored this would throw that work
    /// away. Reported so a client can see WHY a retiring runtime has not gone.
    #[serde(default)]
    pub active_background_tasks: u32,
    /// The runtime can exit now: no main turn and no background task left.
    /// Computed by the runtime from the SAME two counters, so a client can
    /// never be told "idle" by an accounting the drain disagrees with.
    #[serde(default)]
    pub quiescent: bool,
    /// The concurrent main-turn admission limit, when one exists. This is
    /// the real ActiveTurns capacity, not an invented number.
    #[serde(default)]
    pub turn_capacity: Option<u32>,
    /// The runtime has begun a shutdown. Two reasons share this commit point:
    /// an explicit `Quit`, and retirement for a generation handover — the
    /// second carries `retiring_reason`.
    #[serde(default)]
    pub shutting_down: bool,
    /// Why the runtime is retiring, when it is doing so for a generation
    /// handover (`BuildMismatch` / `ConfigChanged`). `None` for a plain
    /// `Quit` and for a runtime that is still serving.
    #[serde(default)]
    pub retiring_reason: Option<crate::RestartReason>,
    /// The live tasks behind `active_background_tasks`, oldest first. Empty on
    /// an idle runtime and on every runtime older than this field; additive so
    /// an old client still decodes. Read-only: see [`UiBackgroundTaskBlocker`].
    #[serde(default)]
    pub blockers: Vec<UiBackgroundTaskBlocker>,
    /// The live MAIN turns behind `active_turns`, oldest first. Empty on an
    /// idle runtime and on every runtime older than this field; additive so an
    /// old client still decodes. Read-only: see [`UiTurnBlocker`].
    #[serde(default)]
    pub turn_blockers: Vec<UiTurnBlocker>,
}

/// How long a main turn may show no observable activity before a handover
/// labels it "no observable progress".
///
/// This is a HINT threshold, never a verdict: crossing it changes only how the
/// blocker is described. Nothing here — and nothing downstream of it — may
/// auto-cancel, auto-kill, or force-retire a turn. A real long task that keeps
/// emitting events never reaches it.
pub const STALE_TURN_WARN_AFTER: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// One main turn still running in a runtime, as observed at answer time.
///
/// Facts a handover wait can show: which session, how long the turn has run,
/// and how long since it last did anything observable. Deliberately carries no
/// generation id, lock state, or other implementation detail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiTurnBlocker {
    pub session_id: SessionId,
    /// Runtime-observed age at answer time.
    pub elapsed_ms: u64,
    /// Time since the last observable activity signal for this turn.
    pub idle_ms: u64,
}

impl UiTurnBlocker {
    /// Whether this turn has shown no observable activity for at least
    /// `threshold`. A description, not a decision: the caller (a human, via
    /// the handover UI) decides whether to interrupt or force-retire.
    pub fn suspected_stalled_after(&self, threshold: std::time::Duration) -> bool {
        self.idle_ms >= threshold.as_millis() as u64
    }
}

impl RuntimeHealth {
    /// Whether the runtime may exit now. The one predicate shared by the
    /// client-side handover wait and the runtime's own drain.
    pub fn quiescent(&self) -> bool {
        self.active_turns == 0 && self.active_background_tasks == 0
    }

    /// Whether the runtime is retiring for a generation handover (as opposed
    /// to an explicit `Quit`).
    pub fn retiring(&self) -> bool {
        self.shutting_down && self.retiring_reason.is_some()
    }
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
    /// How long the call had been running when the snapshot was taken, by the
    /// runtime's clock, so a reconnecting client does not restart it at zero.
    #[serde(default)]
    pub elapsed_ms: u64,
    /// The bounded end of the command's live output so far.
    #[serde(default)]
    pub output_tail: String,
    /// True when `output_tail` dropped earlier output.
    #[serde(default)]
    pub output_truncated: bool,
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

/// A task's own terminal declaration, projected from its durable TaskFinished stop.
/// This is not verification evidence and does not replace session lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum UiTaskDeclaration {
    Answered,
    Completed,
}

/// A one-line session summary for the Sessions screen (spec §52).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiSessionSummary {
    pub id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_status: Option<crate::UiTaskStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_terminal: Option<crate::UiTaskTerminal>,
    pub goal: String,
    pub status: String,
    /// Latest durable task declaration; absent for legacy or non-declaration endings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declaration: Option<UiTaskDeclaration>,
    pub model: String,
    pub updated_at: String,
    /// Repository root the session belongs to. Filled by the runtime that
    /// owns the session and by the WebUI aggregation router (multi-project
    /// grouping); omitted on the wire when unknown so old fixtures and
    /// clients keep parsing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
}

/// One background process that is still lifecycle-active in the runtime.
///
/// This is a reconnect projection, not history. Terminal tasks never appear
/// here even though the execution registry may retain their records for
/// `get`/`wait`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiActiveBackgroundTask {
    pub task_id: String,
    pub program: String,
    pub args: Vec<String>,
    /// Runtime-observed age at snapshot time.
    pub elapsed_ms: u64,
    /// The process-group leader's OS pid, when known. Additive; older clients
    /// decode it as absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

/// One live background task that is keeping a runtime from becoming idle.
///
/// Read-only status detail: a publishing runtime names the real blocker
/// instead of a bare count, so a client waiting on a handover can show what is
/// owed. It carries no authority — stopping the task still goes through the
/// session-scoped `CancelBackgroundTask` command, never a raw signal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiBackgroundTaskBlocker {
    pub task_id: String,
    pub program: String,
    pub args: Vec<String>,
    /// Runtime-observed age at answer time.
    pub elapsed_ms: u64,
    /// The session that owns the task, when known. `CancelBackgroundTask` is
    /// session-scoped, so this is what makes the blocker stoppable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    /// Bounded tail of the task's combined output, for a read-only view that
    /// does not need the interactive UI.
    #[serde(default)]
    pub log_tail: String,
}

/// Everything a client needs to render a session's header and transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiSessionSnapshot {
    pub id: SessionId,
    pub repository: Option<String>,
    pub goal: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_status: Option<crate::UiTaskStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_terminal: Option<crate::UiTaskTerminal>,
    pub model: Option<ModelRef>,
    pub mode: PermissionProfile,
    /// VCS branch, if the repository is a git repo.
    pub branch: Option<String>,
    /// Persisted status string (e.g. "running", "completed").
    pub status: String,
    /// Typed in-flight terminalization stage. Present only while the runtime
    /// still owns an active turn after the final assistant message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finalization_stage: Option<crate::FinalizationStage>,
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
    /// Registry-backed background processes still active for this session.
    /// Additive/defaulted so older runtimes decode as no known live process.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active_background_tasks: Vec<UiActiveBackgroundTask>,
    #[serde(default)]
    pub plan: Option<UiPlan>,
    #[serde(default)]
    pub diff: Option<UiDiff>,
    #[serde(default)]
    pub checkpoints: Vec<UiCheckpoint>,
    /// Durable goal recaps for this session's goals (long-goal P3), oldest
    /// first. Each maps to one persisted GoalCheckpoint; a reopened client
    /// interleaves them into history by `transcript_ordinal`. Additive.
    #[serde(default)]
    pub recaps: Vec<crate::UiGoalRecap>,
    /// User shell executions: the active one (if any) plus a bounded recent
    /// history, newest last. Additive/defaulted like the rest of this block.
    #[serde(default)]
    pub user_shells: Vec<UiUserShell>,
    #[serde(default)]
    pub completion_report: Option<UiCompletionReport>,
    /// Runtime-projected Thinking Level state, in the user's own vocabulary.
    /// Absent on old runtimes so a new client keeps its boot-time value; a
    /// present value with `access: unsupported` means the model cannot be
    /// asked, which the client says rather than inventing a level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<UiThinkingState>,
    /// Deprecated compatibility field. New runtimes emit `single`; historical
    /// values never control tool exposure. Clients must not display a selector.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_profile: Option<String>,
    /// Product collaboration axis (`chat | plan | goal`). The runtime routes
    /// submissions and applies the read-only planning overlay from this value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collaboration: Option<String>,
    /// Every delegated child of this session, oldest first, from durable storage.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<crate::UiChildAgent>,
}

/// How a model's thinking can be controlled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum UiThinkingAccess {
    /// The model does not reason.
    Unsupported,
    /// The model reasons, but nobody can ask for a different amount.
    Fixed,
    /// The caller can choose a level.
    #[default]
    Adjustable,
}

/// The session's Thinking Level, in CodeLeveler's own vocabulary.
///
/// Every field is a canonical level (`auto`, `off`, `minimal`, `low`, `medium`,
/// `high`, `max`). A client renders these and offers `choices`; it must not
/// derive a level from a model name, from a provider's parameter, or from any
/// other field on the wire — the runtime is the only place that knows what a
/// level means for a model, and it answers here in the user's words.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiThinkingState {
    /// The configured level for this model: its own `[models.<id>] thinking`,
    /// else the global `thinking`.
    pub configured: ThinkingLevel,
    /// The session's explicit override. `None` means the session inherits
    /// `configured` — which is not the same as `Some(Auto)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_override: Option<ThinkingLevel>,
    /// What the user asked for: `session_override` when set, else `configured`.
    pub current: ThinkingLevel,
    /// What is actually in effect for this model right now.
    ///
    /// Equal to `current` when this model can express it. Otherwise `auto`: the
    /// request carries no reasoning override at all, and the provider's own
    /// default applies. A client that showed `current` as if it were running
    /// would be reporting a setting that is not in use.
    pub effective: ThinkingLevel,
    /// Whether this model's thinking can be controlled at all.
    pub access: UiThinkingAccess,
    /// The levels worth offering on this model: one entry per level that
    /// actually differs, `auto` first. Empty when `access` is not adjustable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<ThinkingLevel>,
}
