//! `leveler-client-protocol` — the stable message contract between UI clients
//! (TUI today; Desktop / VS Code / Web / SDK later) and the CodeLeveler runtime.
//!
//! A client speaks two languages:
//!
//! - [`ClientCommand`] — intents sent *into* the runtime (submit a message,
//!   cancel a turn, quit).
//! - [`RuntimeEvent`] — facts flowing *out* of the runtime (a message was added,
//!   assistant text streamed in, a turn finished).
//!
//! The transport is the [`InteractiveRuntimeClient`] trait: `send` a command,
//! `subscribe` to the event stream, `snapshot` the current session. UI code
//! depends only on this crate — never on the concrete runtime, providers,
//! tools, or storage .
//!
//! Defines the protocol used by terminal clients
//! shell (multi-turn text, streaming). Later phases grow the enums (plan, diff,
//! approvals, attachments, multi-agent) — new variants only, so
//! existing clients keep compiling.
//!
//! ## Versioning, resync & data classification (M6)
//!
//! - **Versioning**: [`ProtocolEnvelope`] carries a [`ProtocolVersion`]. Same
//!   major = compatible (minor is additive); an unknown major is rejected, not
//!   mis-parsed. The in-process client skips the envelope; a future
//!   local-socket/cloud transport uses it. Golden serde fixtures pin the wire
//!   shape within a major.
//! - **Resync**: [`UiSessionSnapshot::last_sequence`] is the anchor. A client
//!   that fell behind (broadcast lag, reconnect) takes a fresh `snapshot()` and
//!   resumes the event stream *after* that sequence — no double-apply, no gap.
//!   Transient deltas are never re-sent (they carry no replay value); a
//!   canonical completion is always recoverable from the snapshot.
//! - **Data classification**: a public/syncable boundary accepts only the
//!   explicit `EngineEvent::public_projection()` DTO from `leveler-engine`.
//!   `DataClass` is audit metadata, not authorization; source, model output,
//!   commands, paths, prompts, and full context stay local.
#![forbid(unsafe_code)]

mod agents;
mod approval;
mod client;
mod command;
mod command_envelope;
mod contribution;
mod event;
mod failure;
mod foreign;
mod global_tasks;
mod goal_command;
mod goals;
mod media;
mod observability;
mod origin;
mod progress;
mod retirement;
/// Structured scheduled-automation management shared by every client.
pub mod schedule;
/// WebSocket framing for a session: the shapes every session client speaks.
pub mod session_wire;
mod skills;
mod snapshot;
mod terminal;
/// Durable usage reporting: the runtime's own model-call ledger, projected.
mod usage;
mod version;
pub use terminal::{TerminalOperation, TerminalRequest, TerminalState};
mod wire_types;

pub use agents::{
    UiAgentCapability, UiAgentDetail, UiAgentDraft, UiAgentEntry, UiAgentProblem, UiAgentScope,
    UiAgentSource, UiAgentStatus, UiChildAgentIdentity, UiShadowedAgent,
};
pub use approval::{
    ClarificationQuestionKind, UiApprovalRequest, UiClarificationQuestion, UiClarificationRequest,
    UiPendingInteraction,
};
pub use client::{ClientError, InteractiveRuntimeClient};
pub use command::{ClientCommand, RestartReason};
pub use command_envelope::{CommandEnvelope, CommandReceipts, Receipt};
pub use contribution::{CONTRIBUTION_FINDINGS_MAX, UiChildContribution, UiFinding};
pub use failure::{
    FailureCategory, FailureDelivery, FailureRetryability, FailureSource, UiFailure,
};
pub use foreign::{UiForeignOrigin, UiForeignSession, UiForeignTool};
pub use goal_command::{GoalCommand, parse_goal_command};
pub use skills::{
    UiShadowedSkill, UiSkillDetail, UiSkillEntry, UiSkillProblem, UiSkillScope, UiSkillSource,
    UiSkillStatus,
};

