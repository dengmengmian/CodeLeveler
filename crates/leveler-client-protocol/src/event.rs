//! [`RuntimeEvent`] — facts the runtime emits for clients to render.
//!
//! Events cover opened sessions, user messages, assistant text,
//! streaming, coarse agent activity, and turn lifecycle. Streaming is modeled
//! as `Started` → `TextDelta*` → `Completed` so that when true token-level
//! streaming lands in the executor later, clients need no change .

use serde::{Deserialize, Serialize};

use leveler_core::ToolCallId;

use super::approval::{UiApprovalRequest, UiClarificationRequest};
use super::media::AttachmentRef;
use super::progress::{UiCompletionReport, UiDiff, UiPlan, UiVerification};
use super::snapshot::{MessageId, UiCheckpoint, UiMessage, UiSessionSnapshot, UiSessionSummary};

/// Stable `TurnCompletedUnverified.reason` when the turn finished without any
/// VCS-tracked source edits — so clients can show a calm "ended · repo unchanged"
/// marker (analysis/Q&A closeout) instead of an "unverified" delivery warning.
pub const REASON_NO_CODE_CHANGES: &str = "no_code_changes";
/// Stable UI token: work changed files, but the project supplied no applicable
/// automatic verification command. Clients localize the explanatory detail.
pub const REASON_NO_AUTOMATIC_VERIFICATION: &str = "no_automatic_verification";

/// Severity for a transient notification .
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationLevel {
    Info,
    Warning,
    Error,
}

