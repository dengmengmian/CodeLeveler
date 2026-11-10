//! [`RuntimeEvent`] — facts the runtime emits for clients to render.
//!
//! Events cover opened sessions, user messages, assistant text,
//! streaming, coarse agent activity, and turn lifecycle. Streaming is modeled
//! as `Started` → `TextDelta*` → `Completed` so that when true token-level
//! streaming lands in the executor later, clients need no change .

use serde::{Deserialize, Serialize};

use leveler_core::{ApprovalId, ClarificationId, CommandId, ToolCallId};

use super::approval::{UiApprovalRequest, UiClarificationRequest};
use super::media::AttachmentRef;
use super::progress::{FinalizationStage, UiCompletionReport, UiDiff, UiPlan};
use super::snapshot::{
    MessageId, UiCheckpoint, UiInputState, UiMessage, UiSessionSnapshot, UiSessionSummary,
};
use super::wire_types::AnswerEffect;

/// Severity for a transient notification .
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum NotificationLevel {
    Info,
    Warning,
    Error,
}

/// One finished turn's provider usage, projected from the runtime's durable
/// `model_requests` rows.
///
/// This is a settled fact, not a running sum of streamed snapshots: re-reading
/// it after a reconnect, a resume or a replay yields the same figures.
/// `unmeasured_requests` keeps a call whose provider reported no usage
/// distinguishable from a measured zero.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiTurnUsage {
    /// Provider calls in the turn, retries included — each is a billed call.
    pub requests: u32,
    /// Calls whose provider reported no usage at all.
    pub unmeasured_requests: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Subset of `input_tokens`; `None` when no row recorded it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
}

impl UiTurnUsage {
    /// `input + output`. Cached input is a slice of the prompt, never an
    /// addition to the total.
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }
}
/// The four-way reading of a settled child's result. "Finished with nothing
/// to flag" and "stopped with nothing to show" are opposite facts; a client
/// renders them from this field, never from the summary text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ChildOutcome {
    CompletedWithFindings,
    CompletedNoFindings,
    IncompletePartial,
    IncompleteNoResult,
}

/// How a settled child's activation ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ChildStop {
    Completed,
    Incomplete,
    Budget,
    Cancelled,
    Failed,
    Lost,
}

/// Which bound stopped a child whose stop is [`ChildStop::Budget`]. A mirror
/// of the runtime's typed limit, not a second reason vocabulary: without it a
/// client can only say "budget", which renders a wall-clock timeout and a
/// spent token budget the same. `Duration` is the wall clock — the bound the
/// runtime's child cap enforces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ChildLimit {
    Duration,
    ModelTokens,
    Cost,
    Commands,
    ModifiedFiles,
    /// The child's own round window.
    RoundWindow,
    /// The absolute round ceiling.
    RoundCeiling,
}

/// What one child contributed, as counts plus its capability contract.
///
/// A flat mirror of the runtime's projection rather than the runtime type
/// itself: this crate is the stable wire, so an internal refactor of the
/// ledger must not change what clients parse.
///
/// `findings_total` is a count, not a score: it says how much this child
/// reported, never whether any of it mattered. What the parent did about it
/// is in the transcript, where the parent said it.
/// Where a delegated child's lifecycle stands, as the runtime records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum UiChildState {
    /// An activation is live.
    Running,
    /// Its activation died with a runtime window; the next turn of the session
    /// continues it or settles it as lost.
    Interrupted,
    /// It has its one terminal.
    Settled,
}

/// One delegated child, projected from the durable record — what a client
/// that reconnects, or opens a session, renders without having seen a single
/// live event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiChildAgent {
    pub id: String,
    pub nickname: String,
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    #[serde(default)]
    pub read_only: bool,
    /// The declarative agent it was spawned from, when it was. `None` for a
    /// built-in role spawn and for children recorded before agents existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<crate::UiChildAgentIdentity>,
    /// The child's short task title, fixed at spawn: its identity, distinct
    /// from the full instructions in `purpose`. `None` on children recorded
    /// before titles existed; a presentation fallback may project `purpose`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// What it was asked to do.
    pub purpose: String,
    pub state: UiChildState,
    /// Whether it reached the end of its task, once settled. Carried beside
    /// `outcome` because rows settled before the outcome was typed have only
    /// this bit.
    #[serde(default)]
    pub ok: bool,
    /// Whether its parent continued while it ran.
    #[serde(default)]
    pub background: bool,
    /// Exclusive write scope fixed at spawn (empty when late-bound or read-only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scope: Vec<String>,
    /// How many times it was continued after an interruption.
    #[serde(default)]
    pub resumes: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<ChildOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<ChildStop>,
    /// Which bound fired when `stop` is [`ChildStop::Budget`]. `None` for
    /// every other stop and for terminals recorded before it was carried.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<ChildLimit>,
    /// The recorded settlement summary, once settled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Model usage recorded under this child's id.
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    /// `None` when no call carried a price — unknown, not zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd_micros: Option<u64>,
}

/// One fact of a past turn, as the live stream carried it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiHistoryEntry {
    /// Milliseconds from the start of the turn this entry belongs to, taken
    /// from the durable record times — a replay has no live clock.
    pub turn_elapsed_ms: u64,
    /// The first entry of a turn.
    #[serde(default)]
    pub turn_start: bool,
    pub event: RuntimeEvent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ChildContribution {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_role: Option<String>,
    /// Whether this child held a physically read-only toolset. It used to be
    /// a list of semantic capability labels; what a client needs is the
    /// structural bound.
    #[serde(default)]
    pub read_only: bool,
    pub findings_total: u32,
}

impl ChildContribution {
    /// The child ran and reported nothing. A real answer, not an empty state.
    pub fn reported_nothing(&self) -> bool {
        self.findings_total == 0
    }
}

/// How a stopped command call ended, as the runtime established it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum UiCommandStop {
    /// The whole process tree was confirmed gone.
    Confirmed,
    /// The tree was signalled, but its termination could not be confirmed.
    Unconfirmed,
}

/// The interrupted call a recovery stop refuses to guess about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RecoveryStop {
    pub call_id: String,
    pub tool: String,
}