pub use event::{
    ChildContribution, ChildLimit, ChildOutcome, ChildStop, NotificationLevel, RecoveryStop,
    RuntimeEvent, UiChildAgent, UiChildState, UiCommandStop, UiHistoryEntry, UiTurnUsage,
    parse_runtime_event,
};
pub use global_tasks::{
    UiGlobalTaskIndex, UiGlobalTaskSourceError, UiGlobalTaskSourceErrorKind, UiGlobalTaskSummary,
    UiTaskStatus, UiTaskTerminal,
};
pub use goals::{UiGoalRecap, UiUnfinishedGoal};
pub use media::{AttachmentId, AttachmentKind, AttachmentRef};
pub use observability::{
    OBSERVABILITY_REQUESTS_MAX, OBSERVABILITY_WINDOW_MAX, ObservationClass, UiAgentObservation,
    UiEventRelation, UiLaneAccounting, UiObservabilityLoaded, UiObservationField, UiObservationRow,
    UiRecoveryObservation, UiRequestObservation, UiSessionObservation, UiToolAggregate,
    classify_tool,
};
pub use origin::ClientOrigin;
pub use progress::{
    FinalizationStage, PlanStepStatus, UiCompletionReport, UiDiff, UiDiffFile, UiPlan, UiPlanState,
    UiPlanStep,
};
pub use retirement::{RetireDecision, RetireRequest};
pub use session_wire::{DownstreamMessage, ProjectStatus, UpstreamMessage};
pub use snapshot::{
    MessageId, RuntimeHealth, RuntimeInfo, RuntimeStatus, STALE_TURN_WARN_AFTER,
    UiActiveBackgroundTask, UiActiveToolCall, UiBackgroundTaskBlocker, UiBackgroundTaskRecord,
    UiBackgroundTaskState, UiCheckpoint, UiExecutionTask, UiInputState, UiInterruptedCallState,
    UiInterruptedToolCall, UiMessage, UiMessageKind, UiRole, UiSessionHistory, UiSessionSnapshot,
    UiSessionSummary, UiTaskDeclaration, UiThinkingAccess, UiThinkingState, UiToolTiming,
    UiTurnBlocker, UiUserShell,
};
pub use usage::{UiModelUsage, UiUsageReport, UiUsageScope};
pub use version::{
    PROTOCOL_VERSION, ProtocolCapability, ProtocolEnvelope, ProtocolError, ProtocolVersion,
};
pub use wire_types::{AnswerEffect, ApprovalDecision, ApprovalPolicy, PermissionProfile};

#[cfg(feature = "testing")]
pub mod mock;

// Re-export the shared domain types the protocol references, so clients get one
// import surface and never generate provider-specific formats themselves.
pub use leveler_core::{
    ApprovalId, BuildIdentity, CheckpointId, ClarificationId, CommandId, RuntimeId, SessionId,
    ToolCallId, UserShellId,
};
pub use leveler_model::{ModelRef, ThinkingLevel};

/// Prefix stamped on the message that replaces compacted history, so clients can
/// render it as a distinct "history summary" block rather than a user message.
pub const COMPACTION_SUMMARY_PREFIX: &str = "对话摘要（已压缩历史）";

/// Prefix of the engine's compaction breadcrumb. A context fold replaces the
/// elided history with a message that begins with this marker; the `/compact`
/// summary (`COMPACTION_SUMMARY_PREFIX`) may follow it.
pub const COMPACTION_BREADCRUMB_PREFIX: &str = "[Earlier context was compacted";

/// Whether a message is a compaction summary rather than something the user
/// typed. Both shapes count: the `/compact` summary and the engine breadcrumb
/// (which may wrap a generated summary). Clients use one predicate so a fold
/// never reappears as a conversation row.
pub fn is_compaction_summary_text(text: &str) -> bool {
    let text = text.trim_start();
    text.starts_with(COMPACTION_SUMMARY_PREFIX) || text.starts_with(COMPACTION_BREADCRUMB_PREFIX)
}

/// Authoritative wire-tag policy for snapshot versioned replacements.
pub use command::SNAPSHOT_VERSIONED_COMMANDS;

#[cfg(test)]
mod compaction_summary_tests {
    use super::{
        COMPACTION_BREADCRUMB_PREFIX, COMPACTION_SUMMARY_PREFIX, is_compaction_summary_text,
    };

    #[test]
    fn both_compaction_shapes_are_recognised() {
        assert!(is_compaction_summary_text(&format!(
            "{COMPACTION_SUMMARY_PREFIX}：\nfolded"
        )));
        assert!(is_compaction_summary_text(&format!(
            "{COMPACTION_BREADCRUMB_PREFIX} to fit the window: 2 steps elided.]\n\n\
             {COMPACTION_SUMMARY_PREFIX}：\nfolded"
        )));
        assert!(!is_compaction_summary_text("please fix the parser"));
        assert!(!is_compaction_summary_text(&format!(
            "note: {COMPACTION_SUMMARY_PREFIX} appears in this sentence"
        )));
    }
}