/// An event flowing from the runtime to clients.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeEvent {
    /// The runtime finished booting and is ready for commands.
    RuntimeReady,
    /// A session was opened / its snapshot refreshed.
    SessionOpened { session: UiSessionSnapshot },
    /// Session metadata changed (model/mode/branch) without touching the
    /// transcript — refresh the header only.
    SessionUpdated { session: UiSessionSnapshot },
    /// The runtime needs the user to approve a risky action .
    ApprovalRequested { request: UiApprovalRequest },
    /// The agent is asking the user a clarifying question (spec §35).
    ClarificationRequested { request: UiClarificationRequest },
    /// An imported attachment was processed and stored (spec §39).
    AttachmentAdded { attachment: AttachmentRef },
    /// Importing an attachment failed.
    AttachmentProcessingFailed { error: String },
    /// A user message was appended to the transcript.
    UserMessageAdded { message: UiMessage },
    /// A new assistant message began; deltas will target this id.
    AssistantMessageStarted { message_id: MessageId },
    /// A retry attempt began. Remove the prior transient message, if present,
    /// and clear its reasoning before applying new deltas.
    AssistantAttemptReset { message_id: Option<MessageId> },
    /// A chunk of assistant text for an in-flight message.
    AssistantTextDelta {
        message_id: MessageId,
        delta: String,
    },
    /// A chunk of model reasoning/summary, rendered separately from the answer.
    ReasoningDelta { delta: String },
    /// The assistant message is complete.
    AssistantMessageCompleted { message_id: MessageId },
    /// Coarse progress label from the runtime, shown in the status line.
    AgentActivity { label: String },
    /// Project behavior constraints loaded for this turn. Sources are
    /// workspace-relative paths; instruction contents never enter UI chrome.
    ProjectRulesLoaded { sources: Vec<String> },
    /// A tool call started .
    ToolCallStarted {
        id: ToolCallId,
        name: String,
        /// Compacted JSON arguments.
        arguments: String,
        /// True when this call ran in the concurrent read-only batch; a UI can
        /// group such calls as one parallel burst.
        #[serde(default)]
        parallel: bool,
    },
    /// A tool call finished. `preview` is the runtime's truncated output;
    /// `duration_ms` is measured client-side.
    ToolCallCompleted {
        id: ToolCallId,
        ok: bool,
        preview: String,
        duration_ms: u64,
    },
    /// The execution plan was created or a step's status changed (spec §20).
    PlanUpdated { plan: UiPlan },
    /// Verification progress: a check finished or the run concluded (spec §22).
    VerificationUpdated { verification: UiVerification },
    /// The working-tree diff was (re)computed (spec §21).
    DiffUpdated { diff: UiDiff },
    /// A conversation checkpoint was created (spec §68).
    CheckpointCreated { checkpoint: UiCheckpoint },
    /// The list of stored sessions (spec §52).
    SessionList { sessions: Vec<UiSessionSummary> },
    /// Context package info from an orchestrated run (spec §53).
    ContextUpdated {
        candidate_files: Vec<String>,
        estimated_tokens: u32,
    },
    /// Real token usage reported by the model for the latest request. The
    /// context gauge tracks how full the window is; `input_tokens` already
    /// includes the whole prompt (system + history + tools), so the window in
    /// use is `input_tokens + output_tokens`.
    TokenUsage {
        input_tokens: u32,
        output_tokens: u32,
        /// Subset of `input_tokens` the provider served from its prefix cache.
        /// Zero when the provider reports no cache stats.
        cached_input_tokens: u32,
    },
    /// An orchestrated run completed; carries the summary report (spec §23).
    SessionCompleted { report: UiCompletionReport },
    /// The current turn finished successfully.
    TurnCompleted,
    /// The assistant naturally finished its answer, without claiming that an
    /// external task was independently verified as complete.
    TurnAnswered,
    /// The turn stopped at an output limit even after bounded continuation.
    TurnTruncated { error: String },
    /// The executor stopped cleanly but did not reach a successful terminal
    /// state (for example, budget exhaustion or an unresolved goal).
    TurnIncomplete { reason: String },
    /// The turn finished its work, but leveler could not independently verify
    /// it (no verification gate produced passing evidence). Done, not verified —
    /// distinct from `TurnIncomplete` (which means the work did not finish).
    TurnCompletedUnverified { reason: String },
    /// The current turn failed.
    TurnFailed { error: String },
    /// The current turn was cancelled (resumable).
    TurnCancelled,
    /// A spawned sub-agent started or finished (multi-agent delegation). One
    /// block per agent id, updated in place from running → done.
    SubAgentUpdated {
        id: String,
        nickname: String,
        role: String,
        /// false while running; true once the agent finished.
        done: bool,
        /// Whether it finished successfully (only meaningful when `done`).
        ok: bool,
        /// The task while running; a short result summary once done.
        detail: String,
    },
    /// Live execution state and cumulative model usage for one spawned agent.
    SubAgentProgress {
        id: String,
        active: bool,
        input_tokens: u32,
        output_tokens: u32,
        cached_input_tokens: u32,
    },
    /// A transient notification for the status line.
    Notification {
        level: NotificationLevel,
        message: String,
    },
    /// A background process task was started (`run_command` background=true).
    BackgroundTaskStarted {
        task_id: String,
        program: String,
        args: Vec<String>,
    },
    /// A background task finished (exit or kill).
    BackgroundTaskExited {
        task_id: String,
        exit_code: Option<i32>,
        duration_ms: u64,
        ok: bool,
    },
    /// Project memory listing (response to [`crate::ClientCommand::ListMemory`]).
    MemoryList {
        memory_dir: String,
        active: Vec<UiMemoryEntry>,
        archived: Vec<UiMemoryEntry>,
    },
    /// Side-question (`/btw`) started; not persisted to session history.
    BtwStarted { question: String },
    /// Side-question answer chunk (often one full answer in MVP).
    BtwTextDelta { delta: String },
    /// Side-question finished successfully.
    BtwCompleted,
    /// Side-question failed.
    BtwFailed { error: String },
    /// Coarse turn-progress / closeout signal (additive; protocol minor ≥ 1.2).
    ///
    /// No free-form paths or tool output — safe to surface in TUI chrome and
    /// optional remote summaries. Unknown older clients that reject new
    /// variants should skip events via [`crate::event::parse_runtime_event`].
    TurnProgress {
        /// Active | Closing | Terminal (and similar host phase labels).
        phase: String,
        closing: bool,
        no_progress_streak: u32,
        closeout_deny_rounds: u32,
    },
}

