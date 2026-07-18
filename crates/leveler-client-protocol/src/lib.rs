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
//! verification, approvals, attachments, multi-agent) — new variants only, so
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

mod approval;
mod client;
mod command;
mod command_envelope;
mod event;
mod media;
mod progress;
mod snapshot;
mod version;
mod wire_types;

pub use approval::{UiApprovalRequest, UiClarificationRequest, UiPendingInteraction};
pub use client::{ClientError, InteractiveRuntimeClient};
pub use command::ClientCommand;
pub use command_envelope::{
    CommandEnvelope, CommandReceipts, Receipt, Recovery, recovery_for_tool,
};
pub use event::{
    NotificationLevel, REASON_NO_AUTOMATIC_VERIFICATION, REASON_NO_CODE_CHANGES, RuntimeEvent,
    UiMemoryEntry, parse_runtime_event,
};
pub use media::{AttachmentId, AttachmentKind, AttachmentRef};
pub use progress::{
    CheckState, PlanStepStatus, UiCheck, UiCompletionReport, UiDiff, UiDiffFile, UiPlan,
    UiPlanStep, UiVerification,
};
pub use snapshot::{
    MessageId, RuntimeStatus, UiActiveToolCall, UiCheckpoint, UiMessage, UiRole, UiSessionSnapshot,
    UiSessionSummary,
};
pub use version::{PROTOCOL_VERSION, ProtocolEnvelope, ProtocolError, ProtocolVersion};
pub use wire_types::{ApprovalDecision, PermissionProfile};

#[cfg(feature = "testing")]
pub mod mock;

// Re-export the shared domain types the protocol references, so clients get one
// import surface and never generate provider-specific formats themselves.
pub use leveler_core::{
    ApprovalId, CheckpointId, ClarificationId, CommandId, SessionId, ToolCallId,
};
pub use leveler_model::ModelRef;

/// Prefix stamped on the message that replaces compacted history, so clients can
/// render it as a distinct "history summary" block rather than a user message.
pub const COMPACTION_SUMMARY_PREFIX: &str = "对话摘要（已压缩历史）";