/// An event flowing from the runtime to clients.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeEvent {
    GlobalTasksLoaded {
        requester_session_id: crate::SessionId,
        query_id: String,
        index: crate::UiGlobalTaskIndex,
    },
    /// The answer to [`crate::ClientCommand::ManageSchedule`]. `response`
    /// carries domain failures too; a transport failure is the only case where
    /// no answer arrives.
    ScheduleManaged {
        requester_session_id: crate::SessionId,
        query_id: crate::CommandId,
        response: Box<crate::schedule::ScheduleResponse>,
    },
    /// The runtime finished booting and is ready for commands.
    RuntimeReady,
    /// The build this client's stream is connected to, observed when the
    /// stream is first established. A long-lived runtime can outlive an
    /// application update, so the client must learn at connect time whether
    /// it is talking to the build it was installed with. Like
    /// [`RuntimeEvent::RuntimeReconnected`], an unknown identity proves
    /// nothing and is never a version change.
    RuntimeBuildObserved {
        #[serde(default)]
        runtime_build: leveler_core::BuildIdentity,
    },
    /// The client's event stream dropped and was re-established.
    ///
    /// Emitted by the local transport, never by a runtime: it states only what
    /// the client observed — that it is connected again, and to which build.
    /// Whether the user must hear that work may have stopped, or that a
    /// restart is needed to switch versions, is a presentation decision; the
    /// transport cannot know either fact.
    ///
    /// A runtime built before [`leveler_core::BuildIdentity`] existed reports
    /// the default identity, which is UNKNOWN. Unknown is not a version
    /// change and must never be presented as one.
    RuntimeReconnected {
        #[serde(default)]
        runtime_build: leveler_core::BuildIdentity,
        /// Whether the runtime PROCESS changed across the break, when both
        /// sides of it were observed: `Some(true)` a restart, `Some(false)`
        /// the same process, `None` nothing proven. A stream reset on a live
        /// runtime is `Some(false)` and cost no work.
        #[serde(default)]
        runtime_restarted: Option<bool>,
    },
    /// A reconnect re-established the stream but could not re-read the
    /// session. The client's view may be pre-break state; it must say so
    /// rather than present stale facts as current.
    SessionResyncFailed {
        /// The load/snapshot failure, unformatted. Presentation owns wording.
        error: String,
    },
    /// The user acknowledged this session's crash window. `closed` interrupted
    /// tool calls were marked "outcome unknown, not replayed"; zero means
    /// there was nothing left to close (a retry or a concurrent
    /// acknowledgement). This states what the runtime wrote, not that any
    /// interrupted operation succeeded.
    CrashWindowAcknowledged {
        session_id: leveler_core::SessionId,
        closed: u32,
    },
    /// A session was opened / its snapshot refreshed.
    SessionOpened { session: UiSessionSnapshot },
    /// Session metadata changed (model/mode/branch) without touching the
    /// transcript — refresh the header only.
    SessionUpdated { session: UiSessionSnapshot },
    /// The runtime needs the user to approve a risky action .
    ApprovalRequested { request: UiApprovalRequest },
    /// A pending approval was resolved (by any connected client, or by a
    /// timeout/cancel). Clients dismiss the matching prompt so a second client
    /// never answers an approval that no longer exists.
    ApprovalResolved { id: ApprovalId },
    /// The agent is asking the user a clarifying question (spec §35).
    ClarificationRequested { request: UiClarificationRequest },
    /// A pending clarification was resolved (by any client, timeout, or cancel).
    ClarificationResolved { id: ClarificationId },
    /// An imported attachment was processed and stored (spec §39).
    AttachmentAdded {
        attachment: AttachmentRef,
        /// Original delivery identity; absent for legacy raw-send imports.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command_id: Option<CommandId>,
    },
    /// Importing an attachment failed.
    AttachmentProcessingFailed {
        error: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command_id: Option<CommandId>,
    },
    /// A user message was appended to the transcript.
    UserMessageAdded { message: UiMessage },
    /// The runtime accepted a mid-turn input and made it durable, but no model
    /// round has received it yet.
    ///
    /// This is the ACK boundary: unlike [`Self::UserMessageAdded`], it does not
    /// claim a transcript row exists. Every connected client paints the same
    /// row from this event, not from the submitting client's optimism.
    InputAdmitted { message: UiMessage },
    /// A previously admitted input reached a terminal state.
    ///
    /// `Delivered` means its transcript row is durable (and `ordinal` locates
    /// it); `Cancelled` means it never will be. A cancelled input is never
    /// resurrected by a later turn.
    InputSettled {
        message_id: MessageId,
        state: UiInputState,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ordinal: Option<u64>,
    },
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
    /// A model reasoning segment began. Presentation opens ONE mutable
    /// reasoning block on this; it must not append a new transcript item.
    ReasoningStarted,
    /// The current reasoning segment ended cleanly. `elapsed_ms` is the
    /// runtime's own measurement of the segment. Presentation freezes its
    /// mutable block into an immutable history item on this. A segment that
    /// never receives this was interrupted, not completed.
    ReasoningCompleted { elapsed_ms: u64 },
    /// The assistant message is complete.
    AssistantMessageCompleted { message_id: MessageId },
    /// The runtime named the text it accepted as this turn's delivery.
    ///
    /// The text is already on screen as an assistant message. This event does
    /// not append another copy. It tells the client which existing block is
    /// the answer, so a later progress note cannot take that place when the
    /// turn ends. `text` is the runtime's published delivery, compared by
    /// exact trimmed equality — not a guess about length or wording.
    DeliveryCommitted { text: String },
    /// The assistant has produced its final response, while the runtime is
    /// still settling the task before its one authoritative terminal event.
    TurnFinalizing { stage: FinalizationStage },
    /// Coarse progress label from the runtime, shown in the status line.
    AgentActivity { label: String },
    /// Heartbeat while a long command tool runs (runtime observability). Lets a
    /// client show "运行 cargo test" with a live elapsed instead of a bare
    /// "等待模型". Structured so TUI/Web/logs can consume it uniformly.
    CommandProgress { label: String, elapsed_ms: u64 },
    /// A model round is about to retry the same request (transient). Belongs in
    /// ephemeral status, NOT the transcript: a brief network blip must not
    /// spam the conversation. `attempt` is 1-based (the retry about to happen)
    /// and `max_attempts` the retry budget; once `attempt == max_attempts`
    /// fails the turn is surfaced, not hidden behind an unbounded wait.
    ModelRetrying {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
    },
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
        /// The 1-based model step whose response requested this call — the real
        /// execution-round boundary. Every call from one model response carries
        /// the same value, so a UI groups one response's calls together even
        /// when the round produced no visible assistant prose. Legacy events
        /// omit it; a client must then fall back to its old grouping rather
        /// than invent a boundary.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_step: Option<u32>,
        /// Whether this call acts on the turn's answer — the runtime's own
        /// classification, stated once so no surface re-derives it from the
        /// tool name. A peer that omits it is read as
        /// [`AnswerEffect::unstated`].
        ///
        /// [`AnswerEffect::unstated`]: crate::AnswerEffect::unstated
        #[serde(default, skip_serializing_if = "Option::is_none")]
        answer_effect: Option<AnswerEffect>,
    },
    /// An admitted tool reached an actual dispatch boundary. Announcement and
    /// approval wait time are not execution time.
    ToolTimingUpdated {
        id: ToolCallId,
        timing: crate::UiToolTiming,
    },
    /// A tool call finished. `preview` is the runtime's truncated output;
    /// `duration_ms` is measured client-side.
    ToolCallCompleted {
        id: ToolCallId,
        ok: bool,
        preview: String,
        duration_ms: u64,
        /// Canonical unified diff of what an edit ACTUALLY changed, with the
        /// line numbers it landed on, produced by the tool that made it.
        ///
        /// A UI renders an edit from THIS, not from the call's arguments: an
        /// `apply_patch` hunk is located by content and `replace` matches a
        /// substring, so the request never says where the change went. `None`
        /// for every non-edit call, and for an edit whose location could not
        /// be established — the UI must then show no line numbers rather than
        /// invent one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        applied_diff: Option<String>,
        /// Exit code of the process a command call ran, when it exited.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        /// Present only when the call was stopped: whether the runtime
        /// confirmed its whole process tree gone. A client shows "stopped"
        /// for `confirmed` alone; `unconfirmed` is an unknown outcome.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop: Option<UiCommandStop>,
    },
    /// Live output from a running command tool call. `stream` is `stdout` or
    /// `stderr`; `chunk` is one or more whole, sanitized lines. Transient:
    /// clients keep a bounded buffer and the completed preview is the record.
    ToolCallOutput {
        id: ToolCallId,
        stream: String,
        chunk: String,
    },
    /// The execution plan was created or a step's status changed (spec §20).
    PlanUpdated {
        plan: UiPlan,
        /// True when the declaration created the plan, false when it replaced
        /// an existing one. Legacy events omit it and decode as an update,
        /// which is the only presentation their runtime ever had.
        #[serde(default)]
        created: bool,
        /// The tool call that carried the declaration; absent on a resume seed
        /// and on legacy events. Clients use it to label the matching row.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
    },
    /// The working-tree diff changed or was explicitly requested (spec §21).
    DiffUpdated {
        /// Present only for a `RequestDiff` response. `None` is an
        /// authoritative state-change broadcast and is accepted by all clients.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query_id: Option<CommandId>,
        diff: UiDiff,
    },
    /// A requested working-tree diff could not be computed. Distinct from
    /// `DiffUpdated` with zero files, which is a real "no changes" answer:
    /// clients must never present an unreadable workspace as clean.
    DiffFailed {
        /// Present only for a `RequestDiff` response. `None` when the failure
        /// happened during a broadcast refresh.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query_id: Option<CommandId>,
        /// Machine-facing detail (git's own message or the reason the path is
        /// not a work tree). Clients own the localized label.
        message: String,
    },
    /// A conversation checkpoint was created (spec §68).
    CheckpointCreated { checkpoint: UiCheckpoint },
    /// The list of stored sessions (spec §52).
    SessionList { sessions: Vec<UiSessionSummary> },
    /// The locally discovered sessions of another coding agent, newest first.
    /// `error` is present only when discovery itself could not run; an empty
    /// `sessions` list with no error is a real "nothing found".
    ForeignSessionList {
        tool: crate::UiForeignTool,
        sessions: Vec<crate::UiForeignSession>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// A foreign session was imported. `session_id` is the NEW native
    /// CodeLeveler session; the source identity is carried so the client can
    /// keep it visible after the view switches.
    ForeignSessionImported {
        tool: crate::UiForeignTool,
        native_id: String,
        session_id: leveler_core::SessionId,
        /// A source/current-workspace mismatch the person should see before
        /// trusting the continuation. Absent when the directories agree.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        warning: Option<String>,
    },
    /// A foreign session could not be imported. Nothing was created, and the
    /// source file was not modified.
    ForeignSessionImportFailed {
        tool: crate::UiForeignTool,
        native_id: String,
        message: String,
    },
    /// Context package info from an orchestrated run (spec §53).
    ContextUpdated {
        candidate_files: Vec<String>,
        estimated_tokens: u32,
    },
    /// The runtime folded conversation history: `from` transcript messages
    /// became `to`. A stable product fact — clients own the wording and the
    /// locale; the runtime does not send prose for this.
    ContextCompacted { from: u32, to: u32 },
    /// Replay-only. The adaptive-context ladder that climbed the fold
    /// threshold was deleted; the variant survives so an old event log still
    /// decodes, and nothing emits one. Token budgets, not message counts.
    ContextExpanded {
        from_tokens: u32,
        to_tokens: u32,
        reason: String,
    },
    /// A user shell execution (`!command`) started. User-originated direct
    /// host execution — not an agent tool call; clients render it as its own
    /// block and never feed it to the model conversation.
    UserShellStarted {
        execution_id: leveler_core::UserShellId,
        command: String,
        cwd: String,
    },
    /// The same user shell execution moved from the foreground to the
    /// background. It is one process and one execution id; `task_id` links it
    /// to the background task list. Additive; older clients keep showing the
    /// foreground record until it exits.
    UserShellBackgrounded {
        execution_id: leveler_core::UserShellId,
        task_id: String,
    },
    /// Live output from a running user shell. `stream` is `stdout` or
    /// `stderr`. Transient: clients keep a bounded buffer; the runtime does
    /// not persist chunks.
    UserShellOutput {
        execution_id: leveler_core::UserShellId,
        stream: String,
        chunk: String,
    },
    /// The runtime's authoritative retained view of a user shell's output.
    /// It REPLACES the client's accumulated live chunks: the bounded live
    /// channel may have dropped bytes, so appending would preserve a hole.
    /// Emitted before the terminal event when the live view could be lossy
    /// (and always when a background execution finishes).
    UserShellOutputReplaced {
        execution_id: leveler_core::UserShellId,
        output_tail: String,
        output_truncated: bool,
    },
    /// The exact on-disk capture of a user shell execution is complete.
    /// `directory` holds `stdout.log`, `stderr.log` and `combined.log`.
    /// Clients use it for "Copy Output"; it is never inferred from painted
    /// text. Emitted before the terminal event, and only when every stream's
    /// sidecar reports a finished copy — an unfinished capture is never
    /// advertised.
    UserShellCaptureReady {
        execution_id: leveler_core::UserShellId,
        directory: String,
    },
    /// A user shell execution ended. `status` is `success | failed |
    /// cancelled`; `exit_code` is `None` when the process was killed or
    /// never spawned.
    UserShellExited {
        execution_id: leveler_core::UserShellId,
        exit_code: Option<i32>,
        duration_ms: u64,
        status: String,
    },
    /// Real token usage reported by the model for the latest request. The
    /// context gauge tracks how full the window is; `input_tokens` already
    /// includes the whole prompt (system + history + tools), so the window in
    /// use is `input_tokens + output_tokens`.
    ///
    /// A SNAPSHOT of the request that just reported, not a delta: a provider
    /// may emit it several times while one request streams, and the consumer
    /// must replace rather than add. Turn-level accounting is settled once by
    /// [`RuntimeEvent::TurnUsage`].
    TokenUsage {
        input_tokens: u32,
        output_tokens: u32,
        /// Subset of `input_tokens` the provider served from its prefix cache.
        /// Zero when the provider reports no cache stats.
        cached_input_tokens: u32,
        /// Subset of `output_tokens` the provider attributed to reasoning.
        /// `None` when it reported no breakdown — unreported is not a measured
        /// zero.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_tokens: Option<u32>,
    },
    /// The settled provider usage of one finished turn, aggregated from the
    /// runtime's durable model-call ledger. Arrives before the turn terminal
    /// and is replayable; a client uses it instead of summing the transient
    /// [`RuntimeEvent::TokenUsage`] snapshots.
    TurnUsage { usage: UiTurnUsage },
    /// The accounting of the exact next model request, computed by the kernel
    /// before the request is sent. Transient: nothing persists it, and a
    /// reconnecting client gets the latest through its live view.
    ContextUsage {
        accounting: leveler_model::ContextAccounting,
    },
    /// An orchestrated run completed; carries the summary report (spec §23).
    SessionCompleted { report: UiCompletionReport },
    /// Best-effort prediction of the user's next prompt. Transient UI chrome:
    /// it is never persisted as user input and never participates in task
    /// completion. Clients may discard it when the user has started typing.
    PromptSuggestion { text: String },
    /// Best-effort, transient post-turn recap requested after an idle window.
    /// It is display-only and never becomes model-visible conversation.
    AwaySummary { text: String },
    /// The current turn finished successfully.
    TurnCompleted,
    /// The work completed and project verification retains its own result,
    /// but a separate required completion contract produced warnings.
    ///
    /// `unregistered_task` is a structured terminal fact, not a reading of
    /// `reason`: the turn was an answer whose `commit_task` declaration was
    /// refused, so no durable task was registered. It must never settle a
    /// Goal. Omitted on legacy events, where the old semantics apply.
    TurnCompletedWithWarnings {
        reason: String,
        #[serde(default)]
        unregistered_task: bool,
    },
    /// The assistant naturally finished its answer, without claiming that an
    /// external task was independently verified as complete.
    TurnAnswered,
    /// The turn ended at the user's own pause boundary. The goal still owes
    /// the work the user withheld authorization to continue; nothing failed
    /// and nothing is complete. Only the user's own continuation releases it.
    TurnPaused { reason: String },
    /// The turn stopped at an output limit even after bounded continuation.
    TurnTruncated { error: String },
    /// The executor stopped cleanly but did not reach a successful terminal
    /// state (for example, budget exhaustion or an unresolved goal).
    TurnIncomplete { reason: String },
    /// The goal stopped because the model declared it blocked.
    ///
    /// This is a distinct terminal, not a flavour of [`Self::TurnIncomplete`]:
    /// the goal is NOT settled, the work already delivered stays real, and the
    /// user can continue it. `reason` is the model's own declared blocking
    /// summary; it is display-only and never decides the lifecycle — the
    /// durable `TaskFinished.stop` is the authority.
    TurnBlocked { reason: String },
    /// The current turn failed.
    ///
    /// `error` is the legacy display string, kept for compatibility with older
    /// clients (CLI, Web). New presentations must prefer `failure` and must
    /// never parse `error` to decide a category, a retry, or a delivery truth.
    TurnFailed {
        error: String,
        /// Structured product failure. `None` on legacy events and on failures
        /// the runtime could not type; readers fall back to `error`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failure: Option<crate::failure::UiFailure>,
        /// Present when the turn stopped because an earlier tool call was
        /// interrupted before its outcome was recorded. The runtime refuses
        /// to replay it; the client owns the wording and the next step.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recovery: Option<RecoveryStop>,
    },
    /// The current turn was cancelled (resumable).
    TurnCancelled,
    /// The logical task was explicitly cancelled by the user. Terminal: unlike
    /// [`Self::TurnCancelled`] a continuation must not reopen it.
    TaskCancelled,
    /// A spawned sub-agent started or finished (multi-agent delegation). One
    /// block per agent id, updated in place from running → done.
    SubAgentUpdated {
        id: String,
        nickname: String,
        role: String,
        /// The child's short task title, carried on the start only. `None` on
        /// terminals and on children recorded before titles existed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        /// false while running; true once the agent finished.
        done: bool,
        /// Whether it finished successfully (only meaningful when `done`).
        ok: bool,
        /// The task while running; a short result summary once done.
        detail: String,
        /// Built-in capability contract this child was launched under.
        /// `None` means the runtime did not record one, not "no profile".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile_role: Option<String>,
        /// Whether this child holds a physically read-only toolset.
        #[serde(default)]
        read_only: bool,
        /// The declarative agent it was spawned from, carried on the start.
        /// `None` for built-in role spawns, on terminals, and on events
        /// recorded before agents existed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<crate::UiChildAgentIdentity>,
        /// What the parent did with what this child found, once it finished.
        ///
        /// `None` means NOT MEASURED — the runtime produced no projection —
        /// and must never be rendered as a zero. A child that reported
        /// nothing arrives as `Some` with zero counts, which is a different
        /// fact and reads differently to the user.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        contribution: Option<ChildContribution>,
        /// The four-way reading, once done. `None` while running and for
        /// children settled before it was recorded.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        outcome: Option<ChildOutcome>,
        /// How the activation ended, once done.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop: Option<ChildStop>,
        /// Which bound fired when `stop` is `Budget`. `None` for every other
        /// stop, on the start, and for terminals recorded before it existed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<ChildLimit>,
        /// Whether the parent continues while this child runs. Carried on the
        /// start; `None` on a terminal, which says nothing about it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        background: Option<bool>,
        /// Exclusive write scope fixed at spawn (empty when late-bound or
        /// read-only).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        scope: Vec<String>,
    },
    /// A child's lifecycle moved without a start or a terminal: its activation
    /// died with a runtime window (`interrupted`) or a new one began under the
    /// same id (`running`). Clients update the child they already hold.
    SubAgentStateChanged { id: String, state: UiChildState },
    /// Live execution state and cumulative model usage for one spawned agent.
    SubAgentProgress {
        id: String,
        active: bool,
        input_tokens: u32,
        output_tokens: u32,
        cached_input_tokens: u32,
    },
    /// Live tool/step for one spawned sub-agent (attributed by `id`). Transient;
    /// older clients ignore unknown types via [`parse_runtime_event`].
    SubAgentActivity {
        id: String,
        /// `tool_started` or `tool_finished`.
        phase: String,
        tool: String,
        preview: String,
        is_error: bool,
    },
    /// Result of [`crate::ClientCommand::QueryChildContribution`]. Read-only:
    /// a snapshot of the ledger, never a mutation.
    ChildContributionLoaded {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query_id: Option<leveler_core::CommandId>,
        detail: crate::UiChildContribution,
    },
    /// A durable goal checkpoint was cut; render it as a Recap history item
    /// (long-goal P3). Emitted for `/recap`, milestones, context compaction,
    /// and on surfacing an interruption checkpoint — the recap carries its
    /// `checkpoint_id`, so an expanded view presents the same persisted facts.
    GoalRecapCreated { recap: crate::UiGoalRecap },
    /// Result of [`crate::ClientCommand::ListUnfinishedGoals`]. Read-only.
    UnfinishedGoalsLoaded {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query_id: Option<leveler_core::CommandId>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        goals: Vec<crate::UiUnfinishedGoal>,
    },
    /// Result of [`crate::ClientCommand::ListAgents`].
    AgentsLoaded {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query_id: Option<leveler_core::CommandId>,
        agents: Vec<crate::UiAgentEntry>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        problems: Vec<crate::UiAgentProblem>,
    },
    /// Result of [`crate::ClientCommand::GetAgent`]. `agent` is `None` and
    /// `error` says why when the name does not resolve.
    AgentLoaded {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query_id: Option<leveler_core::CommandId>,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<crate::UiAgentDetail>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Result of `CreateAgent` / `UpdateAgent` / `DeleteAgent`. On failure
    /// nothing was written and `error` is the reason.
    AgentMutated {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query_id: Option<leveler_core::CommandId>,
        name: String,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        /// The entry as the registry now resolves it (after a create/update).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<crate::UiAgentEntry>,
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
        /// The process-group leader's OS pid, when the runtime observed one.
        /// A read-only fact for a detail view; it carries no authority.
        /// Additive; older clients ignore the unknown field.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pid: Option<u32>,
    },
    /// A live chunk of a background task's combined stdout/stderr, already
    /// sanitized and capped by the runtime. Additive; older clients ignore the
    /// unknown type. The final [`Self::BackgroundTaskExited`] log stays
    /// authoritative.
    BackgroundTaskOutput { task_id: String, chunk: String },
    /// A still-running background task printed a readiness signal that a caller
    /// was waiting for (its own output matched an explicit pattern). `evidence`
    /// is the line that matched. This is an OBSERVATION, not a health check and
    /// not an exit: the process keeps running, and the terminal event still
    /// follows whenever it ends. Additive; older clients ignore the unknown
    /// type and keep showing the task as running.
    BackgroundTaskReady {
        task_id: String,
        evidence: String,
        /// Milliseconds from process start to the observation.
        #[serde(default)]
        elapsed_ms: u64,
    },
    /// A background task's state could not be established (for example its
    /// adopted driver failed). This is not a natural exit and not a stop: the
    /// UI must show unknown, never Running, failed, or stopped.
    BackgroundTaskUnknown {
        task_id: String,
        duration_ms: u64,
        output: String,
    },
    /// A background task finished (exit or kill). `output` is the task's final
    /// retained log, authoritative over the streamed chunks (a lifecycle
    /// broadcast lag can drop a chunk). Empty when the runtime produced none.
    BackgroundTaskExited {
        task_id: String,
        exit_code: Option<i32>,
        duration_ms: u64,
        ok: bool,
        /// The runtime's terminal state was `Killed`, not a natural exit: a
        /// user cancel, an agent cancel, or session cleanup stopped it. This is
        /// the authoritative distinction between "stopped" and "failed" — a
        /// stopped task is not a failure however its exit code reads. Additive;
        /// older clients ignore the unknown field.
        #[serde(default)]
        stopped: bool,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        output: String,
    },
    /// Authoritative replacement of a session's active background projection.
    /// Emitted after a lifecycle broadcast lag; history is unaffected.
    BackgroundTasksReconciled {
        tasks: Vec<crate::UiActiveBackgroundTask>,
    },
    /// Side-question (`/btw`) started; not persisted to session history.
    BtwStarted { question: String },
    /// Side-question answer chunk (often one full answer in MVP).
    BtwTextDelta { delta: String },
    /// Side-question finished successfully.
    BtwCompleted,
    /// Side-question stopped by the user before it finished. Distinct from
    /// [`Self::BtwFailed`]: the answer was interrupted, not unsuccessful, and
    /// the partial answer (if any) is what the side thread keeps.
    BtwCancelled,
    /// Side-question failed.
    BtwFailed { error: String },
    /// A `/btw` side question started a read-only tool call.
    ///
    /// The side surface has its own activity: the main turn's
    /// [`Self::ToolCallStarted`] / [`Self::ToolCallCompleted`] stay the main
    /// transcript's, and a side question never appears there. Transient and
    /// never persisted, like every other `Btw*` event.
    BtwToolStarted { call_id: ToolCallId, tool: String },
    /// A `/btw` side-question tool call finished.
    ///
    /// Carries the outcome and the measured duration only — no arguments, no
    /// tool output, no error payload — because the side surface is an
    /// observer and its activity is chrome, not a record.
    BtwToolFinished {
        call_id: ToolCallId,
        tool: String,
        is_error: bool,
        elapsed_ms: u64,
    },
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
    },
    /// Result of [`crate::ClientCommand::QueryContext`]. `accounting` is
    /// `None` when no model request has been assembled yet for the session
    /// (a fresh session before its first turn).
    ContextLoaded {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query_id: Option<CommandId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        accounting: Option<leveler_model::ContextAccounting>,
    },
    /// Result of [`crate::ClientCommand::QueryObservability`]. Read-only
    /// projection of durable facts for the current or a historical session.
    /// Echoes the command's `query_id` when the peer sent one. Absent on
    /// protocol 1.5 peers — a current client must not treat that as ownership.
    ObservabilityLoaded {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query_id: Option<CommandId>,
        observation: crate::UiObservabilityLoaded,
    },
    /// Result of [`crate::ClientCommand::QueryUsage`]. A projection of the
    /// runtime's durable model-call ledger for one scope. Read-only: it
    /// answers while a turn runs, and it never drives a provider request.
    UsageLoaded {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query_id: Option<CommandId>,
        report: Box<crate::UiUsageReport>,
    },
    /// Result of [`crate::ClientCommand::ReloadConfig`], and the notification
    /// of an automatically applied generation. `outcome` is one of
    /// `applied` / `unchanged` / `failed`.
    ///
    /// `version` is the generation now in force — the PREVIOUS one on a
    /// failure, because a failed reload keeps the last known good
    /// configuration. `changed` names the dimensions that moved (never their
    /// values, and never a credential). `deferred_to_next_turn` is true when a
    /// turn was already running: it keeps the generation it started with, and
    /// the next turn uses this one.
    ConfigReloaded {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query_id: Option<CommandId>,
        outcome: String,
        version: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thinking: Option<String>,
        #[serde(default)]
        changed: Vec<String>,
        #[serde(default)]
        deferred_to_next_turn: bool,
        /// Settings this generation changed that only a restart of the named
        /// component can apply. Empty means everything applied.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        restart_required: Vec<String>,
    },
    /// Result of [`crate::ClientCommand::QuerySessionHistory`]: the latest
    /// turns in order. `omitted_turns` older turns were left out to bound the
    /// response; they are still in the session.
    SessionHistoryLoaded {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query_id: Option<CommandId>,
        session_id: leveler_core::SessionId,
        entries: Vec<UiHistoryEntry>,
        #[serde(default)]
        omitted_turns: u32,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::{PlanStepStatus, UiDiff, UiDiffFile, UiPlan, UiPlanStep};
    use crate::snapshot::{
        MessageId, UiCheckpoint, UiMessage, UiRole, UiSessionSnapshot, UiSessionSummary,
    };
    use crate::{
        ApprovalId, AttachmentId, AttachmentKind, ClarificationId, CommandId, ModelRef, SessionId,
        ToolCallId,
    };
    use crate::{UiApprovalRequest, UiClarificationRequest};

    fn session_snapshot() -> UiSessionSnapshot {
        UiSessionSnapshot {
            history: None,
            id: SessionId::new("sess-1"),
            repository: Some("repo".to_string()),
            task_status: None,
            task_terminal: None,
            goal: "goal".to_string(),
            model: Some(ModelRef::new("openai", "gpt-4o")),
            mode: crate::PermissionProfile::Assisted,
            branch: Some("main".to_string()),
            status: "busy".to_string(),
            finalization_stage: None,
            messages: vec![UiMessage {
                id: MessageId::new("m1"),
                role: UiRole::User,
                text: "hi".to_string(),
                ordinal: None,
                kind: None,
                images: 0,
            }],
            pending_interactions: vec![],
            available_models: vec![ModelRef::new("openai", "gpt-4o-mini")],
            vision: false,
            last_sequence: Some(7),
            latest_turn_ordinal: None,
            active_tools: Vec::new(),
            interrupted_tool_calls: Vec::new(),
            active_background_tasks: Vec::new(),
            background_tasks: Vec::new(),
            plan: None,
            diff: None,
            checkpoints: Vec::new(),
            recaps: Vec::new(),
            user_shells: Vec::new(),
            completion_report: None,
            thinking: None,
            work_profile: None,
            collaboration: None,
            execution_task: None,
            children: Vec::new(),
            foreign_origin: None,
        }
    }

    #[test]
    fn turn_progress_roundtrips() {
        roundtrip(
            RuntimeEvent::TurnProgress {
                phase: "closing".into(),
                closing: true,
                no_progress_streak: 2,
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

    fn observation_fixture() -> crate::UiObservabilityLoaded {
        crate::UiObservabilityLoaded {
            session: crate::UiSessionObservation {
                session_id: SessionId::new("s1"),
                goal: "g".into(),
                repository: Some("/repo".into()),
                created_at: "t".into(),
                updated_at: "t".into(),
                status: "idle".into(),
                model: "m".into(),
                work_profile: "balanced".into(),
                collaboration: "chat".into(),
                last_sequence: Some(100),
                request_count: 0,
                input_tokens: 0,
                output_tokens: 0,
                avg_latency_ms: None,
                last_latency_ms: None,
                request_failures: 0,
                request_retries: 0,
                tool_started: 0,
                tool_finished: 0,
                compact_count: 0,
                subagent_started: 0,
                duration_ms: None,
                cached_input_tokens: None,
                reasoning_tokens: None,
                cost_usd_micros: None,
                lanes: Vec::new(),
            },
            window: Vec::new(),
            window_from: 1,
            window_to: 1,
            requests: Vec::new(),
            tools: Vec::new(),
            agents: Vec::new(),
            recovery: crate::UiRecoveryObservation {
                interrupted_turns: 0,
                workspace_snapshots: 0,
                review_stages: Vec::new(),
            },
            relations: Vec::new(),
        }
    }

    #[test]
    fn observability_loaded_decodes_protocol_1_5_without_query_id() {
        let json = serde_json::json!({
            "type": "observability_loaded",
            "observation": observation_fixture(),
        });
        let ev: RuntimeEvent = serde_json::from_value(json).expect("1.5 observability_loaded");
        match ev {
            RuntimeEvent::ObservabilityLoaded { query_id, .. } => assert_eq!(query_id, None),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn observability_loaded_echoes_a_1_6_query_id() {
        let ev = RuntimeEvent::ObservabilityLoaded {
            query_id: Some(CommandId::new("q1")),
            observation: observation_fixture(),
        };
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["query_id"], "q1");
        let back: RuntimeEvent = serde_json::from_value(json).unwrap();
        match back {
            RuntimeEvent::ObservabilityLoaded { query_id, .. } => {
                assert_eq!(query_id, Some(CommandId::new("q1")));
            }
            other => panic!("unexpected event: {other:?}"),
        }
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

    /// An older runtime does not send this field at all; the default identity
    /// must decode as unknown rather than as a version change.
    #[test]
    fn runtime_build_observation_roundtrips_and_tolerates_an_absent_build() {
        let build = leveler_core::BuildIdentity {
            version: "1.0.22".into(),
            revision: "abc123".into(),
            dirty: false,
            fingerprint: "abc123-clean".into(),
        };
        roundtrip(
            RuntimeEvent::RuntimeBuildObserved {
                runtime_build: build,
            },
            "runtime_build_observed",
        );

        let legacy: RuntimeEvent =
            serde_json::from_str(r#"{"type":"runtime_build_observed"}"#).unwrap();
        match legacy {
            RuntimeEvent::RuntimeBuildObserved { runtime_build } => {
                assert!(!runtime_build.is_known(), "absent is unknown, not a match");
            }
            other => panic!("unexpected event: {other:?}"),
        }
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
                    grant: None,
                    requires_human_consent: false,
                    id: ApprovalId::new("a1"),
                    tool: "run_command".to_string(),
                    summary: "run ls".to_string(),
                    command: Some("ls".to_string()),
                    risks: vec!["network".to_string()],
                    call_id: None,
                    always_persists: true,
                },
            },
            "approval_requested",
        );
    }

    #[test]
    fn clarification_requested_roundtrips() {
        roundtrip(
            RuntimeEvent::ClarificationRequested {
                request: UiClarificationRequest::single(
                    ClarificationId::new("c1"),
                    "which file?",
                    vec!["a.rs".to_string()],
                ),
            },
            "clarification_requested",
        );
    }

    #[test]
    fn attachment_added_roundtrips() {
        roundtrip(
            RuntimeEvent::AttachmentAdded {
                command_id: None,
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
                command_id: None,
                error: "bad mime".to_string(),
            },
            "attachment_processing_failed",
        );
    }

    #[test]
    fn attachment_result_preserves_command_id_and_accepts_legacy_events() {
        let legacy = serde_json::json!({"type":"attachment_processing_failed","error":"bad image"});
        let old: RuntimeEvent = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(serde_json::to_value(old).unwrap(), legacy);
        let correlated = serde_json::json!({"type":"attachment_processing_failed","error":"bad image","command_id":"upload-1"});
        let new: RuntimeEvent = serde_json::from_value(correlated.clone()).unwrap();
        assert_eq!(serde_json::to_value(new).unwrap(), correlated);
        let success = serde_json::json!({"type":"attachment_added","command_id":"upload-2","attachment":{"id":"att","kind":"text_file","name":"note.txt","mime_type":"text/plain","size_bytes":5,"sha256":"a","width":null,"height":null}});
        let new: RuntimeEvent = serde_json::from_value(success.clone()).unwrap();
        assert_eq!(serde_json::to_value(new).unwrap(), success);
    }

    #[test]
    fn user_message_added_roundtrips() {
        roundtrip(
            RuntimeEvent::UserMessageAdded {
                message: UiMessage {
                    id: MessageId::new("m1"),
                    role: UiRole::User,
                    text: "hello".to_string(),
                    ordinal: None,
                    kind: None,
                    images: 0,
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
    fn delivery_committed_roundtrips_the_named_answer() {
        roundtrip(
            RuntimeEvent::DeliveryCommitted {
                text: "清理前 12 行，清理后 0 行。".into(),
            },
            "delivery_committed",
        );
    }

    #[test]
    fn turn_finalizing_roundtrips_with_a_typed_stage() {
        roundtrip(
            RuntimeEvent::TurnFinalizing {
                stage: FinalizationStage::ResolvingOutcome,
            },
            "turn_finalizing",
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
                model_step: Some(2),
                answer_effect: None,
            },
            "tool_call_started",
        );
    }

    /// A recorded pre-round event has no `model_step`; a reader must decode it
    /// as `None` so it can fall back to its old grouping, not as step 0 (which
    /// would fuse every legacy call into one round).
    #[test]
    fn a_legacy_tool_call_started_decodes_without_a_round_identity() {
        let json = serde_json::json!({
            "type": "tool_call_started",
            "id": "tc1",
            "name": "read",
            "arguments": "{}",
            "parallel": false,
        });
        match serde_json::from_value::<RuntimeEvent>(json).unwrap() {
            RuntimeEvent::ToolCallStarted { model_step, .. } => assert_eq!(model_step, None),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn tool_call_completed_roundtrips() {
        roundtrip(
            RuntimeEvent::ToolCallCompleted {
                exit_code: None,
                stop: None,
                id: ToolCallId::new("tc1"),
                ok: true,
                preview: "ok".to_string(),
                duration_ms: 42,
                applied_diff: None,
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
                    state: Default::default(),
                },
                created: true,
                call_id: Some("call_1".to_string()),
            },
            "plan_updated",
        );
    }

    /// A legacy plan event carries neither the creation/update operation nor
    /// the originating tool call. A reader must decode both as unknown, never
    /// infer that an old payload proves either fact.
    #[test]
    fn a_legacy_plan_updated_decodes_without_an_operation_or_call() {
        let json = serde_json::json!({
            "type": "plan_updated",
            "plan": {
                "steps": [{ "index": 0, "description": "step", "status": "running" }],
            },
        });
        match serde_json::from_value::<RuntimeEvent>(json).unwrap() {
            RuntimeEvent::PlanUpdated {
                created,
                call_id,
                plan,
            } => {
                assert!(!created);
                assert_eq!(call_id, None);
                assert_eq!(plan.steps.len(), 1);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn diff_updated_roundtrips() {
        roundtrip(
            RuntimeEvent::DiffUpdated {
                query_id: Some(CommandId::new("q1")),
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
                    task_status: None,
                    task_terminal: None,
                    id: SessionId::new("s1"),
                    goal: "g".to_string(),
                    status: "done".to_string(),
                    declaration: None,
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
    fn user_shell_lifecycle_roundtrips() {
        roundtrip(
            RuntimeEvent::UserShellStarted {
                execution_id: leveler_core::UserShellId::new("ush-1"),
                command: "cargo test".into(),
                cwd: "/repo".into(),
            },
            "user_shell_started",
        );
        roundtrip(
            RuntimeEvent::UserShellOutput {
                execution_id: leveler_core::UserShellId::new("ush-1"),
                stream: "stdout".into(),
                chunk: "running 312 tests\n".into(),
            },
            "user_shell_output",
        );
        roundtrip(
            RuntimeEvent::UserShellExited {
                execution_id: leveler_core::UserShellId::new("ush-1"),
                exit_code: Some(0),
                duration_ms: 4200,
                status: "success".into(),
            },
            "user_shell_exited",
        );
    }

    #[test]
    fn context_compacted_roundtrip() {
        roundtrip(
            RuntimeEvent::ContextCompacted { from: 100, to: 40 },
            "context_compacted",
        );
    }

    #[test]
    fn context_expanded_roundtrip() {
        roundtrip(
            RuntimeEvent::ContextExpanded {
                from_tokens: 256_000,
                to_tokens: 512_000,
                reason: "reread_pressure".into(),
            },
            "context_expanded",
        );
    }

    #[test]
    fn usage_loaded_roundtrips_with_its_report() {
        let report = crate::UiUsageReport {
            scope: crate::UiUsageScope::Today,
            from: Some("2026-03-05T16:00:00+00:00".into()),
            to: None,
            generated_at: "2026-03-06T01:00:00+00:00".into(),
            requests: 2,
            unmeasured_requests: 1,
            input_tokens: 1_000,
            cached_input_tokens: Some(900),
            output_tokens: 100,
            reasoning_tokens: None,
            cost_usd_micros: Some(4_200),
            priced_requests: 1,
            models: vec![crate::UiModelUsage {
                provider: "deepseek".into(),
                model: "deepseek-flash".into(),
                requests: 2,
                unmeasured_requests: 1,
                input_tokens: 1_000,
                cached_input_tokens: Some(900),
                output_tokens: 100,
                reasoning_tokens: None,
                cost_usd_micros: Some(4_200),
                priced_requests: 1,
            }],
        };
        roundtrip(
            RuntimeEvent::UsageLoaded {
                query_id: Some(CommandId::new("q-usage")),
                report: Box::new(report.clone()),
            },
            "usage_loaded",
        );
        // The absent reasoning figure survives as absence, not as zero.
        let json = serde_json::to_string(&RuntimeEvent::UsageLoaded {
            query_id: None,
            report: Box::new(report),
        })
        .unwrap();
        let back = parse_runtime_event(&json).unwrap().unwrap();
        match back {
            RuntimeEvent::UsageLoaded { report, query_id } => {
                assert!(query_id.is_none());
                assert_eq!(report.reasoning_tokens, None);
                assert_eq!(report.total_tokens(), 1_100);
                assert!(report.cost_is_partial());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn token_usage_roundtrips() {
        roundtrip(
            RuntimeEvent::TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                cached_input_tokens: 40,
                reasoning_tokens: Some(30),
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
                    success: true,
                },
            },
            "session_completed",
        );
    }

    #[test]
    fn generated_chrome_events_roundtrip() {
        roundtrip(
            RuntimeEvent::PromptSuggestion {
                text: "继续验证".into(),
            },
            "prompt_suggestion",
        );
        roundtrip(
            RuntimeEvent::AwaySummary {
                text: "已完成定位，下一步修复。".into(),
            },
            "away_summary",
        );
    }

    #[test]
    fn turn_completed_roundtrips() {
        roundtrip(RuntimeEvent::TurnCompleted, "turn_completed");
    }

    #[test]
    fn turn_completed_with_warnings_roundtrips() {
        roundtrip(
            RuntimeEvent::TurnCompletedWithWarnings {
                reason: "completion review required".to_string(),
                unregistered_task: true,
            },
            "turn_completed_with_warnings",
        );
    }

    /// A warning recorded before the `unregistered_task` fact existed cannot
    /// prove that a task was unregistered; it decodes with the legacy meaning.
    #[test]
    fn a_legacy_warning_event_does_not_claim_an_unregistered_task() {
        let json = serde_json::json!({
            "type": "turn_completed_with_warnings",
            "reason": "completion review required",
        });
        match serde_json::from_value::<RuntimeEvent>(json).unwrap() {
            RuntimeEvent::TurnCompletedWithWarnings {
                unregistered_task, ..
            } => assert!(!unregistered_task),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn distinct_non_completion_end_states_roundtrip() {
        roundtrip(RuntimeEvent::TurnAnswered, "turn_answered");
        roundtrip(
            RuntimeEvent::TurnPaused {
                reason: "waiting for your confirmation".to_string(),
            },
            "turn_paused",
        );
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
    }

    #[test]
    fn turn_failed_roundtrips() {
        roundtrip(
            RuntimeEvent::TurnFailed {
                error: "oops".to_string(),
                failure: None,
                recovery: None,
            },
            "turn_failed",
        );
    }

    #[test]
    fn turn_failed_with_structured_failure_roundtrips() {
        roundtrip(
            RuntimeEvent::TurnFailed {
                error: "legacy".to_string(),
                failure: Some(crate::failure::UiFailure {
                    category: crate::failure::FailureCategory::InvalidRequest,
                    source: crate::failure::FailureSource::Provider,
                    provider: Some("moonshot".into()),
                    model: None,
                    provider_code: None,
                    request_id: None,
                    status: Some(400),
                    retries: None,
                    retryability: crate::failure::FailureRetryability::Never,
                    delivery: crate::failure::FailureDelivery::Responded,
                    summary: "模型服务拒绝了当前请求。".into(),
                    detail: "At path 'properties.plan.items' …".into(),
                }),
                recovery: None,
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
                pid: Some(4242),
            },
            "background_task_started",
        );
        roundtrip(
            RuntimeEvent::BackgroundTaskExited {
                task_id: "bg-1".into(),
                exit_code: Some(0),
                duration_ms: 12,
                ok: true,
                stopped: false,
                output: "done\n".into(),
            },
            "background_task_exited",
        );
    }

    /// Older runtimes predate the stop distinction: the field defaults to
    /// `false`, so a task whose exit code is non-zero still reads as a failure
    /// rather than a stop.
    #[test]
    fn background_task_exited_defaults_a_missing_stopped_bit_to_false() {
        let json = serde_json::json!({
            "type": "background_task_exited",
            "task_id": "bg-1",
            "exit_code": 1,
            "duration_ms": 5,
            "ok": false,
            "output": ""
        });
        let event: RuntimeEvent = serde_json::from_value(json).expect("parse");
        match event {
            RuntimeEvent::BackgroundTaskExited { stopped, ok, .. } => {
                assert!(!stopped, "unknown stop state is not a stop");
                assert!(!ok);
            }
            other => panic!("wrong event: {other:?}"),
        }
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
    fn sub_agent_activity_roundtrips() {
        roundtrip(
            RuntimeEvent::SubAgentActivity {
                id: "agent-1".into(),
                phase: "tool_started".into(),
                tool: "list_files".into(),
                preview: r#"{"path":"."}"#.into(),
                is_error: false,
            },
            "sub_agent_activity",
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