/// Deserialize a runtime event, treating **unknown** `type` tags as
/// `Ok(None)` so a newer runtime can emit additive variants without crashing
/// an older client that still speaks the same major protocol version.
pub fn parse_runtime_event(json: &str) -> Result<Option<RuntimeEvent>, serde_json::Error> {
    let value: serde_json::Value = serde_json::from_str(json)?;
    match serde_json::from_value::<RuntimeEvent>(value.clone()) {
        Ok(event) => Ok(Some(event)),
        Err(err) => {
            // Unknown variant typically surfaces as "unknown variant `…`".
            let msg = err.to_string();
            if msg.contains("unknown variant") {
                Ok(None)
            } else {
                Err(err)
            }
        }
    }
}

/// Compact durable-memory row for TUI list surfaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiMemoryEntry {
    pub id: String,
    pub title: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::{PlanStepStatus, UiCheck, UiDiff, UiDiffFile, UiPlan, UiPlanStep};
    use crate::snapshot::{
        MessageId, UiCheckpoint, UiMessage, UiRole, UiSessionSnapshot, UiSessionSummary,
    };
    use crate::{
        ApprovalId, AttachmentId, AttachmentKind, ClarificationId, ModelRef, SessionId, ToolCallId,
    };
    use crate::{UiApprovalRequest, UiClarificationRequest};

    fn session_snapshot() -> UiSessionSnapshot {
        UiSessionSnapshot {
            id: SessionId::new("sess-1"),
            repository: "repo".to_string(),
            goal: "goal".to_string(),
            model: Some(ModelRef::new("openai", "gpt-4o")),
            mode: crate::PermissionProfile::Assisted,
            branch: Some("main".to_string()),
            status: "busy".to_string(),
            messages: vec![UiMessage {
                id: MessageId::new("m1"),
                role: UiRole::User,
                text: "hi".to_string(),
            }],
            pending_interactions: vec![],
            available_models: vec![ModelRef::new("openai", "gpt-4o-mini")],
            vision: false,
            last_sequence: Some(7),
            active_tools: Vec::new(),
            plan: None,
            verification: None,
            diff: None,
            checkpoints: Vec::new(),
            completion_report: None,
        }
    }

    #[test]
    fn turn_progress_roundtrips() {
        roundtrip(
            RuntimeEvent::TurnProgress {
                phase: "closing".into(),
                closing: true,
                no_progress_streak: 2,
                closeout_deny_rounds: 1,
            },
            "turn_progress",
        );
    }

    #[test]
    fn parse_runtime_event_skips_unknown_variant() {
        let json = r#"{"type":"future_only_signal","payload":{"x":1}}"#;
        // Untagged missing content form used by some peers:
        let json2 = r#"{"type":"totally_unknown_event_v99"}"#;
        assert!(parse_runtime_event(json2).ok().flatten().is_none());
        // Malformed still errors:
        assert!(parse_runtime_event("{").is_err());
        let _ = json; // keep for doc; unknown with extra fields also skipped
        assert!(
            parse_runtime_event(r#"{"type":"not_a_real_runtime_event","foo":1}"#)
                .unwrap()
                .is_none()
        );
    }

    fn roundtrip(event: RuntimeEvent, expected_type: &str) {
        let json = serde_json::to_string(&event).unwrap();
        assert!(
            json.contains(&format!("\"type\":\"{expected_type}\"")),
            "json did not contain expected type: {json}"
        );
        let back: RuntimeEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, event);
    }

    #[test]
    fn runtime_ready_roundtrips() {
        roundtrip(RuntimeEvent::RuntimeReady, "runtime_ready");
    }

    #[test]
    fn session_opened_roundtrips() {
        roundtrip(
            RuntimeEvent::SessionOpened {
                session: session_snapshot(),
            },
            "session_opened",
        );
    }

    #[test]
    fn session_updated_roundtrips() {
        roundtrip(
            RuntimeEvent::SessionUpdated {
                session: session_snapshot(),
            },
            "session_updated",
        );
    }

    #[test]
    fn approval_requested_roundtrips() {
        roundtrip(
            RuntimeEvent::ApprovalRequested {
                request: UiApprovalRequest {
                    id: ApprovalId::new("a1"),
                    tool: "run_command".to_string(),
                    summary: "run ls".to_string(),
                    command: Some("ls".to_string()),
                    risks: vec!["network".to_string()],
                },
            },
            "approval_requested",
        );
    }

    #[test]
    fn clarification_requested_roundtrips() {
        roundtrip(
            RuntimeEvent::ClarificationRequested {
                request: UiClarificationRequest {
                    id: ClarificationId::new("c1"),
                    question: "which file?".to_string(),
                    options: vec!["a.rs".to_string()],
                },
            },
            "clarification_requested",
        );
    }

    #[test]
    fn attachment_added_roundtrips() {
        roundtrip(
            RuntimeEvent::AttachmentAdded {
                attachment: AttachmentRef {
                    id: AttachmentId::new("att-1"),
                    kind: AttachmentKind::Image,
                    name: "img.png".to_string(),
                    mime_type: "image/png".to_string(),
                    size_bytes: 1024,
                    sha256: "deadbeef".to_string(),
                    width: Some(100),
                    height: None,
                },
            },
            "attachment_added",
        );
    }

    #[test]
    fn attachment_processing_failed_roundtrips() {
        roundtrip(
            RuntimeEvent::AttachmentProcessingFailed {
                error: "bad mime".to_string(),
            },
            "attachment_processing_failed",
        );
    }

    #[test]
    fn user_message_added_roundtrips() {
        roundtrip(
            RuntimeEvent::UserMessageAdded {
                message: UiMessage {
                    id: MessageId::new("m1"),
                    role: UiRole::User,
                    text: "hello".to_string(),
                },
            },
            "user_message_added",
        );
    }

    #[test]
    fn assistant_message_started_roundtrips() {
        roundtrip(
            RuntimeEvent::AssistantMessageStarted {
                message_id: MessageId::new("m1"),
            },
            "assistant_message_started",
        );
    }

    #[test]
    fn assistant_text_delta_roundtrips() {
        roundtrip(
            RuntimeEvent::AssistantTextDelta {
                message_id: MessageId::new("m1"),
                delta: "world".to_string(),
            },
            "assistant_text_delta",
        );
    }

    #[test]
    fn assistant_message_completed_roundtrips() {
        roundtrip(
            RuntimeEvent::AssistantMessageCompleted {
                message_id: MessageId::new("m1"),
            },
            "assistant_message_completed",
        );
    }

    #[test]
    fn agent_activity_roundtrips() {
        roundtrip(
            RuntimeEvent::AgentActivity {
                label: "thinking".to_string(),
            },
            "agent_activity",
        );
    }

    #[test]
    fn tool_call_started_roundtrips() {
        roundtrip(
            RuntimeEvent::ToolCallStarted {
                id: ToolCallId::new("tc1"),
                name: "read".to_string(),
                arguments: "{}".to_string(),
                parallel: false,
            },
            "tool_call_started",
        );
    }

    #[test]
    fn tool_call_completed_roundtrips() {
        roundtrip(
            RuntimeEvent::ToolCallCompleted {
                id: ToolCallId::new("tc1"),
                ok: true,
                preview: "ok".to_string(),
                duration_ms: 42,
            },
            "tool_call_completed",
        );
    }

    #[test]
    fn plan_updated_roundtrips() {
        roundtrip(
            RuntimeEvent::PlanUpdated {
                plan: UiPlan {
                    steps: vec![UiPlanStep {
                        index: 0,
                        description: "step".to_string(),
                        status: PlanStepStatus::Running,
                    }],
                },
            },
            "plan_updated",
        );
    }

    #[test]
    fn verification_updated_roundtrips() {
        roundtrip(
            RuntimeEvent::VerificationUpdated {
                verification: crate::progress::UiVerification {
                    checks: vec![UiCheck {
                        name: "fmt".to_string(),
                        status: crate::progress::CheckState::Passed,
                        evidence: None,
                    }],
                    passed: Some(true),
                },
            },
            "verification_updated",
        );
    }

    #[test]
    fn diff_updated_roundtrips() {
        roundtrip(
            RuntimeEvent::DiffUpdated {
                diff: UiDiff {
                    files: vec![UiDiffFile {
                        path: "a.rs".to_string(),
                        added: 1,
                        removed: 0,
                        patch: None,
                    }],
                },
            },
            "diff_updated",
        );
    }

    #[test]
    fn checkpoint_created_roundtrips() {
        roundtrip(
            RuntimeEvent::CheckpointCreated {
                checkpoint: UiCheckpoint {
                    id: leveler_core::CheckpointId::new("chk1"),
                    label: "start".to_string(),
                    ordinal: 0,
                },
            },
            "checkpoint_created",
        );
    }

    #[test]
    fn session_list_roundtrips() {
        roundtrip(
            RuntimeEvent::SessionList {
                sessions: vec![UiSessionSummary {
                    id: SessionId::new("s1"),
                    goal: "g".to_string(),
                    status: "done".to_string(),
                    model: "openai/gpt-4o".to_string(),
                    updated_at: "now".to_string(),
                    repository: None,
                }],
            },
            "session_list",
        );
    }

    #[test]
    fn context_updated_roundtrips() {
        roundtrip(
            RuntimeEvent::ContextUpdated {
                candidate_files: vec!["a.rs".to_string()],
                estimated_tokens: 1234,
            },
            "context_updated",
        );
    }

    #[test]
    fn token_usage_roundtrips() {
        roundtrip(
            RuntimeEvent::TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                cached_input_tokens: 40,
            },
            "token_usage",
        );
    }

    #[test]
    fn session_completed_roundtrips() {
        roundtrip(
            RuntimeEvent::SessionCompleted {
                report: crate::progress::UiCompletionReport {
                    files_changed: 1,
                    added: 2,
                    removed: 3,
                    checks_passed: 4,
                    checks_total: 5,
                    success: true,
                },
            },
            "session_completed",
        );
    }

    #[test]
    fn turn_completed_roundtrips() {
        roundtrip(RuntimeEvent::TurnCompleted, "turn_completed");
    }

    #[test]
    fn distinct_non_completion_end_states_roundtrip() {
        roundtrip(RuntimeEvent::TurnAnswered, "turn_answered");
        roundtrip(
            RuntimeEvent::TurnTruncated {
                error: "token limit".to_string(),
            },
            "turn_truncated",
        );
        roundtrip(
            RuntimeEvent::TurnIncomplete {
                reason: "round budget".to_string(),
            },
            "turn_incomplete",
        );
        roundtrip(
            RuntimeEvent::TurnCompletedUnverified {
                reason: "no verification gate".to_string(),
            },
            "turn_completed_unverified",
        );
    }

    #[test]
    fn turn_failed_roundtrips() {
        roundtrip(
            RuntimeEvent::TurnFailed {
                error: "oops".to_string(),
            },
            "turn_failed",
        );
    }

    #[test]
    fn turn_cancelled_roundtrips() {
        roundtrip(RuntimeEvent::TurnCancelled, "turn_cancelled");
        roundtrip(
            RuntimeEvent::BackgroundTaskStarted {
                task_id: "bg-1".into(),
                program: "sleep".into(),
                args: vec!["1".into()],
            },
            "background_task_started",
        );
        roundtrip(
            RuntimeEvent::BackgroundTaskExited {
                task_id: "bg-1".into(),
                exit_code: Some(0),
                duration_ms: 12,
                ok: true,
            },
            "background_task_exited",
        );
    }

    #[test]
    fn sub_agent_progress_is_a_distinct_runtime_event() {
        let json = serde_json::json!({
            "type": "sub_agent_progress",
            "id": "agent-2",
            "active": true,
            "input_tokens": 2400,
            "output_tokens": 180,
            "cached_input_tokens": 1200
        });
        let event = serde_json::from_value::<RuntimeEvent>(json);
        assert!(
            event.is_ok(),
            "sub-agent usage must not reuse global TokenUsage"
        );
    }

    #[test]
    fn notification_roundtrips() {
        for level in [
            NotificationLevel::Info,
            NotificationLevel::Warning,
            NotificationLevel::Error,
        ] {
            roundtrip(
                RuntimeEvent::Notification {
                    level,
                    message: "hello".to_string(),
                },
                "notification",
            );
        }
    }
}
