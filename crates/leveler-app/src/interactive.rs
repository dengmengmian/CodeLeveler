//! [`InProcessRuntimeClient`] — the first [`InteractiveRuntimeClient`], bridging
//! the runtime's synchronous observer callbacks, cancellation tokens, and
//! approver into the async, broadcast-shaped client protocol the TUI consumes
//! .
//!
//! The runtime exposes progress as a synchronous `&mut (dyn FnMut(AgentEvent) + Send)`
//! observer with no channel. This client wraps that callback in a closure that
//! forwards each event into a `tokio::sync::broadcast`, and drives the turn on a
//! blocking thread (the turn future is not `Send`). Embedded `send` returns
//! after dispatch; the daemon composition waits for durable turn admission
//! before its transport writes an ACK.
//!
//! Approvals round-trip over the protocol: [`ChannelApprover`] emits an
//! `ApprovalRequested` event and awaits the matching `ApprovalDecision` command
//! via a per-request oneshot . Model/mode switches update the fields
//! used for subsequent turns and refresh the header via `SessionUpdated`.
//!
//! Assistant text arrives as whole-round `AgentEvent::AssistantText` (the
//! executor uses the non-streaming path today), republished as
//! `Started → TextDelta → Completed` so the protocol stays streaming-shaped for
//! when token streaming lands later.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::sync::{broadcast, oneshot};
use tokio_util::sync::CancellationToken;

use leveler_core::{
    BootId, BootLiveness, BootLivenessProbe, CheckpointId, ClarificationId, CommandId, SessionId,
};
use leveler_execution::{
    AdoptedTaskSpec, Approver, AutoApprove, BackgroundTaskLifetime, OutputStream,
    PermissionProfile, StreamOutcome,
};
use leveler_media::{MediaError, MediaStore};
use leveler_model::{
    ContentPart, ImageSource, Message, ModelProfile, ModelRef, ModelRequest, ModelRuntime, Role,
    ThinkingAccess, ThinkingCapabilities, ThinkingLevel, ToolChoice, TranscriptOrigin,
    resolve_reasoning_effort,
};
use leveler_storage::{
    Database, ForeignSessionOriginRepository, MessageRepository, SessionInputRepository,
    SessionInputState, SessionRecapRepository, SessionRepository, TurnRepository,
};

use leveler_client_protocol::{
    ApprovalDecision as UiApprovalDecision, ApprovalPolicy, AttachmentId, AttachmentKind,
    AttachmentRef, ClientCommand, ClientError, CommandEnvelope, InteractiveRuntimeClient,
    MessageId, NotificationLevel, RuntimeEvent, UiCheckpoint, UiForeignOrigin, UiForeignTool,
    UiInputState, UiInterruptedCallState, UiInterruptedToolCall, UiMemoryActionResult, UiMessage,
    UiMessageKind, UiPlan, UiPlanState, UiRole, UiSessionSnapshot, UiSessionSummary,
    UiThinkingAccess, UiThinkingState,
};

/// Whether a turn auto-approves risky actions. True when the session opted in
/// (`AutoApprove`), or the daemon was started with a global `--auto-approve`
/// (backward-compatible fallback). Keeping the global as an OR means a plain
/// daemon can host a per-session unattended goal AND interactive sessions at the
/// same time, while `serve --auto-approve` still auto-approves everything.
fn should_auto_approve(session_policy: Option<ApprovalPolicy>, global_auto_approve: bool) -> bool {
    matches!(session_policy, Some(ApprovalPolicy::AutoApprove)) || global_auto_approve
}

/// Project the session's Thinking Level for a client, in the user's own words.
///
/// Every field comes from a canonical source: the configured level the model
/// profile carries (per-model override, else the global default), and the
/// session's stored override. Nothing here is derived by reading back a
/// provider's resolved parameter, which would lose the user's intent — the same
/// `xhigh` can come from `max`, from an internal override, or from a harness
/// call, and only the canonical side knows which.
///
/// `effective` is what is actually in use: equal to `current` when this model
/// can express it, else `auto`, because a level the model cannot express is
/// never rounded into a neighbouring one and the request then carries no
/// reasoning override at all.
pub fn ui_thinking_state(
    profile: Option<&ModelProfile>,
    session_override: Option<ThinkingLevel>,
) -> Option<UiThinkingState> {
    let profile = profile?;
    let caps = ThinkingCapabilities::of(profile.capabilities.reasoning, &profile.reasoning);
    let configured = profile.thinking.unwrap_or(ThinkingLevel::Auto);
    let current = session_override.unwrap_or(configured);
    let effective = if caps.project(current).is_some() {
        current
    } else {
        ThinkingLevel::Auto
    };
    Some(UiThinkingState {
        configured,
        session_override,
        current,
        effective,
        access: match caps.access() {
            ThinkingAccess::Unsupported => UiThinkingAccess::Unsupported,
            ThinkingAccess::Fixed => UiThinkingAccess::Fixed,
            ThinkingAccess::Adjustable => UiThinkingAccess::Adjustable,
        },
        choices: caps.levels().to_vec(),
    })
}

fn unresolvable(command_id: &CommandId) -> ClientError {
    ClientError::Unresolvable(format!(
        "command {} was admitted, but the boot dispatching it ended before its outcome was \
         recorded",
        command_id.as_str()
    ))
}

fn execution_decision(value: UiApprovalDecision) -> leveler_execution::ApprovalDecision {
    match value {
        UiApprovalDecision::ApproveOnce => leveler_execution::ApprovalDecision::ApproveOnce,
        UiApprovalDecision::ApproveSession => leveler_execution::ApprovalDecision::ApproveSession,
        UiApprovalDecision::ApproveProject => leveler_execution::ApprovalDecision::ApproveProject,
        UiApprovalDecision::ApproveAlways => leveler_execution::ApprovalDecision::ApproveAlways,
        UiApprovalDecision::Deny => leveler_execution::ApprovalDecision::Deny,
    }
}

/// A command ACK means the engine has committed the turn's write-ahead input
/// record. The model loop may still be running, but a process death after this
/// point can reconstruct the transcript from durable state.
///
/// This is the one ACK contract for every interactive client: an in-process
/// (embedded) TUI, the daemon `serve` exposes, the `/web` UI over its own
/// in-process runtime, and a connected WebUI all wait on the same acceptance.
/// It is not a completion signal — only `leveler_engine::EngineEvent::TurnStarted`
/// reaching this receiver, which the engine emits after `TurnStore::start_owned` has
/// committed the turn row.
async fn await_turn_acceptance(
    accepted: oneshot::Receiver<Result<(), ClientError>>,
) -> Result<(), ClientError> {
    accepted.await.map_err(|_| {
        ClientError::Runtime("turn worker stopped before durable admission".to_string())
    })?
}

/// A refusal because another process owns the session, in the user's words.
fn ownership_conflict(error: &crate::AppError) -> ClientError {
    ClientError::OwnershipConflict(error.to_string())
}

/// Why a fresh turn was never durably admitted, as its caller hears it.
fn turn_rejection<T>(result: &Result<T, crate::AppError>) -> ClientError {
    match result {
        Err(
            error @ (crate::AppError::SessionRunningElsewhere
            | crate::AppError::SessionOwnershipUnknown),
        ) => ownership_conflict(error),
        Err(error @ crate::AppError::RecoveryConfirmationRequired { .. }) => {
            ClientError::RecoveryRequired(error.to_string())
        }
        Err(error) => ClientError::Runtime(error.to_string()),
        Ok(_) => ClientError::Runtime("turn ended before durable admission".to_string()),
    }
}

#[cfg(test)]
mod turn_rejection_tests {
    use super::turn_rejection;
    use leveler_client_protocol::ClientError;

    /// The crash-window gate has its own wire category: a client that received
    /// it must not present it as an ordinary runtime failure.
    #[test]
    fn a_recovery_gate_is_typed_not_a_generic_runtime_error() {
        let result: Result<(), crate::AppError> =
            Err(crate::AppError::RecoveryConfirmationRequired {
                call_id: "call-7".to_string(),
                tool: "run_command".to_string(),
            });
        match turn_rejection(&result) {
            ClientError::RecoveryRequired(message) => {
                assert!(message.contains("call-7"), "{message}");
            }
            other => panic!("unexpected rejection: {other:?}"),
        }
    }
}

/// Integration-test-only crash barrier. It is inert unless a daemon process
/// was built with the non-default `test-crash-barrier` feature and receives a
/// unique marker path directly under its isolated `LEVELER_HOME`. The test
/// kills the process after the marker is created, so this thread deliberately
/// never resumes.
#[cfg(feature = "test-crash-barrier")]
fn hit_after_turn_started_test_barrier() {
    hit_test_crash_barrier(
        "LEVELER_TEST_AFTER_TURN_STARTED_BARRIER",
        b"after_turn_started\n",
    );
}

/// Same contract, between a successful dispatch and its receipt being settled:
/// the exact window that leaves a `dispatching` receipt behind a dead boot.
#[cfg(feature = "test-crash-barrier")]
fn hit_before_receipt_settled_test_barrier() {
    hit_test_crash_barrier(
        "LEVELER_TEST_BEFORE_RECEIPT_SETTLED_BARRIER",
        b"before_receipt_settled\n",
    );
}

#[cfg(feature = "test-crash-barrier")]
fn hit_test_crash_barrier(variable: &str, marker: &[u8]) {
    let Some(raw_path) = std::env::var_os(variable) else {
        return;
    };
    let Some(raw_home) = std::env::var_os("LEVELER_HOME") else {
        return;
    };
    let path = PathBuf::from(raw_path);
    let home = PathBuf::from(raw_home);
    let valid_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(".test-crash-barrier-"));
    if path.parent() != Some(home.as_path()) || !valid_name {
        tracing::warn!(path = %path.display(), "ignored invalid test crash barrier path");
        return;
    }
    let write_result = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .and_then(|mut file| std::io::Write::write_all(&mut file, marker));
    if let Err(error) = write_result {
        tracing::warn!(%error, path = %path.display(), "could not announce test crash barrier");
        return;
    }
    loop {
        std::thread::park();
    }
}

/// Production/default builds contain no environment-controlled crash path.
#[cfg(not(feature = "test-crash-barrier"))]
fn hit_after_turn_started_test_barrier() {}

#[cfg(not(feature = "test-crash-barrier"))]
fn hit_before_receipt_settled_test_barrier() {}

fn execution_mode(value: leveler_client_protocol::PermissionProfile) -> PermissionProfile {
    match value {
        leveler_client_protocol::PermissionProfile::RequestApproval => {
            PermissionProfile::RequestApproval
        }
        leveler_client_protocol::PermissionProfile::Assisted => PermissionProfile::Assisted,
        leveler_client_protocol::PermissionProfile::FullAccess => PermissionProfile::FullAccess,
    }
}

fn protocol_mode(value: PermissionProfile) -> leveler_client_protocol::PermissionProfile {
    match value {
        PermissionProfile::RequestApproval => {
            leveler_client_protocol::PermissionProfile::RequestApproval
        }
        PermissionProfile::Assisted => leveler_client_protocol::PermissionProfile::Assisted,
        PermissionProfile::FullAccess => leveler_client_protocol::PermissionProfile::FullAccess,
    }
}

use crate::event_bridge::{EventBridge, turn_runtime_event};
use crate::prompt_assist::{self, AssistKind};
use crate::prompt_bridge::{
    ChannelApprover, ChannelClarifier, PendingApprovals, PendingClarifications, resolve_approval,
    resolve_clarification, supersede_pending_approvals, validate_pending_session,
};
use crate::workspace_view::{compute_diff, detect_branch_label};

fn spawn_auxiliary_assist(
    app: Arc<Application>,
    active: Arc<crate::active_turns::ActiveTurns>,
    assists: Arc<AuxiliaryAssists>,
    session_id: SessionId,
    model: ModelRef,
    events: crate::event_bridge::RuntimeEventSender,
    kind: AssistKind,
) {
    if active.is_running(&session_id) {
        return;
    }
    let (generation, cancellation) = assists.begin(&session_id, kind);
    tokio::spawn(async move {
        // A review belongs to the turn it describes. Read the settled turn
        // BEFORE the model call so a turn that starts later cannot be credited
        // with this text; the `active.is_running` guard below already refuses
        // to publish after such a turn started.
        let recap_turn = if matches!(kind, AssistKind::AwaySummary) {
            settled_turn_id(&app, &session_id).await
        } else {
            None
        };
        let Some(generated) =
            prompt_assist::generate(&app, &session_id, model, kind, cancellation).await
        else {
            assists.finish(&session_id, kind, generation);
            return;
        };
        if !assists.is_current(&session_id, kind, generation)
            || active.is_running(&session_id)
            || prompt_assist::transcript_len(&app, &session_id).await
                != Some(generated.transcript_len)
        {
            assists.finish(&session_id, kind, generation);
            return;
        }
        assists.finish(&session_id, kind, generation);
        let event = match kind {
            AssistKind::PromptSuggestion => RuntimeEvent::PromptSuggestion {
                text: generated.text,
            },
            AssistKind::AwaySummary => {
                // Durable BEFORE the client sees it: a reconnect must replay
                // this exact review instead of regenerating one, and a crash
                // after delivery must not lose it. A storage failure leaves
                // the review visible for this session but not replayable; it
                // is reported rather than silently turning into an empty row.
                if let Some(turn_id) = recap_turn {
                    record_session_recap(&app, &session_id, &turn_id, &generated.text).await;
                }
                RuntimeEvent::AwaySummary {
                    text: generated.text,
                }
            }
        };
        let _ = events.send(event);
    });
}

/// The newest settled turn of a session, or `None` when the turn table has
/// nothing to attach a review to.
async fn settled_turn_id(app: &Arc<Application>, session_id: &SessionId) -> Option<String> {
    let db = app.open_database().await.ok()?;
    TurnRepository::new(&db)
        .latest_settled(session_id)
        .await
        .ok()
        .flatten()
        .map(|turn| turn.id)
}

/// Persist the review a client is about to see. Idempotent by turn identity.
async fn record_session_recap(
    app: &Arc<Application>,
    session_id: &SessionId,
    turn_id: &str,
    text: &str,
) {
    let db = match app.open_database().await {
        Ok(db) => db,
        Err(error) => {
            tracing::warn!(
                session = session_id.as_str(),
                error = %error,
                "the review could not be persisted for replay"
            );
            return;
        }
    };
    if let Err(error) = SessionRecapRepository::new(&db)
        .record(session_id, turn_id, text, leveler_core::now())
        .await
    {
        tracing::warn!(
            session = session_id.as_str(),
            error = %error,
            "the review could not be persisted for replay"
        );
    }
}

/// The instruction used to summarize a conversation for compaction (spec §53).
const COMPACT_PROMPT: &str = "Summarize the conversation so far into a concise \
    briefing that preserves the task, decisions made, key facts learned, and \
    open questions, so work can continue without the full history. Reply with \
    ONLY the summary.";

/// Goal text interactive entry points create sessions with before any real
/// message exists. Replaced by [`title_from_first_message`] on first submit.
const PLACEHOLDER_GOAL: &str = "interactive session";

/// Session title from the first message: the first sentence of the first
/// non-empty line (CJK/latin sentence-final punctuation only — `.` would
/// mangle paths and version numbers), capped at 40 chars.
fn title_from_first_message(content: &str) -> Option<String> {
    crate::session_projection::title_from_first_message(content)
}

fn session_status_wire(
    status: leveler_lifecycle::SessionStatus,
    task_status: leveler_client_protocol::UiTaskStatus,
) -> &'static str {
    use leveler_client_protocol::UiTaskStatus;
    if status == leveler_lifecycle::SessionStatus::Running
        && !matches!(
            task_status,
            UiTaskStatus::Running | UiTaskStatus::WaitingUser
        )
    {
        "interrupted"
    } else {
        status.as_str()
    }
}

fn emit_project_rules(events: &crate::event_bridge::RuntimeEventSender, repo: Option<&Path>) {
    let Some(repo) = repo else {
        return;
    };
    let loaded = leveler_context::load_rules(repo);
    for error in &loaded.errors {
        // This channel only names loaded sources to the UI; a failed read is
        // evidence, not a source, so it goes to the operator log rather than
        // being counted as a loaded rule.
        tracing::warn!(
            event = "project_rule_unreadable",
            source = %error.source,
            kind = error.kind.as_key(),
            detail = %error.detail
        );
    }
    let sources = loaded
        .documents
        .into_iter()
        .map(|rule| rule.source)
        .collect::<Vec<_>>();
    if !sources.is_empty() {
        let _ = events.send(RuntimeEvent::ProjectRulesLoaded { sources });
    }
}

/// Whether a plain Enter / SubmitMessage should use the Goal turn profile
/// (`update_goal` / goal_mode) instead of Chat content turn.
///
/// Pure policy: the collaboration dispatch owns the answer, so TUI, remote
/// clients, tests and the runtime cannot disagree about which axis is Goal.
pub(crate) fn collaboration_routes_submit_to_goal(collaboration: &str) -> bool {
    crate::CollaborationExecution::of_wire(collaboration).runs_goal_lifecycle()
}

/// Whether a persisted task terminal is one a `继续` may re-enter through the
/// resume path.
///
/// Resumable work windows: a user interruption (Esc / force-cancel) and an
/// explicit resource boundary (a task stopped at its budget still owes work).
/// A `failed` run is resumable only when the runtime typed the failure as a
/// recoverable provider/network fault — a schema defect, a protocol violation
/// or an internal invariant has no repaired path that another model round
/// follows.
///
/// `blocked` is a statement about one work window, not a verdict on the
/// user's intent: the goal is still owed, so `继续` re-enters the same
/// recorded goal as a retry (the original objective is the resume lineage's
/// objective, never the continuation word). An explicitly cancelled task
/// stays terminal — retrying it would reopen work the user ended.
///
/// `completed` is resumable **only** when the durable stop reason was
/// [`leveler_lifecycle::StopReason::Answered`]: a turn that ended with an
/// answer never declared the goal complete, and the model's own text may say
/// the work is unfinished — the exact shape of a prematurely-closed long task.
/// `StopReason::Completed` (or a legacy row with no stop at all) IS the
/// declaration, and stays terminal.
pub(crate) fn task_outcome_is_resumable(
    outcome: &leveler_lifecycle::TaskOutcome,
    stop: Option<leveler_lifecycle::StopReason>,
    failure: Option<&leveler_model::ModelError>,
) -> bool {
    // The agent owns the resume rule; this reader must never grow its own copy
    // (the terminal that keeps a session's Goal route uses the same one).
    leveler_agent::coding::task_outcome_is_resumable(*outcome, stop, failure)
}

/// What a continuation intent means for a session's persisted terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContinuationState {
    /// An interrupted, budget-stopped, blocked or recoverably-failed task
    /// with a transcript to rebuild from. A `继续` resumes it.
    Resumable,
    /// The user explicitly cancelled the task. A `继续` must not reopen it.
    Cancelled,
    /// Nothing to resume: completed, non-recoverably failed, already running,
    /// or no terminal at all. A `继续` is an ordinary message.
    NotResumable,
}

use crate::Application;
use crate::active_turns::{ActiveTurns, RetireAdmission};

/// A session's `/btw` side thread: its own model conversation and the cancel
/// handle of its in-flight answer.
///
/// `history` is deliberately separate from the session's stored messages. A
/// side thread observes the main run but never becomes part of it, so a long
/// side chat cannot grow the main turn's context.
#[derive(Default)]
struct BtwSession {
    /// The side thread's own user/assistant turns, oldest first.
    history: Vec<Message>,
    /// Cancel handle of the answer being generated right now, if any. `None`
    /// when the side thread is idle.
    cancel: Option<CancellationToken>,
    /// Monotonic per-session side-question ordinal, used ONLY to correlate
    /// diagnostic traces of one question end to end. Never a state machine:
    /// nothing reads it to decide behavior.
    seq: u64,
}

/// Per-session side threads, keyed by session. Wrapped in `Arc<Mutex<..>>`
/// because a detached answer worker clears its own handle on completion.
type BtwThreads = Arc<Mutex<HashMap<SessionId, BtwSession>>>;

/// One runtime-quiescence reading.
///
/// The retire drain and `runtime_info` both build it from [`runtime_quiescence`],
/// so "the runtime is idle" means exactly one thing: no main turn and no
/// runtime-owned background task left. Independently hosted services are not
/// retiring runtime work; their owner stays alive through an engine update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RuntimeQuiescence {
    pub(crate) active_turns: u32,
    pub(crate) active_background_tasks: u32,
}

impl RuntimeQuiescence {
    pub(crate) fn quiescent(self) -> bool {
        self.active_turns == 0 && self.active_background_tasks == 0
    }
}

/// The ONE runtime-quiescence computation. `runtime_info` (what a client
/// reads) and the retire drain (what the process waits on) both go through
/// this, so a client can never be told "idle" by an accounting the drain
/// disagrees with.
async fn runtime_quiescence(
    active: &ActiveTurns,
    background: &leveler_execution::BackgroundTaskRegistry,
) -> Result<RuntimeQuiescence, ClientError> {
    let (turns, _) = active.load();
    Ok(RuntimeQuiescence {
        active_turns: turns as u32,
        active_background_tasks: background
            .try_update_blockers()
            .await
            .map_err(ClientError::Runtime)?
            .len() as u32,
    })
}

#[derive(Default)]
struct SessionStreams {
    streams: Mutex<HashMap<SessionId, SessionStream>>,
    shutdown: CancellationToken,
}

struct SessionStream {
    events: crate::event_bridge::RuntimeEventSender,
    cancellation: CancellationToken,
    /// When this stream was last handed to a producer. A stream is derived
    /// state — the durable event log is the authority — so an idle one can be
    /// dropped and recreated on the next publish.
    last_used: std::time::Instant,
}

/// How many idle session streams one runtime keeps. A stream reserves a full
/// 2048-slot ring of `RuntimeEvent` (936 bytes, measured), about 1.9 MiB, plus
/// one background-registry bridge task. A schedule that creates a fresh
/// session per run would otherwise grow that set for the life of a resident
/// runtime. Thirty-two streams bound the rings near 61 MiB — eight times the
/// runtime's four-turn admission capacity — while staying far below the
/// 24-hour soak's 512 MiB growth budget.
///
/// This is a resource bound, not a visibility bound: a reconnecting client
/// replays durable history and the next publish recreates the stream.
const SESSION_STREAM_CACHE_CAPACITY: usize = 32;

/// A stream younger than this is never evicted, so a client that has just
/// attached has time to subscribe before its ring is reclaimed.
const SESSION_STREAM_IDLE_GRACE: std::time::Duration = std::time::Duration::from_secs(60);

impl Drop for SessionStreams {
    fn drop(&mut self) {
        self.shutdown.cancel();
        for stream in self.streams.get_mut().unwrap().values() {
            stream.events.close();
        }
    }
}

/// An in-process runtime client backed by an [`Application`].
///
/// Cloning yields another handle to the SAME runtime: every mutable field is
/// shared, so a detached turn can publish a fact it committed (the session's
/// Goal route ending, say) through the same event streams and projection as the
/// command that started it.
#[derive(Clone)]
pub struct InProcessRuntimeClient {
    app: Arc<Application>,
    /// Defaults for sessions created by this runtime service.
    default_runtime: SessionRuntimeConfig,
    /// Model/mode/path selected independently by each live or restored session.
    session_runtime: Arc<Mutex<HashMap<SessionId, SessionRuntimeConfig>>>,
    // Serialize committed settings with their live permission projection.
    settings_edits: Arc<tokio::sync::Mutex<()>>,
    /// When true, skip the approval overlay (AutoApprove) so unattended TUI
    /// PTY drivers and CI dogfood can run interactive turns.
    auto_approve: bool,
    /// Root of the content-addressed image store (`<state_dir>/media`, under the
    /// global home — not the project).
    media_root: PathBuf,
    /// Compatibility stream containing events from every session.
    events: broadcast::Sender<RuntimeEvent>,
    /// Session-scoped streams used by daemon/socket clients.
    session_events: Arc<SessionStreams>,
    pending: PendingApprovals,
    pending_clarify: PendingClarifications,
    /// Serializes durable clarification settlement with turn admission.
    ///
    /// A client answer settles the request and then starts the continuation.
    /// A new turn must not slip between those steps and steal the session, and
    /// an answer must not race a new turn's admission check. One lock makes
    /// "the question is gone" and "the continuation owns the session" one
    /// ordered decision.
    durable_clarify: Arc<tokio::sync::Mutex<()>>,
    /// User shell executions (`!command`): active + bounded history.
    /// Arc so detached shell workers update it directly.
    user_shells: Arc<crate::user_shell::UserShellStore>,
    /// Conversation checkpoints, isolated by owning session (spec §68).
    /// Arc so the async compact worker can drop them after a successful rewrite.
    checkpoints: Arc<crate::checkpoints::CheckpointStore>,
    /// Live client-facing state that is not part of the message transcript.
    /// A reconnecting UI receives this through `snapshot()`.
    live_views: Arc<crate::live_view::LiveViews>,
    /// Per-session `/btw` side threads: their own conversation and the cancel
    /// handle of an in-flight side answer. Never merged into a main turn.
    btw: BtwThreads,
    /// Per-session ownership and cancellation of active main turns.
    active: Arc<ActiveTurns>,
    /// Set when the runtime owner begins an explicit shutdown (Quit);
    /// reported in health and never bypasses ownership fencing.
    shutting_down: Arc<std::sync::atomic::AtomicBool>,
    /// Why this runtime is retiring, when a client asked it to hand over. Set
    /// once alongside `shutting_down`; read by `runtime_info` so a waiting
    /// client can say WHICH generation change asked for the handover.
    retiring_reason: Arc<std::sync::Mutex<Option<leveler_client_protocol::RestartReason>>>,
    /// Cancelled to retire the process once work has drained. `None` for an
    /// in-process runtime, which has no process of its own to retire.
    process_shutdown: Option<CancellationToken>,
    /// Attached local UI count, shared with the transport's RAII guard so a
    /// dropped client decrements it even after an abrupt disconnect. `None`
    /// for an in-process runtime, which has no transport to count.
    client_presence: Option<leveler_local_transport::LocalWaiters>,
    /// Commands this boot is handling. A command enters before its receipt is
    /// written and leaves only once the receipt is settled — or once the path
    /// handling it has ended without settling it — so a `dispatching` receipt
    /// of this boot that is not in here will never be settled by this boot.
    in_flight: InFlightCommands,
    /// Input the user sent while a turn was already running, per session.
    /// Every entry has a durable journal row and is drained by the agent loop
    /// at the top of a round; the journal, not this map, is the authority.
    steering: Arc<Mutex<HashMap<SessionId, Vec<leveler_agent::PendingUserInput>>>>,
    /// A `/goal` declaration that arrived while a turn was already running.
    /// The durable axis is written first; this is the in-flight handoff that
    /// lets the RUNNING executor pick the declaration up at its next round
    /// instead of finishing as Chat and making the user type `继续`.
    goal_steering: Arc<Mutex<HashMap<SessionId, Arc<Mutex<GoalSteer>>>>>,
    /// The cancellation handle of every child running in a session's turn,
    /// by child id, so a user can stop one child without stopping the turn.
    child_cancels: ChildCancels,
    /// The cancellation handle of every tool call executing in a session's
    /// turn, by call id, so a user can stop one command without the turn.
    tool_call_cancels: ChildCancels,
    /// The attention handle of every tool call executing in a session's turn.
    /// Cancelling one is not a stop: it asks an observation-only call
    /// (`wait_task`) to return what it has so the user's message is answered
    /// now instead of after the wait's own bound.
    tool_call_yields: ChildCancels,
    /// Optional prompt/recap generation, cancelled before a real turn starts.
    auxiliary_assists: Arc<AuxiliaryAssists>,
    /// Highest configuration generation whose effects (session refresh,
    /// notification) were already announced, so a watcher and an explicit
    /// `/reload` observing the same generation announce it once.
    config_announced: Arc<std::sync::atomic::AtomicU64>,
}

type ChildCancels = Arc<Mutex<HashMap<SessionId, HashMap<String, CancellationToken>>>>;

/// Per-session ownership of optional model chrome. A recap and a next-step
/// suggestion are different calls and keep their own slots. A real user turn
/// still preempts both before they can compete for provider capacity.
#[derive(Default)]
struct AuxiliaryAssists {
    next_generation: std::sync::atomic::AtomicU64,
    active: Mutex<HashMap<(SessionId, AssistKind), (u64, CancellationToken)>>,
}

impl AuxiliaryAssists {
    fn begin(&self, session_id: &SessionId, kind: AssistKind) -> (u64, CancellationToken) {
        let generation = self
            .next_generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        let cancellation = CancellationToken::new();
        if let Some((_, previous)) = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                (session_id.clone(), kind),
                (generation, cancellation.clone()),
            )
        {
            previous.cancel();
        }
        (generation, cancellation)
    }

    fn cancel(&self, session_id: &SessionId) {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        active.retain(|(id, _), (_, cancellation)| {
            if id == session_id {
                cancellation.cancel();
                false
            } else {
                true
            }
        });
    }

    fn is_current(&self, session_id: &SessionId, kind: AssistKind, generation: u64) -> bool {
        self.active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(session_id.clone(), kind))
            .is_some_and(|(current, token)| *current == generation && !token.is_cancelled())
    }

    fn finish(&self, session_id: &SessionId, kind: AssistKind, generation: u64) {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let key = (session_id.clone(), kind);
        if active
            .get(&key)
            .is_some_and(|(current, _)| *current == generation)
        {
            active.remove(&key);
        }
    }
}

#[cfg(test)]
mod auxiliary_assist_tests {
    use super::{AssistKind, AuxiliaryAssists};
    use leveler_core::SessionId;

    #[test]
    fn suggestion_and_recap_keep_separate_slots() {
        let assists = AuxiliaryAssists::default();
        let session = SessionId::new("s");
        let (suggestion, suggestion_token) = assists.begin(&session, AssistKind::PromptSuggestion);
        let (recap, recap_token) = assists.begin(&session, AssistKind::AwaySummary);

        assert!(assists.is_current(&session, AssistKind::PromptSuggestion, suggestion));
        assert!(assists.is_current(&session, AssistKind::AwaySummary, recap));
        assert!(!suggestion_token.is_cancelled());
        assert!(!recap_token.is_cancelled());

        assists.cancel(&session);
        assert!(suggestion_token.is_cancelled());
        assert!(recap_token.is_cancelled());
        assert!(!assists.is_current(&session, AssistKind::PromptSuggestion, suggestion));
    }
}

/// Command id → number of deliveries of it this boot is handling.
pub(crate) type InFlightCommands = Arc<Mutex<HashMap<String, usize>>>;

/// One delivery's membership in [`InProcessRuntimeClient::in_flight`].
struct InFlightDelivery {
    registry: InFlightCommands,
    command_id: String,
}

impl InFlightDelivery {
    fn enter(registry: &InFlightCommands, command_id: &CommandId) -> Self {
        *registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(command_id.as_str().to_string())
            .or_default() += 1;
        Self {
            registry: registry.clone(),
            command_id: command_id.as_str().to_string(),
        }
    }

    /// Register, then write the receipt (`admit`). The order is the invariant:
    /// a `dispatching` receipt of this boot is never observable while its
    /// command is missing from the registry.
    async fn admit<T>(
        registry: &InFlightCommands,
        command_id: &CommandId,
        admit: impl std::future::Future<Output = T>,
    ) -> (Self, T) {
        let entry = Self::enter(registry, command_id);
        (entry, admit.await)
    }

    /// The receipt could not be settled although the dispatch finished: stay
    /// registered for the rest of this boot rather than let the receipt read
    /// as abandoned by a path that did not abandon it. A later boot judges it.
    fn keep_for_this_boot(self) {
        std::mem::forget(self);
    }
}

impl Drop for InFlightDelivery {
    fn drop(&mut self) {
        let mut registry = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(count) = registry.get_mut(&self.command_id) {
            *count -= 1;
            if *count == 0 {
                registry.remove(&self.command_id);
            }
        }
    }
}

/// In-flight `/goal` state for one session.
///
/// `requested` survives for the rest of the running turn: once a goal is
/// declared, the executor must keep the goal contract for every later round
/// of that turn. `pending` holds declarations not yet injected; the executor
/// drains it at the top of a round, which also persists the text as a normal
/// user-input row.
#[derive(Default)]
struct GoalSteer {
    requested: bool,
    pending: Vec<String>,
}

/// Drains one session's steering queue for the agent loop, and holds the
/// session's running children's cancellation handles.
struct SessionSteering {
    session_id: SessionId,
    queues: Arc<Mutex<HashMap<SessionId, Vec<leveler_agent::PendingUserInput>>>>,
    /// Session-scoped stream that receives the delivered settlement. The
    /// executor appends the transcript row; this is how the client learns the
    /// exact input it drew reached the model.
    events: crate::event_bridge::RuntimeEventSender,
    /// The accepted input this turn consumes as its opening message, when the
    /// host promoted one. It must not also be drained as a steer.
    seed: Option<String>,
    goal: Arc<Mutex<GoalSteer>>,
    children: ChildCancels,
    tool_calls: ChildCancels,
    /// Paired with `tool_calls`: the same call ids, holding the "hand the round
    /// back" handle instead of the "stop this call" handle.
    tool_yields: ChildCancels,
}

impl SessionSteering {
    /// Whether this session already holds user input the next round will
    /// consume. Read-only on purpose: it must not take the text, only tell a
    /// newly started observation call that there is nothing to wait for.
    fn input_is_waiting(&self) -> bool {
        let queued = self
            .queues
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&self.session_id)
            .is_some_and(|queue| !queue.is_empty());
        queued
            || !self
                .goal
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pending
                .is_empty()
    }
}

impl leveler_agent::SteeringSource for SessionSteering {
    fn take_pending(&self) -> Vec<String> {
        self.take_pending_inputs()
            .into_iter()
            .map(|input| input.text)
            .collect()
    }

    fn take_pending_inputs(&self) -> Vec<leveler_agent::PendingUserInput> {
        self.queues
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(&self.session_id)
            .map(std::mem::take)
            .unwrap_or_default()
    }

    fn input_consumed(&self, id: &str, ordinal: Option<u64>) {
        let _ = self.events.send(RuntimeEvent::InputSettled {
            message_id: MessageId::new(id),
            state: UiInputState::Delivered,
            reason: String::new(),
            ordinal,
        });
    }

    fn seed_input_id(&self) -> Option<String> {
        self.seed.clone()
    }

    fn goal_requested(&self) -> bool {
        self.goal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .requested
    }

    fn take_goal_declaration(&self) -> Option<String> {
        let mut goal = self
            .goal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if goal.pending.is_empty() {
            None
        } else {
            Some(goal.pending.remove(0))
        }
    }

    fn child_started(&self, id: &str, cancel: CancellationToken) {
        self.children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(self.session_id.clone())
            .or_default()
            .insert(id.to_string(), cancel);
    }

    fn child_ended(&self, id: &str) {
        release_cancel(&self.children, &self.session_id, id);
    }

    fn tool_call_started(&self, id: &str, cancel: CancellationToken, yield_now: CancellationToken) {
        for (handles, token) in [
            (&self.tool_calls, cancel),
            (&self.tool_yields, yield_now.clone()),
        ] {
            handles
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(self.session_id.clone())
                .or_default()
                .insert(id.to_string(), token);
        }
        // Input that arrived BEFORE this call registered is not lost: the
        // message is answered at the top of the next round whether or not the
        // call was already running, so an observation call that starts here
        // has nothing left to observe for.
        if self.input_is_waiting() {
            yield_now.cancel();
        }
    }

    fn tool_call_ended(&self, id: &str) {
        release_cancel(&self.tool_calls, &self.session_id, id);
        release_cancel(&self.tool_yields, &self.session_id, id);
    }
}

/// The `/btw` mode contract, with the person's question quoted inside it.
///
/// The contract bounds SIDE EFFECTS, not tool calls: read-only investigation is
/// allowed (the surface handed to the model holds no mutating tool), while
/// workspace mutation and any change to the main task stay forbidden. The row
/// is a runtime notice. It is not submitted user input, and it is not appended
/// to the main transcript.
///
/// `runtime_context` is the binding read-only projection of the main task
/// ([`crate::side_question`]): the ids and facts needed to answer a status
/// question from the runtime instead of from unbound files.
fn side_question_message(question: &str, runtime_context: &str) -> Message {
    Message::user(
        format!(
            "【旁问 / btw】这是主任务之外的旁问。回答需要时可以使用只读工具。\n\n不要修改工作区，不要推进或改变主任务，也不要改动它的目标、计划、完成状态或执行状态。\n\n独立回答这个旁问，然后回到主任务，不改变主任务。\n\n{runtime_context}\n\n【旁问】{question}"
        ),
        TranscriptOrigin::RuntimeNotice {
            notice: leveler_model::RuntimeNoticeKind::SideQuestion,
        },
    )
}

/// One streamed user-shell chunk: the store tail and the persisted/forwarded
/// transient event move together, so live and reconnect views cannot drift.
async fn append_user_shell_chunk(
    store: &crate::user_shell::UserShellStore,
    log: &leveler_engine::EventLog<'_>,
    forward: &mut (dyn FnMut(leveler_engine::EngineEvent) + Send),
    session_id: &SessionId,
    id: &leveler_core::UserShellId,
    chunk: leveler_execution::OutputChunk,
) {
    store.append_output(session_id, id, &chunk.text);
    let _ = log
        .append(
            None,
            leveler_engine::EngineEvent::UserShellOutput {
                execution_id: id.clone(),
                stream: crate::user_shell::stream_tag(chunk.stream).to_string(),
                chunk: chunk.text,
            },
            forward,
        )
        .await;
}

/// Persist the terminal result of a background process task as a durable engine
/// fact.
///
/// The starting call's own `ToolCallFinished` recorded only that the process
/// STARTED. When the task finally exits, that result has to be attributable to
/// the same call — otherwise a test moved to the background after 15 seconds
/// runs, passes, and is invisible to `update_goal(complete)` both live and after
/// a resume. The registry owns the task→call link; this writes the fact that
/// call_id, the argv, and the exit decided.
///
/// The terminal state is observable even when it provides no verification
/// evidence. The lifecycle bridge and turn-boundary reconcile share the same
/// fenced, idempotent append.
pub(crate) async fn persist_background_task_finished(
    app: &Application,
    session_id: SessionId,
    task: &leveler_execution::BackgroundTaskSnapshot,
) {
    if !task.status.is_terminal() || task.owner_scope.as_deref() != Some(session_id.as_str()) {
        return;
    }
    let origin = if task.id.starts_with("host-") {
        app.background_tasks()
            .hosted_task_origin(&task.id, session_id.as_str())
    } else {
        Some(leveler_execution::BackgroundTaskOrigin {
            call_id: task.origin_call_id.clone(),
            goal_id: task.goal_id.clone(),
        })
    };
    let Some(leveler_execution::BackgroundTaskOrigin {
        call_id: Some(call_id),
        goal_id,
    }) = origin
    else {
        // User shells and internal services have no tool call to attribute.
        return;
    };
    const OUTPUT_TAIL_BYTES: usize = 4096;
    let output_start = leveler_core::ceil_char_boundary(
        &task.log,
        task.log.len().saturating_sub(OUTPUT_TAIL_BYTES),
    );
    let task_id = &task.id;
    // The start must be durable before the result. A task handed off from a
    // foreground call can exit before the live start row reaches the log, and
    // a lifecycle replayed from the log must still pair this result with what
    // ran, under which call and lifetime. The insert is a no-op when the live
    // start is already recorded.
    let start = leveler_engine::EngineEvent::BackgroundTaskStarted {
        call_id: call_id.clone(),
        task_id: task.id.clone(),
        program: task.program.clone(),
        args: task.args.clone(),
        lifetime: task.lifetime.as_str().to_string(),
    };
    let event = leveler_engine::EngineEvent::BackgroundTaskFinished {
        call_id,
        task_id: task_id.clone(),
        goal_id,
        program: task.program.clone(),
        args: task.args.clone(),
        // A zero exit is the only pass. An exit whose code could not be
        // established is not a pass either.
        is_error: task.status != leveler_execution::BackgroundTaskStatus::Exited
            || task.exit_code != Some(0),
        exit_code: task.exit_code,
        status: Some(task.status),
        output: task.log[output_start..].to_owned(),
        output_truncated: task.log_truncated || output_start != 0,
    };
    // The result belongs to the call that started it, not to whichever turn
    // happens to be open now; the goal reader scopes by call identity.
    let result = async {
        let db = app.open_database().await?;
        let runtime = app.runtime_id()?;
        let boot = app.boot_id()?;
        let now = leveler_core::now();
        let (_, start_payload) = start
            .to_row()
            .map_err(crate::session::app_error_from_engine)?;
        // The start is required before the terminal fact; if it cannot be
        // written, the finish is not written either and the reconcile path
        // retries both later. A terminal row without a start would be an
        // orphan that replay has to guess at.
        db.append_background_task_started_if_missing(
            &session_id,
            &runtime,
            &boot,
            task_id,
            &start_payload,
            now,
        )
        .await?;
        let (_, payload) = event
            .to_row()
            .map_err(crate::session::app_error_from_engine)?;
        db.append_background_task_finished(&session_id, &runtime, &boot, task_id, &payload, now)
            .await?;
        Ok::<_, crate::AppError>(())
    }
    .await;
    if let Err(error) = result {
        tracing::warn!(%error, %task_id, "could not persist background terminal fact");
    }
}

/// Record background results that finished while nothing was watching.
///
/// A client-attached session gets these facts live from the lifecycle bridge. A
/// headless run, or a runtime that was busy elsewhere, has no such stream, so
/// the turn boundary is where the durable record catches up — including the
/// turn that RESUMES after a crash. Idempotent: an already-recorded task is
/// skipped, so a result is never counted twice.
pub(crate) async fn reconcile_background_evidence(app: &Application, session_id: &SessionId) {
    let registry = app.background_tasks();
    let mut snapshots = registry
        .local_terminal_snapshots_for_scope(Some(session_id.as_str()), |_| true, usize::MAX)
        .await;
    match registry.hosted_terminal_snapshots_for_scope(session_id.as_str()) {
        Ok(hosted) => snapshots.extend(hosted),
        Err(error) => tracing::warn!(%error, "cannot reconcile hosted background facts"),
    }
    for task in snapshots {
        persist_background_task_finished(app, session_id.clone(), &task).await;
    }
}

/// Terminal process facts the Execution Host kept for this session.
///
/// The host outlives the runtime that admitted a task, so its archive can prove
/// what an engine event has not recorded yet — a task that finished while no
/// runtime was attached. Reading it claims no process and acknowledges no
/// result; the projection only reports what the fact proves. An unreadable
/// archive is reported and then treated as absent: a projection must not fail
/// a reconnect over diagnostics it could not collect, and it must never invent
/// the missing states either.
fn hosted_terminal_facts(
    app: &Application,
    session_id: &SessionId,
) -> Vec<leveler_execution::BackgroundTaskSnapshot> {
    match app
        .background_tasks()
        .hosted_terminal_snapshots_for_scope(session_id.as_str())
    {
        Ok(tasks) => tasks,
        Err(error) => {
            tracing::warn!(
                %error,
                session = session_id.as_str(),
                "cannot read Execution Host terminal facts"
            );
            Vec::new()
        }
    }
}

/// Live goal tasks whose admitting runtime generation is proven gone.
///
/// The Execution Host still owns the process, but the generation that asked
/// for the work cannot receive its result. The projection reports the task as
/// Lost even when the engine never wrote a start row, so a crash inside the
/// spawn call does not erase the work from view. Reading the archive claims no
/// process; an unreadable archive is reported and treated as absent.
fn hosted_owner_lost_facts(
    app: &Application,
    session_id: &SessionId,
) -> Vec<leveler_execution::BackgroundTaskSnapshot> {
    match app
        .background_tasks()
        .hosted_owner_lost_snapshots_for_scope(session_id.as_str())
    {
        Ok(tasks) => tasks,
        Err(error) => {
            tracing::warn!(
                %error,
                session = session_id.as_str(),
                "cannot read Execution Host owner-loss facts"
            );
            Vec::new()
        }
    }
}

/// Persist the terminal fact for an execution that completed in the background.
///
/// The original foreground ownership token is stale once another turn acquires
/// the session, so this takes the CURRENT token only when it still belongs to
/// this runtime. If another runtime owns the session, the live event is still
/// delivered but nothing is written: a stale runtime must never extend the log.
async fn persist_background_user_shell_finished(
    app: Arc<Application>,
    session_id: SessionId,
    execution_id: leveler_core::UserShellId,
    exit_code: Option<i32>,
    duration_ms: u64,
    status: String,
    output_tail: String,
    output_truncated: bool,
    full_output: Option<String>,
) {
    let Ok(db) = app.open_database().await else {
        return;
    };
    let Ok(engine) = app.task_engine(&db) else {
        return;
    };
    // A live turn from this runtime already owns the session: reuse its token.
    // An idle session has no writer, so acquire the unowned epoch briefly and
    // release it after the append. A foreign owner means this runtime must not
    // write; the live event remains the only delivery.
    let (token, acquired) = match engine.current_ownership(&session_id).await {
        Ok(Some(token)) => (token, false),
        Ok(None) => match engine.acquire_if_unowned(&session_id).await {
            Ok(Some(token)) => (token, true),
            Ok(None) | Err(_) => return,
        },
        Err(_) => return,
    };
    let log = leveler_engine::EventLog::new_owned(&db, session_id, token.clone());
    let mut forward = |_event: leveler_engine::EngineEvent| {};
    let _ = log
        .append(
            None,
            leveler_engine::EngineEvent::UserShellFinished {
                execution_id,
                exit_code,
                duration_ms,
                status,
                output_tail,
                output_truncated,
                full_output,
            },
            &mut forward,
        )
        .await;
    if acquired {
        let _ = engine.release_ownership(&token).await;
    }
}

/// The runtime's background status as the observer projection names it. One
/// mapping, so the side question and the Background Tasks page cannot describe
/// the same task differently.
fn background_status_label(status: leveler_execution::BackgroundTaskStatus) -> &'static str {
    use leveler_execution::BackgroundTaskStatus as Status;
    match status {
        Status::Running => "running",
        Status::Killing => "killing",
        Status::Exited => "exited",
        Status::Killed => "stopped",
        Status::Unknown => "unknown",
    }
}

/// Keep the newest `max` bytes of a log tail, on a char boundary. An injected
/// context must stay bounded even when a task retained hundreds of kilobytes.
fn truncate_tail(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let cut = leveler_core::ceil_char_boundary(text, text.len() - max);
    text[cut..].to_string()
}

/// Drop a settled child's or call's handle.
fn release_cancel(handles: &ChildCancels, session_id: &SessionId, id: &str) {
    if let Some(running) = handles
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get_mut(session_id)
    {
        running.remove(id);
    }
}

#[derive(Debug, Clone)]
struct SessionRuntimeConfig {
    /// `None` is a deterministic-only session: host commands run, agent turns
    /// fail with a clear configuration error. Never a placeholder model.
    model: Option<ModelRef>,
    mode: PermissionProfile,
    sandbox: bool,
    collaboration: String,
    /// Per-session approval policy. `AutoApprove` skips the approval overlay for
    /// this session only; the daemon-wide `auto_approve` still applies as a
    /// fallback (see `approver`). Not persisted: a restored session falls back to
    /// `Interactive` (daemon-crash continuation is out of scope for this gate).
    approval_policy: ApprovalPolicy,
    /// This session's Thinking Level override, in CodeLeveler's own vocabulary.
    /// `None` means the session inherits the configured level — which is not the
    /// same as `Some(Auto)`, a session that explicitly asked for no override.
    thinking: Option<ThinkingLevel>,
}

// This cache owns only the transient approval policy. Hydration must never
// overwrite an explicit attach that occurred while durable fields were read.
fn install_persisted_config(
    configs: &Mutex<HashMap<SessionId, SessionRuntimeConfig>>,
    session_id: &SessionId,
    mut config: SessionRuntimeConfig,
) -> SessionRuntimeConfig {
    let mut configs = configs.lock().unwrap();
    if let Some(current) = configs.get(session_id) {
        config.approval_policy = current.approval_policy;
    }
    configs.insert(session_id.clone(), config.clone());
    config
}

/// Why an agent turn cannot start on a session that has no model.
///
/// A deterministic session is legitimate: `!command` and scheduled shell
/// actions are real host executions that never call a model. Only the agent
/// surfaces require one, and they say so instead of inventing a default.
fn no_model_message(session_id: &SessionId) -> String {
    format!(
        "session {session_id} has no model configured; host commands still run, but an agent turn needs a provider and model — run `leveler init`, then choose one with /model"
    )
}

fn require_model(
    config: &SessionRuntimeConfig,
    session_id: &SessionId,
) -> Result<ModelRef, ClientError> {
    config
        .model
        .clone()
        .ok_or_else(|| ClientError::Runtime(no_model_message(session_id)))
}

/// The action name a Dream result carries; batch Dream is a distinct command.
fn dream_action_name(batch: bool) -> &'static str {
    if batch { "batch_dream" } else { "dream" }
}

/// Whether `answer` is the decision this request already recorded.
///
/// An empty wire answer is an explicit skip. Only the identical decision may
/// be treated as a retried submission whose acknowledgement was lost; a
/// different choice belongs to a request that is over.
fn clarification_answer_matches(
    terminal: &leveler_engine::SettledClarification,
    answer: &str,
) -> bool {
    if answer.trim().is_empty() {
        terminal.outcome == "skipped"
    } else {
        terminal.outcome == "answered" && terminal.result_text == answer
    }
}

/// The honest answer to a submission the request can no longer accept.
fn stale_clarification_answer(terminal: &leveler_engine::SettledClarification) -> ClientError {
    let state = match terminal.outcome.as_str() {
        "cancelled" => "该问题已随任务取消而关闭",
        "answered" | "skipped" => "该问题已由其他客户端处理",
        "unattended" | "timed_out" => "该问题已按无人应答结束",
        _ => "该问题已结束",
    };
    ClientError::Runtime(format!("{state}，本次回答未生效"))
}

/// Whether a durable user answer still has no continuation.
///
/// `settle_clarification` commits the decision and the tool result that
/// carries it as one batch; the runtime then admits the next window. A process
/// that dies between those writes leaves a goal that still owes work with
/// nobody driving it — the answer is durable, so it must not be lost.
///
/// Only that exact tail qualifies. Once any event other than the reaper's own
/// bookkeeping follows the tool result, the answered window moved on: the
/// interrupted turn is ordinary work the user resumes explicitly, not an
/// answer waiting for the runtime.
async fn clarification_continuation_is_owed(
    db: &leveler_storage::Database,
    stores: &leveler_storage::EngineStores,
    session_id: &SessionId,
) -> Result<bool, ClientError> {
    let storage_error = |error: leveler_storage::StorageError| {
        ClientError::Runtime(format!("owed clarification: {error}"))
    };
    let turns = stores
        .turns
        .list_for_session(session_id)
        .await
        .map_err(storage_error)?;
    let Some(latest) = turns.last() else {
        return Ok(false);
    };
    if latest.status != "interrupted" {
        return Ok(false);
    }
    // A goal paused by the user waits for the user's explicit authorization;
    // a decision recorded before the pause does not lift that boundary.
    if let Some(task) = stores
        .tasks
        .task_for_session(session_id)
        .await
        .map_err(storage_error)?
    {
        let goals = stores.goals.for_task(&task).await.map_err(storage_error)?;
        if goals.iter().any(|goal| {
            goal.state == leveler_storage::GoalState::Running && goal.user_paused_at.is_some()
        }) {
            return Ok(false);
        }
    }
    let log = leveler_engine::EventLog::new(stores.events.as_ref(), session_id.clone());
    // An unanswered question is already waiting for the user; it is not an
    // answer whose continuation is owed.
    if !log
        .pending_clarifications()
        .await
        .map_err(|error| ClientError::Runtime(format!("owed clarification: {error}")))?
        .is_empty()
    {
        return Ok(false);
    }
    let rows = stores
        .events
        .load_by_types(
            session_id,
            &[
                "clarification_requested",
                "clarification_answered",
                "tool_call_finished",
            ],
        )
        .await
        .map_err(storage_error)?;
    let decode = |row: &leveler_storage::EventRecord| {
        if row.schema_version > leveler_storage::EVENT_SCHEMA_VERSION {
            return Err(ClientError::Runtime(format!(
                "owed clarification: event {} has schema_version {} > supported {}",
                row.id,
                row.schema_version,
                leveler_storage::EVENT_SCHEMA_VERSION
            )));
        }
        leveler_engine::EngineEvent::from_payload(&row.payload)
            .map_err(|error| ClientError::Runtime(format!("owed clarification: {error}")))
    };
    let mut asked: std::collections::HashMap<String, (String, String)> =
        std::collections::HashMap::new();
    let mut answered: Option<(i64, ClarificationId, String, Option<String>)> = None;
    for row in &rows {
        match decode(row)? {
            leveler_engine::EngineEvent::ClarificationRequested {
                id, call_id, tool, ..
            } => {
                asked.insert(id.as_str().to_string(), (call_id, tool));
            }
            leveler_engine::EngineEvent::ClarificationAnswered { id, outcome, .. } => {
                answered = Some((row.sequence, id, outcome, row.turn_id.clone()));
            }
            _ => {}
        }
    }
    let Some((answer_sequence, id, outcome, answer_turn)) = answered else {
        return Ok(false);
    };
    // The question must belong to the turn whose interruption is being
    // recovered. An answered question in an older turn is history.
    if answer_turn.as_deref() != Some(latest.id.as_str()) {
        return Ok(false);
    }
    // Cancellation and non-user outcomes never continue a window.
    if outcome != "answered" && outcome != "skipped" {
        return Ok(false);
    }
    let Some((call_id, tool)) = asked.get(id.as_str()) else {
        return Ok(false);
    };
    // A request from before the tool identity was persisted cannot be routed
    // back to a model; it stays readable history.
    if call_id.is_empty() || tool.is_empty() {
        return Ok(false);
    }
    let mut closure = None;
    for row in &rows {
        if row.sequence <= answer_sequence || row.turn_id.as_deref() != answer_turn.as_deref() {
            continue;
        }
        if let leveler_engine::EngineEvent::ToolCallFinished {
            call_id: closed, ..
        } = decode(row)?
            && &closed == call_id
        {
            closure = Some(row.sequence);
        }
    }
    // The terminal and its tool result are one atomic batch. A missing result
    // means this is not the shape the durable path wrote; never drive from it.
    let Some(closure_sequence) = closure else {
        return Ok(false);
    };
    let after = leveler_storage::EventRepository::new(db)
        .first_after_except_types(
            session_id,
            closure_sequence,
            &[
                "turn_finished",
                "goal_checkpoint_created",
                "sub_agent_interrupted",
                "sub_agent_finished",
            ],
        )
        .await
        .map_err(storage_error)?;
    Ok(after.is_none())
}

impl InProcessRuntimeClient {
    /// Build a client that runs turns with the given model, mode, and sandbox
    /// setting.
    pub fn new(
        app: Arc<Application>,
        model: ModelRef,
        mode: PermissionProfile,
        sandbox: bool,
    ) -> Self {
        Self::new_with_options(app, model, mode, sandbox, false)
    }

    /// Hand the runtime the token that retires its process, so
    /// `ShutdownWhenIdle` can actually end the daemon once work drains. An
    /// in-process runtime leaves this unset: it does not own a process.
    pub fn with_process_shutdown(mut self, token: CancellationToken) -> Self {
        self.process_shutdown = Some(token);
        self
    }

    /// Share the transport's attached-UI counter with this runtime, so idle
    /// eviction can tell `no client` from `a client is watching`.
    pub fn with_client_presence(mut self, waiters: leveler_local_transport::LocalWaiters) -> Self {
        self.client_presence = Some(waiters);
        self
    }

    /// Retire the process once there is no attached client and nothing is
    /// owed, for `idle_timeout`.
    ///
    /// This is resource reclamation, NOT generation handover: a daemon exists
    /// to outlive the TUI that started its work, so the countdown only starts
    /// when `active_turns == 0`, `active_background_tasks == 0`, and no local
    /// UI is attached. Any of those becoming true again resets it. A runtime
    /// already retiring (`ShutdownWhenIdle`) or quitting owns shutdown, so
    /// this supervisor stands down rather than racing it.
    ///
    /// A runtime with no process token or no client counter (the embedded
    /// path) has nothing to evict and this is a no-op.
    pub fn spawn_idle_eviction(&self, idle_timeout: std::time::Duration) {
        let Some(token) = self.process_shutdown.clone() else {
            return;
        };
        let Some(clients) = self.client_presence.clone() else {
            return;
        };
        let active = self.active.clone();
        let background = self.app.background_tasks().clone();
        let shutting_down = self.shutting_down.clone();
        tokio::spawn(async move {
            // Probe often enough that the timeout is honoured closely, without
            // a busy loop when the timeout is long.
            let poll = (idle_timeout / 5).max(std::time::Duration::from_millis(20));
            let mut idle_since = tokio::time::Instant::now();
            loop {
                // Retiring/Quit own the shutdown; never race them.
                if shutting_down.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                let quiescence = runtime_quiescence(&active, &background).await;
                if let Err(error) = &quiescence {
                    tracing::error!(%error, "cannot observe idle runtime blockers");
                }
                if quiescence.is_ok_and(RuntimeQuiescence::quiescent) && clients.count() == 0 {
                    if idle_since.elapsed() >= idle_timeout {
                        tracing::info!(
                            idle_secs = idle_timeout.as_secs(),
                            "no clients and no work; retiring the idle runtime"
                        );
                        token.cancel();
                        return;
                    }
                } else {
                    idle_since = tokio::time::Instant::now();
                }
                tokio::time::sleep(poll).await;
            }
        });
    }

    /// Whether this process currently holds a main-turn or context-op lease
    /// for `session_id`.
    ///
    /// Distinct from a durable `turns.status = running` row: EventBridge
    /// releases the lease *after* the terminal client event is enqueued, so
    /// the row can commit while this is still true. A follow-up command
    /// (`RestoreCheckpoint`, `SubmitMessage`, compact) that admits the same
    /// session must wait for this to go false, not only for `list_running`.
    pub fn has_live_turn(&self, session_id: &SessionId) -> bool {
        self.active.is_running(session_id)
    }

    /// Start the continuation a durable user answer is still owed, if this
    /// session is in exactly that state.
    ///
    /// Idempotent with the answer and admission paths through the same gate:
    /// whichever caller arrives first admits the window, and every later caller
    /// sees a live turn and stops.
    async fn resume_owed_clarification(&self, session_id: &SessionId) -> Result<bool, ClientError> {
        let db = self
            .app
            .open_database()
            .await
            .map_err(|error| ClientError::Runtime(error.to_string()))?;
        let stores = leveler_storage::EngineStores::from_database(&db);
        let _gate = self.durable_clarify.lock().await;
        if self.active.is_running(session_id) {
            return Ok(false);
        }
        if !clarification_continuation_is_owed(&db, &stores, session_id).await? {
            return Ok(false);
        }
        self.continue_after_durable_answer(session_id).await?;
        Ok(true)
    }

    /// Start every continuation a crash between a durable answer and its next
    /// window left owed.
    ///
    /// The scan is bounded by goals that still owe work, not by session
    /// history. One session that cannot continue (for example its model was
    /// removed) is reported and does not stop the recovery of the others.
    pub async fn reconcile_owed_clarification_continuations(&self) -> Result<usize, ClientError> {
        let db = self
            .app
            .open_database()
            .await
            .map_err(|error| ClientError::Runtime(error.to_string()))?;
        let stores = leveler_storage::EngineStores::from_database(&db);
        let goals = stores
            .goals
            .unfinished()
            .await
            .map_err(|error| ClientError::Runtime(format!("owed clarification: {error}")))?;
        let mut started = 0;
        for goal in goals {
            let Some(session_id) = stores
                .tasks
                .session_for_task(&goal.task_id)
                .await
                .map_err(|error| ClientError::Runtime(format!("owed clarification: {error}")))?
            else {
                continue;
            };
            match self.resume_owed_clarification(&session_id).await {
                Ok(true) => started += 1,
                Ok(false) => {}
                Err(error) => tracing::warn!(
                    %error,
                    session = %session_id,
                    "an owed clarification continuation could not start"
                ),
            }
        }
        Ok(started)
    }

    /// Like [`Self::new`], with an explicit auto-approve switch for unattended
    /// interactive TUI sessions.
    pub fn new_with_options(
        app: Arc<Application>,
        model: ModelRef,
        mode: PermissionProfile,
        sandbox: bool,
        auto_approve: bool,
    ) -> Self {
        Self::build(app, Some(model), mode, sandbox, auto_approve)
    }

    /// Build a runtime with no default model.
    ///
    /// Deterministic execution (`RunUserShell`, scheduled actions) never
    /// consults a model, so the runtime that serves it does not require one.
    /// Creating a session is still a legitimate operation; every agent turn on
    /// such a session fails with an accurate model-configuration error.
    pub fn new_without_model(
        app: Arc<Application>,
        mode: PermissionProfile,
        sandbox: bool,
        auto_approve: bool,
    ) -> Self {
        Self::build(app, None, mode, sandbox, auto_approve)
    }

    fn build(
        app: Arc<Application>,
        model: Option<ModelRef>,
        mode: PermissionProfile,
        sandbox: bool,
        auto_approve: bool,
    ) -> Self {
        let (events, _) = broadcast::channel(2048);
        let session_events = Arc::new(SessionStreams::default());
        let media_root = app.layout.state_dir.join("media");
        let in_flight = app.in_flight_commands();
        // One flag, shared: the runtime reports it and admission enforces it.
        let shutting_down = Arc::new(std::sync::atomic::AtomicBool::new(false));
        Self {
            app,
            default_runtime: SessionRuntimeConfig {
                model,
                mode,
                sandbox,
                // A session the transport creates resolves its axis from the
                // request; this is the fallback for a session attached without
                // one. Same product default as a fresh Application.
                collaboration: leveler_lifecycle::CollaborationMode::default()
                    .as_str()
                    .to_string(),
                // Per-session default; the daemon-wide `auto_approve` still
                // applies as the fallback in `approver`.
                approval_policy: ApprovalPolicy::Interactive,
                thinking: None,
            },
            session_runtime: Arc::new(Mutex::new(HashMap::new())),
            settings_edits: Arc::new(tokio::sync::Mutex::new(())),
            auto_approve,
            media_root,
            events,
            session_events,
            pending: Arc::new(Mutex::new(HashMap::new())),
            pending_clarify: Arc::new(Mutex::new(HashMap::new())),
            durable_clarify: Arc::new(tokio::sync::Mutex::new(())),
            steering: Arc::new(Mutex::new(HashMap::new())),
            goal_steering: Arc::new(Mutex::new(HashMap::new())),
            child_cancels: Arc::new(Mutex::new(HashMap::new())),
            tool_call_cancels: Arc::new(Mutex::new(HashMap::new())),
            tool_call_yields: Arc::new(Mutex::new(HashMap::new())),
            auxiliary_assists: Arc::new(AuxiliaryAssists::default()),
            config_announced: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            user_shells: Arc::new(crate::user_shell::UserShellStore::default()),
            checkpoints: Arc::new(crate::checkpoints::CheckpointStore::default()),
            live_views: Arc::new(crate::live_view::LiveViews::default()),
            btw: Arc::new(Mutex::new(HashMap::new())),
            active: Arc::new(ActiveTurns::with_retiring(shutting_down.clone())),
            shutting_down: shutting_down.clone(),
            retiring_reason: Arc::new(std::sync::Mutex::new(None)),
            process_shutdown: None,
            client_presence: None,
            in_flight,
        }
    }

    /// Relay automatically applied configuration generations to every attached
    /// client: one event, one refreshed snapshot per open session. Called by
    /// the composition roots that also start the watcher.
    pub fn spawn_config_bridge(self: &Arc<Self>) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let mut applied = self.app.config_manager().subscribe();
        let mut failures = self.app.config_manager().subscribe_failures();
        let weak = Arc::downgrade(self);
        handle.spawn(async move {
            loop {
                tokio::select! {
                    received = applied.recv() => match received {
                        Ok(applied) => {
                            let Some(client) = weak.upgrade() else {
                                break;
                            };
                            client.announce_applied_config(&applied, None).await;
                        }
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            tracing::warn!(skipped, "config change stream lagged");
                            // A missed generation still leaves the runtime on the
                            // latest one; ask it for the truth rather than replaying
                            // a stale event.
                            let Some(client) = weak.upgrade() else {
                                break;
                            };
                            let snapshot = client.app.config_snapshot();
                            let applied = crate::config_manager::ConfigApplied {
                                version: snapshot.version,
                                previous_version: snapshot.version,
                                change: Default::default(),
                                provider: None,
                                model: snapshot.loaded.default_model.clone(),
                                thinking: None,
                            };
                            client.announce_applied_config(&applied, None).await;
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    },
                    received = failures.recv() => match received {
                        Ok((version, error)) => {
                            let Some(client) = weak.upgrade() else {
                                break;
                            };
                            client.announce_reload_failure(version, error, None);
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    },
                }
            }
        });
    }

    /// Publish one applied generation to every attached client. Effects are
    /// announced at most once per version, so a watcher reload and a `/reload`
    /// that observed the same generation do not notify twice.
    async fn announce_applied_config(
        &self,
        applied: &crate::config_manager::ConfigApplied,
        query_id: Option<CommandId>,
    ) {
        let fresh = self
            .config_announced
            .swap(applied.version, std::sync::atomic::Ordering::SeqCst)
            != applied.version;
        let running = self.active.load().0 > 0;
        let event = RuntimeEvent::ConfigReloaded {
            query_id,
            outcome: "applied".to_string(),
            version: applied.version,
            error: None,
            provider: applied.provider.clone(),
            model: applied.model.clone(),
            thinking: applied.thinking.clone(),
            changed: applied
                .change
                .labels()
                .into_iter()
                .map(str::to_string)
                .collect(),
            deferred_to_next_turn: running,
            restart_required: Self::restart_required(applied),
        };
        self.broadcast_event(event);
        if !fresh {
            return;
        }
        if applied.change.default_model || applied.change.models || applied.change.thinking {
            self.refresh_open_sessions().await;
        }
    }

    /// Settings that the applied generation changed but that cannot be applied
    /// to a live process. Named facts, never a silent success.
    fn restart_required(applied: &crate::config_manager::ConfigApplied) -> Vec<String> {
        let mut components = Vec::new();
        if applied.change.ui {
            components.push("ui_language".to_string());
        }
        if applied.change.browser {
            components.push("browser".to_string());
        }
        components
    }

    /// Report a load that kept the previous generation. Deliberately NOT a
    /// success with a warning: the outcome string is `failed` and the reason
    /// is the load error.
    fn announce_reload_failure(&self, version: u64, error: String, query_id: Option<CommandId>) {
        self.broadcast_event(RuntimeEvent::ConfigReloaded {
            query_id,
            outcome: "failed".to_string(),
            version,
            error: Some(error),
            provider: None,
            model: None,
            thinking: None,
            changed: Vec::new(),
            deferred_to_next_turn: false,
            restart_required: Vec::new(),
        });
    }

    /// Send one runtime event to the global compatibility stream AND every
    /// open session stream: a configuration fact belongs to the runtime, and a
    /// client watching any session must learn it.
    fn broadcast_event(&self, event: RuntimeEvent) {
        let _ = self.events.send(event.clone());
        let streams = self
            .session_events
            .streams
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for stream in streams.values() {
            let _ = stream.events.send(event.clone());
        }
    }

    /// Republish metadata for every session this runtime already knows, so a
    /// client adopts the model/thinking/available-model list the NEXT turn will
    /// use. A running turn's snapshot still names the model it is using.
    async fn refresh_open_sessions(&self) {
        let mut ids: Vec<SessionId> = self
            .session_events
            .streams
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .cloned()
            .collect();
        ids.extend(
            self.session_runtime
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .keys()
                .cloned(),
        );
        ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        ids.dedup();
        for session_id in ids {
            if let Ok(session) = self.snapshot(&session_id).await {
                let _ = self
                    .events_for(&session_id)
                    .send(RuntimeEvent::SessionUpdated { session });
            }
        }
    }

    /// `/reload`: force a fresh read of every configured source and report what
    /// actually happened. A running turn is never interrupted; it keeps the
    /// generation it started with.
    async fn reload_runtime_config(&self, query_id: Option<CommandId>) -> Result<(), ClientError> {
        let outcome = self
            .app
            .reload_config(crate::config_manager::ReloadOrigin::Command, true)
            .await;
        match outcome {
            crate::config_manager::ReloadOutcome::Applied(applied) => {
                self.announce_applied_config(&applied, query_id).await;
            }
            crate::config_manager::ReloadOutcome::Unchanged { version } => {
                self.broadcast_event(RuntimeEvent::ConfigReloaded {
                    query_id,
                    outcome: "unchanged".to_string(),
                    version,
                    error: None,
                    provider: None,
                    model: None,
                    thinking: None,
                    changed: Vec::new(),
                    deferred_to_next_turn: false,
                    restart_required: Vec::new(),
                });
            }
            crate::config_manager::ReloadOutcome::Failed { error, version } => {
                self.announce_reload_failure(version, error, query_id);
            }
        }
        Ok(())
    }

    /// Answer a delivery that found its command's receipt `dispatching`.
    ///
    /// Settled meanwhile → that answer. Admitted by an unknown boot (a row
    /// from before boots were recorded) → no answer. Admitted by this boot →
    /// still being handled while it is in flight, abandoned otherwise.
    /// Admitted by another boot → still being handled while that boot holds
    /// its lease, abandoned once the lease is free. Only an abandoned receipt
    /// becomes unresolvable, durably, so later deliveries need no probe.
    async fn answer_unsettled(
        &self,
        receipts: &leveler_storage::CommandReceiptRepository<'_>,
        envelope: &CommandEnvelope,
        fingerprint: &str,
    ) -> Result<(), ClientError> {
        let command_id = &envelope.command_id;
        let unknown = || {
            ClientError::OutcomeUnknown(format!(
                "command {} is still being dispatched, or its outcome is not yet known",
                command_id.as_str()
            ))
        };
        let abandoned = match receipts
            .dispatching_boot(command_id)
            .await
            .map_err(|e| ClientError::OutcomeUnknown(e.to_string()))?
        {
            leveler_storage::DispatchingBoot::NotDispatching => false,
            leveler_storage::DispatchingBoot::Unknown => return Err(unknown()),
            leveler_storage::DispatchingBoot::Boot(boot)
                if self.app.started_boot_id().as_ref() == Some(&boot) =>
            {
                !self
                    .in_flight
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .contains_key(command_id.as_str())
            }
            leveler_storage::DispatchingBoot::Boot(boot) => {
                crate::runtime_boot::boot_liveness(&self.app.layout.state_dir, &boot)
                    == leveler_core::BootLiveness::Dead
            }
        };
        if abandoned
            && receipts
                .mark_unresolvable(command_id)
                .await
                .map_err(|e| ClientError::OutcomeUnknown(e.to_string()))?
        {
            return Err(unresolvable(command_id));
        }
        // Not abandoned, or it settled between the read and the write: answer
        // from whatever the receipt says now.
        match receipts
            .classify_terminal(command_id, &envelope.session_id, fingerprint)
            .await
            .map_err(|e| ClientError::OutcomeUnknown(e.to_string()))?
        {
            Some(leveler_storage::Admission::AlreadyCompleted) => Ok(()),
            Some(leveler_storage::Admission::Unresolvable) => Err(unresolvable(command_id)),
            _ => Err(unknown()),
        }
    }

    /// Session settings are read from the same durable authority as snapshots.
    async fn session_model(&self, session_id: &SessionId) -> Result<ModelRef, ClientError> {
        let config = self.runtime_config(session_id).await?;
        require_model(&config, session_id)
    }

    /// Everything a memory management action attributes its model call to: the
    /// real session database and the session's configured route.
    async fn memory_action_context(
        &self,
        session_id: &SessionId,
    ) -> Result<(Database, ModelRef), String> {
        let db = self
            .app
            .open_database()
            .await
            .map_err(|error| format!("memory action database unavailable: {error}"))?;
        let model = self
            .session_model(session_id)
            .await
            .map_err(|error| error.to_string())?;
        Ok((db, model))
    }

    async fn save_agent(
        &self,
        session_id: SessionId,
        scope: leveler_client_protocol::UiAgentScope,
        draft: leveler_client_protocol::UiAgentDraft,
        query_id: Option<leveler_core::CommandId>,
        create: bool,
    ) {
        let name = draft.name.trim().to_string();
        let result = async {
            let model = self
                .session_model(&session_id)
                .await
                .map_err(|error| error.to_string())?;
            self.app
                .save_agent(scope, &draft, create, Some(&model))
                .await
        }
        .await;
        let (ok, error, agent) = match result {
            Ok(entry) => (true, None, Some(entry)),
            Err(error) => (false, Some(error), None),
        };
        let _ = self
            .events_for(&session_id)
            .send(RuntimeEvent::AgentMutated {
                query_id,
                name,
                ok,
                error,
                agent,
            });
    }

    fn forget_session(&self, session_id: &SessionId) {
        if let Some(stream) = self
            .session_events
            .streams
            .lock()
            .unwrap()
            .remove(session_id)
        {
            stream.cancellation.cancel();
            stream.events.close();
        }
        self.session_runtime.lock().unwrap().remove(session_id);
        self.live_views.clear(session_id);
        self.checkpoints.drop_session(session_id);
        self.drop_btw_thread(session_id);
        self.user_shells.drop_session(session_id);
        self.steering.lock().unwrap().remove(session_id);
        self.goal_steering.lock().unwrap().remove(session_id);
        // The session's cached prompt index and activity outlive the turn that
        // produced them; a deleted session must not keep either in memory.
        self.app.memory().forget_session(session_id.as_str());
    }

    /// Drop session streams nobody is watching once the cache is over its
    /// cap, oldest first.
    ///
    /// A stream is only evicted when it has no subscriber of its own (the
    /// compatibility broadcast does not count — it carries one copy globally,
    /// not one per session), no main turn is running for it, and it has been
    /// idle for the grace period. Eviction closes the stream and cancels its
    /// background bridge task; the next publish for that session recreates
    /// both. The durable event log, not the ring, is what a later client
    /// replays.
    fn evict_idle_session_streams(&self, now: std::time::Instant) {
        let mut streams = self.session_events.streams.lock().unwrap();
        if streams.len() <= SESSION_STREAM_CACHE_CAPACITY {
            return;
        }
        let mut idle: Vec<(std::time::Instant, SessionId)> = streams
            .iter()
            .filter(|(session_id, stream)| {
                stream.events.session_receiver_count() == 0
                    && !self.active.is_running(session_id)
                    && now.saturating_duration_since(stream.last_used) >= SESSION_STREAM_IDLE_GRACE
            })
            .map(|(session_id, stream)| (stream.last_used, session_id.clone()))
            .collect();
        idle.sort_by_key(|(last_used, _)| *last_used);
        let excess = streams.len() - SESSION_STREAM_CACHE_CAPACITY;
        for (_, session_id) in idle.into_iter().take(excess) {
            if let Some(stream) = streams.remove(&session_id) {
                stream.cancellation.cancel();
                stream.events.close();
            }
        }
    }

    fn events_for(&self, session_id: &SessionId) -> crate::event_bridge::RuntimeEventSender {
        let mut streams = self.session_events.streams.lock().unwrap();
        if let Some(stream) = streams.get_mut(session_id) {
            stream.last_used = std::time::Instant::now();
            return stream.events.clone();
        }
        let (sender, _) = broadcast::channel(2048);
        let all_events = self.events.clone();
        let events = crate::event_bridge::RuntimeEventSender::projected(
            sender,
            all_events,
            self.live_views.clone(),
            session_id.clone(),
        );
        let cancellation = self.session_events.shutdown.child_token();
        streams.insert(
            session_id.clone(),
            SessionStream {
                events: events.clone(),
                cancellation: cancellation.clone(),
                last_used: std::time::Instant::now(),
            },
        );
        drop(streams);
        self.evict_idle_session_streams(std::time::Instant::now());
        let background_registry = self.app.background_tasks().clone();
        let mut background_events = background_registry.subscribe();
        let background_session_id = session_id.clone();
        let background_sender = events.clone();
        let background_user_shells = self.user_shells.clone();
        // Weak: the bridge task must not keep the whole application (and its
        // database pool) alive for the life of the registry subscription.
        let background_app = std::sync::Arc::downgrade(&self.app);
        tokio::spawn(async move {
            loop {
                match tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => break,
                    event = background_events.recv() => event,
                } {
                    Ok(leveler_execution::BackgroundTaskEvent::Started { owner_scope, task })
                        if owner_scope.as_deref() == Some(background_session_id.as_str()) =>
                    {
                        let _ = background_sender.send(RuntimeEvent::BackgroundTaskStarted {
                            task_id: task.id,
                            program: task.program,
                            args: task.args,
                            pid: task.pid,
                        });
                    }
                    Ok(leveler_execution::BackgroundTaskEvent::Output {
                        owner_scope,
                        task_id,
                        chunk,
                    }) if owner_scope.as_deref() == Some(background_session_id.as_str()) => {
                        let _ = background_sender
                            .send(RuntimeEvent::BackgroundTaskOutput { task_id, chunk });
                    }
                    // Readiness observed while the process keeps running. It is
                    // forwarded as its own fact so the client can collapse the
                    // log without ever treating the task as finished.
                    Ok(leveler_execution::BackgroundTaskEvent::Ready {
                        owner_scope,
                        task_id,
                        evidence,
                        elapsed_ms,
                    }) if owner_scope.as_deref() == Some(background_session_id.as_str()) => {
                        let _ = background_sender.send(RuntimeEvent::BackgroundTaskReady {
                            task_id,
                            evidence,
                            elapsed_ms,
                        });
                    }
                    Ok(leveler_execution::BackgroundTaskEvent::Exited { owner_scope, task })
                        if owner_scope.as_deref() == Some(background_session_id.as_str()) =>
                    {
                        // The runtime status is the authority, never the exit
                        // code: a killed task can read either way, and a user
                        // or session cleanup stop is not a failure.
                        let stopped =
                            task.status == leveler_execution::BackgroundTaskStatus::Killed;
                        let ok = task.status == leveler_execution::BackgroundTaskStatus::Exited
                            && task.exit_code == Some(0);
                        // An adopted user shell keeps its own execution record
                        // and gets the same terminal facts. The background
                        // registry is the lifecycle authority; this projection
                        // does not create a second task.
                        let shell_status = match task.status {
                            leveler_execution::BackgroundTaskStatus::Exited
                                if task.exit_code == Some(0) =>
                            {
                                "success"
                            }
                            leveler_execution::BackgroundTaskStatus::Exited => "failed",
                            leveler_execution::BackgroundTaskStatus::Killed => "cancelled",
                            leveler_execution::BackgroundTaskStatus::Unknown => "unknown",
                            leveler_execution::BackgroundTaskStatus::Running
                            | leveler_execution::BackgroundTaskStatus::Killing => "unknown",
                        };
                        let registry_output = task.log.clone();
                        let registry_truncated = task.log_truncated;
                        if let Some(execution_id) = background_user_shells
                            .execution_for_task(&background_session_id, &task.id)
                        {
                            // The registry log tags every chunk with its
                            // stream. The user-shell record is raw source
                            // output, so a complete capture replaces it; the
                            // registry log is only the fallback when no capture
                            // finished.
                            let capture = background_app.upgrade().and_then(|app| {
                                crate::user_shell::complete_capture_tail(
                                    &crate::user_shell::output_directory(
                                        &app.layout.state_dir,
                                        &background_session_id,
                                        &execution_id,
                                    ),
                                    crate::user_shell::OUTPUT_TAIL_CAP,
                                )
                            });
                            let (shell_output, shell_truncated) = match capture {
                                Some((text, truncated)) => (text, truncated),
                                None => (registry_output, registry_truncated),
                            };
                            let finished = background_user_shells.finish_background(
                                &background_session_id,
                                &task.id,
                                task.exit_code,
                                shell_status,
                                task.duration_ms,
                                Some(&shell_output),
                            );
                            if let Some(shell) = &finished {
                                // The live background view may have dropped
                                // chunks too; replace it with the retained
                                // record before the terminal event.
                                let _ =
                                    background_sender.send(RuntimeEvent::UserShellOutputReplaced {
                                        execution_id: execution_id.clone(),
                                        output_tail: shell.output_tail.clone(),
                                        output_truncated: shell.output_truncated,
                                    });
                                // The registry's retained log is capped. The
                                // exact capture lives beside it; announce it
                                // as a structured fact rather than injecting a
                                // path line into the output text.
                                if let Some(directory) = &shell.full_output {
                                    let _ = background_sender.send(
                                        RuntimeEvent::UserShellCaptureReady {
                                            execution_id: execution_id.clone(),
                                            directory: directory.clone(),
                                        },
                                    );
                                }
                                let _ = background_sender.send(RuntimeEvent::UserShellExited {
                                    execution_id: execution_id.clone(),
                                    exit_code: shell.exit_code,
                                    duration_ms: task.duration_ms,
                                    status: shell_status.to_string(),
                                });
                            }
                            if let Some(app) = background_app.upgrade() {
                                let session = background_session_id.clone();
                                let exit_code = task.exit_code;
                                let duration_ms = task.duration_ms;
                                let status = shell_status.to_string();
                                let full_output = finished
                                    .as_ref()
                                    .and_then(|shell| shell.full_output.clone());
                                tokio::spawn(async move {
                                    persist_background_user_shell_finished(
                                        app,
                                        session,
                                        execution_id,
                                        exit_code,
                                        duration_ms,
                                        status,
                                        shell_output,
                                        shell_truncated,
                                        full_output,
                                    )
                                    .await;
                                });
                            }
                        }
                        // Keep the terminal fact for resume even when no
                        // observer is waiting for the originating tool.
                        if let Some(app) = background_app.upgrade() {
                            persist_background_task_finished(
                                &app,
                                background_session_id.clone(),
                                &task,
                            )
                            .await;
                        }
                        if task.status == leveler_execution::BackgroundTaskStatus::Unknown {
                            let _ = background_sender.send(RuntimeEvent::BackgroundTaskUnknown {
                                task_id: task.id,
                                duration_ms: task.duration_ms,
                                output: task.log,
                            });
                        } else {
                            let _ = background_sender.send(RuntimeEvent::BackgroundTaskExited {
                                task_id: task.id,
                                exit_code: task.exit_code,
                                duration_ms: task.duration_ms,
                                ok,
                                stopped,
                                output: task.log,
                            });
                        }
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "background lifecycle stream lagged");
                        // Drop the lagged cursor first. Events after this
                        // subscription either appear in the snapshot or remain
                        // queued for the next receive, so the replacement
                        // cannot leave a stale active id behind.
                        background_events = background_registry.subscribe();
                        let snapshots = match background_registry.try_all_snapshots().await {
                            Ok(tasks) => tasks,
                            Err(error) => {
                                let _ = background_sender.send(RuntimeEvent::Notification {
                                    level: NotificationLevel::Error,
                                    message: format!("无法读取后台任务状态: {error}"),
                                });
                                continue;
                            }
                        };
                        let owned = snapshots.into_iter().filter(|task| {
                            task.owner_scope.as_deref() == Some(background_session_id.as_str())
                        });
                        let mut tasks = Vec::new();
                        let app = background_app.upgrade();
                        for task in owned {
                            if task.status.is_terminal() {
                                if let Some(app) = &app {
                                    persist_background_task_finished(
                                        app,
                                        background_session_id.clone(),
                                        &task,
                                    )
                                    .await;
                                }
                            } else {
                                tasks.push(task);
                            }
                        }
                        let tasks = tasks
                            .into_iter()
                            .map(|task| leveler_client_protocol::UiActiveBackgroundTask {
                                task_id: task.id,
                                program: task.program,
                                args: task.args,
                                elapsed_ms: task.duration_ms,
                                pid: task.pid,
                                ready_evidence: task.ready.map(|ready| ready.evidence),
                                lifetime: Some(task.lifetime.as_str().to_string()),
                            })
                            .collect();
                        let _ = background_sender
                            .send(RuntimeEvent::BackgroundTasksReconciled { tasks });
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        events
    }

    /// Associate an externally created session with this client's defaults.
    /// Legacy in-process entry points create the session through `Application`
    /// before constructing the runtime client.
    ///
    /// It only SEEDS a session this client has no live config for. A session
    /// whose config was already written by `persist_runtime_config` (create,
    /// `/clear`, `SetPermissionProfile`) keeps that value: re-inserting the
    /// client's launch defaults would overwrite the durable mode with a stale
    /// one — a second truth source for the same session.
    pub fn attach_session(&self, session_id: SessionId) {
        self.session_runtime
            .lock()
            .unwrap()
            .entry(session_id)
            .or_insert_with(|| self.default_runtime.clone());
    }

    /// The effective approval policy the daemon would use for `session_id` —
    /// the live per-session config if present, else the DB-restored config
    /// (which never carries a persisted policy, so it falls back to
    /// `Interactive`). A read-only diagnostic: lets a caller (and a regression)
    /// confirm a restored session is fail-closed and never silently auto-approves.
    pub async fn effective_approval_policy(&self, session_id: &SessionId) -> ApprovalPolicy {
        self.runtime_config(session_id)
            .await
            .map(|config| config.approval_policy)
            .unwrap_or(ApprovalPolicy::Interactive)
    }

    async fn runtime_config(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionRuntimeConfig, ClientError> {
        let db = self
            .app
            .open_database()
            .await
            .map_err(|error| ClientError::Runtime(error.to_string()))?;
        let settings = SessionRepository::new(&db)
            .settings_snapshot(session_id)
            .await
            .map_err(|error| ClientError::Runtime(error.to_string()))?
            .ok_or_else(|| ClientError::SessionNotFound(session_id.clone()))?;
        let config = self.config_from_settings(session_id, &settings)?;
        Ok(install_persisted_config(
            &self.session_runtime,
            session_id,
            config,
        ))
    }

    /// Publish the session row after a turn that may have changed its routes.
    ///
    /// Two facts live only in the durable session projection:
    ///
    /// - the collaboration axis. The terminal commit ends Goal mode in the
    ///   SAME transaction as the terminal fact (a goal nothing can continue
    ///   ends the route), so no client may keep painting a Goal turn: the next
    ///   submit would be predicted as Goal while the runtime runs Chat.
    /// - the goal the session still owes. A turn that did not drive it (an
    ///   ordinary question in a Chat session that owes a paused goal) must not
    ///   make clients forget it: the row is how a client re-reads the goal's
    ///   own pause state, and it is the same projection another client, a
    ///   reconnect and a replay read.
    ///
    /// A turn whose axis did not move and which owes no goal — the
    /// overwhelming majority — pays one indexed read and no snapshot.
    async fn publish_routes_after_turn(&self, session_id: &SessionId, axis_at_start: &str) {
        let axis_moved = self
            .app
            .session_product_axes(session_id)
            .await
            .is_ok_and(|axis| axis.as_str() != axis_at_start);
        if !axis_moved && !self.goal_still_owed(session_id).await {
            return;
        }
        if let Ok(session) = self.snapshot(session_id).await {
            let _ = self
                .events_for(session_id)
                .send(RuntimeEvent::SessionUpdated { session });
        }
    }

    /// Whether the session's durable task still has a running goal.
    ///
    /// Best effort, like every other projection read here: a client that cannot
    /// be told keeps the state it already had, and the next snapshot (or the
    /// terminal itself) is the authority.
    async fn goal_still_owed(&self, session_id: &SessionId) -> bool {
        let Ok(db) = self.app.open_database().await else {
            return false;
        };
        let stores = leveler_storage::EngineStores::from_database(&db);
        let Ok(Some(task)) = stores.tasks.task_for_session(session_id).await else {
            return false;
        };
        stores.goals.for_task(&task).await.is_ok_and(|goals| {
            goals
                .iter()
                .any(|goal| goal.state == leveler_storage::GoalState::Running)
        })
    }

    fn config_from_settings(
        &self,
        session_id: &SessionId,
        settings: &leveler_storage::SessionSettingsSnapshot,
    ) -> Result<SessionRuntimeConfig, ClientError> {
        let record = &settings.session;
        self.app
            .validate_session_workspace(record.repository.as_deref())
            .map_err(|error| ClientError::Runtime(error.to_string()))?;
        let approval_policy = self
            .session_runtime
            .lock()
            .unwrap()
            .get(session_id)
            .map(|config| config.approval_policy)
            .unwrap_or(ApprovalPolicy::Interactive);
        Self::decode_settings(
            session_id,
            settings,
            approval_policy,
            self.app.configured_default_model(),
        )
    }

    fn decode_settings(
        session_id: &SessionId,
        settings: &leveler_storage::SessionSettingsSnapshot,
        approval_policy: ApprovalPolicy,
        configured_default: Option<ModelRef>,
    ) -> Result<SessionRuntimeConfig, ClientError> {
        let record = &settings.session;
        let stored = match record.model.as_deref() {
            Some(raw) => Some(ModelRef::parse(raw).ok_or_else(|| {
                ClientError::Runtime(format!(
                    "session {session_id} stores invalid model reference `{raw}`"
                ))
            })?),
            // A deterministic-only session. It has no model to inherit: a
            // later default must not turn it into an agent session behind the
            // user's back.
            None => None,
        };
        // An explicit `/model` choice is durable and wins. Otherwise the session
        // INHERITS the current configured default, so editing `default_model`
        // reaches the next turn without a restart. A default this environment
        // cannot resolve leaves the session on the model it already had — an
        // unusable global default must never strand a working session.
        let model = match (stored, settings.model_pinned) {
            (Some(stored), true) => Some(stored),
            (Some(stored), false) => Some(configured_default.unwrap_or(stored)),
            (None, _) => None,
        };
        let mode = crate::session::mode_from_str(&settings.mode).ok_or_else(|| {
            ClientError::Runtime(format!(
                "session {session_id} stores invalid execution mode `{}`",
                settings.mode
            ))
        })?;
        if !matches!(
            settings.kind.as_str(),
            "direct" | "orchestrate" | "orchestrated"
        ) {
            return Err(ClientError::Runtime(format!(
                "session {session_id} stores invalid execution kind `{}`",
                settings.kind
            )));
        }
        let thinking = settings
            .thinking
            .as_deref()
            .map(|raw| {
                ThinkingLevel::parse(raw).ok_or_else(|| {
                    ClientError::Runtime(format!(
                        "session {session_id} stores invalid thinking level `{raw}`"
                    ))
                })
            })
            .transpose()?;
        Ok(SessionRuntimeConfig {
            model,
            mode,
            sandbox: settings.sandbox,
            collaboration: record.collaboration.clone(),
            approval_policy,
            thinking,
        })
    }

    /// Only the CAS transaction authorizes replacing a setting. Runtime views
    /// read the committed row; they never persist a cached full-row replacement.
    async fn edit_session_settings(
        &self,
        command: &ClientCommand,
        expected: i64,
    ) -> Result<bool, ClientError> {
        use leveler_storage::{SessionEdit, SessionEditResult};
        let _projection_guard = self.settings_edits.lock().await;
        let session_id = command
            .session_id()
            .ok_or_else(|| ClientError::Runtime("setting command requires a session".into()))?;
        let edit = match command {
            ClientCommand::RenameSession { name, .. } => {
                let name = name.trim();
                if name.is_empty() {
                    return Err(ClientError::Runtime("名称不能为空".into()));
                }
                SessionEdit::Title(name.to_string())
            }
            ClientCommand::SelectModel { model, .. }
            | ClientCommand::SetDefaultModel { model, .. } => {
                if !self.app.model_refs().contains(model) {
                    return Err(ClientError::Runtime(format!(
                        "model `{model}` is not configured"
                    )));
                }
                SessionEdit::Model(model.to_string())
            }
            ClientCommand::SetPermissionProfile { mode, .. } => {
                SessionEdit::Permission(execution_mode(*mode).as_str().to_string())
            }
            ClientCommand::SetThinkingLevel { level, .. } => {
                SessionEdit::Thinking(level.map(|level| level.as_str().to_string()))
            }
            ClientCommand::SetProductAxes { collaboration, .. } => {
                if !matches!(collaboration.as_str(), "chat" | "plan" | "goal") {
                    return Err(ClientError::Runtime(format!(
                        "invalid collaboration `{collaboration}`"
                    )));
                }
                SessionEdit::Collaboration(collaboration.clone())
            }
            _ => {
                return Err(ClientError::Runtime(
                    "version policy command has no setting implementation".into(),
                ));
            }
        };
        let db = self
            .app
            .open_database()
            .await
            .map_err(|error| ClientError::Runtime(error.to_string()))?;
        match SessionRepository::new(&db)
            .edit_at_version(session_id, expected, &edit, leveler_core::now())
            .await
            .map_err(|error| ClientError::Runtime(error.to_string()))?
        {
            SessionEditResult::Conflict(_) => return Ok(false),
            SessionEditResult::Applied(_) => {}
        }
        // Leaving Goal mode through the general axis command must also drop
        // the in-flight declaration. Otherwise a Goal declared while a turn
        // ran would keep `goal_requested()` true and upgrade a LATER Chat turn
        // — a second, invisible writer for the axis.
        if let ClientCommand::SetProductAxes { collaboration, .. } = command
            && collaboration == "chat"
        {
            let goal = self.goal_state_for(session_id);
            let mut goal = goal
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            goal.requested = false;
            goal.pending.clear();
        }
        if matches!(
            command,
            ClientCommand::SelectModel { .. } | ClientCommand::SetDefaultModel { .. }
        ) {
            // The resolved context policy is per-model: the published accounting
            // names the previous model's window, reservation and soft threshold.
            // Keeping it would show a denominator the next request never uses,
            // so drop it. The next assembled request republishes it under the
            // new policy; until then the client shows nothing.
            self.live_views.clear_context(session_id);
        }
        if matches!(command, ClientCommand::SetPermissionProfile { .. }) {
            let config = self.runtime_config(session_id).await?;
            self.app
                .set_live_permission_profile(session_id.as_str(), config.mode);
            supersede_pending_approvals(&self.pending, session_id);
        }
        if let ClientCommand::SetDefaultModel { model, .. } = command {
            // A persistent default is read by the NEXT bare launch, whose
            // environment may not contain a model that only a `--config-dir`
            // bundle offered. Writing it anyway is how a temporary environment
            // bricks the default one (`model … is not configured` at startup),
            // so the choice stays session-scoped and the user is told why.
            if self.app.model_is_in_default_environment(model) {
                crate::global_config::GlobalConfig::save_default_model(model).map_err(|error| {
                    ClientError::Runtime(format!(
                        "已切换到 {model}，但默认模型保存失败：{error}；下次启动可能恢复原模型"
                    ))
                })?;
                // The file write is not application. Reconcile the RUNNING
                // runtime now so this process never serves a generation older
                // than the file it just wrote (the watcher would get there a
                // moment later; a turn started in between must not miss it).
                if let crate::config_manager::ReloadOutcome::Failed { error, .. } = self
                    .app
                    .reload_config(crate::config_manager::ReloadOrigin::Write, true)
                    .await
                {
                    return Err(ClientError::Runtime(format!(
                        "已切换到 {model} 并写入默认模型，但运行时应用失败：{error}；\
                         运行时仍在使用上一版有效配置"
                    )));
                }
            } else {
                let _ = self
                    .events_for(session_id)
                    .send(RuntimeEvent::Notification {
                        level: leveler_client_protocol::NotificationLevel::Warning,
                        message: format!(
                            "已切换模型 {model}，但未写入默认：它只在自定义配置目录里，\
                             写入会让下次普通启动失败"
                        ),
                    });
            }
        }
        let session = self.snapshot(session_id).await?;
        let _ = self
            .events_for(session_id)
            .send(RuntimeEvent::SessionUpdated { session });
        if matches!(command, ClientCommand::RenameSession { .. }) {
            let _ = self.events.send(RuntimeEvent::SessionList {
                sessions: self.list_sessions().await?,
            });
        }
        Ok(true)
    }

    async fn persist_turn_collaboration(
        &self,
        session_id: &SessionId,
        collaboration: &str,
    ) -> Result<(), ClientError> {
        let _projection_guard = self.settings_edits.lock().await;
        let db = self
            .app
            .open_database()
            .await
            .map_err(|error| ClientError::Runtime(error.to_string()))?;
        SessionRepository::new(&db)
            .set_turn_collaboration(session_id, collaboration, leveler_core::now())
            .await
            .map_err(|error| ClientError::Runtime(error.to_string()))
    }

    /// Enter Goal mode and record the declaration in one durable write.
    ///
    /// The Goal axis without an objective is a state no client can explain
    /// and no recovery can repair; this is the only writer for `/goal <task>`.
    async fn persist_goal_declaration(
        &self,
        session_id: &SessionId,
        objective: &str,
    ) -> Result<(), ClientError> {
        let _projection_guard = self.settings_edits.lock().await;
        let db = self
            .app
            .open_database()
            .await
            .map_err(|error| ClientError::Runtime(error.to_string()))?;
        // Store the objective trimmed: the bind step compares this exact
        // text, and surrounding whitespace would make the CAS miss.
        SessionRepository::new(&db)
            .set_goal_declaration(
                session_id,
                leveler_lifecycle::CollaborationMode::Goal.as_str(),
                objective.trim(),
                leveler_core::now(),
            )
            .await
            .map_err(|error| ClientError::Runtime(error.to_string()))
    }

    /// `/resume-claude` / `/resume-codex` with no id: list this project's
    /// sessions from the source agent's local store for the picker.
    ///
    /// Discovery is bounded by the adapter crate (newest first, age and count
    /// caps) and reads the source store without writing to it.
    async fn handle_list_foreign_sessions(
        &self,
        requester_session_id: SessionId,
        tool: UiForeignTool,
    ) -> Result<(), ClientError> {
        let adapter = crate::foreign_import::adapter_tool(tool);
        let environment = self.app.environment.clone();
        let cwd = self
            .app
            .layout
            .primary_workspace()
            .map(|path| path.to_path_buf());
        let listed = tokio::task::spawn_blocking(move || {
            let root = leveler_foreign_session::default_root(adapter, &environment)
                .ok_or_else(|| "无法确定用户主目录（HOME / USERPROFILE 未设置）".to_string())?;
            leveler_foreign_session::discover(
                adapter,
                &root,
                cwd.as_deref(),
                leveler_foreign_session::MAX_SESSIONS_PER_SOURCE,
                std::time::SystemTime::now(),
            )
            .map_err(|error| error.to_string())
        })
        .await;
        let event = match listed {
            Ok(Ok(sessions)) => RuntimeEvent::ForeignSessionList {
                tool,
                sessions: sessions
                    .iter()
                    .map(crate::foreign_import::ui_session)
                    .collect(),
                error: None,
            },
            Ok(Err(error)) => RuntimeEvent::ForeignSessionList {
                tool,
                sessions: Vec::new(),
                error: Some(error),
            },
            Err(error) => RuntimeEvent::ForeignSessionList {
                tool,
                sessions: Vec::new(),
                error: Some(format!("本地扫描线程失败: {error}")),
            },
        };
        let _ = self.events_for(&requester_session_id).send(event);
        Ok(())
    }

    /// Import one foreign session and continue it in a NEW native session.
    ///
    /// The source is read once in a blocking thread (find + bounded digest),
    /// then never trusted again: the history tool re-reads the exact file on
    /// demand. Nothing is replayed and no source file is written.
    async fn handle_import_foreign_session(
        &self,
        requester_session_id: SessionId,
        tool: UiForeignTool,
        native_id: String,
    ) -> Result<(), ClientError> {
        let adapter = crate::foreign_import::adapter_tool(tool);
        let environment = self.app.environment.clone();
        let id = native_id.clone();
        let loaded = tokio::task::spawn_blocking(move || {
            let root = leveler_foreign_session::default_root(adapter, &environment)
                .ok_or_else(|| "无法确定用户主目录（HOME / USERPROFILE 未设置）".to_string())?;
            let summary = leveler_foreign_session::find(adapter, &root, &id)
                .map_err(|error| error.to_string())?;
            let digest =
                leveler_foreign_session::digest(&summary).map_err(|error| error.to_string())?;
            Ok::<_, String>((summary, digest))
        })
        .await;
        let (summary, digest) = match loaded {
            Ok(Ok(loaded)) => loaded,
            Ok(Err(message)) => {
                self.send_foreign_import_failure(&requester_session_id, tool, native_id, message);
                return Ok(());
            }
            Err(error) => {
                self.send_foreign_import_failure(
                    &requester_session_id,
                    tool,
                    native_id,
                    format!("读取线程失败: {error}"),
                );
                return Ok(());
            }
        };

        let mut config = match self.runtime_config(&requester_session_id).await {
            Ok(config) => config,
            Err(error) => {
                self.send_foreign_import_failure(
                    &requester_session_id,
                    tool,
                    native_id,
                    format!("无法读取当前会话配置: {error}"),
                );
                return Ok(());
            }
        };
        let Some(model) = config
            .model
            .clone()
            .or_else(|| self.app.configured_default_model())
        else {
            self.send_foreign_import_failure(
                &requester_session_id,
                tool,
                native_id,
                "当前没有可用模型；先配置模型再导入".to_string(),
            );
            return Ok(());
        };
        config.model = Some(model.clone());
        // Imported work is evidence, not a goal declaration: the continuation
        // starts in Chat and the person can promote it with `/goal`.
        config.collaboration = leveler_lifecycle::CollaborationMode::Chat
            .as_str()
            .to_string();
        let title: String = digest
            .summary
            .title
            .trim()
            .chars()
            .take(80)
            .collect::<String>()
            .trim()
            .to_string();
        let title = if title.is_empty() {
            format!("继续 {} 会话", tool.display_name())
        } else {
            title
        };
        let session_id = match self
            .app
            .create_daemon_session(
                Some(&model),
                &title,
                config.mode,
                leveler_lifecycle::CollaborationMode::Chat,
                false,
            )
            .await
        {
            Ok(session_id) => session_id,
            Err(error) => {
                self.send_foreign_import_failure(
                    &requester_session_id,
                    tool,
                    native_id,
                    format!("新建会话失败: {error}"),
                );
                return Ok(());
            }
        };

        let warning = crate::foreign_import::cwd_mismatch(
            summary.cwd.as_deref(),
            self.app.layout.primary_workspace(),
        );
        let source_cwd = summary.cwd.as_ref().map(|cwd| cwd.display().to_string());
        let prepared: Result<(), String> = async {
            self.persist_runtime_config(&session_id, config.clone())
                .await
                .map_err(|error| error.to_string())?;
            let db = self
                .app
                .open_database()
                .await
                .map_err(|error| error.to_string())?;
            ForeignSessionOriginRepository::new(&db)
                .insert(
                    session_id.as_str(),
                    tool.as_str(),
                    &summary.native_id,
                    &summary.source_path.display().to_string(),
                    source_cwd.as_deref(),
                    leveler_core::now(),
                )
                .await
                .map_err(|error| error.to_string())?;
            let text = crate::foreign_import::briefing(
                &digest,
                &leveler_core::now().to_rfc3339(),
                warning.as_deref(),
            );
            let message = Message::user(
                text,
                TranscriptOrigin::RuntimeNotice {
                    notice: leveler_model::RuntimeNoticeKind::ForeignSessionImport,
                },
            );
            let payload = serde_json::to_string(&message).map_err(|error| error.to_string())?;
            MessageRepository::new(&db)
                .append(&session_id, &[payload], leveler_core::now())
                .await
                .map_err(|error| error.to_string())?;
            Ok(())
        }
        .await;
        if let Err(message) = prepared {
            self.discard_session(&session_id).await;
            self.send_foreign_import_failure(&requester_session_id, tool, native_id, message);
            return Ok(());
        }

        let _ = self
            .events_for(&requester_session_id)
            .send(RuntimeEvent::ForeignSessionImported {
                tool,
                native_id,
                session_id: session_id.clone(),
                warning,
            });
        let _ = self.events.send(RuntimeEvent::SessionList {
            sessions: self.list_sessions().await?,
        });
        match self.snapshot(&session_id).await {
            Ok(session) => {
                let _ = self
                    .events_for(&requester_session_id)
                    .send(RuntimeEvent::SessionOpened { session });
            }
            Err(error) => {
                self.notify_error(
                    &requester_session_id,
                    format!("会话已导入，但无法打开新会话: {error}；它已保留在会话列表中"),
                );
                return Ok(());
            }
        }

        // The continuation turn carries only the harness directive; the
        // briefing above is already durable and arrives as prior context.
        let admission = match self.active.admit(&session_id) {
            Ok(admission) => admission,
            Err(error) => {
                self.notify_error(
                    &session_id,
                    format!("会话已导入，但接续回合无法启动: {error}；新会话已保留，可手动继续"),
                );
                return Ok(());
            }
        };
        self.checkpoint_before_turn(&session_id, "导入会话").await;
        let parts = vec![ContentPart::Text {
            text: crate::foreign_import::CONTINUATION_DIRECTIVE.to_string(),
        }];
        let accepted = self.spawn_content_turn(
            session_id.clone(),
            parts,
            TranscriptOrigin::RuntimeNotice {
                notice: leveler_model::RuntimeNoticeKind::ForeignSessionImport,
            },
            admission,
            config,
            model,
        );
        if let Err(error) = await_turn_acceptance(accepted).await {
            self.notify_error(
                &session_id,
                format!("会话已导入，但接续回合启动失败: {error}；新会话已保留，可手动继续"),
            );
        }
        Ok(())
    }

    fn send_foreign_import_failure(
        &self,
        requester_session_id: &SessionId,
        tool: UiForeignTool,
        native_id: String,
        message: String,
    ) {
        let _ =
            self.events_for(requester_session_id)
                .send(RuntimeEvent::ForeignSessionImportFailed {
                    tool,
                    native_id,
                    message,
                });
    }

    async fn persist_runtime_config(
        &self,
        session_id: &SessionId,
        config: SessionRuntimeConfig,
    ) -> Result<(), ClientError> {
        let db = self
            .app
            .open_database()
            .await
            .map_err(|error| ClientError::Runtime(error.to_string()))?;
        let sessions = SessionRepository::new(&db);
        if sessions
            .get(session_id)
            .await
            .map_err(|error| ClientError::Runtime(error.to_string()))?
            .is_none()
        {
            return Err(ClientError::SessionNotFound(session_id.clone()));
        }
        // A deterministic-only session has no model to persist. Writing `NULL`
        // over an existing model would also erase an explicit choice; the row
        // already says what it is.
        if let Some(model) = &config.model {
            sessions
                .update_model(session_id, &model.to_string(), leveler_core::now())
                .await
                .map_err(|error| ClientError::Runtime(error.to_string()))?;
        }
        sessions
            .set_execution(
                session_id,
                config.mode.as_str(),
                config.sandbox,
                "direct",
                leveler_core::now(),
            )
            .await
            .map_err(|error| ClientError::Runtime(error.to_string()))?;
        sessions
            .set_axes(
                session_id,
                &config.collaboration,
                "single",
                leveler_core::now(),
            )
            .await
            .map_err(|error| ClientError::Runtime(error.to_string()))?;
        sessions
            .set_thinking(
                session_id,
                config.thinking.map(|level| level.as_str()),
                leveler_core::now(),
            )
            .await
            .map_err(|error| ClientError::Runtime(error.to_string()))?;
        // One permission change, both projections: the row above is the
        // durable record, this is the same value as RUNNING state. Without
        // it a turn already executing keeps authorizing under the profile it
        // started with while the UI and the row show the new one.
        self.app
            .set_live_permission_profile(session_id.as_str(), config.mode);
        self.session_runtime
            .lock()
            .unwrap()
            .insert(session_id.clone(), config);
        Ok(())
    }

    /// Record a checkpoint at the current transcript length, before a turn runs.
    ///
    /// When the transcript length cannot be determined the checkpoint is
    /// skipped entirely — a fallback ordinal of 0 would restore to an empty
    /// transcript, silently wiping the whole conversation.
    async fn checkpoint_before_turn(&self, session_id: &SessionId, label: &str) {
        let loaded = match self.app.open_database().await {
            Ok(db) => MessageRepository::new(&db)
                .count(session_id)
                .await
                .map(|n| n as usize)
                .map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        let Some(ordinal) = checkpoint_ordinal(loaded) else {
            tracing::warn!(
                session_id = %session_id.as_str(),
                "skipping checkpoint: transcript length unavailable (a 0-ordinal \
                 fallback would restore to an empty conversation)"
            );
            return;
        };
        let label: String = label.chars().take(40).collect();
        let checkpoint = UiCheckpoint {
            id: CheckpointId::generate(),
            label,
            ordinal: ordinal as u32,
        };
        // Capture the workspace too (git repos only), so restoring the
        // checkpoint rolls back the files the turn changed — including
        // command-driven mutations (plan B9 on top of A8).
        let snapshot = match self.app.layout.primary_workspace() {
            None => None,
            Some(root) => match leveler_execution::WorkspaceSnapshot::capture(root).await {
                Ok(snapshot) => snapshot, // None: not a git repo (transcript-only)
                Err(error) => {
                    tracing::warn!("checkpoint workspace snapshot failed: {error}");
                    None
                }
            },
        };
        self.checkpoints
            .record(session_id, checkpoint.clone(), snapshot);
        let _ = self
            .events_for(session_id)
            .send(RuntimeEvent::CheckpointCreated { checkpoint });
    }

    /// Tell the user a direct shell command was rejected, or return the
    /// rejection to the caller.
    ///
    /// A scheduled delivery has no user watching the session. Answering `Ok`
    /// for work the runtime refused would acknowledge a delivery that no
    /// execution will ever carry, leaving the dispatcher to discover the loss
    /// only by timing out into `unknown`. Delivery callers therefore get a
    /// definite error; interactive callers keep the notice-only behaviour.
    fn reject_user_shell(
        &self,
        session_id: &SessionId,
        origin: &Option<CommandId>,
        message: String,
    ) -> Result<(), ClientError> {
        if origin.is_some() {
            Err(ClientError::Runtime(message))
        } else {
            self.notify_error(session_id, message);
            Ok(())
        }
    }

    /// Run one explicit user shell command (`!command`). USER-ORIGINATED
    /// DIRECT EXECUTION: no model, no agent loop, no tool registry. The
    /// ActiveTurns slot enforces the Idle/AgentTurn/UserShell foreground
    /// mutex; host safety (confinement, sandbox, env scrub, tree kill) rides
    /// the same substrate as agent shell execution.
    async fn handle_run_user_shell(
        &self,
        session_id: SessionId,
        command: String,
        origin: Option<CommandId>,
    ) -> Result<(), ClientError> {
        let trimmed = command.trim();
        if trimmed.is_empty() {
            return self.reject_user_shell(
                &session_id,
                &origin,
                "在 ! 后输入要执行的命令".to_string(),
            );
        }
        // The hang guard protects the user too (backgrounding via `&`,
        // credential-file reads, comment-swallowed commands) — same rules as
        // the agent's shell tool, stated to the user instead of the model.
        if let Some(reason) = leveler_tools::tools::refuse_shell_script(trimmed) {
            return self.reject_user_shell(&session_id, &origin, format!("命令被拒绝: {reason}"));
        }
        let config = self.runtime_config(&session_id).await?;
        let admission = match self.active.admit(&session_id) {
            Ok(admission) => admission,
            Err(crate::active_turns::TurnAdmissionError::Busy(_)) => {
                return self.reject_user_shell(
                    &session_id,
                    &origin,
                    "Agent 正在运行 · 等待完成或先取消当前任务".to_string(),
                );
            }
            Err(error) => {
                return self.reject_user_shell(&session_id, &origin, error.to_string());
            }
        };
        let cancel = admission.cancellation();
        let (runner, request, cwd) =
            match self
                .app
                .user_shell_execution(config.mode, config.sandbox, trimmed)
            {
                Ok(parts) => parts,
                Err(error) => {
                    self.active.finish(&admission);
                    return self.reject_user_shell(
                        &session_id,
                        &origin,
                        format!("无法构造执行环境: {error}"),
                    );
                }
            };
        let id = leveler_core::UserShellId::new(format!("ush-{}", leveler_core::new_uuid_string()));
        let cwd_display = cwd.display().to_string();
        let handoff = CancellationToken::new();
        self.user_shells.begin(
            &session_id,
            id.clone(),
            trimmed.to_string(),
            cwd_display.clone(),
            origin.clone(),
            cancel.clone(),
            handoff.clone(),
        );

        let app = self.app.clone();
        let events = self.events_for(&session_id);
        let store = self.user_shells.clone();
        let active = self.active.clone();
        let command_text = trimmed.to_string();
        // spawn_blocking + block_on (the turn-spawn pattern): the worker's
        // future borrows a `&mut dyn FnMut` observer across awaits, so it is
        // !Send by construction; the blocking-thread bridge sidesteps that
        // without loosening the EventLog observer contract.
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            handle.block_on(async move {
                // Canonical facts go through the session EventLog with no turn
                // attribution (persist-before-forward; transient output chunks
                // skip the store automatically) and are projected by the SAME
                // exhaustive EventBridge every other client fact uses.
                let mut bridge = crate::event_bridge::EventBridge::new(events.clone());
                let db = match app.open_database().await {
                    Ok(db) => db,
                    Err(error) => {
                        store.finish(&session_id, &id, None, "failed", None, None);
                        active.finish(&admission);
                        let _ = events.send(RuntimeEvent::Notification {
                            level: NotificationLevel::Error,
                            message: format!("无法打开会话数据库: {error}"),
                        });
                        return;
                    }
                };
                // A stale runtime must never stamp user shell facts into a
                // session another runtime now owns — and if this runtime may
                // not own the session, it must not run a shell there at all.
                let fenced = async {
                    let engine = app.task_engine(&db)?;
                    engine
                        .acquire_ownership(&session_id)
                        .await
                        .map_err(crate::session::app_error_from_engine)
                }
                .await;
                let token = match fenced {
                    Ok(token) => token,
                    Err(error) => {
                        store.finish(&session_id, &id, None, "failed", None, None);
                        active.finish(&admission);
                        let _ = events.send(RuntimeEvent::Notification {
                            level: NotificationLevel::Error,
                            message: format!("无法获得会话执行所有权: {error}"),
                        });
                        return;
                    }
                };
                let log =
                    leveler_engine::EventLog::new_owned(&db, session_id.clone(), token.clone());
                let mut forward = |event: leveler_engine::EngineEvent| bridge.forward(event);
                let _ = log
                    .append(
                        None,
                        crate::user_shell::started_event(
                            &id,
                            &command_text,
                            &cwd_display,
                            origin.as_ref(),
                        ),
                        &mut forward,
                    )
                    .await;

                let shell_write_scope = request.write_scope.clone();
                let shell_cwd = request.cwd.clone();
                let shell_program = command_text.clone();
                let runner_for_resume = runner.clone();
                // The in-memory tail is bounded by design. This directory is
                // the exact byte copy that makes a capped result still
                // completely retrievable, before and after a background handoff.
                let tee_directory = crate::user_shell::prepare_output_directory(
                    &app.layout.state_dir,
                    &session_id,
                    &id,
                );
                let run_tee = tee_directory.clone();
                let (tx, mut rx) = tokio::sync::mpsc::channel(64);
                let mut run = tokio::spawn({
                    let cancel = cancel.clone();
                    let handoff = handoff.clone();
                    async move {
                        runner
                            .run_streaming_handoff_tee(
                                request,
                                cancel,
                                tx,
                                handoff,
                                Some(leveler_execution::OutputTee { directory: run_tee }),
                            )
                            .await
                    }
                });
                let joined = loop {
                    tokio::select! {
                        joined = &mut run => break joined,
                        chunk = rx.recv() => match chunk {
                            Some(chunk) => {
                                append_user_shell_chunk(
                                    &store,
                                    &log,
                                    &mut forward,
                                    &session_id,
                                    &id,
                                    chunk,
                                )
                                .await;
                            }
                            None => break run.await,
                        },
                    }
                };
                let outcome = match joined {
                    Ok(outcome) => outcome,
                    Err(join_error) => {
                        StreamOutcome::Completed(Err(leveler_execution::ProcessError::Io {
                            program: "user-shell".into(),
                            source: std::io::Error::other(join_error.to_string()),
                        }))
                    }
                };
                let result = if let StreamOutcome::HandedOff(parts) = outcome {
                    let parts = *parts;
                    if cancel.is_cancelled() {
                        // Cancellation won before the transfer committed.
                        store.complete_background_request(
                            &session_id,
                            &id,
                            Err("命令已取消".to_string()),
                        );
                        runner_for_resume
                            .finish_handoff(parts, cancel.clone())
                            .await
                    } else {
                        let started = store
                            .active_started(&session_id, &id)
                            .unwrap_or_else(std::time::Instant::now);
                        let (initial_output, initial_truncated) = store
                            .foreground_output(&session_id, &id)
                            .unwrap_or_default();
                        let observer_events = events.clone();
                        let observer_store = store.clone();
                        let observer_session = session_id.clone();
                        let observer_id = id.clone();
                        let observer =
                            std::sync::Arc::new(move |stream: OutputStream, text: &str| {
                                observer_store.append_background_output(
                                    &observer_session,
                                    &observer_id,
                                    text,
                                );
                                let stream = match stream {
                                    OutputStream::Stdout => "stdout",
                                    OutputStream::Stderr => "stderr",
                                };
                                let _ = observer_events.send(RuntimeEvent::UserShellOutput {
                                    execution_id: observer_id.clone(),
                                    stream: stream.to_string(),
                                    chunk: text.to_string(),
                                });
                            });
                        // The registry calls this before its reaper can emit
                        // a terminal event, so the live view always learns the
                        // task link before the exit of the same execution.
                        let moved_slot: Arc<Mutex<Option<crate::user_shell::BackgroundedActive>>> =
                            Arc::new(Mutex::new(None));
                        let link_store = store.clone();
                        let link_events = events.clone();
                        let link_session = session_id.clone();
                        let link_id = id.clone();
                        let link_slot = moved_slot.clone();
                        let capture_dir = tee_directory.clone();
                        let on_adopted: Arc<dyn Fn(&str) + Send + Sync> =
                            Arc::new(move |task_id: &str| {
                                let moved = link_store.background(
                                    &link_session,
                                    &link_id,
                                    task_id.to_string(),
                                    capture_dir.clone(),
                                );
                                let _ = link_events.send(RuntimeEvent::UserShellBackgrounded {
                                    execution_id: link_id.clone(),
                                    task_id: task_id.to_string(),
                                });
                                *link_slot.lock().unwrap() = moved;
                            });
                        let shell_deadline = parts.request.timeout;
                        let spec = AdoptedTaskSpec {
                            goal_id: None,
                            owner_scope: Some(session_id.as_str().to_string()),
                            writer_scope: "user-shell".to_string(),
                            // The registry row is a display fact for a process
                            // that is already running; the shell string is the
                            // useful label, not a respawn recipe.
                            program: shell_program,
                            args: Vec::new(),
                            cwd: shell_cwd,
                            write_scope: shell_write_scope,
                            lifetime: BackgroundTaskLifetime::Runtime,
                            started,
                            // A user shell that left the foreground keeps the
                            // execution window its own request carried; the
                            // handoff adds no second budget and removes none.
                            deadline: Some(shell_deadline),
                            initial_output: initial_output.clone(),
                            observer: Some(observer),
                            on_adopted: Some(on_adopted),
                            mutation_baseline: None,
                            // A user shell is not a tool call: it has its own
                            // execution record and never enters the model's
                            // verification evidence.
                            origin_call_id: None,
                        };
                        match app
                            .background_tasks()
                            .adopt(parts, rx, spec, CancellationToken::new())
                            .await
                        {
                            Ok(task_id) => {
                                let moved = moved_slot.lock().unwrap().take();
                                let backgrounded =
                                    leveler_engine::EngineEvent::UserShellBackgrounded {
                                        execution_id: id.clone(),
                                        task_id: task_id.clone(),
                                        output_tail: initial_output,
                                        output_truncated: initial_truncated,
                                    };
                                // Persist for cold resume, but deliver the live
                                // fact on the session stream directly: the
                                // foreground's bridge has already served its
                                // role and must not become a second delivery
                                // authority for this transfer.
                                let mut persist_only = |_event: leveler_engine::EngineEvent| {};
                                let _ = log.append(None, backgrounded, &mut persist_only).await;
                                active.finish(&admission);
                                let released = async {
                                    app.task_engine(&db)?
                                        .release_ownership(&token)
                                        .await
                                        .map_err(crate::session::app_error_from_engine)
                                }
                                .await;
                                if let Err(error) = released {
                                    tracing::warn!(
                                        %error,
                                        session = session_id.as_str(),
                                        "background user shell could not release the session"
                                    );
                                }
                                if let Some(moved) = moved {
                                    moved.reply(Ok(task_id));
                                }
                                return;
                            }
                            Err(error) => {
                                store.complete_background_request(
                                    &session_id,
                                    &id,
                                    Err(error.error.clone()),
                                );
                                rx = error.chunks;
                                let mut finish = Box::pin(
                                    runner_for_resume.finish_handoff(error.handoff, cancel.clone()),
                                );
                                let result = loop {
                                    tokio::select! {
                                        result = &mut finish => break result,
                                        chunk = rx.recv() => match chunk {
                                            Some(chunk) => {
                                                append_user_shell_chunk(
                                                    &store,
                                                    &log,
                                                    &mut forward,
                                                    &session_id,
                                                    &id,
                                                    chunk,
                                                )
                                                .await;
                                            }
                                            None => break finish.await,
                                        },
                                    }
                                };
                                while let Some(chunk) = rx.recv().await {
                                    append_user_shell_chunk(
                                        &store,
                                        &log,
                                        &mut forward,
                                        &session_id,
                                        &id,
                                        chunk,
                                    )
                                    .await;
                                }
                                result
                            }
                        }
                    }
                } else {
                    match outcome {
                        StreamOutcome::Completed(result) => result,
                        StreamOutcome::HandedOff(_) => unreachable!("handled above"),
                    }
                };
                // Any pending transfer request lost the race with a natural
                // exit. Answer it explicitly; it must not wait for a timeout.
                store.complete_background_request(&session_id, &id, Err("命令已结束".to_string()));
                // Drain chunks that were buffered before the receivers closed.
                while let Some(chunk) = rx.recv().await {
                    append_user_shell_chunk(&store, &log, &mut forward, &session_id, &id, chunk)
                        .await;
                }
                let status = crate::user_shell::terminal_status(&result);
                // The live channel is bounded and drops rather than block the
                // child, so the tail this store streamed can be missing bytes
                // — including the end of the output. When that happened, the
                // runner's own capture is the real head+tail and the completed
                // record uses it; its drop marker already reports retained
                // truncation, so the separate note is not added on top.
                let authoritative = match &result {
                    Ok(output) if output.live_dropped_bytes > 0 => {
                        Some(crate::user_shell::authoritative_tail(output))
                    }
                    _ => None,
                };
                if authoritative.is_none()
                    && let Ok(output) = &result
                    && output.truncated
                {
                    store.mark_output_truncated(&session_id, &id);
                }
                let exit_code = match &result {
                    Ok(output) => output.exit_code,
                    Err(_) => None,
                };
                if let Err(error) = &result
                    && !matches!(
                        error,
                        leveler_execution::ProcessError::Cancelled
                            | leveler_execution::ProcessError::CancelUnconfirmed
                    )
                {
                    // Spawn/sandbox failures never produced output — put the host
                    // error where the user will look for it. The live view is a
                    // separate projection of this same fact: the store keeps it
                    // for reconnect and the terminal record, the event makes the
                    // refusal legible while the command is still on screen.
                    let reason = format!("{error}\n");
                    store.append_output(&session_id, &id, &reason);
                    let _ = events.send(RuntimeEvent::UserShellOutput {
                        execution_id: id.clone(),
                        stream: "stderr".to_string(),
                        chunk: reason,
                    });
                }
                let (retained_tail, retained_truncated) = store
                    .foreground_output(&session_id, &id)
                    .unwrap_or_default();
                let output_tail = authoritative.clone().unwrap_or(retained_tail);
                // A live-channel drop alone is not truncation: the authoritative
                // capture may still hold every byte. Only a capture that dropped
                // its middle, or a tail longer than the store can retain, is
                // reported as truncated.
                let capture_dropped = match &result {
                    Ok(output) => output.dropped_bytes > output.live_dropped_bytes,
                    Err(_) => false,
                };
                let output_truncated = retained_truncated
                    || authoritative.as_ref().is_some_and(|text| {
                        capture_dropped || text.len() > crate::user_shell::OUTPUT_TAIL_CAP
                    });
                if authoritative.is_some() {
                    // The live channel dropped bytes. Replace the client's
                    // accumulated chunks with the runtime's real view before
                    // the terminal event, so the finished record is not a
                    // hole-punched stream.
                    let _ = events.send(RuntimeEvent::UserShellOutputReplaced {
                        execution_id: id.clone(),
                        output_tail: output_tail.clone(),
                        output_truncated,
                    });
                }
                // The complete capture is a separate fact. It is advertised
                // only when every stream's sidecar says the copy finished;
                // never as a path line mixed into the output text.
                let full_output = crate::user_shell::complete_output_directory(&tee_directory);
                let finish_tail = authoritative.as_ref().map(|_| output_tail.as_str());
                let duration_ms = store
                    .finish(
                        &session_id,
                        &id,
                        exit_code,
                        status,
                        finish_tail,
                        full_output.clone(),
                    )
                    .unwrap_or(0);
                // Release the session BEFORE the terminal event is published.
                // A client enables its composer on `UserShellExited`, so the
                // next `SubmitMessage` can arrive within microseconds; holding
                // the slot past the event leaves a window where the command is
                // visibly finished and the session still answers "already has
                // an active turn". Nothing below needs the slot: the process
                // has exited, its output is drained, and `store.finish` has
                // already made `CancelUserShell` a no-op. This matches the
                // failure paths above, which release before they notify.
                active.finish(&admission);
                let _ = log
                    .append(
                        None,
                        leveler_engine::EngineEvent::UserShellFinished {
                            execution_id: id.clone(),
                            exit_code,
                            duration_ms,
                            status: status.to_string(),
                            output_tail,
                            output_truncated,
                            full_output,
                        },
                        &mut forward,
                    )
                    .await;
                // The shell has exited and its terminal fact is written: the
                // session is nobody's until the next execution acquires it. A
                // crash before this line leaves the task to a dead boot, which
                // the next acquire takes over.
                let released = async {
                    app.task_engine(&db)?
                        .release_ownership(&token)
                        .await
                        .map_err(crate::session::app_error_from_engine)
                }
                .await;
                if let Err(error) = released {
                    tracing::warn!(
                        %error,
                        session = session_id.as_str(),
                        "user shell could not release the session"
                    );
                }
            });
        });
        Ok(())
    }

    /// Answer a clarification from a client.
    ///
    /// A live waiter is the fast path: the parked turn continues by itself and
    /// records its own terminal through the recorder. With no live waiter —
    /// the runtime restarted, or the turn that asked was already reaped — the
    /// durable request is the authority: one client answer settles it exactly
    /// once and the goal continues in a fresh window without the user typing
    /// anything.
    async fn handle_answer_clarification(
        &self,
        request_id: ClarificationId,
        answer: String,
        session_id: Option<SessionId>,
    ) -> Result<(), ClientError> {
        if resolve_clarification(&self.pending_clarify, &request_id, answer.clone()).is_ok() {
            return Ok(());
        }
        let Some(session_id) = session_id else {
            return Err(ClientError::Runtime(
                "pending clarification not found or already resolved".to_string(),
            ));
        };
        let db = self
            .app
            .open_database()
            .await
            .map_err(|error| ClientError::Runtime(error.to_string()))?;
        let stores = leveler_storage::EngineStores::from_database(&db);
        let log = leveler_engine::EventLog::new(stores.events.as_ref(), session_id.clone());
        let missing = || {
            ClientError::Runtime("pending clarification not found or already resolved".to_string())
        };
        // Serialize with turn admission: the question's terminal and the
        // continuation's admission are one ordered decision.
        let _gate = self.durable_clarify.lock().await;
        // A live turn owns the session; its waiter was consumed (another
        // client answered first) or is about to be. A durable terminal here
        // would race the recorder's own. A retry of the answer that landed is
        // a success even while its continuation runs — but a decision this
        // request never recorded is not this client's answer.
        if self.active.is_running(&session_id) {
            let Some(terminal) = log
                .clarification_settlement(&request_id)
                .await
                .map_err(|error| ClientError::Runtime(error.to_string()))?
            else {
                return Err(missing());
            };
            return if clarification_answer_matches(&terminal, &answer) {
                Ok(())
            } else {
                Err(stale_clarification_answer(&terminal))
            };
        }
        match log
            .settle_clarification(&request_id, &answer)
            .await
            .map_err(|error| ClientError::Runtime(error.to_string()))?
        {
            leveler_engine::ClarificationSettlement::Settled(_) => {
                self.continue_after_durable_answer(&session_id).await
            }
            // Another answer already settled it. A retry of that same decision
            // may be the moment the goal gets the window a crash left owed;
            // any other answer changed nothing and must not read as success,
            // and no answer may start a second continuation.
            leveler_engine::ClarificationSettlement::AlreadySettled(terminal) => {
                if !clarification_answer_matches(&terminal, &answer) {
                    return Err(stale_clarification_answer(&terminal));
                }
                if clarification_continuation_is_owed(&db, &stores, &session_id).await? {
                    self.continue_after_durable_answer(&session_id).await
                } else {
                    Ok(())
                }
            }
            leveler_engine::ClarificationSettlement::Unknown => Err(missing()),
        }
    }

    /// Admit and start the goal window that the answered question was holding.
    ///
    /// Called with the durable settlement already written and the answer gate
    /// held. The command returns before the turn is accepted; the acceptance
    /// error is surfaced as a notification because the answer itself is
    /// durable and must not be reported as failed.
    async fn continue_after_durable_answer(
        &self,
        session_id: &SessionId,
    ) -> Result<(), ClientError> {
        let config = self.runtime_config(session_id).await?;
        let model = require_model(&config, session_id)?;
        let admission = self
            .active
            .admit(session_id)
            .map_err(|error| ClientError::Runtime(error.to_string()))?;
        let accepted = self.spawn_goal_continuation(session_id.clone(), admission, config, model);
        let events = self.events_for(session_id);
        tokio::spawn(async move {
            if let Err(error) = await_turn_acceptance(accepted).await {
                let _ = events.send(RuntimeEvent::Notification {
                    level: NotificationLevel::Error,
                    message: format!("回答后无法继续目标：{error}"),
                });
            }
        });
        Ok(())
    }

    /// Refuse to start a new turn while an answerable question is pending.
    ///
    /// The question is the decision the task is waiting on; a new turn would
    /// overlap the work the answer belongs to. Rows from before the request
    /// carried its turn/tool identity are history, not a card: they must not
    /// block a session forever.
    async fn refuse_if_clarification_pending(
        &self,
        session_id: &SessionId,
    ) -> Result<(), ClientError> {
        let db = self
            .app
            .open_database()
            .await
            .map_err(|error| ClientError::Runtime(error.to_string()))?;
        let stores = leveler_storage::EngineStores::from_database(&db);
        let log = leveler_engine::EventLog::new(stores.events.as_ref(), session_id.clone());
        let pending = log
            .pending_clarifications()
            .await
            .map_err(|error| ClientError::Runtime(error.to_string()))?;
        let Some(request) = pending.into_iter().find(|request| request.is_actionable()) else {
            return Ok(());
        };
        Err(ClientError::Runtime(format!(
            "该会话仍在等待用户回答一个问题：{}；请先回答或跳过，或取消任务。",
            request.question
        )))
    }

    /// The shared turn-launch preamble: admit the session (one active main
    /// turn), optionally name a placeholder session after its first message,
    /// capture the pre-turn checkpoint, and emit the optimistic user-message
    /// notification. Every path that starts a main turn goes through here so
    /// the sequence cannot drift between commands. This client notification is
    /// not a persistence witness; daemon ACKs wait on the turn record instead.
    async fn stage_turn(
        &self,
        session_id: &SessionId,
        text: &str,
        retitle: bool,
        images: usize,
        message_id: Option<MessageId>,
        seed_input_id: Option<String>,
    ) -> Result<crate::active_turns::TurnLease, ClientError> {
        // Admission and durable clarification settlement are one ordered
        // decision: a new turn must not start while a question is answerable,
        // and an answer's continuation must not race a turn that is starting.
        let admission = {
            let _gate = self.durable_clarify.lock().await;
            if !self.active.is_running(session_id) {
                self.refuse_if_clarification_pending(session_id).await?;
            }
            self.active
                .admit(session_id)
                .map_err(|error| ClientError::Runtime(error.to_string()))?
        };
        self.auxiliary_assists.cancel(session_id);
        if retitle {
            self.retitle_placeholder_session(session_id, text).await;
        }
        self.checkpoint_before_turn(session_id, text).await;
        // The lease knows which accepted input this turn consumes before the
        // executor exists. A retry of the same submission can then tell
        // "already promoted" from "still waiting as a steer".
        self.active
            .set_seed_input(session_id, seed_input_id.clone());
        let kind = seed_input_id.as_ref().map(|_| UiMessageKind::PendingInput);
        let _ = self
            .events_for(session_id)
            .send(RuntimeEvent::UserMessageAdded {
                message: UiMessage {
                    id: message_id
                        .unwrap_or_else(|| MessageId::new(leveler_core::new_uuid_string())),
                    role: UiRole::User,
                    text: text.to_string(),
                    ordinal: None,
                    kind,
                    images,
                },
            });
        // Accepted-but-undelivered input from an earlier turn (a crash, or an
        // ending that raced the last round) belongs to this one. The seed is
        // excluded: the executor consumes that row as the opening message.
        self.hydrate_pending_inputs(session_id, seed_input_id.as_deref())
            .await;
        Ok(admission)
    }

    /// Every journaled input still awaiting delivery, oldest first.
    async fn pending_input_rows(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<leveler_storage::SessionInputRecord>, ClientError> {
        let db = self
            .app
            .open_database()
            .await
            .map_err(|error| ClientError::Runtime(error.to_string()))?;
        SessionInputRepository::new(&db)
            .pending(session_id)
            .await
            .map_err(|error| ClientError::Runtime(error.to_string()))
    }

    /// Put journaled input back into the live handoff for the turn about to
    /// start. The journal is durable across process death; this is the one
    /// place that turns those rows back into deliverable input.
    async fn hydrate_pending_inputs(&self, session_id: &SessionId, except: Option<&str>) {
        let rows = match self.pending_input_rows(session_id).await {
            Ok(rows) => rows,
            Err(error) => {
                // The rows stay durable; a later turn hydrates them. Log the
                // reason instead of pretending the turn has them.
                tracing::warn!(
                    session = session_id.as_str(),
                    error = %error,
                    "could not hydrate accepted input; it remains journaled"
                );
                return;
            }
        };
        let mut hydrated = false;
        for row in rows {
            if Some(row.id.as_str()) == except {
                continue;
            }
            self.enqueue_input(session_id, row.id, row.content);
            hydrated = true;
        }
        if hydrated {
            self.signal_pending_input(session_id);
        }
    }

    /// Add one durable input to a session's live handoff. Idempotent by id, so
    /// a retried admission and a journal re-hydration cannot deliver it twice.
    fn enqueue_input(&self, session_id: &SessionId, id: String, text: String) {
        let mut queues = self
            .steering
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let queue = queues.entry(session_id.clone()).or_default();
        if queue
            .iter()
            .any(|input| input.id.as_deref() == Some(id.as_str()))
        {
            return;
        }
        queue.push(leveler_agent::PendingUserInput { id: Some(id), text });
    }

    /// Persist one accepted append. The ACK for its command is written only
    /// after this returns, so a crash after acknowledgement always leaves a
    /// row Resume can find.
    async fn admit_input(
        &self,
        session_id: &SessionId,
        id: &str,
        content: &str,
    ) -> Result<leveler_storage::SessionInputRecord, ClientError> {
        let db = self
            .app
            .open_database()
            .await
            .map_err(|error| ClientError::Runtime(error.to_string()))?;
        SessionInputRepository::new(&db)
            .admit(id, session_id, None, content, leveler_core::now())
            .await
            .map_err(|error| ClientError::Runtime(error.to_string()))
    }

    fn emit_input_admitted(&self, session_id: &SessionId, id: &str, text: &str) {
        let _ = self
            .events_for(session_id)
            .send(RuntimeEvent::InputAdmitted {
                message: UiMessage {
                    id: MessageId::new(id),
                    role: UiRole::User,
                    text: text.to_string(),
                    ordinal: None,
                    kind: Some(UiMessageKind::PendingInput),
                    images: 0,
                },
            });
    }

    fn emit_input_settled(
        &self,
        session_id: &SessionId,
        id: &str,
        state: UiInputState,
        reason: &str,
        ordinal: Option<u64>,
    ) {
        let _ = self
            .events_for(session_id)
            .send(RuntimeEvent::InputSettled {
                message_id: MessageId::new(id),
                state,
                reason: reason.to_string(),
                ordinal,
            });
    }
    /// Settle one pending input as cancelled. `None` means it was already
    /// delivered or cancelled: a late cancel must not rewrite history.
    async fn cancel_input(
        &self,
        session_id: &SessionId,
        id: &str,
        reason: &str,
    ) -> Result<Option<leveler_storage::SessionInputRecord>, ClientError> {
        let db = self
            .app
            .open_database()
            .await
            .map_err(|error| ClientError::Runtime(error.to_string()))?;
        SessionInputRepository::new(&db)
            .cancel(session_id, id, reason, leveler_core::now())
            .await
            .map_err(|error| ClientError::Runtime(error.to_string()))
    }

    /// Settle every still-pending input of a cancelled task. The rows stay as
    /// an audit trail; the live queue is cleared so nothing can be handed to a
    /// later turn. Input already delivered is untouched.
    async fn cancel_pending_inputs(&self, session_id: &SessionId, reason: &str) {
        self.steering
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session_id);
        let settled = match self.app.open_database().await {
            Ok(db) => {
                SessionInputRepository::new(&db)
                    .cancel_pending(session_id, reason, leveler_core::now())
                    .await
            }
            Err(error) => {
                tracing::warn!(
                    session = session_id.as_str(),
                    error = %error,
                    "could not settle pending input after a cancel"
                );
                return;
            }
        };
        match settled {
            Ok(rows) => {
                for row in rows {
                    self.emit_input_settled(
                        session_id,
                        &row.id,
                        UiInputState::Cancelled,
                        &row.reason,
                        None,
                    );
                }
            }
            Err(error) => tracing::warn!(
                session = session_id.as_str(),
                error = %error,
                "could not settle pending input after a cancel"
            ),
        }
    }

    /// Input the user typed while a turn ran and that no round consumed.
    ///
    /// A steer is accepted the moment the client sends it and is consumed at
    /// the top of the next model round. Text that arrives in the window between
    /// the final round's start and the turn's settle has no round left, and
    /// dropping it would lose something the person actually typed. The turn is
    /// over, so this is exactly the situation the client's own steer fallback
    /// describes: deliver it as an ordinary message.
    ///
    /// A `/goal` declared in that same window is delivered as the goal turn it
    /// asked for; its durable axis is already written, so nothing is lost if
    /// this race is lost too.
    async fn deliver_unconsumed_input(&self, session_id: &SessionId) {
        // Read the slot without creating one: a session that never declared a
        // goal must not gain an empty entry just because a turn ended.
        let declaration = {
            let slots = self
                .goal_steering
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            slots.get(session_id).and_then(|slot| {
                let mut goal = slot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if goal.pending.is_empty() {
                    return None;
                }
                goal.requested = true;
                Some(goal.pending.remove(0))
            })
        };
        if let Some(task) = declaration {
            let result = Box::pin(self.send(ClientCommand::RunGoal {
                session_id: session_id.clone(),
                content: task.clone(),
                require_new_turn: false,
            }))
            .await;
            if result.is_err() {
                let slot = self
                    .goal_steering
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(session_id)
                    .cloned();
                if let Some(slot) = slot {
                    let mut goal = slot
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    goal.pending.insert(0, task);
                    goal.requested = true;
                }
            }
            return;
        }
        // The journal is the authority, not the in-memory queue. Dropping the
        // queue first is lossless (every entry has a row) and stops the next
        // turn from delivering an entry that this path also promotes.
        self.steering
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session_id);
        let pending = match self.pending_input_rows(session_id).await {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(
                    session = session_id.as_str(),
                    error = %error,
                    "could not read accepted input after a turn; it stays journaled"
                );
                return;
            }
        };
        if pending.is_empty() {
            return;
        }
        // Another client started work in this window. It owns the turn now:
        // hand the rows to it instead of racing admission. They stay journaled
        // until that turn actually appends them.
        if self.active.is_running(session_id) {
            for row in &pending {
                self.enqueue_input(session_id, row.id.clone(), row.content.clone());
            }
            self.signal_pending_input(session_id);
            return;
        }
        // Promote exactly one row as the next turn's opening message, so its
        // transcript row consumes that journal row atomically. stage_turn
        // hydrates the rest as separate steers; nothing is joined with "\n".
        let seed = pending[0].clone();
        let result = Box::pin(self.handle_submit_message(
            session_id.clone(),
            seed.content.clone(),
            Vec::new(),
            None,
            Some(seed.id.clone()),
        ))
        .await;
        if let Err(error) = result {
            // Nothing is lost: the rows are still pending. A running turn gets
            // them as steers; otherwise they wait for the next submission or
            // Resume, which hydrates them from the journal.
            for row in &pending {
                self.enqueue_input(session_id, row.id.clone(), row.content.clone());
            }
            if self.active.is_running(session_id) {
                self.signal_pending_input(session_id);
            } else {
                tracing::warn!(
                    session = session_id.as_str(),
                    error = %error,
                    "accepted input could not start a turn; it stays journaled for Resume"
                );
            }
        }
    }

    /// New user input is waiting for a session's running turn.
    ///
    /// The text itself is consumed at the top of the next model round, so the
    /// only thing that has to happen NOW is that an observation call gives the
    /// round back. A `wait_task` watching a live service returns what it has
    /// instead of holding the turn for its own bound; the process is untouched.
    /// The queue is written before this runs, so an input that arrives between
    /// the call returning and the next round is still seen.
    fn signal_pending_input(&self, session_id: &SessionId) {
        let tokens: Vec<CancellationToken> = self
            .tool_call_yields
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session_id)
            .map(|calls| calls.values().cloned().collect())
            .unwrap_or_default();
        for token in tokens {
            token.cancel();
        }
    }

    /// Steering for one session. Cloned into the executor, which drains it at
    /// the top of every round.
    /// Remove a session that was created but never handed to a client, so a
    /// failure partway through setup does not leave an orphan row the user
    /// sees in the session list and cannot explain.
    async fn discard_session(&self, session_id: &SessionId) {
        let removed: Result<(), anyhow::Error> = async {
            let db = self.app.open_database().await?;
            SessionRepository::new(&db).delete(session_id).await?;
            Ok(())
        }
        .await;
        if let Err(error) = removed {
            tracing::warn!(
                session_id = %session_id,
                error = %error,
                "could not roll back a half-created session"
            );
        }
    }

    fn steering_for(&self, session_id: &SessionId) -> Arc<SessionSteering> {
        Arc::new(SessionSteering {
            session_id: session_id.clone(),
            queues: self.steering.clone(),
            events: self.events_for(session_id),
            seed: self.active.seed_input_id(session_id),
            goal: self.goal_state_for(session_id),
            children: self.child_cancels.clone(),
            tool_calls: self.tool_call_cancels.clone(),
            tool_yields: self.tool_call_yields.clone(),
        })
    }

    /// The one in-flight `/goal` slot for a session, shared between the
    /// command handler that writes it and the running executor that drains it.
    fn goal_state_for(&self, session_id: &SessionId) -> Arc<Mutex<GoalSteer>> {
        self.goal_steering
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(session_id.clone())
            .or_default()
            .clone()
    }

    fn clarifier(
        &self,
        session_id: &SessionId,
        cancel: CancellationToken,
    ) -> Arc<ChannelClarifier> {
        Arc::new(ChannelClarifier {
            events: self.events_for(session_id),
            pending: self.pending_clarify.clone(),
            cancel,
            session_id: session_id.clone(),
        })
    }

    /// Best-effort reaper for zombie `running` turns left by kill / unclean exit.
    ///
    /// `Some(session)` reaps that session only (cancel / force-cancel).
    /// `None` reaps every still-running turn in `scope` (process quit settles
    /// only this boot's own). Returns the running turns it had to leave alone:
    /// ones another live boot runs, or whose boot cannot be probed — this
    /// process has no way to reach them.
    async fn reap_running_turns(
        &self,
        session: Option<&SessionId>,
        scope: leveler_engine::ReapScope,
    ) -> Result<Vec<leveler_engine::ReapConflict>, ClientError> {
        let engine = match self
            .app
            .open_database()
            .await
            .and_then(|db| self.app.task_engine(&db))
        {
            Ok(engine) => engine,
            Err(error) => {
                tracing::warn!("cannot reap running turns: {error}");
                return Err(ClientError::Runtime(error.to_string()));
            }
        };
        match leveler_engine::reap_after_restart(&engine, session, scope).await {
            Ok(outcome) => {
                if let Err(error) =
                    crate::session::checkpoint_reaped_sessions(&engine, &outcome.reaped_sessions)
                        .await
                {
                    return Err(ClientError::Runtime(error.to_string()));
                }
                for conflict in &outcome.conflicts {
                    tracing::warn!(
                        session = conflict.session_id.as_str(),
                        refusal = ?conflict.refusal,
                        "not reaping running turns without proof their boot has ended"
                    );
                }
                if !outcome.events.is_empty() {
                    tracing::warn!(
                        reaped = outcome.events.len(),
                        session = session.map(|s| s.as_str()),
                        "reaped zombie running turns"
                    );
                }
                Ok(outcome.conflicts)
            }
            Err(error) => {
                tracing::warn!("failed to reap running turns: {error}");
                Err(ClientError::Runtime(error.to_string()))
            }
        }
    }

    /// Cancel a session with nothing of this boot running in it: settle what
    /// this boot or a dead boot left behind. A turn another live boot runs is
    /// out of this process's reach — the answer says so rather than claiming
    /// a cancel that did not happen.
    async fn cancel_orphaned(&self, session_id: &SessionId) -> Result<(), ClientError> {
        let conflicts = self
            .reap_running_turns(
                Some(session_id),
                leveler_engine::ReapScope::OwnAndEndedBoots,
            )
            .await?;
        if conflicts
            .iter()
            .any(|conflict| conflict.refusal == leveler_engine::ReapRefusal::LiveBoot)
        {
            return Err(ownership_conflict(
                &crate::AppError::SessionRunningElsewhere,
            ));
        }
        if !conflicts.is_empty() {
            return Err(ownership_conflict(
                &crate::AppError::SessionOwnershipUnknown,
            ));
        }
        Ok(())
    }

    /// Build the first user message's content parts from text and image
    /// attachments, loading (and base64-encoding) each image from the store.
    ///
    /// An image that cannot be loaded fails the whole turn. Sending the text
    /// without it would hand the model a question about a picture it was never
    /// shown, and the answer would read exactly like a real one.
    fn content_parts(
        &self,
        text: &str,
        attachments: &[AttachmentRef],
    ) -> Result<Vec<ContentPart>, String> {
        let store = MediaStore::new(&self.media_root);
        let mut parts = Vec::new();
        if !text.trim().is_empty() {
            parts.push(ContentPart::Text {
                text: text.to_string(),
            });
        }
        for att in attachments {
            if att.kind != AttachmentKind::Image {
                continue;
            }
            let (media_type, data) = store
                .load_base64(&att.sha256)
                .map_err(|e| format!("{}: {e}", att.name))?;
            parts.push(ContentPart::Image {
                source: ImageSource::Base64 { media_type, data },
            });
        }
        Ok(parts)
    }

    /// Refuse a submit that carries an image the chosen model cannot read.
    ///
    /// Declared capability is the only thing that can be checked before the
    /// request is built; the alternative is a provider 400 the user has to
    /// decode, or worse, an answer about a picture that was dropped.
    async fn refuse_unreadable_images(
        &self,
        model: &ModelRef,
        attachments: &[AttachmentRef],
    ) -> Result<(), ClientError> {
        if !attachments.iter().any(|a| a.kind == AttachmentKind::Image) {
            return Ok(());
        }
        let vision = self
            .app
            .registry()
            .profile(model)
            .await
            .map(|p| p.capabilities.vision)
            .unwrap_or(false);
        if vision {
            return Ok(());
        }
        Err(ClientError::Runtime(format!(
            "模型 {model} 不支持图片输入；请换一个支持视觉的模型，或移除图片后重发"
        )))
    }

    fn cancel_active(&self, session_id: &SessionId) -> bool {
        self.active.cancel(session_id)
    }

    /// Surface an error to the UI's status line.
    fn notify_error(&self, session_id: &SessionId, message: String) {
        let _ = self
            .events_for(session_id)
            .send(RuntimeEvent::Notification {
                level: NotificationLevel::Error,
                message,
            });
    }

    fn notify_info(&self, session_id: &SessionId, message: String) {
        let _ = self
            .events_for(session_id)
            .send(RuntimeEvent::Notification {
                level: NotificationLevel::Info,
                message,
            });
    }

    /// Move one running foreground shell to the background registry. The
    /// transfer is performed by the worker that owns the streaming driver; this
    /// command only asks and waits for the authoritative answer. No process is
    /// spawned or restarted here.
    async fn handle_background_user_shell(
        &self,
        session_id: SessionId,
        execution_id: leveler_core::UserShellId,
    ) -> Result<(), ClientError> {
        match self
            .user_shells
            .request_background(&session_id, &execution_id)
        {
            crate::user_shell::BackgroundRequest::NotFound => {
                self.notify_error(&session_id, "该命令已结束或不存在,无需转后台".to_string())
            }
            crate::user_shell::BackgroundRequest::AlreadyBackground(task_id) => {
                self.notify_info(&session_id, format!("该命令已在后台运行: {task_id}"))
            }
            crate::user_shell::BackgroundRequest::Pending => {
                self.notify_info(&session_id, "正在转后台,请稍候".to_string())
            }
            crate::user_shell::BackgroundRequest::Requested(reply) => {
                match tokio::time::timeout(std::time::Duration::from_secs(10), reply).await {
                    Ok(Ok(Ok(task_id))) => {
                        self.notify_info(&session_id, format!("已转后台: {task_id}"))
                    }
                    Ok(Ok(Err(error))) => {
                        self.notify_error(&session_id, format!("转后台失败: {error}"))
                    }
                    Ok(Err(_)) => {
                        self.notify_error(&session_id, "转后台请求未完成,命令仍在运行".to_string())
                    }
                    Err(_) => self.notify_error(&session_id, "转后台超时,命令仍在运行".to_string()),
                }
            }
        }
        Ok(())
    }

    /// Truncate a session's stored messages after `ordinal`, propagating any
    /// database failure to the caller (never swallowed).
    async fn truncate_messages(
        &self,
        session_id: &SessionId,
        ordinal: usize,
    ) -> Result<(), anyhow::Error> {
        let db = self.app.open_database().await?;
        MessageRepository::new(&db)
            .truncate_after(session_id, ordinal)
            .await?;
        Ok(())
    }

    /// After /clear, /compact, or checkpoint restore: cut Plan/Evidence/Progress
    /// inheritance so the next turn is a fresh task epoch.
    async fn reset_task_epoch(&self, session_id: &SessionId) -> Result<(), anyhow::Error> {
        reset_session_task_epoch(self.app.as_ref(), session_id).await
    }

    /// Refuse context ops while a model turn (or another context op) owns the
    /// session — concurrent clear/compact/restore would race the transcript.
    fn admit_context_op(
        &self,
        session_id: &SessionId,
        op: &str,
    ) -> Result<crate::active_turns::TurnLease, ClientError> {
        self.active.admit(session_id).map_err(|error| match error {
            crate::active_turns::TurnAdmissionError::Busy(_) => {
                ClientError::Runtime(format!("当前有进行中的回合，请先等待完成或取消后再{op}"))
            }
            other => ClientError::Runtime(other.to_string()),
        })
    }

    /// Drop all UI checkpoints (and their workspace snapshots) for a session.
    /// Used after /clear and /compact when the transcript is no longer aligned
    /// with prior checkpoint ordinals.
    fn drop_session_checkpoints(&self, session_id: &SessionId) {
        self.checkpoints.drop_session(session_id);
    }

    /// After restore to `ordinal`, drop later checkpoints (and their snapshots)
    /// so the UI cannot re-restore a point that no longer exists in the transcript.
    fn prune_checkpoints_after_restore(&self, session_id: &SessionId, restored_ordinal: u32) {
        self.checkpoints
            .prune_after_restore(session_id, restored_ordinal);
    }

    /// Name a placeholder interactive session after its first real message:
    /// the sidebars (web + TUI resume) show the `goal` column, and a wall of
    /// "interactive session" rows is unreadable. Only the placeholder is ever
    /// overwritten — user-named goals stay untouched.
    async fn retitle_placeholder_session(&self, session_id: &SessionId, content: &str) {
        let Some(title) = title_from_first_message(content) else {
            return;
        };
        let Ok(db) = self.app.open_database().await else {
            return;
        };
        let repo = SessionRepository::new(&db);
        if let Ok(Some(record)) = repo.get(session_id).await
            && (record.goal == PLACEHOLDER_GOAL || record.goal.trim().is_empty())
        {
            let _ = repo.update_goal(session_id, &title).await;
        }
    }

    /// List stored sessions, most-recent first, as UI summaries. Sessions
    /// whose goal is still the interactive placeholder display the first
    /// sentence of their first user message instead — this also names the
    /// history created before placeholder retitling existed.
    async fn list_sessions(&self) -> Result<Vec<UiSessionSummary>, ClientError> {
        let db = self
            .app
            .open_database()
            .await
            .map_err(|error| ClientError::Runtime(error.to_string()))?;
        let facts = db
            .session_facts(false)
            .await
            .map_err(|error| ClientError::Runtime(error.to_string()))?;
        let probe = crate::runtime_boot::StateDirBootLiveness::new(&self.app.layout.state_dir);
        let mut summaries = Vec::with_capacity(facts.len());
        for facts in facts {
            let id = SessionId::new(facts.session.id.clone());
            let live = self.active.is_running(&id);
            let waiting = self
                .pending
                .lock()
                .unwrap()
                .values()
                .any(|p| p.binding.session_id == id)
                || self
                    .pending_clarify
                    .lock()
                    .unwrap()
                    .values()
                    .any(|p| p.binding.session_id == id);
            let (task_status, terminal) =
                crate::session_projection::project_task(&facts, &probe, live, waiting)?;
            let declaration =
                terminal
                    .as_ref()
                    .and_then(|terminal| match terminal.stop.as_deref() {
                        Some("answered") => {
                            Some(leveler_client_protocol::UiTaskDeclaration::Answered)
                        }
                        Some("completed") => {
                            Some(leveler_client_protocol::UiTaskDeclaration::Completed)
                        }
                        _ => None,
                    });
            let goal = crate::session_projection::project_title(
                &facts.session.goal,
                facts.first_user_text.as_deref(),
            );
            let r = facts.session;
            summaries.push(UiSessionSummary {
                id,
                goal,
                status: session_status_wire(r.status, task_status).into(),
                task_status: Some(task_status),
                task_terminal: terminal,
                declaration,
                // Display-only list projection: empty means no model. The
                // authoritative snapshot carries the `None`.
                model: r.model.unwrap_or_default(),
                updated_at: facts.last_activity_at,
                repository: r.repository,
            });
        }
        Ok(summaries)
    }

    fn approver(&self, session_id: &SessionId, cancel: CancellationToken) -> Arc<dyn Approver> {
        let session_policy = self
            .session_runtime
            .lock()
            .unwrap()
            .get(session_id)
            .map(|config| config.approval_policy);
        if should_auto_approve(session_policy, self.auto_approve) {
            Arc::new(AutoApprove)
        } else {
            Arc::new(ChannelApprover {
                events: self.events_for(session_id),
                pending: self.pending.clone(),
                cancel,
                session_id: session_id.clone(),
            })
        }
    }

    fn spawn_turn(
        &self,
        session_id: SessionId,
        content: String,
        attachments: Vec<AttachmentRef>,
        admission: crate::active_turns::TurnLease,
        config: SessionRuntimeConfig,
        model: ModelRef,
    ) -> oneshot::Receiver<Result<(), ClientError>> {
        let parts = match self.content_parts(&content, &attachments) {
            Ok(parts) => parts,
            Err(error) => {
                let message = format!("附件读取失败，本轮未发送: {error}");
                self.notify_error(&session_id, message.clone());
                self.active.finish(&admission);
                let (tx, rx) = oneshot::channel();
                let _ = tx.send(Err(ClientError::Runtime(message)));
                return rx;
            }
        };
        self.spawn_content_turn(
            session_id,
            parts,
            TranscriptOrigin::UserInput,
            admission,
            config,
            model,
        )
    }

    fn spawn_goal_turn(
        &self,
        session_id: SessionId,
        content: String,
        admission: crate::active_turns::TurnLease,
        config: SessionRuntimeConfig,
        model: ModelRef,
    ) -> oneshot::Receiver<Result<(), ClientError>> {
        // Single interactive path: direct goal loop (update_goal + tools +
        // spawn_agent). Orchestrate is not used for sessions.
        self.spawn_direct_goal_turn(session_id, content, admission, config, model)
    }

    fn spawn_direct_goal_turn(
        &self,
        session_id: SessionId,
        content: String,
        admission: crate::active_turns::TurnLease,
        config: SessionRuntimeConfig,
        model: ModelRef,
    ) -> oneshot::Receiver<Result<(), ClientError>> {
        self.spawn_goal_windows(session_id, Some(content), admission, config, model)
    }

    /// Continue an owed goal after a durable user decision, with no new user
    /// message and a fresh admission: the answered question is the instruction.
    fn spawn_goal_continuation(
        &self,
        session_id: SessionId,
        admission: crate::active_turns::TurnLease,
        config: SessionRuntimeConfig,
        model: ModelRef,
    ) -> oneshot::Receiver<Result<(), ClientError>> {
        self.spawn_goal_windows(session_id, None, admission, config, model)
    }

    /// Drive a Goal turn as one LOGICAL turn made of one or more work windows.
    ///
    /// A window ends through `update_goal(yield)`: the objective still owes
    /// work, nothing obstructs it, and the model chose to close the window.
    /// The runtime continues the SAME goal in a fresh window with no user
    /// message and no new admission — the cancellation token, task-cancel
    /// flag, approver, clarifier and steering all belong to the logical turn,
    /// not to a window. Every other terminal (completed, blocked, cancelled,
    /// failed, budget-limited) ends the logical turn here.
    ///
    /// `content = None` starts directly in continuation mode: the first window
    /// is the runtime re-entering an owed goal after a durable user answer.
    /// The continuation decision is made from the structured `StopReason` the
    /// engine returned, never from model text or a projected event.
    fn spawn_goal_windows(
        &self,
        session_id: SessionId,
        content: Option<String>,
        admission: crate::active_turns::TurnLease,
        config: SessionRuntimeConfig,
        model: ModelRef,
    ) -> oneshot::Receiver<Result<(), ClientError>> {
        let app = self.app.clone();
        let client = self.clone();
        let events = self.events_for(&session_id);
        let active = self.active.clone();
        self.active.note_model(&session_id, &model);
        let mode = config.mode;
        let sandbox = config.sandbox;
        let axis_at_start = config.collaboration.clone();
        let repo = self.app.layout.repo_root.clone();
        let cancel = admission.cancellation();
        let approver = self.approver(&session_id, cancel.clone());
        let clarifier = self.clarifier(&session_id, cancel.clone());
        // Text the user sends while this turn runs lands here and is injected
        // at the top of the next round.
        let steering = self.steering_for(&session_id);
        let (accepted_tx, accepted_rx) = oneshot::channel();

        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            handle.block_on(async move {
                emit_project_rules(&events, repo.as_deref());
                let accepted_tx = Arc::new(Mutex::new(Some(accepted_tx)));
                // The user's own message opens the first window only; every
                // later window is the runtime continuing an owed goal. A
                // `None` content means the answer that resumed the goal IS the
                // opening of its continuation window.
                let mut content = content;
                let mut first_window = true;
                let mut started = false;
                let mut user_cancelled = false;
                loop {
                    // Each window owns its bridge: the terminal publisher is
                    // single-use. The terminal event is held in the slot so
                    // this loop, not the engine callback, decides whether it
                    // ends the logical turn (publish inside the admission
                    // boundary) or only closes a window (publish now; the
                    // logical turn stays admitted for its next window).
                    let terminal: Arc<Mutex<Option<RuntimeEvent>>> = Arc::new(Mutex::new(None));
                    let mut bridge = EventBridge::new(events.clone())
                        .with_progress(active.clone(), session_id.clone())
                        .with_terminal_publisher({
                            let terminal = terminal.clone();
                            move |event| {
                                *terminal
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                    Some(event);
                            }
                        });
                    let observer_acceptance = accepted_tx.clone();
                    let result = {
                        let mut observer = |event: leveler_engine::EngineEvent| {
                            if matches!(event, leveler_engine::EngineEvent::TurnStarted { .. })
                                && let Some(tx) = observer_acceptance
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .take()
                            {
                                let _ = tx.send(Ok(()));
                                hit_after_turn_started_test_barrier();
                            }
                            bridge.forward(event);
                        };
                        if let Some(text) = content.take() {
                            app.run_in_session_with_clarifier(
                                &session_id,
                                &model,
                                mode,
                                &text,
                                approver.clone(),
                                clarifier.clone(),
                                sandbox,
                                Some(steering.clone() as Arc<dyn leveler_agent::SteeringSource>),
                                admission.task_cancel(),
                                &mut observer,
                                cancel.clone(),
                            )
                            .await
                        } else {
                            // No instruction: continue the goal the previous
                            // window yielded. A goal that settled since (the
                            // user cancelled the logical task) is not reopened
                            // and writes nothing — see `AppError::GoalSettled`.
                            app.resume_in_session_continuation(
                                &session_id,
                                approver.clone(),
                                clarifier.clone(),
                                Some(steering.clone() as Arc<dyn leveler_agent::SteeringSource>),
                                admission.task_cancel(),
                                &mut observer,
                                cancel.clone(),
                            )
                            .await
                        }
                    };
                    if first_window {
                        first_window = false;
                        // `TurnStarted` is the only consumer of the acceptance
                        // sender. Still holding it after the turn returned
                        // means the turn never reached durable admission.
                        let mut slot = accepted_tx
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        started = match slot.take() {
                            Some(tx) => {
                                let _ = tx.send(Err(turn_rejection(&result)));
                                false
                            }
                            None => true,
                        };
                    }
                    let terminal = terminal
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .take();
                    let yielded = matches!(
                        &result,
                        Ok(outcome) if outcome.stop_reason == leveler_agent::StopReason::Yielded
                    );
                    if yielded && started {
                        // The window ended inside the goal: its terminal is a
                        // durable fact and is published now, while the logical
                        // turn keeps its admission for the next window.
                        if let Some(event) = terminal {
                            let _ = events.send(event);
                        }
                        if admission
                            .task_cancel()
                            .load(std::sync::atomic::Ordering::SeqCst)
                        {
                            // The user ended the LOGICAL task between windows.
                            // No window is running, so this commits the
                            // cancelled terminal exactly like the idle path
                            // the command handler uses when nothing is active.
                            match app.cancel_task(&session_id).await {
                                Ok(Some(event)) => {
                                    // The idle cancel path returns the durable
                                    // terminal fact; project it through a fresh
                                    // bridge, inside the admission boundary.
                                    let projection_events = events.clone();
                                    active.finish_with(&admission, || {
                                        let mut cancel_bridge = EventBridge::new(projection_events);
                                        cancel_bridge.forward(event);
                                    });
                                }
                                Ok(None) => {
                                    active.finish(&admission);
                                }
                                Err(error) => {
                                    active.finish_with(&admission, || {
                                        let _ = events.send(RuntimeEvent::Notification {
                                            level: NotificationLevel::Error,
                                            message: format!("任务取消失败：{error}"),
                                        });
                                    });
                                }
                            }
                            user_cancelled = true;
                            break;
                        }
                        if cancel.is_cancelled() {
                            // Esc ended the turn after its window had already
                            // yielded. Nothing more is owed to publish: the
                            // goal keeps its resumable truth.
                            user_cancelled = true;
                            active.finish(&admission);
                            break;
                        }
                        continue;
                    }
                    // A cancel is the user saying stop: pending text is not
                    // delivered as a new turn. Every other ending owes the
                    // person an answer to what they typed.
                    user_cancelled = user_cancelled
                        || matches!(
                            &result,
                            Err(crate::AppError::Agent(leveler_agent::AgentError::Cancelled))
                        );
                    if matches!(&result, Err(crate::AppError::GoalSettled(_))) {
                        // The goal settled before this continuation could
                        // start. The last durable terminal is the authority and
                        // this window wrote nothing: stop silently.
                        active.finish(&admission);
                        break;
                    }
                    // TaskFinished is the normal path's one terminal authority.
                    // Publish it inside the admission boundary; only failures
                    // that never reached that commit need the wrapper fallback.
                    active.finish_with(&admission, || match terminal {
                        Some(event) => {
                            let _ = events.send(event);
                        }
                        None => {
                            let _ = events.send(turn_runtime_event(result));
                        }
                    });
                    break;
                }
                // The goal's terminal may have ended this session's Goal route;
                // publish the row so the UI stops claiming a Goal that is over.
                client
                    .publish_routes_after_turn(&session_id, &axis_at_start)
                    .await;
                // Only a turn that durably started can owe its journaled
                // input a promotion. Re-promoting after a turn that never
                // started repeats the same refusal without end, and the spin
                // holds the admission slot its own recovery needs.
                if started && !user_cancelled {
                    client.deliver_unconsumed_input(&session_id).await;
                }
                // Session-owned background reap moved to the engine's terminal
                // settlement (finish_from_result) so chat-routed continuations
                // are covered too — one reap site, not one per spawn function
                // (R006 R6-P4).
            });
        });
        accepted_rx
    }

    /// Start (or re-open) Goal mode from `/goal <task>`.
    ///
    /// The durable axis is written before the turn starts, so ordinary
    /// submissions after it — and after a resume — stay in the goal profile
    /// until an explicit `/goal clear` flips the axis back. This is the
    /// runtime's single mapping; no client carries its own goal flag.
    async fn run_goal_turn(
        &self,
        session_id: SessionId,
        content: String,
        require_new_turn: bool,
    ) -> Result<(), ClientError> {
        // A client that sends the explicit command while a turn already runs
        // must get the same upgrade as a text steer. Refusing with "session is
        // busy" would make Goal entry depend on which command shape a client
        // happened to send. An unattended delivery is different: it must own
        // the turn it starts, because its terminal is attributed to that one
        // delivery. Joining another client's work would make the schedule
        // settle (and charge) work it did not start.
        if self.active.is_running(&session_id) {
            if require_new_turn {
                return Err(ClientError::Runtime(
                    "the session already has an active turn; this delivery requires a new \
                     turn and does not join another client's work"
                        .into(),
                ));
            }
            return self.steer_into_goal(session_id, content).await;
        }
        let config = self.runtime_config(&session_id).await?;
        // A Goal turn is an agent turn. Refuse before the declaration is
        // persisted: a deterministic session must not be given a goal it can
        // never execute.
        require_model(&config, &session_id)?;
        let axis_changed =
            config.collaboration != leveler_lifecycle::CollaborationMode::Goal.as_str();
        // The axis and the declaration are one durable fact. Writing the axis
        // first and leaving the text in this process's memory is the window
        // this closes: a crash there used to leave a Goal session whose
        // objective had never been persisted anywhere.
        self.persist_goal_declaration(&session_id, &content).await?;
        if axis_changed && let Ok(session) = self.snapshot(&session_id).await {
            let _ = self
                .events_for(&session_id)
                .send(RuntimeEvent::SessionUpdated { session });
        }
        let config = self.runtime_config(&session_id).await?;
        let model = require_model(&config, &session_id)?;
        let cancel = self
            .stage_turn(&session_id, &content, true, 0, None, None)
            .await?;
        let accepted = self.spawn_goal_turn(session_id, content, cancel, config, model);
        await_turn_acceptance(accepted).await
    }

    /// Hand a `/goal <task>` declaration to the turn that is already running.
    ///
    /// Writing the axis alone would leave the running Chat executor to finish
    /// on its next stage summary, and the user would have to say `继续` — the
    /// exact behavior this path exists to remove. The declaration is queued
    /// for the executor, which turns Goal mode on at its next round, injects
    /// the user's text as an ordinary user row, and continues until
    /// `update_goal(complete|blocked)`.
    async fn steer_into_goal(
        &self,
        session_id: SessionId,
        task: String,
    ) -> Result<(), ClientError> {
        // The durable form and the in-memory copy must be byte-identical:
        // the executor consumes the durable declaration by compare-and-swap
        // on this text.
        let task = task.trim().to_string();
        // Durable before in-memory: if this process dies before the running
        // executor drains the declaration, the session's Goal axis and its
        // objective are still one consistent pair on disk, and the next Goal
        // turn adopts it.
        self.persist_goal_declaration(&session_id, &task).await?;
        {
            let goal = self.goal_state_for(&session_id);
            let mut goal = goal
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            goal.requested = true;
            goal.pending.push(task);
        }
        self.signal_pending_input(&session_id);
        let events = self.events_for(&session_id);
        if let Ok(session) = self.snapshot(&session_id).await {
            let _ = events.send(RuntimeEvent::SessionUpdated { session });
        }
        let _ = events.send(RuntimeEvent::Notification {
            level: leveler_client_protocol::NotificationLevel::Info,
            message: "已切换到 Goal 模式 · 当前这一轮会继续到目标达成或阻塞".to_string(),
        });
        Ok(())
    }

    /// `/goal clear`: the explicit way back to Chat.
    async fn clear_goal_axis(&self, session_id: SessionId) -> Result<(), ClientError> {
        self.persist_turn_collaboration(
            &session_id,
            leveler_lifecycle::CollaborationMode::Chat.as_str(),
        )
        .await?;
        {
            let goal = self.goal_state_for(&session_id);
            let mut goal = goal
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            goal.requested = false;
            goal.pending.clear();
        }
        let events = self.events_for(&session_id);
        if let Ok(session) = self.snapshot(&session_id).await {
            let _ = events.send(RuntimeEvent::SessionUpdated { session });
        }
        let _ = events.send(RuntimeEvent::Notification {
            level: leveler_client_protocol::NotificationLevel::Info,
            message: "已退出 Goal 模式 · 后续消息按 Chat 处理".to_string(),
        });
        Ok(())
    }

    async fn goal_status(&self, session_id: SessionId) -> Result<(), ClientError> {
        let config = self.runtime_config(&session_id).await?;
        let label = if config.collaboration == leveler_lifecycle::CollaborationMode::Goal.as_str() {
            "Goal"
        } else {
            "Chat"
        };
        let _ = self
            .events_for(&session_id)
            .send(RuntimeEvent::Notification {
                level: leveler_client_protocol::NotificationLevel::Info,
                message: format!("当前协作模式：{label}"),
            });
        Ok(())
    }

    /// Route an ordinary user message through the vision gate, then
    /// a chat/content or goal turn chosen by the session's collaboration axis.
    async fn handle_submit_message(
        &self,
        session_id: SessionId,
        content: String,
        attachments: Vec<AttachmentRef>,
        _origin: Option<&CommandId>,
        seed_input_id: Option<String>,
    ) -> Result<(), ClientError> {
        // A promoted append is already the user's instruction for this turn.
        // Re-parsing it as `/goal` or a continuation would turn the text they
        // typed into a control command a second time.
        let promoted = seed_input_id.is_some();
        // `/goal` is product semantics, not a client feature. A client that
        // does not parse slash commands (Desktop, an older build, a steering
        // message) still reaches the same durable axis and the same executor
        // profile through this one parse.
        if !promoted
            && attachments.is_empty()
            && let Some(command) = leveler_client_protocol::parse_goal_command(&content)
        {
            use leveler_client_protocol::GoalCommand;
            return match command {
                GoalCommand::Run(task) if self.active.is_running(&session_id) => {
                    self.steer_into_goal(session_id, task).await
                }
                GoalCommand::Run(task) => self.run_goal_turn(session_id, task, false).await,
                GoalCommand::Clear => self.clear_goal_axis(session_id).await,
                GoalCommand::Status => self.goal_status(session_id).await,
                GoalCommand::Usage => Err(ClientError::Runtime(
                    "用法: /goal <任务>（/goal clear 返回 Chat，/goal status 查看模式）".into(),
                )),
            };
        }
        let config = self.runtime_config(&session_id).await?;
        // Every route below runs a model. A session that exists only for
        // deterministic host commands says so here, before it stages a turn.
        let model = require_model(&config, &session_id)?;
        // Continuation, pause and amendment are read by the MODEL from the
        // user's message and declared through the structured intent tool; the
        // runtime validates that declaration against the durable goal. This
        // path never classifies the text, so any language and phrasing reaches
        // the same machinery.
        // The TUI asks the user about this first, but the reducer is one
        // client. Every other one — web, headless, a model changed between
        // staging and sending — arrives here, and here the answer is the same:
        // a model that cannot read an image is told so, never handed the text
        // with the picture removed.
        self.refuse_unreadable_images(&model, &attachments).await?;
        let seed_message_id = seed_input_id.as_deref().map(MessageId::new);
        let cancel = self
            .stage_turn(
                &session_id,
                &content,
                true,
                image_count(&attachments),
                seed_message_id,
                seed_input_id,
            )
            .await?;
        // CollaborationMode is the single source of turn profile:
        // goal → goal_mode / update_goal path; chat|plan → content turn.
        // Plan read_only is applied inside engine from session.collaboration.
        let accepted = if collaboration_routes_submit_to_goal(&config.collaboration) {
            if !attachments.is_empty() {
                // The content entry resolves the same session-owned Goal
                // profile while preserving every attachment.
                self.spawn_turn(session_id, content, attachments, cancel, config, model)
            } else {
                self.spawn_goal_turn(session_id, content, cancel, config, model)
            }
        } else {
            self.spawn_turn(session_id, content, attachments, cancel, config, model)
        };
        await_turn_acceptance(accepted).await
    }

    /// Route an explicit resume command into the session's logical task.
    ///
    /// The command itself is the client's express request to resume; the
    /// runtime decides whether a task is really resumable. The user's words are
    /// not classified here — a continuation said in an ordinary message is
    /// read by the model and declared through the structured intent tool.
    async fn handle_resume_task(
        &self,
        session_id: SessionId,
        content: String,
        origin: Option<&CommandId>,
    ) -> Result<(), ClientError> {
        // Continuation intent is not enough: the runtime must also own a
        // resumable task. An explicitly cancelled task must never be silently
        // reopened, so it is told apart from the ordinary "nothing to resume".
        match self.continuation_state(&session_id).await? {
            ContinuationState::Resumable => {}
            ContinuationState::Cancelled => {
                // Refuse as a rejected submission, not a silent no-op: the
                // client optimistically went busy and showed the input, and it
                // must roll both back. A plain notification would leave it
                // waiting for a turn that never starts.
                return Err(ClientError::Runtime(
                    "该任务已被取消，无法继续；如需重新开始，请直接描述任务。".to_string(),
                ));
            }
            ContinuationState::NotResumable => {
                // Nothing to resume: the same text is a normal message.
                return self
                    .handle_submit_message(session_id, content, Vec::new(), origin, None)
                    .await;
            }
        }
        // The instruction is the user's own words. The original objective stays
        // the task's first user message and the resume path seeds the prior
        // plan/ledger, so what the user adds here is an addition and never a
        // replacement.
        tracing::info!(
            session = %session_id.as_str(),
            "explicit resume command re-enters the session's logical task"
        );
        self.resume_session_task(session_id, content).await
    }

    /// Re-enter the session's logical task with the user's own instruction.
    ///
    /// The caller has already decided that a resume is warranted; this is the
    /// mechanics every continuation path shares (staging, the model pinned on
    /// the session row, the resume turn).
    async fn resume_session_task(
        &self,
        session_id: SessionId,
        content: String,
    ) -> Result<(), ClientError> {
        let instruction = Message::user_input(content.clone());
        // Resuming runs the agent on the session's stored model. A session
        // without one has nothing to resume.
        let config = self.runtime_config(&session_id).await?;
        require_model(&config, &session_id)?;
        let axis_at_start = self
            .app
            .session_product_axes(&session_id)
            .await
            .map_err(|error| ClientError::Runtime(error.to_string()))?
            .as_str()
            .to_string();
        let cancel = self
            .stage_turn(&session_id, &content, false, 0, None, None)
            .await?;
        // A resume continues the execution whose model is on the session row,
        // not the current default: a configuration reload must not silently
        // move an interrupted task onto another model. Name that model so the
        // live view reports what is actually running.
        if let Ok(db) = self.app.open_database().await
            && let Ok(Some(record)) = SessionRepository::new(&db).get(&session_id).await
            && let Some(model) = record.model.as_deref().and_then(ModelRef::parse)
        {
            self.active.note_model(&session_id, &model);
        }
        let accepted = self.spawn_resume_turn(session_id, instruction, cancel, axis_at_start);
        await_turn_acceptance(accepted).await
    }

    /// What an explicit resume command means for this session.
    ///
    /// The user chose the resume action in a client; the user's words are not
    /// classified here. The persisted terminal is the authority: an
    /// `interrupted` turn (user
    /// Esc, force-cancel) or a `failed` turn whose typed failure is a
    /// recoverable provider/network fault is resumable. A task the user
    /// explicitly cancelled is reported as such (a continuation must not
    /// reopen it); everything else — completed, blocked, non-recoverably
    /// failed — has simply nothing to resume. A fenced recovery interruption
    /// without TaskFinished can resume only its latest valid input after the
    /// recovery owner has released the task.
    async fn continuation_state(
        &self,
        session_id: &SessionId,
    ) -> Result<ContinuationState, ClientError> {
        if self.active.is_running(session_id) {
            return Ok(ContinuationState::NotResumable);
        }
        let db = self
            .app
            .open_database()
            .await
            .map_err(|error| ClientError::Runtime(error.to_string()))?;
        let stores = leveler_storage::EngineStores::from_database(&db);
        let Some(facts) = db
            .session_facts_for(session_id)
            .await
            .map_err(|error| ClientError::Runtime(error.to_string()))?
        else {
            return Ok(ContinuationState::NotResumable);
        };
        // Only a genuinely later admitted turn supersedes a terminal. A late
        // TurnFinished for the same turn cannot reopen Cancelled/Completed.
        let newer_turn = facts.turns.last().is_some_and(|turn| {
            facts
                .terminal_turn_ordinal
                .is_some_and(|ordinal| turn.ordinal > ordinal)
        });
        // Owed work outranks the last terminal. A goal that is still running
        // says no window has finished the task, and the resume path re-enters
        // that goal by identity. This matters exactly when the terminal in
        // front of the user belongs to something else: after a blocked window
        // the question they asked ends as its own settled Chat turn, and a
        // `继续` after it is still about the goal's work.
        if let Ok(Some(task)) = stores.tasks.task_for_session(session_id).await {
            let running = stores
                .goals
                .for_task(&task)
                .await
                .map_err(|error| ClientError::Runtime(error.to_string()))?
                .into_iter()
                .filter(|goal| goal.state == leveler_storage::GoalState::Running)
                .count();
            if running == 1 {
                return Ok(ContinuationState::Resumable);
            }
        }
        if let Some(row) = facts.terminal.as_ref().filter(|_| !newer_turn) {
            if row.schema_version > leveler_storage::EVENT_SCHEMA_VERSION {
                return Err(ClientError::Runtime(
                    "task terminal schema is newer than this reader".into(),
                ));
            }
            let event = leveler_engine::EngineEvent::from_payload(&row.payload)
                .map_err(|error| ClientError::Runtime(error.to_string()))?;
            let leveler_engine::EngineEvent::TaskFinished {
                outcome,
                failure,
                stop,
                ..
            } = event
            else {
                return Err(ClientError::Runtime(
                    "task_finished row carried a different event".into(),
                ));
            };
            if outcome == leveler_lifecycle::TaskOutcome::Cancelled {
                return Ok(ContinuationState::Cancelled);
            }
            if !task_outcome_is_resumable(&outcome, stop, failure.as_ref()) {
                return Ok(ContinuationState::NotResumable);
            }
        } else {
            let Some(turn) = facts.turns.last().filter(|turn| {
                turn.status == "interrupted" && matches!(turn.kind.as_str(), "user" | "chat")
            }) else {
                return Ok(ContinuationState::NotResumable);
            };
            // Recovery committed this input under fencing. Do not let legacy
            // resume's compatibility search skip a corrupt latest input and
            // quietly bind continuation to an unrelated older objective.
            let persisted = leveler_storage::TurnRepository::new(&db)
                .list_by_ids(session_id, std::slice::from_ref(&turn.id))
                .await
                .map_err(|error| ClientError::Runtime(error.to_string()))?
                .pop()
                .ok_or_else(|| {
                    ClientError::Runtime("recovered interrupted turn is missing".into())
                })?;
            let payload = persisted.payload.as_deref().ok_or_else(|| {
                ClientError::Runtime("recovered interrupted turn has no initiating payload".into())
            })?;
            leveler_engine::decode_turn_initiating_message_opt(payload)
                .map_err(|error| ClientError::Runtime(error.to_string()))?;
            let Some(task) = stores
                .tasks
                .task_for_session(session_id)
                .await
                .map_err(|error| ClientError::Runtime(error.to_string()))?
            else {
                return Ok(ContinuationState::NotResumable);
            };
            let Some(owner) = stores
                .ownership
                .current(&task)
                .await
                .map_err(|error| ClientError::Runtime(error.to_string()))?
            else {
                return Ok(ContinuationState::NotResumable);
            };
            if owner.boot.is_some() || owner.runtime.is_some() {
                return Ok(ContinuationState::NotResumable);
            }
            // Eligibility is a read, not an ownership grant. The unchanged
            // stage/admission path must still win the current engine CAS.
        }
        // Resume rebuilds from the transcript; an empty one has nothing to
        // continue and would be refused by the engine anyway.
        let messages = MessageRepository::new(&db)
            .count(session_id)
            .await
            .map_err(|error| ClientError::Runtime(error.to_string()))?;
        Ok(if messages > 0 {
            ContinuationState::Resumable
        } else {
            ContinuationState::NotResumable
        })
    }

    /// Drive `AgentEngine::resume` with the user's continuation message.
    ///
    /// The same plumbing as a goal turn — admission, approver/clarifier,
    /// steering, terminal publication — so cancelling, approving and resuming
    /// behave exactly as they do for any other turn. Only the harness entry
    /// point (`resume_with_instruction`) and what it seeds differ.
    fn spawn_resume_turn(
        &self,
        session_id: SessionId,
        instruction: Message,
        admission: crate::active_turns::TurnLease,
        axis_at_start: String,
    ) -> oneshot::Receiver<Result<(), ClientError>> {
        let app = self.app.clone();
        let client = self.clone();
        let events = self.events_for(&session_id);
        let active = self.active.clone();
        let repo = self.app.layout.repo_root.clone();
        let cancel = admission.cancellation();
        let approver = self.approver(&session_id, cancel.clone());
        let clarifier = self.clarifier(&session_id, cancel.clone());
        let steering = self.steering_for(&session_id);
        let (accepted_tx, accepted_rx) = oneshot::channel();

        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            handle.block_on(async move {
                emit_project_rules(&events, repo.as_deref());
                let terminal_active = active.clone();
                let terminal_admission = admission.clone();
                let terminal_events = events.clone();
                let mut bridge = EventBridge::new(events.clone())
                    .with_progress(active.clone(), session_id.clone())
                    .with_terminal_publisher(move |event| {
                        terminal_active.finish_with(&terminal_admission, || {
                            let _ = terminal_events.send(event);
                        });
                    });
                let accepted_tx = Arc::new(Mutex::new(Some(accepted_tx)));
                let observer_acceptance = accepted_tx.clone();
                let result = {
                    let mut observer = |event: leveler_engine::EngineEvent| {
                        if matches!(event, leveler_engine::EngineEvent::TurnStarted { .. })
                            && let Some(tx) = observer_acceptance
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .take()
                        {
                            let _ = tx.send(Ok(()));
                            hit_after_turn_started_test_barrier();
                        }
                        bridge.forward(event);
                    };
                    app.resume_in_session_with_instruction(
                        &session_id,
                        instruction,
                        approver,
                        clarifier,
                        Some(steering as Arc<dyn leveler_agent::SteeringSource>),
                        admission.task_cancel(),
                        &mut observer,
                        cancel,
                    )
                    .await
                };
                // `TurnStarted` is the only consumer of the acceptance
                // sender. Still holding it after the turn returned means the
                // turn never reached durable admission.
                let started = {
                    let mut slot = accepted_tx
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    match slot.take() {
                        Some(tx) => {
                            let _ = tx.send(Err(turn_rejection(&result)));
                            false
                        }
                        None => true,
                    }
                };
                // A cancel is the user saying stop: pending text is not
                // delivered as a new turn. Every other ending owes the person
                // an answer to what they typed.
                let user_cancelled = matches!(
                    &result,
                    Err(crate::AppError::Agent(leveler_agent::AgentError::Cancelled))
                );
                if !bridge.terminal_published() {
                    active.finish_with(&admission, || {
                        let _ = events.send(turn_runtime_event(result));
                    });
                } else {
                    active.finish(&admission);
                }
                // A resume that ends its goal for good also ends Goal mode.
                client
                    .publish_routes_after_turn(&session_id, &axis_at_start)
                    .await;
                // Only a turn that durably started can owe its journaled
                // input a promotion. Re-promoting after a turn that never
                // started repeats the same refusal without end, and the spin
                // holds the admission slot its own recovery needs.
                if started && !user_cancelled {
                    client.deliver_unconsumed_input(&session_id).await;
                }
            });
        });
        accepted_rx
    }

    /// `/recap` (long-goal P3): cut a durable goal checkpoint and surface it
    /// as a Recap history item.
    ///
    /// This is NOT a transcript summary: the checkpoint projects
    /// authoritative facts through the engine's one canonical builder and is
    /// persisted before anything is shown. The semantic wording is attempted
    /// first with a hard timeout; any failure degrades to a structured-only
    /// checkpoint with a deterministic display — never a lost checkpoint,
    /// never "recap unavailable".
    fn spawn_recap(&self, session_id: SessionId, model: ModelRef) {
        let app = self.app.clone();
        let events = self.events_for(&session_id);
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            handle.block_on(async move {
                let result: Result<Option<leveler_client_protocol::UiGoalRecap>, String> = async {
                    let db = app.open_database().await.map_err(|e| e.to_string())?;
                    let stores = leveler_storage::EngineStores::from_database(&db);
                    let engine = app.task_engine(&db).map_err(|e| e.to_string())?;
                    // Recap only the exact Goal lineage named by the latest
                    // durable turn. A latest Chat or legacy ambiguous row must
                    // not fall back to an older goal in the session.
                    let Some(scope) = leveler_agent::coding::latest_session_goal_checkpoint_scope(
                        &stores,
                        &session_id,
                    )
                    .await
                    .map_err(|e| e.to_string())?
                    else {
                        return Ok(None);
                    };
                    let semantic = Self::recap_semantic(&app, &session_id, model.clone()).await;
                    let record = leveler_agent::coding::create_goal_checkpoint(
                        &engine,
                        &session_id,
                        &scope,
                        leveler_lifecycle::CheckpointReason::Manual,
                        app.layout
                            .primary_workspace()
                            .map(leveler_agent::coding::GitWorkspace::new)
                            .as_ref()
                            .map(|workspace| {
                                workspace as &dyn leveler_agent::coding::WorkspaceFacts
                            }),
                        semantic,
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                    Ok(record.as_ref().map(crate::goal_recap::project_goal_recap))
                }
                .await;
                match result {
                    Ok(Some(recap)) => {
                        let _ = events.send(RuntimeEvent::GoalRecapCreated { recap });
                    }
                    Ok(None) => {
                        let _ = events.send(RuntimeEvent::Notification {
                            level: leveler_client_protocol::NotificationLevel::Info,
                            message: "当前会话没有可回顾的 Goal".to_string(),
                        });
                    }
                    Err(error) => {
                        let _ = events.send(RuntimeEvent::Notification {
                            level: leveler_client_protocol::NotificationLevel::Warning,
                            message: format!("阶段回顾失败：{error}"),
                        });
                    }
                }
            });
        });
    }

    /// Bounded, optional semantic wording for a manual recap. Grounded in the
    /// projected structured facts plus a short recent-transcript tail;
    /// reasoning never reaches this input (it is dropped at the stream
    /// boundary and never persisted). Every failure returns `None`.
    async fn recap_semantic(
        app: &Application,
        session_id: &SessionId,
        model: ModelRef,
    ) -> Option<leveler_agent::coding::SemanticRecap> {
        const RECAP_SEMANTIC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
        let work = async {
            let db = app.open_database().await.ok()?;
            let raw = leveler_engine::RawTranscript::load_lossy(&db, session_id)
                .await
                .ok()?;
            if raw.is_empty() {
                return None;
            }
            let tail: String = raw
                .messages
                .iter()
                .rev()
                .filter_map(|m| {
                    let text = m.text_content();
                    (!text.is_empty()).then_some(text)
                })
                .take(8)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n---\n");
            let tail: String = tail.chars().take(6_000).collect();
            let prompt = format!(
                "请基于下面的对话片段，用两行中文总结当前任务进展。\
                 第一行：当前阶段与已完成的内容（一句话）。\
                 第二行：以「下一步：」开头，说明接下来要做什么。\
                 只依据给出的内容，不要编造未发生的事实。\n\n{tail}"
            );
            let mut request =
                ModelRequest::new(model.clone(), vec![Message::text(Role::User, prompt)]);
            request.tool_choice = ToolChoice::None;
            request.max_output_tokens = Some(256);
            // A recap is a small bounded wording task, not coding work.
            request.reasoning_effort = Some(leveler_model::ReasoningEffort::Low);
            let started = std::time::Instant::now();
            let outcome = app
                .registry()
                .generate(request, CancellationToken::new())
                .await;
            let latency_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
            crate::observability::record_auxiliary_call(
                app,
                &db,
                session_id,
                leveler_storage::ModelCallKind::SemanticRecap,
                &model.provider,
                &model.model,
                Some("low"),
                &outcome,
                latency_ms,
                None,
                None,
            )
            .await
            .ok()?;
            let resp = outcome.ok()?;
            let text = resp.message.text_content();
            let mut lines = text.lines().filter(|l| !l.trim().is_empty());
            let display = lines.next()?.trim().to_string();
            let next_action = lines.next().map(|l| {
                l.trim()
                    .trim_start_matches("下一步：")
                    .trim_start_matches("下一步:")
                    .trim()
                    .to_string()
            });
            Some(leveler_agent::coding::SemanticRecap {
                goal_summary: Some(text.trim().to_string()),
                display_summary: Some(display),
                next_action: next_action.filter(|s| !s.is_empty()),
            })
        };
        tokio::time::timeout(RECAP_SEMANTIC_TIMEOUT, work)
            .await
            .unwrap_or_default()
    }

    /// Answer one `/btw` question on the session's side thread.
    ///
    /// The side thread owns its own conversation (`BtwSession::history`) and
    /// its own cancel handle; it never takes the main turn's cancel token and
    /// never appends to `MessageRepository`. Each round re-reads the main
    /// transcript so a follow-up sees the main run's current state, while the
    /// side thread's own earlier turns carry the side conversation.
    fn spawn_btw(
        &self,
        session_id: SessionId,
        question: String,
        config: SessionRuntimeConfig,
        model: ModelRef,
    ) {
        let app = self.app.clone();
        let events = self.events_for(&session_id);
        // The session axes the read-only side surface is composed from. Read
        // before the side thread starts, so a later profile switch cannot make
        // the tool composition and the recorded turn disagree.
        let mode = config.mode;
        let sandbox = config.sandbox;
        let btw = self.btw.clone();

        // One answer at a time per side thread: a second question while one is
        // streaming would race the history append and the single "generating"
        // state. Refuse it visibly instead of interleaving two answers.
        let (cancel, history, btw_seq) = {
            let mut threads = btw
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let running = threads
                .get(&session_id)
                .and_then(|thread| thread.cancel.as_ref())
                .is_some_and(|token| !token.is_cancelled());
            if running {
                let _ = events.send(RuntimeEvent::Notification {
                    level: NotificationLevel::Warning,
                    message: "上一条旁问还在回答，先等它结束或按 Ctrl+C 停止".to_string(),
                });
                return;
            }
            let thread = threads.entry(session_id.clone()).or_default();
            thread.seq += 1;
            let btw_seq = thread.seq;
            let token = CancellationToken::new();
            thread.cancel = Some(token.clone());
            // `active_background_tasks` used to be logged here from
            // `try_active_ids_for_scope(..).unwrap_or(0)`: an unavailable lock
            // became "0 tasks", which reads as a fact but is not one. Unknown
            // is not zero, so the field is gone rather than made "more
            // accurate" with an extra await on the side-question path.
            tracing::info!(
                session = %session_id,
                btw_seq,
                history_messages = thread.history.len(),
                model = %model,
                "BTW_START"
            );
            (token, thread.history.clone(), btw_seq)
        };

        let handle = tokio::runtime::Handle::current();
        // Snapshot the live view and the admitted turn NOW, before the blocking
        // side thread starts: the observer describes the main task at the
        // moment it was asked, and copying here means the side thread never
        // locks or blocks the main turn.
        let live_view = self.live_views.view(&session_id);
        let turn = self
            .active
            .snapshots()
            .into_iter()
            .find(|turn| turn.session_id == session_id);
        let cwd = self
            .app
            .layout
            .primary_workspace()
            .map(|root| root.display().to_string());
        tokio::task::spawn_blocking(move || {
            handle.block_on(async move {
                let _ = events.send(RuntimeEvent::BtwStarted {
                    question: question.clone(),
                });

                let result: Result<String, String> = async {
                    let db = app.open_database().await.map_err(|e| e.to_string())?;
                    let profile = app
                        .registry()
                        .profile(&model)
                        .await
                        .map_err(|e| e.to_string())?;
                    let overrides = app
                        .execution_overrides_for_session(Some(session_id.as_str()), Some(&model))
                        .await
                        .map_err(|e| e.to_string())?;
                    let policy = leveler_agent::coding::resolve_execution_policy(
                        &profile,
                        leveler_agent::coding::ExecutionRole::Main,
                        &leveler_agent::coding::TurnProfile::Chat {
                            continuation: leveler_agent::ContinuationPolicy::UntilTerminal,
                            limits: leveler_agent::StepLimits::default(),
                        },
                        overrides.as_ref(),
                    );
                    // The main task's runtime projection: the same facts the
                    // Background Tasks page renders, bound by id, copied out
                    // without taking any lock the main turn could hold.
                    let runtime_context = {
                        let snapshots = app
                            .background_tasks()
                            .try_all_snapshots()
                            .await
                            .unwrap_or_default();
                        let background_tasks: Vec<crate::side_question::BackgroundTaskFact> =
                            snapshots
                                .into_iter()
                                .filter(|task| {
                                    task.owner_scope.as_deref() == Some(session_id.as_str())
                                })
                                .map(|task| {
                                    let command = if task.args.is_empty() {
                                        task.program.clone()
                                    } else {
                                        format!("{} {}", task.program, task.args.join(" "))
                                    };
                                    crate::side_question::BackgroundTaskFact {
                                        task_id: task.id,
                                        command: command.clone(),
                                        pid: task.pid,
                                        status: background_status_label(task.status),
                                        elapsed_ms: task.duration_ms,
                                        exit_code: task.exit_code,
                                        output_tail: truncate_tail(&task.log, 8 * 1024),
                                    }
                                })
                                .collect();
                        let running_task_ids: Vec<String> = background_tasks
                            .iter()
                            .filter(|task| task.status == "running" || task.status == "killing")
                            .map(|task| task.task_id.clone())
                            .collect();
                        let active_tool_names: Vec<String> = live_view
                            .active_tools
                            .iter()
                            .map(|tool| tool.name.clone())
                            .collect();
                        let wait_on = turn.as_ref().and_then(|_| {
                            crate::side_question::derive_wait(&running_task_ids, &active_tool_names)
                        });
                        let ctx = crate::side_question::SideQuestionContext {
                            session_id: session_id.as_str().to_string(),
                            cwd: cwd.clone(),
                            turn_elapsed_ms: turn.as_ref().map(|turn| turn.elapsed_ms),
                            turn_idle_ms: turn.as_ref().map(|turn| turn.idle_ms),
                            wait_on,
                            active_tools: live_view
                                .active_tools
                                .iter()
                                .map(|tool| crate::side_question::ActiveToolFact {
                                    name: tool.name.clone(),
                                    elapsed_ms: tool.elapsed_ms,
                                    output_tail: truncate_tail(&tool.output_tail, 4 * 1024),
                                })
                                .collect(),
                            background_tasks,
                        };
                        crate::side_question::render(&ctx)
                    };
                    let question_message = side_question_message(&question, &runtime_context);
                    // The read-only tool surface the side question may use.
                    // Composed from the same owner as a normal turn, narrowed to
                    // the observe-class tools — no mutating tool and no harness
                    // control — so a side question cannot change the workspace or
                    // steer the main task. `None` when the model cannot call
                    // tools at all, which keeps the request unchanged for it.
                    //
                    // It is built BEFORE any pressure figure is taken: the
                    // projection must be of the request that is actually sent,
                    // and a tool-bearing request replays captured reasoning the
                    // way its route contract requires. Measuring a tool-less
                    // request instead would drop that reasoning from the wire
                    // (the provider validates it) and would understate both the
                    // fold decision and the recorded prompt size.
                    let side_tools = if profile.capabilities.tool_calling {
                        Some(
                            app.side_question_tools(
                                &model,
                                mode,
                                sandbox,
                                Some(session_id.as_str()),
                            )
                            .await
                            .map_err(|e| format!("构建旁问只读工具失败: {e}"))?,
                        )
                    } else {
                        None
                    };
                    let tool_definitions = side_tools
                        .as_ref()
                        .map(|(registry, _)| registry.definitions())
                        .unwrap_or_default();
                    // A side question executes its read-only tools through the
                    // SAME ToolHost admission + execution pipeline a turn uses.
                    // The app never reaches `ToolRegistry::execute` directly, so
                    // the one execution boundary stays one boundary.
                    let side_executor = side_tools.as_ref().map(|(registry, tool_context)| {
                        leveler_agent::Executor::new(
                            app.registry(),
                            Arc::new(registry.clone()),
                            tool_context.clone(),
                            model.clone(),
                            1,
                        )
                        .with_approver(Arc::new(AutoApprove))
                    });
                    let route = app.registry().route_identity(&model);
                    let project = |messages: &[Message]| {
                        leveler_model::RequestProjection::project_for_route(
                            messages,
                            &tool_definitions,
                            policy.reasoning_replay,
                            policy.reasoning_retention,
                            route.as_ref(),
                        )
                    };
                    // Pressure includes the entire side conversation and the
                    // question, before choosing a snapshot or a summary.
                    let measure_main = |main: &[Message]| {
                        let mut complete = main.to_vec();
                        complete.extend(history.iter().cloned());
                        complete.push(question_message.clone());
                        project(&complete).estimated_tokens()
                    };
                    let raw = leveler_engine::RawTranscript::load_strict(
                        &db,
                        &session_id,
                        "side question",
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                    let log = leveler_engine::EventLog::new(&db, session_id.clone());
                    let threshold = u64::from(policy.context_policy.pressure_threshold);
                    let context = raw
                        .assemble_unfolded_measured(&log, threshold, &measure_main)
                        .await
                        .map_err(|e| e.to_string())?;
                    let mut messages = context.prior;
                    messages.extend(history);
                    messages.push(question_message);
                    let mut projection = project(&messages);
                    if policy.context_policy.folding_enabled()
                        && projection.estimated_tokens() > threshold
                    {
                        let retention = policy.context_policy.retention;
                        let summary_request = leveler_context::summary_request(
                            app.registry().as_ref(),
                            &model,
                            leveler_engine::ModelCallKind::Compaction.default_reasoning_effort(),
                            &messages,
                            retention.keep_recent_messages,
                            retention.keep_recent_tokens,
                            policy.max_output_tokens,
                        )
                        .await
                        .ok_or("旁问上下文超过预算，无法生成有效摘要；原历史已保留")?;
                        let summary_input = summary_request
                            .projection
                            .as_ref()
                            .map(|p| p.estimated_tokens())
                            .unwrap_or_default();
                        let window = u64::from(policy.context_policy.context_window);
                        if window > 0
                            && summary_input
                                .saturating_add(u64::from(
                                    summary_request.max_output_tokens.unwrap_or(0),
                                ))
                                .saturating_add(u64::from(policy.context_policy.headroom))
                                > window
                        {
                            return Err("旁问摘要请求超过模型窗口；原历史已保留".into());
                        }
                        let summary_effort = summary_request.reasoning_effort.map(|e| e.as_wire());
                        let started = std::time::Instant::now();
                        let outcome = app
                            .registry()
                            .generate(summary_request, cancel.clone())
                            .await;
                        let latency_ms =
                            started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
                        crate::observability::record_auxiliary_call(
                            &app,
                            &db,
                            &session_id,
                            leveler_storage::ModelCallKind::Compaction,
                            &model.provider,
                            &model.model,
                            summary_effort,
                            &outcome,
                            latency_ms,
                            None,
                            Some(summary_input),
                        )
                        .await
                        .map_err(|e| format!("记录旁问摘要请求失败: {e}"))?;
                        let response = outcome.map_err(|e| e.to_string())?;
                        let summary = leveler_context::accepted_summary(&response)
                            .ok_or("旁问摘要未正常完成；原历史已保留")?;
                        messages = leveler_context::compact_messages(
                            &messages,
                            retention.keep_recent_messages,
                            retention.keep_recent_tokens,
                            Some(&summary),
                            Some(question.as_str()),
                        );
                        projection = project(&messages);
                        if projection.estimated_tokens() > threshold {
                            return Err("旁问摘要后仍超过上下文预算；原历史已保留".into());
                        }
                    }
                    // A bounded loop: a side question may look things up, but it
                    // is not an open-ended main turn. One step is one model
                    // request plus the read-only calls it asked for; the
                    // exchange lives only in this local `messages` vector.
                    const MAX_SIDE_QUESTION_STEPS: usize = 12;
                    let mut answer = String::new();
                    for step in 0..MAX_SIDE_QUESTION_STEPS {
                        let projection = project(&messages);
                        let estimated_input = projection.estimated_tokens();
                        let mut request = ModelRequest::new(model.clone(), messages.clone());
                        request.tools = tool_definitions.clone();
                        request.tool_choice = if tool_definitions.is_empty() {
                            ToolChoice::None
                        } else {
                            ToolChoice::Auto
                        };
                        request.max_output_tokens = Some(policy.max_output_tokens);
                        request.reasoning_effort = policy.reasoning_effort;
                        request.thinking_disabled = policy.thinking_disabled;
                        request.projection = Some(projection);
                        let request_id = request.request_id.to_string();
                        tracing::info!(
                            session = %session_id,
                            btw_seq,
                            step,
                            request_id = %request_id,
                            message_count = messages.len(),
                            estimated_input_tokens = estimated_input,
                            provider = %model.provider,
                            model = %model.model,
                            "BTW_MODEL_DISPATCH"
                        );
                        let started = std::time::Instant::now();
                        let outcome = app.registry().generate(request, cancel.clone()).await;
                        let latency_ms =
                            started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
                        tracing::info!(
                            session = %session_id,
                            btw_seq,
                            step,
                            request_id = %request_id,
                            elapsed_ms = latency_ms,
                            outcome = if outcome.is_ok() { "ok" } else { "error" },
                            delivery_state = ?outcome.as_ref().err().map(|e| e.delivery_state),
                            error_kind = ?outcome.as_ref().err().map(|e| e.kind),
                            "BTW_MODEL_RETURN"
                        );
                        crate::observability::record_auxiliary_call(
                            &app,
                            &db,
                            &session_id,
                            leveler_storage::ModelCallKind::SideQuestion,
                            &model.provider,
                            &model.model,
                            policy.reasoning_effort.map(|e| e.as_wire()),
                            &outcome,
                            latency_ms,
                            None,
                            Some(estimated_input),
                        )
                        .await
                        .map_err(|e| format!("记录模型请求失败: {e}"))?;
                        let resp = outcome.map_err(|e| e.to_string())?;
                        let calls: Vec<leveler_model::ToolCall> = resp
                            .message
                            .content
                            .iter()
                            .filter_map(|part| match part {
                                ContentPart::ToolCall { call } => Some(call.clone()),
                                _ => None,
                            })
                            .collect();
                        // No call to run (or the last permitted step): the text
                        // is the answer. A final step that still asks for tools
                        // returns its text rather than leaving an unanswered
                        // exchange; nothing is sent after this point.
                        if calls.is_empty() || step + 1 == MAX_SIDE_QUESTION_STEPS {
                            answer = resp.message.text_content();
                            break;
                        }
                        messages.push(resp.message);
                        let (_registry, tool_context) = side_tools
                            .as_ref()
                            .expect("a tool call implies the side surface exists");
                        let executor = side_executor
                            .as_ref()
                            .expect("a tool call implies the side surface exists");
                        let mut parts = Vec::with_capacity(calls.len());
                        for call in calls {
                            tracing::info!(
                                session = %session_id,
                                btw_seq,
                                step,
                                tool = %call.name,
                                call_id = %call.id,
                                "BTW_TOOL_START"
                            );
                            let tool_started = std::time::Instant::now();
                            // The side thread's own activity, carried on the
                            // side thread's own events. The main turn's
                            // `ToolCall*` events stay the main transcript's;
                            // a side question never reaches it.
                            let _ = events.send(RuntimeEvent::BtwToolStarted {
                                call_id: call.id.clone(),
                                tool: call.name.clone(),
                            });
                            let (content, is_error) = match executor
                                .run_read_only_call(call.clone(), tool_context.clone(), &cancel)
                                .await
                            {
                                Ok(outcome) => outcome,
                                Err(error) => {
                                    (format!("tool `{}` failed: {error}", call.name), true)
                                }
                            };
                            parts.push(ContentPart::ToolResult {
                                result: leveler_model::ToolResultContent {
                                    call_id: call.id.clone(),
                                    content,
                                    is_error,
                                },
                            });
                            let elapsed_ms = tool_started.elapsed().as_millis() as u64;
                            tracing::info!(
                                session = %session_id,
                                btw_seq,
                                step,
                                call_id = %call.id,
                                elapsed_ms,
                                is_error,
                                "BTW_TOOL_END"
                            );
                            // Sent on BOTH outcomes, including an admit or
                            // execution error: a terminal side state must not
                            // leave the tool rendered as still running.
                            let _ = events.send(RuntimeEvent::BtwToolFinished {
                                call_id: call.id.clone(),
                                tool: call.name.clone(),
                                is_error,
                                elapsed_ms,
                            });
                        }
                        messages.push(Message::from_parts(Role::Tool, parts, None));
                    }
                    Ok(answer)
                }
                .await;

                let (event, record) = match result {
                    Ok(text) => {
                        if !text.is_empty() {
                            let receivers = events
                                .send(RuntimeEvent::BtwTextDelta {
                                    delta: text.clone(),
                                })
                                .unwrap_or(0);
                            tracing::info!(
                                session = %session_id,
                                btw_seq,
                                event = "BtwTextDelta",
                                bytes = text.len(),
                                receivers,
                                "BTW_EVENT_SEND"
                            );
                        }
                        // A fulfilled answer joins the side thread's own history
                        // so the next question can reference it.
                        (RuntimeEvent::BtwCompleted, Some(text))
                    }
                    // Cancelled: the user stopped it. Anyone could have stopped
                    // it, so this path reports the truth instead of calling it a
                    // completion or a failure.
                    Err(_) if cancel.is_cancelled() => (RuntimeEvent::BtwCancelled, None),
                    Err(error) => (RuntimeEvent::BtwFailed { error }, None),
                };
                {
                    let mut threads = btw
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let thread = threads.entry(session_id.clone()).or_default();
                    thread.cancel = None;
                    if let Some(text) = record {
                        thread.history.push(Message::user_input(question.clone()));
                        if !text.is_empty() {
                            thread.history.push(Message::text(Role::Assistant, text));
                        }
                    }
                }
                let event_name = match &event {
                    RuntimeEvent::BtwCompleted => "BtwCompleted",
                    RuntimeEvent::BtwCancelled => "BtwCancelled",
                    RuntimeEvent::BtwFailed { .. } => "BtwFailed",
                    _ => "other",
                };
                let delivered = events.send(event).unwrap_or(0);
                tracing::info!(
                    session = %session_id,
                    btw_seq,
                    event = event_name,
                    receivers = delivered,
                    "BTW_EVENT_SEND"
                );
            });
        });
    }

    /// Stop a session's in-flight `/btw` answer. Idempotent and scoped to the
    /// side thread: it can never touch a main turn.
    fn cancel_btw(&self, session_id: &SessionId) {
        let token = self
            .btw
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session_id)
            .and_then(|thread| thread.cancel.clone());
        if let Some(token) = token {
            token.cancel();
        }
    }

    /// Drop a session's side thread, stopping any in-flight answer. Used when
    /// the main conversation it observed is replaced (clear / checkpoint
    /// restore), so a follow-up cannot reference turns that no longer exist.
    fn drop_btw_thread(&self, session_id: &SessionId) {
        let thread = self
            .btw
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session_id);
        if let Some(token) = thread.and_then(|thread| thread.cancel) {
            token.cancel();
        }
    }

    fn spawn_content_turn(
        &self,
        session_id: SessionId,
        parts: Vec<ContentPart>,
        origin: TranscriptOrigin,
        admission: crate::active_turns::TurnLease,
        config: SessionRuntimeConfig,
        model: ModelRef,
    ) -> oneshot::Receiver<Result<(), ClientError>> {
        let app = self.app.clone();
        let client = self.clone();
        let events = self.events_for(&session_id);
        let active = self.active.clone();
        self.active.note_model(&session_id, &model);
        let mode = config.mode;
        let sandbox = config.sandbox;
        let axis_at_start = config.collaboration.clone();
        let repo = self.app.layout.repo_root.clone();
        let cancel = admission.cancellation();
        let approver = self.approver(&session_id, cancel.clone());
        let clarifier = self.clarifier(&session_id, cancel.clone());
        // Steers and per-child cancels reach this chat turn through the same
        // source the goal turn uses.
        let steering = self.steering_for(&session_id);
        let (accepted_tx, accepted_rx) = oneshot::channel();

        // The runtime's observer is `&mut dyn FnMut`, so the turn future is not
        // `Send` and cannot be `tokio::spawn`ed. Drive it on a blocking thread
        // with the current runtime handle — provider/DB clients stay on their
        // home runtime.
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            handle.block_on(async move {
                emit_project_rules(&events, repo.as_deref());
                let terminal_active = active.clone();
                let terminal_admission = admission.clone();
                let terminal_events = events.clone();
                let mut bridge = EventBridge::new(events.clone())
                    .with_progress(active.clone(), session_id.clone())
                    .with_terminal_publisher(move |event| {
                        terminal_active.finish_with(&terminal_admission, || {
                            let _ = terminal_events.send(event);
                        });
                    });
                let accepted_tx = Arc::new(Mutex::new(Some(accepted_tx)));
                let observer_acceptance = accepted_tx.clone();
                let result = {
                    let mut observer = |event: leveler_engine::EngineEvent| {
                        if matches!(event, leveler_engine::EngineEvent::TurnStarted { .. })
                            && let Some(tx) = observer_acceptance
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .take()
                        {
                            let _ = tx.send(Ok(()));
                            hit_after_turn_started_test_barrier();
                        }
                        bridge.forward(event);
                    };
                    app.run_in_session_with_content(
                        &session_id,
                        &model,
                        mode,
                        parts,
                        origin,
                        approver,
                        clarifier,
                        sandbox,
                        Some(steering as Arc<dyn leveler_agent::SteeringSource>),
                        admission.task_cancel(),
                        &mut observer,
                        cancel,
                    )
                    .await
                };
                // `TurnStarted` is the only consumer of the acceptance
                // sender. Still holding it after the turn returned means the
                // turn never reached durable admission.
                let started = {
                    let mut slot = accepted_tx
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    match slot.take() {
                        Some(tx) => {
                            let _ = tx.send(Err(turn_rejection(&result)));
                            false
                        }
                        None => true,
                    }
                };
                // A cancel is the user saying stop: pending text is not
                // delivered as a new turn. Every other ending owes the person
                // an answer to what they typed.
                let user_cancelled = matches!(
                    &result,
                    Err(crate::AppError::Agent(leveler_agent::AgentError::Cancelled))
                );
                if !bridge.terminal_published() {
                    active.finish_with(&admission, || {
                        let _ = events.send(turn_runtime_event(result));
                    });
                } else {
                    active.finish(&admission);
                }
                // A turn can end the session's Goal route (a final goal
                // terminal, or a `commit_task` goal that has now settled).
                // Clients display the row, so the new axis is published while
                // the turn that committed it is still the only writer.
                client
                    .publish_routes_after_turn(&session_id, &axis_at_start)
                    .await;
                // Only a turn that durably started can owe its journaled
                // input a promotion. Re-promoting after a turn that never
                // started repeats the same refusal without end, and the spin
                // holds the admission slot its own recovery needs.
                if started && !user_cancelled {
                    client.deliver_unconsumed_input(&session_id).await;
                }
            });
        });
        accepted_rx
    }
    /// Restore a checkpoint: roll back the transcript, task epoch, and (git)
    /// workspace to the checkpoint, surfacing any partial-failure honestly.
    async fn handle_restore_checkpoint(
        &self,
        session_id: SessionId,
        checkpoint_id: CheckpointId,
    ) -> Result<(), ClientError> {
        let admission = self.admit_context_op(&session_id, "恢复检查点")?;
        let ordinal = self.checkpoints.ordinal_of(&session_id, &checkpoint_id);
        if let Some(ordinal) = ordinal {
            // A failed truncate means the conversation did NOT roll
            // back; the user must know rather than see a fake restore.
            match self.truncate_messages(&session_id, ordinal as usize).await {
                Ok(()) => {
                    // Cut post-checkpoint Plan/Evidence/Progress so
                    // resume does not inherit work after the restore point.
                    if let Err(error) = self.reset_task_epoch(&session_id).await {
                        self.notify_error(
                            &session_id,
                            format!("对话已回滚,但任务状态重置失败: {error}"),
                        );
                    }
                    self.drop_btw_thread(&session_id);
                    // Roll the workspace back to the checkpoint's
                    // snapshot (git repos). A failure is surfaced —
                    // the transcript rolled back but files did not.
                    let snapshot = self.checkpoints.snapshot_of(&checkpoint_id);
                    // What the user is told once it is done. Every failure
                    // path already speaks; a COMPLETE restore said nothing at
                    // all, so the screen simply went blank — no checkpoint
                    // named, no word on whether the files had moved.
                    let mut landed = Some(format!(
                        "已回退到检查点 #{ordinal} · 对话与工作区已回到该处"
                    ));
                    match snapshot {
                        Some(snapshot) => {
                            if let Err(error) = leveler_execution::WorkspaceSnapshot::restore(
                                self.app
                                    .layout
                                    .require_workspace()
                                    .map_err(|error| ClientError::Runtime(error.to_string()))?,
                                &snapshot,
                            )
                            .await
                            {
                                landed = None;
                                self.notify_error(
                                    &session_id,
                                    format!("对话已回滚,但工作区文件回滚失败: {error}"),
                                );
                            } else {
                                let event = match compute_diff(
                                    self.app
                                        .layout
                                        .require_workspace()
                                        .map_err(|error| ClientError::Runtime(error.to_string()))?,
                                    true,
                                ) {
                                    Ok(diff) => RuntimeEvent::DiffUpdated {
                                        query_id: None,
                                        diff,
                                    },
                                    Err(error) => RuntimeEvent::DiffFailed {
                                        query_id: None,
                                        message: error.to_string(),
                                    },
                                };
                                let _ = self.events_for(&session_id).send(event);
                            }
                        }
                        None => {
                            landed = Some(format!(
                                "已回退到检查点 #{ordinal} · 对话已回退;工作区非 git 仓库,文件未回滚"
                            ));
                        }
                    }
                    // Discard checkpoints that pointed past the restore.
                    self.prune_checkpoints_after_restore(&session_id, ordinal);
                    if let Ok(session) = self.snapshot(&session_id).await {
                        let _ = self
                            .events_for(&session_id)
                            .send(RuntimeEvent::SessionOpened { session });
                    }
                    // After the snapshot: it replaces the client's view, and a
                    // notification sent before it would be cleared with the
                    // rest of the old session state.
                    if let Some(message) = landed {
                        let _ = self
                            .events_for(&session_id)
                            .send(RuntimeEvent::Notification {
                                level: NotificationLevel::Info,
                                message,
                            });
                    }
                }
                Err(error) => self.notify_error(&session_id, format!("恢复检查点失败: {error}")),
            }
        } else {
            self.notify_error(
                &session_id,
                "未找到该检查点（可能已被清空、压缩或更早的回滚移除）".to_string(),
            );
        }
        self.active.finish(&admission);
        Ok(())
    }
    /// Compact the session's context off the request path, owning the
    /// session so Submit/clear/restore cannot race the transcript rewrite.
    async fn handle_compact_context(&self, session_id: SessionId) -> Result<(), ClientError> {
        // Own the session while compact runs so Submit/clear/restore
        // cannot race the transcript rewrite.
        let admission = self.admit_context_op(&session_id, "压缩上下文")?;
        let cancel = admission.cancellation();
        let app = self.app.clone();
        let events = self.events_for(&session_id);
        let config = match self.runtime_config(&session_id).await {
            Ok(config) => config,
            Err(error) => {
                self.active.finish(&admission);
                // Surface the same way as in-flight compact failures.
                let _ = events.send(RuntimeEvent::TurnFailed {
                    error: format!("压缩失败：{error}"),
                    failure: None,
                    recovery: None,
                });
                self.notify_error(&session_id, format!("压缩失败：{error}"));
                return Ok(());
            }
        };
        // Compaction summarizes through a model. A deterministic-only session
        // has none, and the failure is the same surfaced shape.
        let model = match require_model(&config, &session_id) {
            Ok(model) => model,
            Err(error) => {
                self.active.finish(&admission);
                let _ = events.send(RuntimeEvent::TurnFailed {
                    error: format!("压缩失败：{error}"),
                    failure: None,
                    recovery: None,
                });
                self.notify_error(&session_id, format!("压缩失败：{error}"));
                return Ok(());
            }
        };
        let active = self.active.clone();
        let checkpoints = self.checkpoints.clone();
        let live_views = self.live_views.clone();
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            handle.block_on(async move {
                let rewrote =
                    compact_conversation(&app, &events, &live_views, &model, &session_id, cancel)
                        .await;
                // Only wipe checkpoints when the transcript was actually
                // replaced — a short/no-op compact must keep them.
                if rewrote {
                    checkpoints.drop_session(&session_id);
                }
                active.finish(&admission);
            });
        });
        Ok(())
    }
    /// Clear the conversation: drop stored messages and reset task epoch,
    /// surfacing a DB failure rather than showing a false-empty transcript.
    async fn handle_clear_conversation(&self, session_id: SessionId) -> Result<(), ClientError> {
        let admission = self.admit_context_op(&session_id, "清空会话")?;
        // Drop all stored messages so the next turn starts fresh. A DB
        // failure must be surfaced — silently keeping the history while
        // the UI shows an empty conversation is a lie.
        match self.truncate_messages(&session_id, 0).await {
            Ok(()) => {
                if let Err(error) = self.reset_task_epoch(&session_id).await {
                    self.notify_error(
                        &session_id,
                        format!("会话已清空,但任务状态重置失败: {error}"),
                    );
                }
                self.live_views.clear(&session_id);
                self.drop_session_checkpoints(&session_id);
                self.drop_btw_thread(&session_id);
                if let Ok(session) = self.snapshot(&session_id).await {
                    let _ = self
                        .events_for(&session_id)
                        .send(RuntimeEvent::SessionOpened { session });
                }
            }
            Err(error) => self.notify_error(&session_id, format!("清空会话失败: {error}")),
        }
        self.active.finish(&admission);
        Ok(())
    }
    // The import result is asynchronous; carry this envelope's immutable
    // identity into the worker rather than keeping a shared current-command id.
    fn import_attachment_data(
        &self,
        session_id: SessionId,
        name: String,
        data_base64: String,
        command_id: Option<CommandId>,
    ) -> Result<(), ClientError> {
        let media_root = self.media_root.clone();
        let events = self.events_for(&session_id);
        tokio::task::spawn_blocking(move || {
            let store = MediaStore::new(&media_root);
            let result = match store.import_base64(&data_base64) {
                Ok(stored) => Ok(AttachmentRef {
                    id: AttachmentId::new(leveler_core::new_uuid_string()),
                    kind: AttachmentKind::Image,
                    name,
                    mime_type: stored.mime_type,
                    size_bytes: stored.size_bytes,
                    sha256: stored.sha256,
                    width: Some(stored.width),
                    height: Some(stored.height),
                }),
                Err(MediaError::Unsupported(_)) => {
                    let mime = mime_from_name(&name);
                    match store.put_base64(&data_base64, &mime) {
                        Ok((sha256, size_bytes)) => Ok(AttachmentRef {
                            id: AttachmentId::new(leveler_core::new_uuid_string()),
                            kind: kind_from_mime(&mime),
                            name,
                            mime_type: mime,
                            size_bytes,
                            sha256,
                            width: None,
                            height: None,
                        }),
                        Err(error) => Err(error.to_string()),
                    }
                }
                Err(error) => Err(error.to_string()),
            };
            match result {
                Ok(attachment) => {
                    let _ = events.send(RuntimeEvent::AttachmentAdded {
                        attachment,
                        command_id,
                    });
                }
                Err(error) => {
                    let _ =
                        events.send(RuntimeEvent::AttachmentProcessingFailed { error, command_id });
                }
            }
        });
        Ok(())
    }
}

impl InProcessRuntimeClient {
    /// Dispatch a command that arrived with its durable delivery identity.
    /// Only commands whose authorization derives from this exact user action
    /// consume it; everything else keeps the ordinary dispatcher.
    async fn send_with_origin(
        &self,
        command: ClientCommand,
        origin: &CommandId,
    ) -> Result<(), ClientError> {
        match command {
            ClientCommand::SubmitMessage {
                session_id,
                content,
                attachments,
            } => {
                // An ordinary message stays an ordinary message. Whether it
                // continues an existing goal is the model's reading, declared
                // through the structured intent tool, not a client-side phrase
                // match.
                self.handle_submit_message(session_id, content, attachments, Some(origin), None)
                    .await
            }
            ClientCommand::ResumeTask {
                session_id,
                content,
            } => {
                self.handle_resume_task(session_id, content, Some(origin))
                    .await
            }
            // The execution must carry the delivery identity that admitted
            // it; it is the only evidence that distinguishes this run from
            // another client's identical command in the same session.
            ClientCommand::RunUserShell {
                session_id,
                command,
            } => {
                self.handle_run_user_shell(session_id, command, Some(origin.clone()))
                    .await
            }

            command => self.send(command).await,
        }
    }
}

#[async_trait]
impl InteractiveRuntimeClient for InProcessRuntimeClient {
    async fn send(&self, command: ClientCommand) -> Result<(), ClientError> {
        if command
            .requires_snapshot_version()
            .map_err(|error| ClientError::Runtime(error.to_string()))?
        {
            return Err(ClientError::Runtime("snapshot version required: deliver this setting with its observed expected_version".into()));
        }
        match command {
            ClientCommand::ManageSchedule {
                requester_session_id,
                query_id,
                request,
            } => {
                let run_now = matches!(
                    *request,
                    leveler_client_protocol::schedule::ScheduleRequest::RunNow { .. }
                );
                let mut response = match self.app.schedule_manager().await {
                    Ok(manager) => manager.handle(*request).await,
                    Err(error) => leveler_client_protocol::schedule::ScheduleResponse::Error {
                        message: error.to_string(),
                    },
                };
                // Run-now is an explicit user request; leaving the slot for a
                // later OS wake would make the button appear broken when
                // background scheduling is off. Start the existing dispatcher
                // command, which connects to this runtime as an ordinary local
                // client; nothing here executes the action itself.
                if run_now
                    && let leveler_client_protocol::schedule::ScheduleResponse::Changed {
                        message,
                        ..
                    } = &mut response
                {
                    match spawn_schedule_dispatch(self.app.as_ref()) {
                        Ok(()) => message.push_str("; the dispatcher was started"),
                        Err(error) => message.push_str(&format!(
                            "; but this process could not start the dispatcher: {error}"
                        )),
                    }
                }
                let _ =
                    self.events_for(&requester_session_id)
                        .send(RuntimeEvent::ScheduleManaged {
                            requester_session_id,
                            query_id,
                            response: Box::new(response),
                        });
                Ok(())
            }
            ClientCommand::QueryGlobalTasks {
                requester_session_id,
                query_id,
                include_archived,
            } => {
                let mut index = crate::global_task_index::query_global_tasks(
                    self.app.layout.home(),
                    include_archived,
                )
                .await;
                let local_source = self.app.layout.database_path().display().to_string();
                if index
                    .tasks
                    .iter()
                    .any(|task| task.source_id == local_source)
                {
                    use leveler_client_protocol::UiGlobalTaskSourceErrorKind;
                    let enrichment = async {
                        let db = leveler_storage::Database::connect_read_only(
                            &self.app.layout.database_path(),
                        )
                        .await
                        .map_err(|error| {
                            (UiGlobalTaskSourceErrorKind::Storage, error.to_string())
                        })?;
                        let probe = crate::runtime_boot::StateDirBootLiveness::new(
                            &self.app.layout.state_dir,
                        );
                        let stores = leveler_storage::EngineStores::from_database(&db);
                        for task in index
                            .tasks
                            .iter_mut()
                            .filter(|task| task.source_id == local_source)
                        {
                            let facts = db
                                .session_facts_for(&task.id)
                                .await
                                .map_err(|error| {
                                    (UiGlobalTaskSourceErrorKind::Storage, error.to_string())
                                })?
                                .ok_or_else(|| {
                                    (
                                        UiGlobalTaskSourceErrorKind::Storage,
                                        format!("session {} disappeared during discovery", task.id),
                                    )
                                })?;
                            let live = self.active.is_running(&task.id);
                            let live_waiter = self
                                .pending
                                .lock()
                                .unwrap()
                                .values()
                                .any(|waiter| waiter.binding.session_id == task.id)
                                || self
                                    .pending_clarify
                                    .lock()
                                    .unwrap()
                                    .values()
                                    .any(|waiter| waiter.binding.session_id == task.id);
                            // A question outlives the process that asked it.
                            // The list must agree with the session snapshot
                            // this client would open: a durable, answerable
                            // clarification is still WaitingUser.
                            let waiting = if live_waiter {
                                true
                            } else {
                                leveler_engine::EventLog::new(
                                    stores.events.as_ref(),
                                    task.id.clone(),
                                )
                                .pending_clarifications()
                                .await
                                .map_err(|error| {
                                    (UiGlobalTaskSourceErrorKind::Storage, error.to_string())
                                })?
                                .into_iter()
                                .any(|request| request.is_actionable())
                            };
                            task.status = crate::session_projection::project_task(
                                &facts, &probe, live, waiting,
                            )
                            .map_err(|error| {
                                (UiGlobalTaskSourceErrorKind::Projection, error.to_string())
                            })?
                            .0;
                        }
                        Ok::<(), (UiGlobalTaskSourceErrorKind, String)>(())
                    }
                    .await;
                    if let Err((kind, message)) = enrichment {
                        index.tasks.retain(|task| task.source_id != local_source);
                        index.source_errors.push(
                            leveler_client_protocol::UiGlobalTaskSourceError {
                                source_id: local_source,
                                kind,
                                message,
                            },
                        );
                    }
                }
                let _ =
                    self.events_for(&requester_session_id)
                        .send(RuntimeEvent::GlobalTasksLoaded {
                            requester_session_id,
                            query_id,
                            index,
                        });
                Ok(())
            }
            ClientCommand::RequestPromptSuggestion { session_id } => {
                spawn_auxiliary_assist(
                    self.app.clone(),
                    self.active.clone(),
                    self.auxiliary_assists.clone(),
                    session_id.clone(),
                    self.session_model(&session_id).await?,
                    self.events_for(&session_id),
                    AssistKind::PromptSuggestion,
                );
                Ok(())
            }
            ClientCommand::RequestAwaySummary { session_id } => {
                spawn_auxiliary_assist(
                    self.app.clone(),
                    self.active.clone(),
                    self.auxiliary_assists.clone(),
                    session_id.clone(),
                    self.session_model(&session_id).await?,
                    self.events_for(&session_id),
                    AssistKind::AwaySummary,
                );
                Ok(())
            }
            ClientCommand::SubmitMessage {
                session_id,
                content,
                attachments,
            } => {
                // Same rule as `send_with_origin`: no client-side continuation
                // classification. Attachments stay on the content path.
                self.handle_submit_message(session_id, content, attachments, None, None)
                    .await
            }
            ClientCommand::ResumeTask {
                session_id,
                content,
            } => self.handle_resume_task(session_id, content, None).await,
            ClientCommand::RunGoal {
                session_id,
                content,
                require_new_turn,
            } => {
                self.run_goal_turn(session_id, content, require_new_turn)
                    .await
            }
            ClientCommand::AddAttachment {
                session_id,
                path,
                name,
            } => {
                let media_root = self.media_root.clone();
                let events = self.events_for(&session_id);
                tokio::task::spawn_blocking(move || {
                    let store = MediaStore::new(&media_root);
                    let source = PathBuf::from(&path);
                    let name = name.unwrap_or_else(|| {
                        source
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| path.clone())
                    });
                    match store.import_path(&source) {
                        Ok(stored) => {
                            let _ = events.send(RuntimeEvent::AttachmentAdded {
                                command_id: None,
                                attachment: AttachmentRef {
                                    id: AttachmentId::new(leveler_core::new_uuid_string()),
                                    kind: AttachmentKind::Image,
                                    name,
                                    mime_type: stored.mime_type,
                                    size_bytes: stored.size_bytes,
                                    sha256: stored.sha256,
                                    width: Some(stored.width),
                                    height: Some(stored.height),
                                },
                            });
                        }
                        Err(e) => {
                            let _ = events.send(RuntimeEvent::AttachmentProcessingFailed {
                                command_id: None,
                                error: e.to_string(),
                            });
                        }
                    }
                });
                Ok(())
            }
            ClientCommand::AddAttachmentData {
                session_id,
                name,
                data_base64,
            } => self.import_attachment_data(session_id, name, data_base64, None),
            ClientCommand::AddClipboardImage { session_id } => {
                let media_root = self.media_root.clone();
                let events = self.events_for(&session_id);
                tokio::task::spawn_blocking(move || {
                    let result = (|| -> Result<AttachmentRef, String> {
                        let mut clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
                        let image = clipboard.get_image().map_err(|e| e.to_string())?;
                        let stored = MediaStore::new(&media_root)
                            .import_rgba(image.width as u32, image.height as u32, &image.bytes)
                            .map_err(|e| e.to_string())?;
                        Ok(AttachmentRef {
                            id: AttachmentId::new(leveler_core::new_uuid_string()),
                            kind: AttachmentKind::Image,
                            name: "clipboard.png".to_string(),
                            mime_type: stored.mime_type,
                            size_bytes: stored.size_bytes,
                            sha256: stored.sha256,
                            width: Some(stored.width),
                            height: Some(stored.height),
                        })
                    })();
                    match result {
                        Ok(attachment) => {
                            let _ = events.send(RuntimeEvent::AttachmentAdded {
                                attachment,
                                command_id: None,
                            });
                        }
                        Err(e) => {
                            let _ = events.send(RuntimeEvent::AttachmentProcessingFailed {
                                command_id: None,
                                error: format!("剪贴板图片：{e}"),
                            });
                        }
                    }
                });
                Ok(())
            }
            ClientCommand::ApprovalDecision {
                request_id,
                decision,
            } => resolve_approval(&self.pending, &request_id, execution_decision(decision)),
            ClientCommand::AnswerClarification {
                request_id,
                answer,
                session_id,
            } => {
                self.handle_answer_clarification(request_id, answer, session_id)
                    .await
            }
            ClientCommand::SelectModel { .. }
            | ClientCommand::SetDefaultModel { .. }
            | ClientCommand::SetPermissionProfile { .. }
            | ClientCommand::SetThinkingLevel { .. }
            | ClientCommand::SetProductAxes { .. }
            | ClientCommand::RenameSession { .. } => {
                Err(ClientError::Runtime("snapshot version required".into()))
            }
            ClientCommand::ConfirmPlanToGoal {
                session_id,
                content,
            } => {
                let goal = if content.trim().is_empty() {
                    "Execute the confirmed plan".to_string()
                } else {
                    content
                };
                // Fail before the declaration is written: a confirmed plan
                // needs an execution model.
                let config = self.runtime_config(&session_id).await?;
                let model = require_model(&config, &session_id)?;
                self.persist_goal_declaration(&session_id, &goal).await?;
                let cancel = self
                    .stage_turn(&session_id, &goal, false, 0, None, None)
                    .await?;
                let accepted = self.spawn_goal_turn(session_id, goal, cancel, config, model);
                await_turn_acceptance(accepted).await
            }

            ClientCommand::SteerCurrentTurn {
                session_id,
                content,
                message_id,
            } => {
                // `/goal` is control input with its own durable declaration;
                // it is not a plain append and never enters the input journal.
                if let Some(command) = leveler_client_protocol::parse_goal_command(&content) {
                    use leveler_client_protocol::GoalCommand;
                    if self.active.is_running(&session_id) {
                        return match command {
                            GoalCommand::Run(task) => self.steer_into_goal(session_id, task).await,
                            GoalCommand::Clear => self.clear_goal_axis(session_id).await,
                            GoalCommand::Status => self.goal_status(session_id).await,
                            GoalCommand::Usage => Err(ClientError::Runtime(
                                "用法: /goal <任务>（/goal clear 返回 Chat，/goal status 查看模式）"
                                    .into(),
                            )),
                        };
                    }
                    return Box::pin(self.handle_submit_message(
                        session_id,
                        content,
                        Vec::new(),
                        None,
                        None,
                    ))
                    .await;
                }
                let content = content.trim().to_string();
                if content.is_empty() {
                    return Err(ClientError::Runtime(
                        "empty input is not accepted as an instruction".into(),
                    ));
                }
                let id = message_id
                    .map(|id| id.as_str().to_string())
                    .unwrap_or_else(leveler_core::new_uuid_string);
                // Durable before the ACK. A retry with the same id finds the
                // same row instead of creating a second instruction.
                let row = self.admit_input(&session_id, &id, &content).await?;
                let text = row.content.clone();
                match row.state {
                    SessionInputState::Consumed => {
                        self.emit_input_admitted(&session_id, &id, &text);
                        self.emit_input_settled(
                            &session_id,
                            &id,
                            UiInputState::Delivered,
                            "",
                            row.ordinal,
                        );
                        return Ok(());
                    }
                    SessionInputState::Cancelled => {
                        self.emit_input_admitted(&session_id, &id, &text);
                        self.emit_input_settled(
                            &session_id,
                            &id,
                            UiInputState::Cancelled,
                            &row.reason,
                            None,
                        );
                        return Ok(());
                    }
                    SessionInputState::Pending => {}
                }
                // Announce the durable row before making it available to an
                // executor. Another subscribed client has no optimistic row;
                // it must see admission before a fast round settles the input.
                self.emit_input_admitted(&session_id, &id, &text);
                if self.active.is_running(&session_id) {
                    // A retry of the submission that STARTED this turn is
                    // already its opening message; queueing it again would
                    // deliver the same row twice.
                    if self.active.seed_input_id(&session_id).as_deref() != Some(id.as_str()) {
                        self.enqueue_input(&session_id, id.clone(), text.clone());
                        self.signal_pending_input(&session_id);
                    }
                    return Ok(());
                }
                // The turn ended between the client deciding to steer and this
                // arriving — a real race, because the client's "busy" view lags
                // the runtime by one event. The row is already durable, so the
                // text becomes the next turn's opening message instead of a
                // second, differently-identified submission.
                match Box::pin(self.handle_submit_message(
                    session_id.clone(),
                    text.clone(),
                    Vec::new(),
                    None,
                    Some(id.clone()),
                ))
                .await
                {
                    Ok(()) => Ok(()),
                    Err(error)
                        if matches!(&error, ClientError::Runtime(message)
                            if message.contains("already has an active turn")) =>
                    {
                        // A turn was admitted in the same window. It owns the
                        // input, and the journal row makes the handoff safe.
                        if self.active.seed_input_id(&session_id).as_deref() != Some(id.as_str()) {
                            self.enqueue_input(&session_id, id.clone(), text.clone());
                            self.signal_pending_input(&session_id);
                        }
                        Ok(())
                    }
                    Err(error) => {
                        // No turn was ever durably admitted for it. Settle the
                        // row so a later turn cannot run what the client was
                        // told failed; the client still holds the text.
                        if let Ok(Some(settled)) = self
                            .cancel_input(
                                &session_id,
                                &id,
                                "the turn could not start, so the input was not admitted",
                            )
                            .await
                        {
                            self.emit_input_settled(
                                &session_id,
                                &settled.id,
                                UiInputState::Cancelled,
                                &settled.reason,
                                None,
                            );
                        }
                        Err(error)
                    }
                }
            }

            ClientCommand::RequestDiff {
                session_id,
                query_id,
            } => {
                let repo = self.app.layout.repo_root.clone();
                let events = self.events_for(&session_id);
                tokio::task::spawn_blocking(move || {
                    let event = match repo
                        .as_deref()
                        .ok_or_else(|| "no primary workspace is attached".to_string())
                        .and_then(|root| {
                            compute_diff(root, true).map_err(|error| error.to_string())
                        }) {
                        Ok(diff) => RuntimeEvent::DiffUpdated { query_id, diff },
                        Err(error) => RuntimeEvent::DiffFailed {
                            query_id,
                            message: error.to_string(),
                        },
                    };
                    let _ = events.send(event);
                });
                Ok(())
            }
            ClientCommand::CompactContext { session_id } => {
                self.handle_compact_context(session_id).await
            }
            ClientCommand::ClearConversation { session_id } => {
                self.handle_clear_conversation(session_id).await
            }
            ClientCommand::RestoreCheckpoint {
                session_id,
                checkpoint_id,
            } => {
                self.handle_restore_checkpoint(session_id, checkpoint_id)
                    .await
            }
            ClientCommand::RequestSessionList => {
                let _ = self.events.send(RuntimeEvent::SessionList {
                    sessions: self.list_sessions().await?,
                });
                Ok(())
            }
            ClientCommand::RequestSessionListFor {
                requester_session_id,
            } => {
                let _ = self
                    .events_for(&requester_session_id)
                    .send(RuntimeEvent::SessionList {
                        sessions: self.list_sessions().await?,
                    });
                Ok(())
            }
            ClientCommand::ListForeignSessionsFor {
                requester_session_id,
                tool,
            } => {
                self.handle_list_foreign_sessions(requester_session_id, tool)
                    .await
            }
            ClientCommand::ImportForeignSession {
                requester_session_id,
                tool,
                native_id,
            } => {
                self.handle_import_foreign_session(requester_session_id, tool, native_id)
                    .await
            }
            ClientCommand::OpenSession { session_id } => {
                if let Ok(session) = self.snapshot(&session_id).await {
                    let _ = self.events.send(RuntimeEvent::SessionOpened { session });
                }
                Ok(())
            }
            ClientCommand::NewSessionFor {
                requester_session_id,
            } => {
                // Start fresh WITHOUT destroying anything: the caller's
                // session keeps its transcript and checkpoints and stays in
                // the session list, so `/clear` is a reversible move (reopen
                // the old session) rather than an unrecoverable wipe.
                let mut config = self.runtime_config(&requester_session_id).await?;
                // A new conversation starts on the CURRENT configured default,
                // not on a model the previous conversation picked, and not with
                // its explicit Thinking intent.
                config.thinking = None;
                let new_model = self
                    .app
                    .configured_default_model()
                    .or_else(|| config.model.clone());
                match self
                    .app
                    .create_daemon_session(
                        new_model.as_ref(),
                        PLACEHOLDER_GOAL,
                        config.mode,
                        // `/clear` opens a sibling conversation under the
                        // caller's own axis; an unreadable value falls back to
                        // the product default rather than guessing chat.
                        config.collaboration.parse().unwrap_or_default(),
                        false,
                    )
                    .await
                {
                    Ok(session_id) => {
                        // Persist the axes BEFORE attaching or announcing: a
                        // session that is attached and then fails to persist
                        // is an orphan the client has already switched to.
                        // Carry the caller's runtime axes over so a fresh
                        // conversation is not silently a different permission
                        // permission profile or collaboration.
                        // Anything that fails from here on leaves a row the
                        // client never switched to. Roll it back rather than
                        // leaving an orphan in the session list.
                        let prepared = async {
                            config.model = new_model;
                            self.persist_runtime_config(&session_id, config).await?;
                            self.snapshot(&session_id).await
                        }
                        .await;
                        let session = match prepared {
                            Ok(session) => session,
                            Err(error) => {
                                self.discard_session(&session_id).await;
                                self.notify_error(
                                    &requester_session_id,
                                    format!("新建会话失败: {error}"),
                                );
                                return Ok(());
                            }
                        };
                        // `persist_runtime_config` above already registered
                        // this session's runtime config (mode included), so the
                        // new conversation inherits the caller's axes and no
                        // second write can disagree with the durable row.
                        let _ = self
                            .events_for(&requester_session_id)
                            .send(RuntimeEvent::SessionOpened { session });
                        let _ = self.events.send(RuntimeEvent::SessionList {
                            sessions: self.list_sessions().await?,
                        });
                    }
                    Err(error) => {
                        self.notify_error(&requester_session_id, format!("新建会话失败: {error}"));
                    }
                }
                Ok(())
            }
            ClientCommand::OpenSessionFor {
                requester_session_id,
                session_id,
            } => {
                if let Ok(session) = self.snapshot(&session_id).await {
                    let _ = self
                        .events_for(&requester_session_id)
                        .send(RuntimeEvent::SessionOpened { session });
                }
                Ok(())
            }
            ClientCommand::DeleteSession { session_id } => {
                let deleted: Result<(), anyhow::Error> = async {
                    let db = self.app.open_database().await?;
                    self.app
                        .background_tasks()
                        .kill_session(session_id.as_str())
                        .await
                        .map_err(anyhow::Error::msg)?;
                    SessionRepository::new(&db).delete(&session_id).await?;
                    Ok(())
                }
                .await;
                if let Err(error) = deleted {
                    let _ = self.events.send(RuntimeEvent::Notification {
                        level: NotificationLevel::Error,
                        message: format!("删除会话失败: {error}"),
                    });
                    // The primary failure is already on the error channel; a
                    // refresh of a listing that shares the broken store would
                    // only replace that one honest failure with a second one.
                    return Ok(());
                }
                self.forget_session(&session_id);
                crate::user_shell::remove_session_output(&self.app.layout.state_dir, &session_id);
                let _ = self.events.send(RuntimeEvent::SessionList {
                    sessions: self.list_sessions().await?,
                });
                Ok(())
            }
            ClientCommand::DeleteSessionFor {
                requester_session_id,
                session_id,
            } => {
                let deleted: Result<(), anyhow::Error> = async {
                    let db = self.app.open_database().await?;
                    self.app
                        .background_tasks()
                        .kill_session(session_id.as_str())
                        .await
                        .map_err(anyhow::Error::msg)?;
                    SessionRepository::new(&db).delete(&session_id).await?;
                    Ok(())
                }
                .await;
                if let Err(error) = deleted {
                    self.notify_error(&requester_session_id, format!("删除会话失败: {error}"));
                    return Ok(());
                }
                self.forget_session(&session_id);
                crate::user_shell::remove_session_output(&self.app.layout.state_dir, &session_id);
                let event = RuntimeEvent::SessionList {
                    sessions: self.list_sessions().await?,
                };
                if requester_session_id == session_id {
                    let _ = self.events.send(event);
                } else {
                    let _ = self.events_for(&requester_session_id).send(event);
                }
                Ok(())
            }
            ClientCommand::ArchiveSession { session_id } => {
                let archived: Result<(), anyhow::Error> = async {
                    let db = self.app.open_database().await?;
                    SessionRepository::new(&db)
                        .set_archived(&session_id, Some(leveler_core::now()))
                        .await?;
                    Ok(())
                }
                .await;
                if let Err(error) = archived {
                    self.notify_error(&session_id, format!("归档会话失败: {error}"));
                    return Ok(());
                }
                let _ = self.events.send(RuntimeEvent::SessionList {
                    sessions: self.list_sessions().await?,
                });
                Ok(())
            }
            ClientCommand::ForkSession { session_id } => {
                // Copy record + transcript into a fresh session; the original
                // stays untouched so an alternative direction can be explored.
                let forked: Result<SessionId, anyhow::Error> = async {
                    let db = self.app.open_database().await?;
                    let sessions = SessionRepository::new(&db);
                    let record = sessions
                        .get(&session_id)
                        .await?
                        .ok_or_else(|| anyhow::anyhow!("会话不存在"))?;
                    let (mode, sandbox, kind, _) = sessions
                        .execution(&session_id)
                        .await?
                        .ok_or_else(|| anyhow::anyhow!("会话执行配置不存在"))?;
                    let title = if record.goal == PLACEHOLDER_GOAL {
                        record.goal.clone()
                    } else {
                        format!("{} (分叉)", record.goal)
                    };
                    let engine = self.app.task_engine(&db)?;
                    // A fork copies the conversation AND its model choice; it
                    // inherits the source's pin state so a later default change
                    // treats both sessions the same way.
                    let model_pinned = leveler_storage::SessionRepository::new(&db)
                        .settings_snapshot(&session_id)
                        .await?
                        .map(|settings| settings.model_pinned)
                        .unwrap_or(true);
                    let fork_id = engine
                        .create_task(&leveler_engine::NewSession {
                            workspace: record.repository.clone(),
                            goal: title,
                            model: record.model.clone(),
                            mode,
                            sandbox,
                            kind: leveler_engine::ExecutionKind::parse(&kind)?,
                            axes: Some(leveler_engine::NewSessionAxes {
                                collaboration: record.collaboration.clone(),
                            }),
                            model_pinned,
                        })
                        .await?;
                    let messages = MessageRepository::new(&db);
                    let transcript = messages.load(&session_id).await?;
                    messages
                        .append(&fork_id, &transcript, leveler_core::now())
                        .await?;
                    Ok(fork_id)
                }
                .await;
                match forked {
                    Ok(fork_id) => {
                        // To the session that was forked, not only to the
                        // compatibility stream: a session-scoped client
                        // subscribes to its own stream, so a fork that worked
                        // was silent while one that failed was not.
                        let _ = self
                            .events_for(&session_id)
                            .send(RuntimeEvent::Notification {
                                level: NotificationLevel::Info,
                                message: format!(
                                    "已分叉会话: {fork_id}（原会话不变，/sessions 打开副本）"
                                ),
                            });
                    }
                    Err(error) => {
                        self.notify_error(&session_id, format!("分叉会话失败: {error}"));
                    }
                }
                let sessions = self.list_sessions().await?;
                let _ = self
                    .events_for(&session_id)
                    .send(RuntimeEvent::SessionList {
                        sessions: sessions.clone(),
                    });
                let _ = self.events.send(RuntimeEvent::SessionList { sessions });
                Ok(())
            }
            ClientCommand::Btw {
                session_id,
                question,
            } => {
                let config = self.runtime_config(&session_id).await?;
                let model = require_model(&config, &session_id)?;
                self.spawn_btw(session_id, question, config, model);
                Ok(())
            }
            ClientCommand::CancelBtw { session_id } => {
                self.cancel_btw(&session_id);
                Ok(())
            }
            ClientCommand::CancelCurrentTurn { session_id } => {
                // Accepted input that never reached a model is settled with
                // the task it belonged to; it must not resurrect in a later
                // turn. This runs before the token so an input the executor
                // is appending right now either wins (it was delivered) or
                // loses (it is cancelled).
                self.cancel_pending_inputs(&session_id, "当前任务已取消")
                    .await;
                if !self.cancel_active(&session_id) {
                    // No owned live turn: recover a possible orphan left by an
                    // earlier process. Never race the active executor's own
                    // terminal transition with the reaper.
                    self.cancel_orphaned(&session_id).await?;
                }
                Ok(())
            }
            ClientCommand::CancelTask { session_id } => {
                self.cancel_pending_inputs(&session_id, "任务已取消").await;
                // A running turn carries the flag to its terminal, so the
                // engine records `cancelled` (not resumable `interrupted`).
                // With nothing running, commit that terminal here.
                let axis_before = self
                    .app
                    .session_product_axes(&session_id)
                    .await
                    .map_err(|error| ClientError::Runtime(error.to_string()))?
                    .as_str()
                    .to_string();
                let delivered = self
                    .app
                    .request_active_task_cancel(&session_id)
                    .await
                    .map_err(|error| ClientError::Runtime(error.to_string()))?;
                // A running LOGICAL turn owns the cancellation even in the
                // moment between its work windows, when no durable ownership
                // exists to carry a request row. The turn's own loop settles
                // the task through the same `cancel_task` transaction; taking
                // the idle path here instead would race the next window.
                let held = self.active.is_running(&session_id);
                if held || delivered {
                    self.active.cancel_task(&session_id);
                } else {
                    match self.app.cancel_task(&session_id).await {
                        Ok(Some(event)) => {
                            let mut bridge = EventBridge::new(self.events_for(&session_id));
                            bridge.forward(event);
                            // The cancelled goal can never be continued, so its
                            // terminal ended the session's Goal route too.
                            self.publish_routes_after_turn(&session_id, &axis_before)
                                .await;
                        }
                        Ok(None) => self.notify_error(
                            &session_id,
                            "没有可取消的任务；直接描述新任务即可".to_string(),
                        ),
                        Err(error) => {
                            return Err(ClientError::Runtime(error.to_string()));
                        }
                    }
                }
                Ok(())
            }
            ClientCommand::CancelChild {
                session_id,
                child_id,
            } => {
                match take_cancel(&self.child_cancels, &session_id, &child_id) {
                    // The child observes its token, stops, and settles as
                    // cancelled through the ordinary settlement path.
                    Some(token) => token.cancel(),
                    None => self.notify_error(
                        &session_id,
                        format!("子 Agent {child_id} 已结束或不存在,无需停止"),
                    ),
                }
                Ok(())
            }
            ClientCommand::CancelToolCall {
                session_id,
                call_id,
            } => match take_cancel(&self.tool_call_cancels, &session_id, call_id.as_str()) {
                // The call observes its token, its process tree is killed,
                // and it settles through its ordinary completion.
                Some(token) => {
                    token.cancel();
                    Ok(())
                }
                // Refused, not notified: the client showing "stopping" must
                // learn that nothing is being stopped.
                None => Err(ClientError::Runtime(
                    "该命令已结束或尚未开始执行,无需停止".to_string(),
                )),
            },
            ClientCommand::AcknowledgeCrashWindow { session_id } => {
                // Acknowledging advances the ownership epoch, which would
                // invalidate a live turn's token. Take the same admission gate
                // every turn start uses, so an acknowledgement and a turn
                // cannot interleave; the lease is released before the snapshot
                // so the published state never claims this short critical
                // section was a running turn.
                let lease = self
                    .active
                    .admit(&session_id)
                    .map_err(|error| ClientError::Runtime(error.to_string()))?;
                let acknowledged = async {
                    let closed = self
                        .app
                        .acknowledge_crash_window(&session_id)
                        .await
                        .map_err(|error| match &error {
                            crate::AppError::SessionRunningElsewhere
                            | crate::AppError::SessionOwnershipUnknown => {
                                ownership_conflict(&error)
                            }
                            error => ClientError::Runtime(error.to_string()),
                        })?;
                    Ok::<usize, ClientError>(closed)
                }
                .await;
                self.active.finish(&lease);
                let closed = acknowledged?;
                let _ = self
                    .events_for(&session_id)
                    .send(RuntimeEvent::CrashWindowAcknowledged {
                        session_id: session_id.clone(),
                        closed: u32::try_from(closed).unwrap_or(u32::MAX),
                    });
                // The answer to "did it take" is the runtime's own re-read,
                // not the count alone.
                if let Ok(session) = self.snapshot(&session_id).await {
                    let _ = self
                        .events_for(&session_id)
                        .send(RuntimeEvent::SessionOpened { session });
                }
                Ok(())
            }
            ClientCommand::RunUserShell {
                session_id,
                command,
            } => self.handle_run_user_shell(session_id, command, None).await,
            ClientCommand::BackgroundUserShell {
                session_id,
                execution_id,
            } => {
                self.handle_background_user_shell(session_id, execution_id)
                    .await
            }
            ClientCommand::CancelUserShell {
                session_id,
                execution_id,
            } => {
                match self.user_shells.cancel_token(&session_id, &execution_id) {
                    Some(token) => token.cancel(),
                    None => {
                        self.notify_error(&session_id, "该命令已结束或不存在,无需停止".to_string())
                    }
                }
                Ok(())
            }
            ClientCommand::CancelBackgroundTask {
                session_id,
                task_id,
            } => {
                // Ownership is enforced before signalling: only a task the
                // runtime still attributes to THIS session may be stopped. A
                // task id alone is not authorisation.
                self.app
                    .background_tasks()
                    .kill_owned(&task_id, session_id.as_str())
                    .await
                    .map_err(ClientError::Runtime)?;
                Ok(())
            }
            ClientCommand::QueryContext {
                session_id,
                query_id,
            } => {
                let accounting = self.live_views.view(&session_id).context_usage;
                let _ = self
                    .events_for(&session_id)
                    .send(RuntimeEvent::ContextLoaded {
                        query_id,
                        accounting,
                    });
                Ok(())
            }
            ClientCommand::QueryUsage {
                session_id,
                query_id,
                scope,
                utc_offset_minutes,
            } => {
                // Read-only: a durable aggregate, no turn, no provider call.
                // It therefore answers while a turn runs — the same guarantee
                // `/trace` and `/context` carry.
                let db = self
                    .app
                    .open_database()
                    .await
                    .map_err(|e| ClientError::Runtime(e.to_string()))?;
                let report = crate::usage::query_usage(&db, &session_id, scope, utc_offset_minutes)
                    .await
                    .map_err(|error| ClientError::Runtime(error.to_string()))?;
                let _ = self
                    .events_for(&session_id)
                    .send(RuntimeEvent::UsageLoaded {
                        query_id,
                        report: Box::new(report),
                    });
                Ok(())
            }
            ClientCommand::QueryMemory {
                session_id,
                query_id,
            } => {
                // Read-only projection. It opens each scope's state database,
                // so it runs off the async thread and never starts a turn.
                let status = self.app.memory().ui_status(Some(&session_id)).await;
                let _ = self
                    .events_for(&session_id)
                    .send(RuntimeEvent::MemoryLoaded {
                        query_id,
                        status: Box::new(status),
                    });
                Ok(())
            }
            ClientCommand::ReadMemoryFile {
                session_id,
                query_id,
                scope,
                path,
            } => {
                let file = self.app.memory().read_ui_file(&scope, &path).await;
                let _ = self
                    .events_for(&session_id)
                    .send(RuntimeEvent::MemoryFileLoaded { query_id, file });
                Ok(())
            }
            ClientCommand::RememberMemory {
                session_id,
                query_id,
                content,
            } => {
                let result = match self.app.memory().remember(&content).await {
                    Ok(Some(path)) => UiMemoryActionResult {
                        action: "remember".to_owned(),
                        ok: true,
                        message: format!(
                            "Saved to global memory: {}",
                            path.file_name()
                                .map(|name| name.to_string_lossy().into_owned())
                                .unwrap_or_default()
                        ),
                    },
                    Ok(None) => UiMemoryActionResult {
                        action: "remember".to_owned(),
                        ok: false,
                        message: "Nothing to remember: the note was empty.".to_owned(),
                    },
                    Err(error) => UiMemoryActionResult {
                        action: "remember".to_owned(),
                        ok: false,
                        message: format!("Could not save the note: {error}"),
                    },
                };
                let _ = self
                    .events_for(&session_id)
                    .send(RuntimeEvent::MemoryActionFinished { query_id, result });
                Ok(())
            }
            ClientCommand::RunMemoryDream {
                session_id,
                query_id,
                batch,
            } => {
                let (db, model) = match self.memory_action_context(&session_id).await {
                    Ok(context) => context,
                    Err(error) => {
                        let _ =
                            self.events_for(&session_id)
                                .send(RuntimeEvent::MemoryActionFinished {
                                    query_id,
                                    result: UiMemoryActionResult {
                                        action: dream_action_name(batch).to_owned(),
                                        ok: false,
                                        message: error,
                                    },
                                });
                        return Ok(());
                    }
                };
                let memory = self.app.memory().clone();
                let config = self.app.config_manager().clone();
                let events = self.events_for(&session_id);
                // A Dream can run for minutes. It owns the dream worker slot,
                // so the command returns at once and the settled outcome
                // arrives as one event.
                tokio::spawn(async move {
                    let result = if batch {
                        let outcome = crate::memory::run_batch_dream(
                            &memory,
                            &config,
                            &db,
                            &session_id,
                            &model,
                        )
                        .await;
                        UiMemoryActionResult {
                            action: dream_action_name(true).to_owned(),
                            ok: outcome.succeeded(),
                            message: outcome.message(),
                        }
                    } else {
                        let outcome = crate::memory::run_manual_dream(
                            &memory,
                            &config,
                            &db,
                            &session_id,
                            &model,
                        )
                        .await;
                        UiMemoryActionResult {
                            action: dream_action_name(false).to_owned(),
                            ok: outcome.succeeded(),
                            message: outcome.message(),
                        }
                    };
                    let _ = events.send(RuntimeEvent::MemoryActionFinished { query_id, result });
                });
                Ok(())
            }
            ClientCommand::FlushMemory {
                session_id,
                query_id,
            } => {
                let (db, model) = match self.memory_action_context(&session_id).await {
                    Ok(context) => context,
                    Err(error) => {
                        let _ =
                            self.events_for(&session_id)
                                .send(RuntimeEvent::MemoryActionFinished {
                                    query_id,
                                    result: UiMemoryActionResult {
                                        action: "flush".to_owned(),
                                        ok: false,
                                        message: error,
                                    },
                                });
                        return Ok(());
                    }
                };
                let memory = self.app.memory().clone();
                let config = self.app.config_manager().clone();
                let events = self.events_for(&session_id);
                tokio::spawn(async move {
                    let outcome = memory
                        .flush_capture(&config, &db, &session_id, &model)
                        .await;
                    let _ = events.send(RuntimeEvent::MemoryActionFinished {
                        query_id,
                        result: UiMemoryActionResult {
                            action: "flush".to_owned(),
                            ok: outcome.succeeded(),
                            message: outcome.message(),
                        },
                    });
                });
                Ok(())
            }
            ClientCommand::ToggleMemory {
                session_id,
                query_id,
                enabled,
            } => {
                // A session-scoped preference, not a config write: the
                // configured rollout still decides whether memory may run.
                let result = match self.app.memory().set_session_enabled(&session_id, enabled) {
                    Ok(config) if enabled => UiMemoryActionResult {
                        action: "toggle".to_owned(),
                        ok: true,
                        message: format!(
                            "Memory is on for this session (rollout: {}).",
                            config.rollout.as_str()
                        ),
                    },
                    Ok(_) => UiMemoryActionResult {
                        action: "toggle".to_owned(),
                        ok: true,
                        message: "Memory is paused for this session. Turns during the pause are not captured later.".to_owned(),
                    },
                    Err(error) => UiMemoryActionResult {
                        action: "toggle".to_owned(),
                        ok: false,
                        message: error,
                    },
                };
                let _ = self
                    .events_for(&session_id)
                    .send(RuntimeEvent::MemoryActionFinished { query_id, result });
                Ok(())
            }
            ClientCommand::ForgetMemory {
                session_id,
                query_id,
                scope,
                path,
                content_hash,
            } => {
                let result = self
                    .app
                    .memory()
                    .forget_ui_file(&scope, &path, &content_hash)
                    .await;
                let _ = self
                    .events_for(&session_id)
                    .send(RuntimeEvent::MemoryActionFinished { query_id, result });
                Ok(())
            }
            ClientCommand::QueryObservability {
                session_id,
                query_id,
                center_seq,
                before,
                after,
            } => {
                let db = self
                    .app
                    .open_database()
                    .await
                    .map_err(|e| ClientError::Runtime(e.to_string()))?;
                match crate::observability::query_observability(
                    &db,
                    &session_id,
                    center_seq,
                    before,
                    after,
                )
                .await
                {
                    Ok(observation) => {
                        // Echo the caller's token. `None` is a 1.5 query:
                        // still serve the read model; current clients will not
                        // treat the response as owned.
                        let _ =
                            self.events_for(&session_id)
                                .send(RuntimeEvent::ObservabilityLoaded {
                                    query_id,
                                    observation,
                                });
                    }
                    Err(error) => {
                        self.notify_error(&session_id, format!("observability: {error}"));
                    }
                }
                Ok(())
            }
            ClientCommand::QueryChildContribution {
                session_id,
                child_id,
                query_id,
            } => {
                let db = self
                    .app
                    .open_database()
                    .await
                    .map_err(|e| ClientError::Runtime(e.to_string()))?;
                let stores = leveler_storage::EngineStores::from_database(&db);
                let ledger =
                    crate::contribution_query::last_ledger(stores.events.as_ref(), &session_id)
                        .await;
                // Role and profile are the child's own facts and are not in
                // the ledger; recover them from the spawn event rather than
                // guessing from the id shape.
                let (role, profile_id, capabilities) = crate::contribution_query::child_identity(
                    stores.events.as_ref(),
                    &session_id,
                    &child_id,
                )
                .await;
                let detail = crate::contribution_query::project_child_contribution(
                    ledger.as_ref(),
                    &child_id,
                    &role,
                    profile_id,
                    capabilities,
                );
                let _ = self
                    .events_for(&session_id)
                    .send(RuntimeEvent::ChildContributionLoaded { query_id, detail });
                Ok(())
            }
            ClientCommand::Recap { session_id } => {
                let config = self.runtime_config(&session_id).await?;
                let model = require_model(&config, &session_id)?;
                self.spawn_recap(session_id, model);
                Ok(())
            }
            ClientCommand::ListUnfinishedGoals {
                session_id,
                query_id,
            } => {
                let db = self
                    .app
                    .open_database()
                    .await
                    .map_err(|e| ClientError::Runtime(e.to_string()))?;
                let stores = leveler_storage::EngineStores::from_database(&db);
                let runtime = self
                    .app
                    .runtime_id()
                    .map_err(|e| ClientError::Runtime(e.to_string()))?;
                let goals = crate::goal_discovery::list_unfinished_goals(&stores, &runtime)
                    .await
                    .map(|found| {
                        found
                            .into_iter()
                            .map(|g| leveler_client_protocol::UiUnfinishedGoal {
                                goal_id: g.goal.id.as_str().to_string(),
                                objective: g.goal.objective.clone(),
                                session_id: g.session_id.as_str().to_string(),
                                opened_at: g.goal.opened_at.to_rfc3339(),
                                windows_run: g.goal.windows_run,
                                ours: g.ours,
                                driving: g.driving,
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let _ = self
                    .events_for(&session_id)
                    .send(RuntimeEvent::UnfinishedGoalsLoaded { query_id, goals });
                Ok(())
            }
            ClientCommand::QuerySessionHistory {
                session_id,
                query_id,
            } => {
                let db = self
                    .app
                    .open_database()
                    .await
                    .map_err(|e| ClientError::Runtime(e.to_string()))?;
                match crate::session_history::load_session_history(&db, &session_id).await {
                    Ok((entries, omitted_turns)) => {
                        let _ =
                            self.events_for(&session_id)
                                .send(RuntimeEvent::SessionHistoryLoaded {
                                    query_id,
                                    session_id: session_id.clone(),
                                    entries,
                                    omitted_turns,
                                });
                    }
                    Err(error) => {
                        self.notify_error(&session_id, format!("session history: {error}"));
                    }
                }
                Ok(())
            }
            ClientCommand::ListAgents {
                session_id,
                query_id,
            } => {
                let model = self.session_model(&session_id).await?;
                let (agents, problems) = self.app.list_agents(Some(&model)).await;
                let _ = self
                    .events_for(&session_id)
                    .send(RuntimeEvent::AgentsLoaded {
                        query_id,
                        agents,
                        problems,
                    });
                Ok(())
            }
            ClientCommand::GetAgent {
                session_id,
                name,
                query_id,
            } => {
                let model = self.session_model(&session_id).await?;
                let (agent, error) = match self.app.get_agent(&name, Some(&model)).await {
                    Ok(detail) => (Some(detail), None),
                    Err(error) => (None, Some(error)),
                };
                let _ = self
                    .events_for(&session_id)
                    .send(RuntimeEvent::AgentLoaded {
                        query_id,
                        name,
                        agent,
                        error,
                    });
                Ok(())
            }
            ClientCommand::CreateAgent {
                session_id,
                scope,
                draft,
                query_id,
            } => {
                self.save_agent(session_id, scope, *draft, query_id, true)
                    .await;
                Ok(())
            }
            ClientCommand::UpdateAgent {
                session_id,
                scope,
                draft,
                query_id,
            } => {
                self.save_agent(session_id, scope, *draft, query_id, false)
                    .await;
                Ok(())
            }
            ClientCommand::DeleteAgent {
                session_id,
                scope,
                name,
                query_id,
            } => {
                let (ok, error) = match self.app.delete_agent(scope, &name) {
                    Ok(()) => (true, None),
                    Err(error) => (false, Some(error)),
                };
                let _ = self
                    .events_for(&session_id)
                    .send(RuntimeEvent::AgentMutated {
                        query_id,
                        name,
                        ok,
                        error,
                        agent: None,
                    });
                Ok(())
            }
            ClientCommand::ShutdownWhenIdle { reason } => {
                // Stop admitting new work FIRST: without this the runtime can
                // be kept alive indefinitely by a stream of arriving turns and
                // the replacement never happens.
                self.shutting_down
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                *self
                    .retiring_reason
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(reason);
                tracing::info!(?reason, "runtime retiring once work drains");
                spawn_retire_drain(self);
                Ok(())
            }
            ClientCommand::ForceRetire { reason } => {
                // The explicit escape from a drain that cannot finish. Same
                // commit point as `ShutdownWhenIdle` — stop admitting work and
                // arm the drain — plus cancelling the turns this runtime owns
                // so they reach a terminal now.
                //
                // Background work is deliberately NOT cancelled: it is a
                // user-launched process a handover must not destroy. If any is
                // alive the drain still waits for it, so a force-retire can
                // never silently kill `cargo build` or a dev server.
                self.shutting_down
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                *self
                    .retiring_reason
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(reason);
                tracing::info!(
                    ?reason,
                    "runtime force-retiring; cancelling its own active turns"
                );
                self.active.cancel_all();
                spawn_force_retire_drain(self);
                Ok(())
            }
            ClientCommand::ReloadConfig { query_id } => self.reload_runtime_config(query_id).await,
            ClientCommand::Quit => {
                self.shutting_down
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                self.active.cancel_all();
                // Process is exiting: reaper is the safety net for turns that
                // never got finish() because the OS killed the process mid-flight.
                // Only this boot's own: a sibling process sharing the
                // repository keeps running its turns.
                self.reap_running_turns(None, leveler_engine::ReapScope::OwnBoot)
                    .await?;
                // Runtime-owned OS resources must not outlive the runtime:
                // local (goal-scoped) background tasks and the browser tree are
                // reaped explicitly — Drop never runs on exit paths that call
                // `std::process::exit` or die to SIGTERM (R004 F7). Tasks owned
                // by the persistent Execution Host are deliberately left
                // running: they are the user's services, not this generation's.
                let killed = self.app.background_tasks().kill_all().await;
                if killed > 0 {
                    tracing::info!("shutdown reaped {killed} background task(s)");
                }
                let terminal = self
                    .app
                    .background_tasks()
                    .local_terminal_snapshots_for_scope(None, |_| true, usize::MAX)
                    .await;
                for task in terminal {
                    if let Some(owner) = &task.owner_scope {
                        persist_background_task_finished(
                            &self.app,
                            SessionId::new(owner.clone()),
                            &task,
                        )
                        .await;
                    }
                }
                self.app.browser().shutdown().await;
                // `process::exit` skips Drop, so this is the only chance for a
                // capture or Dream worker to release the lease it still owns.
                self.app.memory().shutdown_workers().await;
                Ok(())
            }
        }
    }

    async fn deliver(&self, envelope: CommandEnvelope) -> Result<(), ClientError> {
        use sha2::{Digest, Sha256};
        // Bind the envelope to its payload before anything keys off its session:
        // version and receipt checks use `envelope.session_id`, so a command
        // whose own target differs must be rejected up front — never check A but
        // act on B (a future authorization boundary). Session-less commands
        // (approval/clarification answers, global queries) carry no target here.
        if let Some(target) = envelope.command.session_id()
            && target != &envelope.session_id
        {
            return Err(ClientError::Runtime(format!(
                "envelope/command session mismatch: envelope targets {}, command targets {}",
                envelope.session_id.as_str(),
                target.as_str()
            )));
        }

        // Session-less commands (global queries like `RequestSessionList`,
        // `Quit`) carry no session target, so they must not be receipted: the
        // command_receipts.session_id foreign key requires a real session row,
        // and clients (the WebUI) send these with an empty session id, which
        // would violate that FK. They are idempotent global dispatches — run
        // them directly, skipping the per-session dedup machinery.
        if envelope.command.session_id().is_none() {
            return self.send(envelope.command).await;
        }

        let db = self
            .app
            .open_database()
            .await
            .map_err(|e| ClientError::Runtime(e.to_string()))?;

        let receipts = leveler_storage::CommandReceiptRepository::new(&db);
        let command_bytes = serde_json::to_vec(&envelope.command)
            .map_err(|e| ClientError::Runtime(format!("cannot fingerprint command: {e}")))?;
        let command_fingerprint = format!("{:x}", Sha256::digest(command_bytes));
        match receipts
            .classify_terminal(
                &envelope.command_id,
                &envelope.session_id,
                &command_fingerprint,
            )
            .await
            .map_err(|e| ClientError::Runtime(e.to_string()))?
        {
            Some(leveler_storage::Admission::AlreadyCompleted) => return Ok(()),
            Some(leveler_storage::Admission::Unresolvable) => {
                return Err(unresolvable(&envelope.command_id));
            }
            Some(leveler_storage::Admission::Uncertain) => {
                return self
                    .answer_unsettled(&receipts, &envelope, &command_fingerprint)
                    .await;
            }
            Some(leveler_storage::Admission::Conflict) => {
                return Err(ClientError::Runtime(format!(
                    "command id {} is already bound to a different session or payload",
                    envelope.command_id.as_str()
                )));
            }
            _ => {}
        }

        // Request-id commands do not carry a session in their payload. Validate
        // first deliveries/retryable failures against the live pending binding;
        // a completed identical receipt above is already authoritative.
        let pending_session = match &envelope.command {
            ClientCommand::ApprovalDecision { request_id, .. } => self
                .pending
                .lock()
                .unwrap()
                .get(request_id)
                .map(|pending| pending.binding.session_id.clone()),
            ClientCommand::AnswerClarification { request_id, .. } => self
                .pending_clarify
                .lock()
                .unwrap()
                .get(request_id)
                .map(|pending| pending.binding.session_id.clone()),
            _ => None,
        };
        if matches!(
            &envelope.command,
            ClientCommand::ApprovalDecision { .. } | ClientCommand::AnswerClarification { .. }
        ) {
            validate_pending_session(&envelope.session_id, pending_session)?;
        }

        let versioned = envelope
            .command
            .requires_snapshot_version()
            .map_err(|error| ClientError::Runtime(error.to_string()))?;
        if versioned && envelope.expected_version.is_none() {
            return Err(ClientError::Runtime(
                "snapshot version required: expected_version must come from the observed snapshot"
                    .into(),
            ));
        }

        // Optimistic concurrency: reject a command issued against a stale view
        // *before* consuming its id, so the client can resync and reissue with a
        // fresh id rather than have this one silently swallowed as a duplicate.
        if !versioned && let Some(expected) = envelope.expected_version {
            let latest = leveler_storage::EventRepository::new(&db)
                .latest_sequence(&envelope.session_id)
                .await
                .map_err(|e| ClientError::Runtime(e.to_string()))?
                .unwrap_or(0);
            if latest != expected {
                return Err(ClientError::Runtime(format!(
                    "version conflict: command expected the log at {expected}, but it is at \
                     {latest}; resync required"
                )));
            }
        }

        // At-least-once dedup with a dispatch lifecycle: only a command whose
        // prior dispatch actually completed is a true duplicate. One whose send
        // failed is retryable; one that never resolved (crash mid-dispatch) is
        // surfaced as uncertain, not silently swallowed as done.
        let boot = self
            .app
            .boot_id()
            .map_err(|e| ClientError::Runtime(format!("cannot start a runtime boot: {e}")))?;
        let (in_flight, admission) = InFlightDelivery::admit(
            &self.in_flight,
            &envelope.command_id,
            receipts.admit(
                &envelope.command_id,
                &envelope.session_id,
                &command_fingerprint,
                &envelope.issued_at,
                &boot,
                leveler_core::now(),
            ),
        )
        .await;
        match admission.map_err(|e| ClientError::Runtime(e.to_string()))? {
            leveler_storage::Admission::AlreadyCompleted => return Ok(()),
            leveler_storage::Admission::Unresolvable => {
                return Err(unresolvable(&envelope.command_id));
            }
            leveler_storage::Admission::Uncertain => {
                // This delivery dispatches nothing; only other registrations
                // may speak for the receipt.
                drop(in_flight);
                return self
                    .answer_unsettled(&receipts, &envelope, &command_fingerprint)
                    .await;
            }
            leveler_storage::Admission::Conflict => {
                return Err(ClientError::Runtime(format!(
                    "command id {} is already bound to a different session or payload",
                    envelope.command_id.as_str()
                )));
            }
            leveler_storage::Admission::Dispatch => {}
        }

        let command_id = envelope.command_id.clone();
        let dispatched = if versioned {
            match self
                .edit_session_settings(
                    &envelope.command,
                    envelope.expected_version.expect("validated version"),
                )
                .await
            {
                Ok(true) => Ok(()),
                Ok(false) => {
                    // The atomic compare guarantees no mutation, so this id can
                    // be retried after an explicit caller resync.
                    receipts
                        .mark_failed(&command_id)
                        .await
                        .map_err(|error| ClientError::Runtime(error.to_string()))?;
                    return Err(ClientError::Runtime(
                        "version conflict: resync required".into(),
                    ));
                }
                Err(error) => Err(error),
            }
        } else {
            match envelope.command {
                ClientCommand::AddAttachmentData {
                    session_id,
                    name,
                    data_base64,
                } => self.import_attachment_data(
                    session_id,
                    name,
                    data_base64,
                    Some(command_id.clone()),
                ),
                command => self.send_with_origin(command, &command_id).await,
            }
        };
        match dispatched {
            Ok(()) => {
                hit_before_receipt_settled_test_barrier();
                let settled = receipts.mark_completed(&command_id).await;
                match settled {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        in_flight.keep_for_this_boot();
                        Err(ClientError::Runtime(e.to_string()))
                    }
                }
            }
            Err(error) => {
                // A returned error does not prove the command had no partial
                // effect. Leave the durable receipt in `dispatching`, so a
                // retry is never blindly executed again. This delivery's
                // registration ends here: nothing will settle the receipt now,
                // and a redelivery is answered unresolvable.
                Err(error)
            }
        }
    }

    fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
        self.events.subscribe()
    }

    fn subscribe_session(&self, session_id: &SessionId) -> broadcast::Receiver<RuntimeEvent> {
        self.events_for(session_id).subscribe()
    }

    async fn snapshot(&self, session_id: &SessionId) -> Result<UiSessionSnapshot, ClientError> {
        // A crash between the durable answer and its continuation leaves the
        // goal owed with nobody driving it. Opening the session is the moment
        // to finish that work — the user does not retype anything, and the
        // snapshot then reports the window as running. The check is narrow and
        // idempotent; a failure is logged and does not hide the session.
        if let Err(error) = self.resume_owed_clarification(session_id).await {
            tracing::warn!(
                %error,
                session = %session_id,
                "could not resume a clarification continuation left owed by a crash"
            );
        }
        // Phase times include pool checkout and SQLite busy-wait. They are not
        // a separate pool-acquire measurement: the pool handle stays inside
        // the storage crate.
        struct SnapshotPhases {
            session: String,
            started: std::time::Instant,
            db_open_ms: u64,
            settings_ms: u64,
            messages_ms: u64,
        }
        impl Drop for SnapshotPhases {
            fn drop(&mut self) {
                let elapsed_ms = self.started.elapsed().as_millis() as u64;
                tracing::debug!(
                    session = %self.session,
                    elapsed_ms,
                    db_open_ms = self.db_open_ms,
                    settings_ms = self.settings_ms,
                    messages_ms = self.messages_ms,
                    "snapshot phases"
                );
                if elapsed_ms >= 1_000 {
                    tracing::warn!(
                        session = %self.session,
                        elapsed_ms,
                        db_open_ms = self.db_open_ms,
                        settings_ms = self.settings_ms,
                        messages_ms = self.messages_ms,
                        "snapshot was slow inside the runtime"
                    );
                }
            }
        }
        let mut phases = SnapshotPhases {
            session: session_id.as_str().to_string(),
            started: std::time::Instant::now(),
            db_open_ms: 0,
            settings_ms: 0,
            messages_ms: 0,
        };
        let db_started = std::time::Instant::now();
        let db = self
            .app
            .open_database()
            .await
            .map_err(|e| ClientError::Runtime(e.to_string()))?;
        phases.db_open_ms = db_started.elapsed().as_millis() as u64;
        let settings_started = std::time::Instant::now();
        let settings = SessionRepository::new(&db)
            .settings_snapshot(session_id)
            .await
            .map_err(|e| ClientError::Runtime(e.to_string()))?
            .ok_or_else(|| ClientError::SessionNotFound(session_id.clone()))?;
        phases.settings_ms = settings_started.elapsed().as_millis() as u64;
        let config = self.config_from_settings(session_id, &settings)?;
        let last_sequence = settings.sequence;
        let record = settings.session;

        let messages_started = std::time::Instant::now();
        let payloads = MessageRepository::new(&db)
            .load_with_input_ids(session_id)
            .await
            .map_err(|e| ClientError::Runtime(e.to_string()))?;
        phases.messages_ms = messages_started.elapsed().as_millis() as u64;
        let mut messages: Vec<UiMessage> = payloads
            .iter()
            .enumerate()
            .filter_map(|(i, (payload, input_id))| {
                ui_message_at(payload, Some(i as u64), input_id.as_deref())
            })
            .collect();
        // Journal rows that are not in the transcript yet (or never will be)
        // are still part of what the user typed. A reopened client must see
        // them, with the same identity the live events used.
        let journal = SessionInputRepository::new(&db)
            .all(session_id)
            .await
            .map_err(|e| ClientError::Runtime(e.to_string()))?;
        for row in journal {
            let kind = match row.state {
                SessionInputState::Pending => UiMessageKind::PendingInput,
                SessionInputState::Cancelled => UiMessageKind::CancelledInput,
                // Its transcript row already carries this identity.
                SessionInputState::Consumed => continue,
            };
            messages.push(UiMessage {
                id: MessageId::new(row.id),
                role: UiRole::User,
                text: row.content,
                ordinal: None,
                kind: Some(kind),
                images: 0,
            });
        }

        let mut available_models = self.app.model_refs();
        available_models.sort_by_key(|m| m.to_string());

        // A live turn keeps the model it started with, even when a reload has
        // already changed the session's NEXT model. Reporting the next model as
        // if it were executing would name a model that is sending nothing.
        // `None` is a deterministic-only session: the snapshot says so rather
        // than naming a model nobody configured.
        let model = self
            .active
            .run_model(session_id)
            .or_else(|| config.model.clone());
        let profile = match model.as_ref() {
            Some(model) => self.app.registry().profile(model).await.ok(),
            None => None,
        };
        // Whether the current model accepts images (spec §42).
        let vision = profile
            .as_ref()
            .map(|p| p.capabilities.vision)
            .unwrap_or(false);
        let thinking = ui_thinking_state(profile.as_ref(), config.thinking);

        let stores = leveler_storage::EngineStores::from_database(&db);
        let mut pending_interactions = Vec::new();
        pending_interactions.extend(
            self.pending
                .lock()
                .unwrap()
                .values()
                .filter(|pending| pending.binding.session_id == *session_id)
                .map(|pending| {
                    leveler_client_protocol::UiPendingInteraction::Approval(pending.request.clone())
                }),
        );
        pending_interactions.extend(
            self.pending_clarify
                .lock()
                .unwrap()
                .values()
                .filter(|pending| pending.binding.session_id == *session_id)
                .map(|pending| {
                    leveler_client_protocol::UiPendingInteraction::Clarification(
                        pending.request.clone(),
                    )
                }),
        );
        // A question outlives the turn that asked it. When no live turn owns
        // the session, the durable pending rows are the authority: a client
        // that reconnects after a restart restores a card it can still answer,
        // and the answer re-enters the goal. Live entries win by id.
        if !self.active.is_running(session_id) {
            let live: std::collections::HashSet<String> = pending_interactions
                .iter()
                .map(|item| match item {
                    leveler_client_protocol::UiPendingInteraction::Approval(request) => {
                        request.id.as_str().to_string()
                    }
                    leveler_client_protocol::UiPendingInteraction::Clarification(request) => {
                        request.id.as_str().to_string()
                    }
                })
                .collect();
            let log = leveler_engine::EventLog::new(stores.events.as_ref(), session_id.clone());
            let pending = log
                .pending_clarifications()
                .await
                .map_err(|error| ClientError::Runtime(error.to_string()))?;
            for request in pending
                .into_iter()
                .filter(|request| request.is_actionable())
            {
                if live.contains(request.id.as_str()) {
                    continue;
                }
                pending_interactions.push(
                    leveler_client_protocol::UiPendingInteraction::Clarification(
                        crate::prompt_bridge::ui_clarification_request(
                            request.id,
                            request.question,
                            request.options,
                            &request.questions,
                        ),
                    ),
                );
            }
        }
        pending_interactions.sort_by_key(|item| match item {
            leveler_client_protocol::UiPendingInteraction::Approval(request) => {
                request.id.as_str().to_string()
            }
            leveler_client_protocol::UiPendingInteraction::Clarification(request) => {
                request.id.as_str().to_string()
            }
        });

        let user_shells = self.user_shells.snapshot(session_id);
        let history = if self.active.is_running(session_id)
            && !user_shells.iter().any(|shell| shell.status == "running")
        {
            let (entries, omitted_turns) =
                crate::session_history::load_session_history(&db, session_id)
                    .await
                    .map_err(|error| ClientError::Runtime(error.to_string()))?;
            (!entries.is_empty()).then_some(leveler_client_protocol::UiSessionHistory {
                entries,
                omitted_turns,
            })
        } else {
            None
        };
        let live = self.live_views.view(session_id);
        let checkpoints = self.checkpoints.list(session_id);

        // Long-goal P3: durable goal recaps survive the process, so a
        // reopened client re-renders them from the persisted rows — same
        // projection as the live event, oldest boundary first.
        let mut recaps = Vec::new();

        let runtime_active = self.active.is_running(session_id);
        let plan = projected_plan(&stores, session_id, live.plan).await?;
        // A goal that still owes work is the durable identity behind an
        // explicit task, whether it was opened by `/goal` or committed from a
        // Chat turn. The snapshot carries it so a reconnecting client restores
        // the task badge and objective, not just the transcript.
        let mut execution_task = None;
        if let Ok(Some(task)) = stores.tasks.task_for_session(session_id).await
            && let Ok(goals) = stores.goals.for_task(&task).await
        {
            for goal in &goals {
                if let Ok(rows) = stores.goal_checkpoints.for_goal(&goal.id).await {
                    recaps.extend(rows.iter().map(crate::goal_recap::project_goal_recap));
                }
            }
            recaps.sort_by(|a, b| {
                a.transcript_ordinal
                    .cmp(&b.transcript_ordinal)
                    .then_with(|| a.created_at.cmp(&b.created_at))
            });
            // Which owed goal this snapshot is about: the one the running
            // turn was opened for when there is one. A paused goal and a new
            // executing goal can be owed at the same time, and "first running
            // row" would show the paused one's wait while the other runs.
            let executing = if runtime_active {
                leveler_storage::TurnRepository::new(&db)
                    .list_running(Some(session_id))
                    .await
                    .ok()
                    .and_then(|turns| crate::session_projection::running_turn_goal(&turns))
            } else {
                None
            };
            execution_task =
                crate::session_projection::project_execution_goal(&goals, executing.as_ref()).map(
                    |goal| leveler_client_protocol::UiExecutionTask {
                        goal_id: goal.id.to_string(),
                        objective: goal.objective.clone(),
                        // A goal the user paused is not being executed, whatever
                        // a turn that merely settles its lease may still report.
                        running: runtime_active && goal.user_paused_at.is_none(),
                        user_paused: goal.user_paused_at.is_some(),
                    },
                );
        }
        let facts = db
            .session_facts_for(session_id)
            .await
            .map_err(|error| ClientError::Runtime(error.to_string()))?
            .ok_or_else(|| ClientError::SessionNotFound(session_id.clone()))?;
        let probe = crate::runtime_boot::StateDirBootLiveness::new(&self.app.layout.state_dir);
        let (task_status, task_terminal) = crate::session_projection::project_task(
            &facts,
            &probe,
            runtime_active,
            !pending_interactions.is_empty(),
        )?;
        let status = session_status_wire(record.status, task_status);

        // The crash window: starts with no finish. A call is only crash
        // residue when no live turn owns it — while a turn runs here, its open
        // calls are ordinary in-flight work. A turn whose owner boot cannot be
        // probed keeps its `Unknown` state rather than being called
        // interrupted.
        let interrupted_tool_calls =
            project_interrupted_tool_calls(&db, &facts, &probe, session_id, runtime_active).await?;

        // Unlike the best-effort fields above, a child row that cannot be
        // decoded fails the snapshot: it is canonical history, and every other
        // reader of it (restart reconciliation, observability) refuses corrupt
        // rows rather than showing a partial truth as the whole one.
        let children = crate::children::project_children(&db, session_id, runtime_active)
            .await
            .map_err(|e| ClientError::Runtime(e.to_string()))?;
        let active_background_tasks: Vec<leveler_client_protocol::UiActiveBackgroundTask> = self
            .app
            .background_tasks()
            .try_active_snapshots()
            .await
            .map_err(ClientError::Runtime)?
            .into_iter()
            .filter(|task| task.owner_scope.as_deref() == Some(session_id.as_str()))
            .map(|task| leveler_client_protocol::UiActiveBackgroundTask {
                task_id: task.id,
                program: task.program,
                args: task.args,
                elapsed_ms: task.duration_ms,
                pid: task.pid,
                ready_evidence: task.ready.map(|ready| ready.evidence),
                lifetime: Some(task.lifetime.as_str().to_string()),
            })
            .collect();
        // The full reconciled view: a reconnect must be able to tell a
        // finished task from one that died with its runtime, not just list
        // what is still alive. A snapshot is also a catch-up boundary: the
        // client can reconnect to a session with no turn ever starting, so the
        // durable record must be completed here, not only at a turn boundary.
        reconcile_background_evidence(&self.app, session_id).await;
        let hosted_terminal = hosted_terminal_facts(&self.app, session_id);
        let owner_lost = hosted_owner_lost_facts(&self.app, session_id);
        let background_tasks = project_background_tasks(
            &db,
            &probe,
            session_id,
            &active_background_tasks,
            &hosted_terminal,
            &owner_lost,
        )
        .await?;
        let foreign_origin = project_foreign_origin(&db, session_id).await;

        Ok(UiSessionSnapshot {
            history,
            id: session_id.clone(),
            repository: record.repository,
            task_status: Some(task_status),
            task_terminal,
            goal: record.goal,
            model,
            mode: protocol_mode(config.mode),
            branch: self
                .app
                .layout
                .primary_workspace()
                .and_then(detect_branch_label),
            status: status.to_string(),
            finalization_stage: runtime_active.then_some(live.finalization_stage).flatten(),
            messages,
            pending_interactions,
            available_models,
            vision,
            last_sequence,
            latest_turn_ordinal: facts.turns.last().map(|turn| turn.ordinal),
            active_tools: live.active_tools,
            interrupted_tool_calls,
            active_background_tasks,
            background_tasks,
            plan,
            diff: live.diff,
            checkpoints,
            recaps,
            user_shells: {
                let live = user_shells;
                let live_ids: std::collections::HashSet<_> =
                    live.iter().map(|shell| shell.id.clone()).collect();
                let mut restored = crate::user_shell::restore_from_events(&db, session_id)
                    .await
                    .map_err(ClientError::Runtime)?;
                restored.retain(|shell| !live_ids.contains(&shell.id));
                restored.extend(live);
                restored
            },
            completion_report: live.completion_report,
            thinking,
            work_profile: Some("single".into()),
            collaboration: Some(config.collaboration.clone()),
            execution_task,
            children,
            foreign_origin,
        })
    }
}

/// Project the session's crash window for a client snapshot.
///
/// A `tool_call_started` with no `tool_call_finished` is either ordinary live
/// work (a turn running here, or a running turn another live runtime owns) or
/// crash residue. Only residue is reported. Liveness of a turn whose boot
/// cannot be probed stays `Unknown`; it is never labelled interrupted.
async fn project_interrupted_tool_calls(
    db: &leveler_storage::Database,
    facts: &leveler_storage::SessionFacts,
    probe: &dyn BootLivenessProbe,
    session_id: &SessionId,
    runtime_active: bool,
) -> Result<Vec<UiInterruptedToolCall>, ClientError> {
    if runtime_active {
        return Ok(Vec::new());
    }
    let running_liveness: HashMap<&str, BootLiveness> = facts
        .turns
        .iter()
        .filter(|turn| turn.status == "running")
        .map(|turn| {
            let liveness = turn
                .owner_boot_id
                .as_deref()
                .map(BootId::new)
                .map(|boot| probe.liveness(&boot))
                .unwrap_or(BootLiveness::Unknown);
            (turn.id.as_str(), liveness)
        })
        .collect();
    Ok(leveler_engine::EventLog::new(db, session_id.clone())
        .dangling_tool_calls()
        .await
        .map_err(|error| ClientError::Runtime(error.to_string()))?
        .into_iter()
        .filter_map(|call| {
            let state = match call.turn_id.as_deref() {
                Some(turn) => match running_liveness.get(turn) {
                    // Another live process still owns this call.
                    Some(BootLiveness::Alive) => return None,
                    Some(BootLiveness::Dead) => UiInterruptedCallState::Interrupted,
                    Some(BootLiveness::Unknown) => UiInterruptedCallState::Unknown,
                    // A turn that is no longer running cannot still be
                    // executing its call.
                    None => UiInterruptedCallState::Interrupted,
                },
                // Legacy rows carry no turn attribution; claiming a definite
                // interruption would be a guess.
                None => UiInterruptedCallState::Unknown,
            };
            Some(UiInterruptedToolCall {
                call_id: call.call_id,
                tool: call.name,
                pending_approval: call.pending_approval,
                state,
            })
        })
        .collect())
}

/// Reconcile this session's background tasks from durable facts and the live
/// registry.
///
/// A missing live entry is not a terminal state: the process may have exited,
/// been cancelled, or died with the runtime that hosted it. Only a durable
/// terminal row — or the Execution Host's archived process fact, when its
/// stable identity agrees with the start — says how a task ended; without one,
/// the owning boot's liveness separates `Lost` from `Unknown`. A live entry
/// never gets a terminal it does not have: the snapshot reports what it can
/// prove, not what would be convenient.
///
/// A task the Execution Host marked owner-lost is `Lost` even when a live
/// process still exists: the process cannot deliver its result to the runtime
/// generation that admitted it, so presenting it as this runtime's Running work
/// would be a false claim. Its duration ends at the proven loss.
async fn project_background_tasks(
    db: &leveler_storage::Database,
    probe: &dyn BootLivenessProbe,
    session_id: &SessionId,
    live: &[leveler_client_protocol::UiActiveBackgroundTask],
    hosted_terminal: &[leveler_execution::BackgroundTaskSnapshot],
    owner_lost: &[leveler_execution::BackgroundTaskSnapshot],
) -> Result<Vec<leveler_client_protocol::UiBackgroundTaskRecord>, ClientError> {
    use leveler_client_protocol::{UiBackgroundTaskRecord, UiBackgroundTaskState};

    let facts = leveler_engine::EventLog::new(db, session_id.clone())
        .background_task_facts()
        .await
        .map_err(|error| ClientError::Runtime(error.to_string()))?;

    let turn_ids: Vec<String> = facts
        .iter()
        .filter_map(|fact| fact.turn_id.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let turns: HashMap<String, leveler_storage::TurnRecord> = if turn_ids.is_empty() {
        HashMap::new()
    } else {
        leveler_storage::TurnRepository::new(db)
            .list_by_ids(session_id, &turn_ids)
            .await
            .map_err(|error| ClientError::Runtime(error.to_string()))?
            .into_iter()
            .map(|turn| (turn.id.clone(), turn))
            .collect()
    };
    let mut liveness: HashMap<String, BootLiveness> = HashMap::new();
    let mut boot_of = |turn_id: &str| -> Option<BootLiveness> {
        let boot = turns.get(turn_id)?.owner_boot_id.clone()?;
        Some(
            *liveness
                .entry(boot.clone())
                .or_insert_with(|| probe.liveness(&BootId::new(boot.as_str()))),
        )
    };

    let live_by_id: HashMap<&str, &leveler_client_protocol::UiActiveBackgroundTask> = live
        .iter()
        .map(|task| (task.task_id.as_str(), task))
        .collect();
    let mut hosted_by_id: HashMap<&str, Vec<&leveler_execution::BackgroundTaskSnapshot>> =
        HashMap::new();
    for task in hosted_terminal {
        hosted_by_id.entry(task.id.as_str()).or_default().push(task);
    }
    let lost_by_id: HashMap<&str, &leveler_execution::BackgroundTaskSnapshot> = owner_lost
        .iter()
        .map(|task| (task.id.as_str(), task))
        .collect();

    // One record per id. Repeated ids are an identity conflict, not two
    // tasks: the client keys its chrome by task id, and neither the live
    // process nor a terminal row can be attributed to one start safely.
    let mut starts_by_id: HashMap<&str, Vec<&leveler_engine::BackgroundTaskFact>> = HashMap::new();
    for fact in &facts {
        starts_by_id
            .entry(fact.task_id.as_str())
            .or_default()
            .push(fact);
    }

    let mut records: Vec<UiBackgroundTaskRecord> = Vec::new();
    let mut emitted: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for fact in &facts {
        if !emitted.insert(fact.task_id.as_str()) {
            continue;
        }
        let starts = &starts_by_id[fact.task_id.as_str()];
        let latest = starts.last().copied().unwrap_or(fact);
        // The host's archive may close a start the engine never recorded a
        // terminal for. It is evidence only when its stable identity agrees:
        // exactly one archived record, for this task id, started by THIS call.
        // A record that cannot be attributed is an identity conflict, never a
        // completion.
        let hosted =
            hosted_by_id
                .get(fact.task_id.as_str())
                .and_then(|records| match records.as_slice() {
                    [record] if record.origin_call_id.as_deref() == Some(fact.call_id.as_str()) => {
                        Some(*record)
                    }
                    _ => None,
                });
        let hosted_conflict = fact.terminal.is_none()
            && hosted_by_id.contains_key(fact.task_id.as_str())
            && hosted.is_none();
        let lost = lost_by_id.get(fact.task_id.as_str()).copied();
        // An owner-lost task is not live work of this runtime even while its
        // process runs, so it never creates the live/terminal conflict case.
        let live_here = live_by_id.contains_key(fact.task_id.as_str()) && lost.is_none();
        let is_conflict = starts.len() > 1
            || hosted_conflict
            || (live_here && (starts.iter().any(|s| s.terminal.is_some()) || hosted.is_some()));
        let state = if is_conflict {
            UiBackgroundTaskState::Conflict
        } else if let Some(terminal) = &fact.terminal {
            terminal_state(terminal)
        } else if let Some(record) = hosted {
            proven_terminal_state(
                Some(record.status),
                record.exit_code,
                record.status != leveler_execution::BackgroundTaskStatus::Exited
                    || record.exit_code != Some(0),
            )
        } else if lost.is_some() {
            UiBackgroundTaskState::Lost
        } else if live_here {
            UiBackgroundTaskState::Running
        } else {
            match latest.turn_id.as_deref().and_then(&mut boot_of) {
                Some(BootLiveness::Dead) => UiBackgroundTaskState::Lost,
                // A live boot that no longer holds the task, an unprobed boot,
                // and a start without attribution all leave the state open.
                Some(BootLiveness::Alive | BootLiveness::Unknown) | None => {
                    UiBackgroundTaskState::Unknown
                }
            }
        };
        let (exit_code, output, output_truncated) = if state == UiBackgroundTaskState::Conflict {
            (None, String::new(), false)
        } else if let Some(terminal) = &fact.terminal {
            (
                terminal.exit_code,
                terminal.output.clone(),
                terminal.output_truncated,
            )
        } else if let Some(record) = hosted {
            // Same tail bound the durable fact uses, so a reconnect and a
            // later turn boundary describe the result identically.
            let (output, output_truncated) = output_tail(&record.log, record.log_truncated);
            (record.exit_code, output, output_truncated)
        } else if let Some(task) = lost {
            let (output, output_truncated) = output_tail(&task.log, task.log_truncated);
            (None, output, output_truncated)
        } else {
            (None, String::new(), false)
        };
        let duration_ms = match state {
            UiBackgroundTaskState::Conflict => None,
            UiBackgroundTaskState::Running => live_by_id
                .get(fact.task_id.as_str())
                .map(|task| task.elapsed_ms),
            UiBackgroundTaskState::Lost => lost.map(|task| task.duration_ms),
            UiBackgroundTaskState::Completed
            | UiBackgroundTaskState::Failed
            | UiBackgroundTaskState::Cancelled
            | UiBackgroundTaskState::Unknown => hosted.map(|record| record.duration_ms),
        };
        records.push(UiBackgroundTaskRecord {
            task_id: fact.task_id.clone(),
            call_id: (state != UiBackgroundTaskState::Conflict).then(|| fact.call_id.clone()),
            program: latest.program.clone(),
            args: latest.args.clone(),
            lifetime: (!latest.lifetime.is_empty()).then(|| latest.lifetime.clone()),
            state,
            exit_code,
            output,
            output_truncated,
            duration_ms,
        });
    }

    // A live task with no durable start (a log written before starts were
    // recorded) is still running work; dropping it would be a silent loss.
    for task in live {
        if starts_by_id.contains_key(task.task_id.as_str())
            || lost_by_id.contains_key(task.task_id.as_str())
        {
            continue;
        }
        records.push(UiBackgroundTaskRecord {
            task_id: task.task_id.clone(),
            call_id: None,
            program: task.program.clone(),
            args: task.args.clone(),
            lifetime: task.lifetime.clone(),
            state: UiBackgroundTaskState::Running,
            exit_code: None,
            output: String::new(),
            output_truncated: false,
            duration_ms: Some(task.elapsed_ms),
        });
    }

    // An owner-lost task whose start never reached the engine log is still a
    // real execution: the host admitted it, and the process may still be
    // running for a generation that is gone. Dropping it would erase work the
    // user started; reporting it Running would claim an owner that does not
    // exist. Lost is the only honest record.
    for task in owner_lost {
        if starts_by_id.contains_key(task.id.as_str()) {
            continue;
        }
        let (output, output_truncated) = output_tail(&task.log, task.log_truncated);
        records.push(UiBackgroundTaskRecord {
            task_id: task.id.clone(),
            call_id: task.origin_call_id.clone(),
            program: task.program.clone(),
            args: task.args.clone(),
            lifetime: Some(task.lifetime.as_str().to_string()),
            state: UiBackgroundTaskState::Lost,
            exit_code: None,
            output,
            output_truncated,
            duration_ms: Some(task.duration_ms),
        });
    }
    Ok(records)
}

/// The bounded output tail retained with a durable fact, so every projection
/// describes the same result with the same bound.
fn output_tail(log: &str, log_truncated: bool) -> (String, bool) {
    const OUTPUT_TAIL_BYTES: usize = 4096;
    let start = leveler_core::ceil_char_boundary(log, log.len().saturating_sub(OUTPUT_TAIL_BYTES));
    (log[start..].to_owned(), log_truncated || start != 0)
}

/// Read a durable terminal as the state it proves. A zero exit is the only
/// pass; a killed or unknown runtime status is never folded into one.
fn terminal_state(
    terminal: &leveler_engine::BackgroundTaskTerminal,
) -> leveler_client_protocol::UiBackgroundTaskState {
    proven_terminal_state(terminal.status, terminal.exit_code, terminal.is_error)
}

/// The same reading from a bare process fact, so the engine's durable row and
/// the Execution Host's archive cannot describe one outcome two ways.
fn proven_terminal_state(
    status: Option<leveler_execution::BackgroundTaskStatus>,
    exit_code: Option<i32>,
    is_error: bool,
) -> leveler_client_protocol::UiBackgroundTaskState {
    use leveler_client_protocol::UiBackgroundTaskState;
    match status {
        Some(leveler_execution::BackgroundTaskStatus::Killed) => UiBackgroundTaskState::Cancelled,
        Some(leveler_execution::BackgroundTaskStatus::Unknown) => UiBackgroundTaskState::Unknown,
        Some(leveler_execution::BackgroundTaskStatus::Exited) => match exit_code {
            Some(0) => UiBackgroundTaskState::Completed,
            Some(_) => UiBackgroundTaskState::Failed,
            // Exited without an established code is not a pass and not a
            // proven failure.
            None => UiBackgroundTaskState::Unknown,
        },
        // `is_error` is the runtime's own terminal reading for rows written
        // before the status was recorded; it is never invented here.
        Some(leveler_execution::BackgroundTaskStatus::Running)
        | Some(leveler_execution::BackgroundTaskStatus::Killing) => UiBackgroundTaskState::Unknown,
        None => match exit_code {
            Some(0) => UiBackgroundTaskState::Completed,
            Some(_) => UiBackgroundTaskState::Failed,
            None if is_error => UiBackgroundTaskState::Failed,
            None => UiBackgroundTaskState::Unknown,
        },
    }
}

/// Arm the process-retire drain: once nothing is still owed, end the process.
/// Shared by `ShutdownWhenIdle` and `ForceRetire`, so both reach the same
/// quiescence reading the client waits on.
fn spawn_retire_drain(runtime: &InProcessRuntimeClient) {
    let Some(token) = runtime.process_shutdown.clone() else {
        return;
    };
    let active = runtime.active.clone();
    let background = runtime.app.background_tasks().clone();
    tokio::spawn(async move {
        // Idle means nothing is still owed: no turn running and no
        // runtime-owned background task alive. A turn ending is not enough — a
        // local background build outliving its turn is exactly the work a
        // replacement would destroy. A task owned by the persistent Execution
        // Host (a dev server or watcher) is NOT part of this: it outlives the
        // generation that launched it, so the replacement keeps it alive.
        // The SAME reading `runtime_info` reports, so a client that keeps
        // asking sees the drain reach zero.
        loop {
            match runtime_quiescence(&active, &background).await {
                Ok(state) if state.quiescent() => break,
                Ok(_) => {}
                Err(error) => tracing::error!(%error, "cannot observe retiring runtime blockers"),
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        tracing::info!("runtime idle; retiring");
        token.cancel();
    });
}

/// Arm the FORCE-retire drain.
///
/// Unlike [`spawn_retire_drain`], this does NOT wait for main turns to reach a
/// terminal: the user has decided a turn that will never terminate must not
/// block the update. It DOES still wait for runtime-owned background work — a
/// handover must never silently destroy a user-launched local process. Only the
/// persistent Execution Host's own services survive the replacement. The replacement generation
/// reconciles the persisted `running` turn left behind (the existing
/// ended-boot reaper), so nothing is lost that a clean exit would have kept.
fn spawn_force_retire_drain(runtime: &InProcessRuntimeClient) {
    let Some(token) = runtime.process_shutdown.clone() else {
        return;
    };
    let background = runtime.app.background_tasks().clone();
    tokio::spawn(async move {
        loop {
            match background.try_update_blockers().await {
                Ok(blockers) if blockers.is_empty() => break,
                Ok(_) => {}
                Err(error) => tracing::error!(%error, "cannot observe update blockers"),
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        tracing::info!("background work settled; force-retiring");
        token.cancel();
    });
}

#[async_trait]
impl leveler_local_transport::LocalRuntimeService for InProcessRuntimeClient {
    fn supports_terminal(&self) -> bool {
        cfg!(unix) && self.app.layout.primary_workspace().is_some()
    }
    async fn terminal(
        &self,
        request: leveler_client_protocol::TerminalRequest,
    ) -> Result<Option<leveler_client_protocol::TerminalState>, ClientError> {
        use leveler_client_protocol::TerminalOperation;
        use leveler_execution::command::PtySize;
        request.validate().map_err(ClientError::Runtime)?;
        if self.app.layout.primary_workspace().is_none() {
            return Err(ClientError::Runtime(
                "integrated terminals require a workspace".into(),
            ));
        }
        // Hydrates durable settings and validates this session's canonical source.
        // The profile itself is deliberately NOT applied to this shell.
        self.runtime_config(&request.session_id).await?;
        let id = format!("host-{}", request.terminal_id.replace('-', ""));
        let owner = request.session_id.as_str();
        let registry = self.app.background_tasks();
        let observation = match request.operation {
            TerminalOperation::Create { cols, rows } => {
                let shell = self
                    .app
                    .environment
                    .var("SHELL")
                    .filter(|s| {
                        std::path::Path::new(s).is_absolute() && std::path::Path::new(s).is_file()
                    })
                    .unwrap_or_else(|| "/bin/zsh".into());
                let mut process = self
                    .app
                    .user_terminal_execution(shell)
                    .map_err(|e| ClientError::Runtime(e.to_string()))?;
                process.pty = Some(PtySize { cols, rows });
                registry
                    .create_terminal(&id, process, owner)
                    .await
                    .map_err(ClientError::Runtime)?
            }
            TerminalOperation::Write { data } => {
                registry
                    .terminal_write(&id, owner, data)
                    .await
                    .map_err(ClientError::Runtime)?;
                return Ok(None);
            }
            TerminalOperation::Resize { cols, rows } => {
                registry
                    .terminal_resize(&id, owner, PtySize { cols, rows })
                    .await
                    .map_err(ClientError::Runtime)?;
                return Ok(None);
            }
            TerminalOperation::Observe { cursor, max_bytes } => registry
                .terminal_observe(&id, owner, cursor, max_bytes)
                .await
                .map_err(ClientError::Runtime)?,
            TerminalOperation::Close => {
                registry
                    .kill_owned(&id, owner)
                    .await
                    .map_err(ClientError::Runtime)?;
                let shutdown = self.process_shutdown.clone().unwrap_or_default();
                registry
                    .wait_owned(
                        &id,
                        owner,
                        Some(std::time::Duration::from_secs(5)),
                        &shutdown,
                    )
                    .await
                    .map_err(ClientError::Runtime)?;
                registry
                    .terminal_observe(&id, owner, 0, 65536)
                    .await
                    .map_err(ClientError::Runtime)?
            }
        };
        let snapshot = observation.snapshot;
        Ok(Some(leveler_client_protocol::TerminalState {
            terminal_id: request.terminal_id,
            shell: snapshot.program,
            cwd: snapshot.cwd.display().to_string(),
            status: match snapshot.status {
                leveler_execution::BackgroundTaskStatus::Running => "running",
                leveler_execution::BackgroundTaskStatus::Killing => "killing",
                leveler_execution::BackgroundTaskStatus::Exited => "exited",
                leveler_execution::BackgroundTaskStatus::Killed => "killed",
                leveler_execution::BackgroundTaskStatus::Unknown => "unknown",
            }
            .into(),
            exit_code: snapshot.exit_code,
            pid: snapshot.pid,
            cols: observation.cols,
            rows: observation.rows,
            output_start: observation.output_start,
            output_end: observation.output_end,
            next_cursor: observation.next_cursor,
            data: observation.data,
            truncated: observation.truncated,
            output_error: observation.output_error,
        }))
    }

    fn has_primary_workspace(&self) -> bool {
        self.app.layout.primary_workspace().is_some()
    }

    /// Re-assert the effective approval policy for an existing session
    /// (attach/resume). Memory-only and keyed by session id: it upgrades or
    /// downgrades exactly ONE session's live policy without touching the DB
    /// (the policy is deliberately not persisted, so restores stay fail-closed
    /// Interactive) and without leaking across sessions (R006 R6-P2).
    async fn attach_session_policy(
        &self,
        session_id: &SessionId,
        policy: ApprovalPolicy,
    ) -> Result<(), ClientError> {
        // Hydrate first so an unknown session errs and a known-but-cold one
        // gets its full runtime config before the policy write.
        let _ = self.runtime_config(session_id).await?;
        let mut map = self.session_runtime.lock().unwrap();
        let entry = map.get_mut(session_id).ok_or_else(|| {
            ClientError::Runtime(format!("session {} is not open", session_id.as_str()))
        })?;
        entry.approval_policy = policy;
        Ok(())
    }

    async fn create_session(
        &self,
        request: leveler_local_transport::CreateSessionRequest,
    ) -> Result<leveler_local_transport::SessionBootstrap, ClientError> {
        use leveler_local_transport::CreateWorkspaceSelection;
        match &request.workspace {
            CreateWorkspaceSelection::RuntimeDefault => {},
            CreateWorkspaceSelection::None if self.app.layout.primary_workspace().is_none() => {},
            CreateWorkspaceSelection::Workspace { path } => {
                let root = std::fs::canonicalize(path).map_err(|error| ClientError::Runtime(error.to_string()))?;
                if self.app.layout.primary_workspace() != Some(root.as_path()) {
                    return Err(ClientError::Runtime("workspace belongs to a different runtime source; connect to its owner".into()));
                }
            },
            _ => return Err(ClientError::Runtime("no-workspace session belongs to the no-workspace runtime source; connect to its owner".into())),
        }
        let fingerprint = format!(
            "create_session:{}",
            serde_json::to_string(&request)
                .map_err(|error| ClientError::Runtime(error.to_string()))?
        );
        if let Some(request_id) = request.request_id.as_ref() {
            let db = self
                .app
                .open_database()
                .await
                .map_err(|e| ClientError::Runtime(e.to_string()))?;
            if let Some(session_id) = leveler_storage::CommandReceiptRepository::new(&db)
                .completed_creation(request_id, &fingerprint)
                .await
                .map_err(|e| ClientError::Runtime(e.to_string()))?
            {
                let session = self.snapshot(&session_id).await?;
                // A deterministic-only session has no model and therefore no
                // context window. Zero is the wire's absent value; clients
                // already treat it as unknown.
                let context_window = match session.model.as_ref() {
                    Some(model) => {
                        self.app
                            .registry()
                            .profile(model)
                            .await
                            .map_err(|e| ClientError::Runtime(e.to_string()))?
                            .limits
                            .context_window
                    }
                    None => 0,
                };
                return Ok(leveler_local_transport::SessionBootstrap {
                    session,
                    context_window,
                    // A retried creation returns the receipt of the first call;
                    // the substitution notice was delivered there.
                    notice: None,
                });
            }
        }
        // An EXPLICIT request is the user's own intent and stays a hard error.
        // A DEFAULT that this environment cannot resolve (a leftover from a
        // bundle that is gone, a hand-edited config) must not leave the UI
        // unable to start: fall back to a configured model and say so.
        let mut substitution: Option<(ModelRef, ModelRef)> = None;
        let model = match request.model.clone() {
            Some(explicit) => {
                if !self.app.model_refs().contains(&explicit) {
                    return Err(ClientError::Runtime(format!(
                        "model `{explicit}` is not configured"
                    )));
                }
                Some(explicit)
            }
            None => {
                // A session created NOW starts on the configuration in force
                // now, not on the model this process launched with.
                let preferred = self
                    .app
                    .configured_default_model()
                    .or_else(|| self.default_runtime.model.clone());
                match preferred {
                    Some(preferred) if self.app.model_refs().contains(&preferred) => {
                        Some(preferred)
                    }
                    Some(preferred) => {
                        let fallback = self.app.fallback_default_model().ok_or_else(|| {
                            ClientError::Runtime(format!(
                                "model `{preferred}` is not configured, and this environment has no\
                                 other configured model to fall back to"
                            ))
                        })?;
                        substitution = Some((fallback.clone(), preferred));
                        Some(fallback)
                    }
                    // No model exists anywhere in this environment. That is a
                    // legitimate deterministic execution context, not a
                    // reason to block the runtime: the session is created
                    // with model = NULL and agent turns fail loudly.
                    None => None,
                }
            }
        };
        let substitution_notice = substitution.as_ref().map(|(used, missing)| {
            format!(
                "默认模型 {missing} 在本环境不可用（已从配置中移除？），本次改用 {used}\
                 ；用 /model 选择并确认一个可用的模型"
            )
        });
        // The composition root may already have learned this while resolving the
        // default model (the daemon path resolves before any session exists, and
        // its stderr is a log file nobody reads).
        let substitution_notice =
            substitution_notice.or_else(|| self.app.take_default_model_notice());
        let (session_id, inserted) = if let Some(request_id) = request.request_id.as_ref() {
            let db = self
                .app
                .open_database()
                .await
                .map_err(|error| ClientError::Runtime(error.to_string()))?;
            self.app
                .task_engine(&db)
                .map_err(|error| ClientError::Runtime(error.to_string()))?
                .create_task_identified(
                    &leveler_engine::NewSession {
                        workspace: self
                            .app
                            .layout
                            .primary_workspace()
                            .map(|root| root.display().to_string()),
                        goal: request.goal.clone(),
                        model: model.as_ref().map(ToString::to_string),
                        mode: execution_mode(request.mode).as_str().to_string(),
                        sandbox: self.default_runtime.sandbox,
                        kind: leveler_engine::ExecutionKind::Direct,
                        axes: Some(leveler_engine::NewSessionAxes {
                            collaboration: request.collaboration.as_str().to_string(),
                        }),
                        // An explicit create request chose this model.
                        model_pinned: request.model.is_some(),
                    },
                    request_id,
                    &fingerprint,
                )
                .await
                .map_err(|error| ClientError::Runtime(error.to_string()))?
        } else {
            (
                self.app
                    .create_daemon_session(
                        model.as_ref(),
                        &request.goal,
                        execution_mode(request.mode),
                        request.collaboration,
                        request.model.is_some(),
                    )
                    .await
                    .map_err(|error| ClientError::Runtime(error.to_string()))?,
                true,
            )
        };
        if inserted {
            let config = SessionRuntimeConfig {
                model: model.clone(),
                mode: execution_mode(request.mode),
                sandbox: self.default_runtime.sandbox,
                collaboration: request.collaboration.as_str().to_string(),
                approval_policy: request.approval_policy,
                thinking: None,
            };
            if request.request_id.is_some() {
                // All durable profile fields already committed with the receipt.
                // Install only this boot's live projection; retries never rewrite it.
                self.app
                    .set_live_permission_profile(session_id.as_str(), config.mode);
                self.session_runtime
                    .lock()
                    .unwrap()
                    .insert(session_id.clone(), config);
            } else {
                self.persist_runtime_config(&session_id, config).await?;
            }
        }
        let session = self.snapshot(&session_id).await?;
        // No model means no context window. Zero is the wire's absent value;
        // clients already treat it as unknown.
        let context_window = match session.model.as_ref() {
            Some(model) => self
                .app
                .registry()
                .profile(model)
                .await
                .map(|profile| profile.limits.context_window)
                .map_err(|error| ClientError::Runtime(error.to_string()))?,
            None => 0,
        };
        Ok(leveler_local_transport::SessionBootstrap {
            session,
            context_window,
            notice: substitution_notice,
        })
    }

    async fn fetch_attachment(
        &self,
        sha256: &str,
    ) -> Result<leveler_local_transport::AttachmentBytes, ClientError> {
        let root = self.media_root.clone();
        let sha = sha256.to_string();
        tokio::task::spawn_blocking(move || MediaStore::new(&root).load_bytes(&sha))
            .await
            .map_err(|error| ClientError::Runtime(error.to_string()))?
            .map(|(mime_type, bytes)| leveler_local_transport::AttachmentBytes { mime_type, bytes })
            .map_err(|_| ClientError::Runtime("attachment not found".to_string()))
    }

    async fn runtime_info(&self) -> Result<leveler_client_protocol::RuntimeInfo, ClientError> {
        let runtime_id = self
            .app
            .runtime_id()
            .map_err(|error| ClientError::Runtime(error.to_string()))?;
        let (active, capacity) = self.active.load();
        let shutting_down = self.shutting_down.load(std::sync::atomic::Ordering::SeqCst);
        let retiring_reason = self
            .retiring_reason
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .to_owned();
        // The SAME reading the drain waits on: a client's view of "idle" is
        // never a second accounting.
        let quiescence = runtime_quiescence(&self.active, self.app.background_tasks()).await?;
        // Name the tasks behind the count, from the same registry the drain
        // waits on. Read-only: stopping one still goes through
        // `CancelBackgroundTask`.
        let blockers = self
            .app
            .background_tasks()
            .try_update_blockers()
            .await
            .map_err(ClientError::Runtime)?
            .into_iter()
            .map(|task| leveler_client_protocol::UiBackgroundTaskBlocker {
                task_id: task.id,
                program: task.program,
                args: task.args,
                elapsed_ms: task.duration_ms,
                session_id: task.owner_scope.map(leveler_core::SessionId::new),
                log_tail: background_log_tail(&task.log),
            })
            .collect();
        // Name the main turns behind the count the same way, with the facts a
        // handover needs: which session, how long it has run, and how long
        // since it last did anything observable. Facts only — no session is
        // ever marked dead or cancelled from here.
        let turn_blockers = self
            .active
            .snapshots()
            .into_iter()
            .map(|turn| leveler_client_protocol::UiTurnBlocker {
                session_id: turn.session_id,
                elapsed_ms: turn.elapsed_ms,
                idle_ms: turn.idle_ms,
            })
            .collect();
        Ok(leveler_client_protocol::RuntimeInfo {
            runtime_id,
            version: env!("CARGO_PKG_VERSION").to_string(),
            // The daemon says which build it IS, not just which version it
            // calls itself: that difference is the whole point of the field.
            build: leveler_core::BuildIdentity::current(),
            config_fingerprint: Some(self.app.config_fingerprint().to_string()),
            pid: std::process::id(),
            health: leveler_client_protocol::RuntimeHealth {
                // Health is admission state, never authority: task writes
                // still require a current OwnershipToken regardless.
                accepting_work: !shutting_down && active < capacity,
                active_turns: quiescence.active_turns,
                active_background_tasks: quiescence.active_background_tasks,
                quiescent: quiescence.quiescent(),
                turn_capacity: Some(capacity as u32),
                shutting_down,
                retiring_reason,
                blockers,
                turn_blockers,
            },
        })
    }

    /// Ask whether this runtime may retire NOW, and commit the answer.
    ///
    /// The decision is the runtime's, taken from its own authority:
    ///
    /// 1. the generation the caller observed must be the one answering, or the
    ///    answer is `GenerationChanged` (nothing is touched);
    /// 2. an already-retiring runtime reports `AlreadyRetiring` with the reason
    ///    already recorded, so a second client adopts the in-flight handover
    ///    instead of inventing a competing one;
    /// 3. work still owed — a main turn, or a runtime-owned background task
    ///    (a local build outliving its turn is exactly the work a replacement
    ///    would destroy) — yields `Busy`, and NOTHING is changed: admission
    ///    stays open and other clients keep working;
    /// 4. only an idle runtime commits, atomically with closing admission.
    ///
    /// A task owned by the persistent Execution Host is deliberately NOT owed
    /// work here: it outlives the generation that launched it, and the
    /// replacement keeps it running.
    async fn try_retire_if_idle(
        &self,
        request: leveler_client_protocol::RetireRequest,
    ) -> Result<leveler_client_protocol::RetireDecision, ClientError> {
        use leveler_client_protocol::RetireDecision;

        // The generation proof comes first and changes nothing: a decision
        // about a runtime the caller never examined would be worthless.
        let info = self.runtime_info().await?;
        if (request.expected_pid != 0 && request.expected_pid != info.pid)
            || !self_matches_generation(&request.expected_build, &info.build)
        {
            return Ok(RetireDecision::GenerationChanged);
        }
        if self.active.is_retiring() {
            let reason = self
                .retiring_reason
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .to_owned();
            return Ok(RetireDecision::AlreadyRetiring { reason });
        }

        // Read the background accounting BEFORE committing. It is a separate
        // owner from turn admission, so it cannot be folded into the same
        // critical section; reading it first can only make the decision more
        // conservative, never optimistically wrong.
        let (active_turns, capacity) = self.active.load();
        let background = self
            .app
            .background_tasks()
            .try_update_blockers()
            .await
            .map_err(ClientError::Runtime)?
            .len() as u32;
        if background > 0 {
            return Ok(RetireDecision::Busy {
                active_turns: active_turns as u32,
                active_background_tasks: background,
            });
        }
        let _ = capacity;

        // Commit: decide, close admission and record the reason. A caller that
        // then sees `Accepted` knows no new work can be admitted, and this
        // runtime exits once whatever it still holds finishes.
        match self.active.retire_if_idle() {
            RetireAdmission::Accepted => {
                *self
                    .retiring_reason
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(request.reason);
                tracing::info!(reason = ?request.reason, "runtime accepted retirement while idle");
                spawn_retire_drain(self);
                Ok(RetireDecision::Accepted)
            }
            RetireAdmission::Busy { active_turns } => Ok(RetireDecision::Busy {
                active_turns,
                active_background_tasks: 0,
            }),
            RetireAdmission::AlreadyRetiring => {
                let reason = self
                    .retiring_reason
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .to_owned();
                Ok(RetireDecision::AlreadyRetiring { reason })
            }
        }
    }
}

/// Whether `expected` really is the build serving this request.
///
/// `matches` (fingerprint when both carry one) is the comparison the rest of
/// the handover uses, so a dirty development build recognises its own artifact
/// instead of being retired on every launch. An unknown identity on either side
/// is NOT a match: nothing is not evidence of agreement.
fn self_matches_generation(
    expected: &leveler_core::BuildIdentity,
    serving: &leveler_core::BuildIdentity,
) -> bool {
    expected.is_known() && serving.is_known() && expected.matches(serving)
}

/// Largest tail of a background task's output carried in a runtime-health
/// answer. The full log stays in the registry; this is only enough for a
/// read-only glance from a client that is not attached to the task stream.
const BACKGROUND_LOG_TAIL_BYTES: usize = 2_000;

/// The last [`BACKGROUND_LOG_TAIL_BYTES`] of `log`, cut on a char boundary.
fn background_log_tail(log: &str) -> String {
    if log.len() <= BACKGROUND_LOG_TAIL_BYTES {
        return log.to_string();
    }
    let start = log.len() - BACKGROUND_LOG_TAIL_BYTES;
    let start = (start..log.len())
        .find(|index| log.is_char_boundary(*index))
        .unwrap_or(log.len());
    log[start..].to_string()
}

fn mime_from_name(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".md") {
        "text/markdown".to_string()
    } else if lower.ends_with(".diff") || lower.ends_with(".patch") {
        "text/x-diff".to_string()
    } else if lower.ends_with(".txt") {
        "text/plain".to_string()
    } else if lower.ends_with(".json") {
        "application/json".to_string()
    } else {
        "application/octet-stream".to_string()
    }
}

fn kind_from_mime(mime: &str) -> AttachmentKind {
    if mime.starts_with("image/") {
        AttachmentKind::Image
    } else if mime.starts_with("text/") || mime == "application/json" {
        AttachmentKind::TextFile
    } else {
        AttachmentKind::Document
    }
}

/// Parse every stored message payload; any corrupt row is a hard error so
/// compact cannot rewrite history after silently dropping rows.
fn parse_history_messages_strict(payloads: &[String]) -> Result<Vec<Message>, String> {
    let mut out = Vec::with_capacity(payloads.len());
    for (i, payload) in payloads.iter().enumerate() {
        match serde_json::from_str::<Message>(payload) {
            Ok(msg) => out.push(msg),
            Err(e) => {
                return Err(format!("第 {} 条历史消息无法解析: {e}", i + 1));
            }
        }
    }
    Ok(out)
}

/// The newest plan declaration with its durable lifecycle classification.
///
/// The live view only knows what THIS process forwarded. After a restart it is
/// empty, so the append-only log is the authority: a plan followed by a task
/// terminal is history and stays viewable, but is never reported as active
/// work. A later `PlanUpdated` row wins by sequence, so a new epoch is active
/// and an old plan cannot overwrite it.
async fn projected_plan(
    stores: &leveler_storage::EngineStores,
    session_id: &SessionId,
    live: Option<UiPlan>,
) -> Result<Option<UiPlan>, ClientError> {
    if let Some(plan) = live {
        return Ok(Some(plan));
    }
    let Some(row) = stores
        .events
        .load_last_by_type(session_id, "plan_updated", None)
        .await
        .map_err(|e| ClientError::Runtime(e.to_string()))?
    else {
        return Ok(None);
    };
    let steps = match leveler_engine::EngineEvent::from_payload(&row.payload)
        .map_err(|e| ClientError::Runtime(e.to_string()))?
    {
        leveler_engine::EngineEvent::PlanUpdated { steps, .. } => steps,
        _ => {
            return Err(ClientError::Runtime(
                "plan_updated row carried a different event".into(),
            ));
        }
    };
    let terminal = stores
        .events
        .load_last_by_type(session_id, "task_finished", None)
        .await
        .map_err(|e| ClientError::Runtime(e.to_string()))?;
    let state = match terminal {
        Some(terminal) if terminal.sequence > row.sequence => {
            match leveler_engine::EngineEvent::from_payload(&terminal.payload)
                .map_err(|e| ClientError::Runtime(e.to_string()))?
            {
                leveler_engine::EngineEvent::TaskFinished {
                    outcome: leveler_lifecycle::TaskOutcome::Completed,
                    ..
                } => UiPlanState::Completed,
                _ => UiPlanState::Interrupted,
            }
        }
        _ => UiPlanState::Active,
    };
    let mut plan = crate::event_bridge::ui_plan(steps);
    plan.state = state;
    Ok(Some(plan))
}

/// Summarize a session's history via the model and replace it with the summary,
/// then push a refreshed snapshot (spec §28, §53).
///
/// Failures are always surfaced (TurnFailed / Notification) — never silent.
/// Returns `true` when the message store was rewritten (caller must drop
/// checkpoints); `false` when history was left unchanged (short/no-op/error).
async fn compact_conversation(
    app: &Application,
    events: &crate::event_bridge::RuntimeEventSender,
    live_views: &crate::live_view::LiveViews,
    model: &ModelRef,
    session_id: &SessionId,
    cancellation: CancellationToken,
) -> bool {
    let notify = |level: NotificationLevel, message: String| {
        let _ = events.send(RuntimeEvent::Notification { level, message });
    };
    let fail = |message: String| {
        let _ = events.send(RuntimeEvent::TurnFailed {
            error: message.clone(),
            failure: None,
            recovery: None,
        });
        notify(NotificationLevel::Error, message);
    };
    let db = match app.open_database().await {
        Ok(db) => db,
        Err(e) => {
            fail(format!("压缩失败：无法打开数据库: {e}"));
            return false;
        }
    };
    let repo = MessageRepository::new(&db);
    let payloads = match repo.load(session_id).await {
        Ok(p) => p,
        Err(e) => {
            fail(format!("压缩失败：无法读取对话历史: {e}"));
            return false;
        }
    };
    // Strict parse: any corrupt row aborts before replace_all can destroy
    // history. Do not filter_map silently — a partial parse + rewrite is data loss.
    let mut request_messages = match parse_history_messages_strict(&payloads) {
        Ok(msgs) => msgs,
        Err(e) => {
            fail(format!("压缩失败：{e}（原对话未改动）"));
            return false;
        }
    };
    if request_messages.len() < 4 {
        notify(NotificationLevel::Info, "对话较短，无需压缩".to_string());
        return false;
    }
    // What the compaction replaces, for the durable `Compacted` fact below.
    let compacted_from = request_messages.len();
    let delivered_background_tasks = request_messages
        .iter()
        .filter_map(|message| message.origin.as_ref())
        .flat_map(TranscriptOrigin::delivered_background_task_ids)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .cloned()
        .collect();
    request_messages.push(Message::text(Role::User, COMPACT_PROMPT));
    let mut request = ModelRequest::new(model.clone(), request_messages);
    let requested_effort = leveler_engine::ModelCallKind::Compaction.default_reasoning_effort();
    let profile = match app.registry().profile(model).await {
        Ok(profile) => profile,
        Err(error) => {
            fail(format!(
                "压缩失败：无法读取模型配置: {error}（原历史未改动）"
            ));
            return false;
        }
    };
    request.reasoning_effort =
        resolve_reasoning_effort(requested_effort, &profile.reasoning).effective;
    request.max_output_tokens = Some(profile.limits.max_output_tokens.min(4096));
    request.projection = Some(leveler_model::RequestProjection::project_for_route(
        &request.messages,
        &request.tools,
        leveler_model::ReasoningReplayContract::resolve(profile.protocol, &profile.compatibility),
        leveler_model::ReasoningRetention::default(),
        app.registry().route_identity(model).as_ref(),
    ));
    let estimated_tokens = request
        .projection
        .as_ref()
        .map(|projection| projection.estimated_tokens());
    let progress =
        match leveler_agent::load_auxiliary_budget_progress(&db, &db, &db, session_id).await {
            Ok(progress) => progress,
            Err(error) => {
                fail(format!(
                    "压缩失败：无法重建任务预算: {error}（原历史未改动）"
                ));
                return false;
            }
        };
    let limits = app.top_level_limits();
    if !leveler_agent::auxiliary_budget_available(
        limits,
        &progress,
        &request,
        profile.pricing.as_ref(),
    ) {
        fail("压缩失败：剩余任务预算不足或已有请求费用未知（原历史未改动）".to_string());
        return false;
    }
    let effort = request.reasoning_effort.map(|value| value.as_wire());
    // Show a spinner while the (blocking, non-streaming) summary is generated —
    // otherwise the whole briefing "appears out of nowhere" with no feedback.
    let _ = events.send(RuntimeEvent::AgentActivity {
        label: "正在压缩上下文…".to_string(),
    });
    let started = std::time::Instant::now();
    let call_cancel = cancellation.child_token();
    let registry = app.registry();
    let generate = registry.generate(request, call_cancel.clone());
    tokio::pin!(generate);
    let response = if let Some(max_duration) = limits.max_duration {
        let remaining = max_duration.saturating_sub(std::time::Duration::from_millis(
            progress.cumulative_duration_ms,
        ));
        tokio::select! {
            result = &mut generate => result,
            _ = tokio::time::sleep(remaining) => {
                call_cancel.cancel();
                generate.await
            }
        }
    } else {
        generate.await
    };
    let latency_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
    if let Err(error) = crate::observability::record_auxiliary_call(
        app,
        &db,
        session_id,
        leveler_storage::ModelCallKind::Compaction,
        &model.provider,
        &model.model,
        effort,
        &response,
        latency_ms,
        progress.budget_scope.as_deref(),
        estimated_tokens,
    )
    .await
    {
        fail(format!(
            "压缩失败：无法记录模型请求: {error}（原历史与任务状态未改动）"
        ));
        return false;
    }
    if progress.budget_scope.is_some() {
        let persisted = async {
            let mut updated =
                leveler_agent::load_auxiliary_budget_progress(&db, &db, &db, session_id).await?;
            updated.cumulative_duration_ms =
                updated.cumulative_duration_ms.saturating_add(latency_ms);
            let (tag, payload) =
                leveler_engine::EngineEvent::ProgressUpdated { ledger: updated }.to_row()?;
            leveler_storage::EventRepository::new(&db)
                .append(session_id, None, &tag, &payload, leveler_core::now())
                .await?;
            Ok::<(), leveler_engine::EngineError>(())
        }
        .await;
        if let Err(error) = persisted {
            fail(format!(
                "压缩失败：无法保存任务预算: {error}（原历史与检查点未改动）"
            ));
            return false;
        }
    }
    let summary = match response {
        Ok(resp) => match leveler_context::accepted_summary(&resp) {
            Some(summary) => summary,
            None => {
                fail(
                    "压缩失败：摘要为空、未完整结束或包含工具调用（原历史与任务状态未改动）"
                        .to_string(),
                );
                return false;
            }
        },
        Err(e) => {
            fail(format!("压缩失败：{e}"));
            return false;
        }
    };

    let summary_msg = Message::user(
        format!(
            "{}：\n{summary}",
            leveler_client_protocol::COMPACTION_SUMMARY_PREFIX
        ),
        TranscriptOrigin::CompactionSummary {
            delivered_background_tasks,
        },
    );
    let background_snapshots = match app.background_tasks().try_active_snapshots().await {
        Ok(tasks) => tasks,
        Err(error) => {
            fail(format!(
                "压缩失败：无法读取后台任务状态: {error}（原历史与任务状态未改动）"
            ));
            return false;
        }
    };
    if let Err(e) =
        commit_compaction_epoch(&db, session_id, summary_msg.clone(), compacted_from).await
    {
        // Fail closed: the transaction rolled back, so the pre-compact
        // state is intact and coherent — never a half-compacted session.
        fail(format!("压缩失败：{e}（原历史与任务状态未改动）"));
        return false;
    }
    // The transcript the previous accounting measured is gone: keeping it would
    // report the pre-compaction size against the post-compaction history. The
    // next turn republishes the folded request's accounting.
    live_views.clear_context(session_id);

    let settings = match SessionRepository::new(&db)
        .settings_snapshot(session_id)
        .await
    {
        Ok(Some(settings)) => settings,
        Ok(None) => {
            fail("压缩已提交，但会话不存在；请重新连接".into());
            return false;
        }
        Err(error) => {
            fail(format!("压缩已提交，但无法读取一致快照：{error}"));
            return false;
        }
    };
    let config = match InProcessRuntimeClient::decode_settings(
        session_id,
        &settings,
        ApprovalPolicy::Interactive,
        app.configured_default_model(),
    ) {
        Ok(config) => config,
        Err(error) => {
            fail(format!("压缩已提交，但无法解码会话设置：{error}"));
            return false;
        }
    };
    let model = config.model;
    let mode = config.mode;
    let thinking_level = config.thinking;
    let last_sequence = settings.sequence;
    let record = settings.session;
    {
        let messages = repo
            .load(session_id)
            .await
            .unwrap_or_default()
            .iter()
            .filter_map(|p| ui_message(p))
            .collect();
        let mut available_models = app.model_refs();
        available_models.sort_by_key(|m| m.to_string());
        let profile = match model.as_ref() {
            Some(model) => app.registry().profile(model).await.ok(),
            None => None,
        };
        let vision = profile
            .as_ref()
            .map(|p| p.capabilities.vision)
            .unwrap_or(false);
        let thinking = ui_thinking_state(profile.as_ref(), thinking_level);
        let live = live_views.view(session_id);
        // This push REPLACES the client's view, so it must carry the same
        // durable plan projection `snapshot` would: after a restart the live
        // view is empty even though the log still holds the plan.
        let live_plan = live.plan.clone();
        let plan = match projected_plan(
            &leveler_storage::EngineStores::from_database(&db),
            session_id,
            live_plan.clone(),
        )
        .await
        {
            Ok(plan) => plan,
            Err(error) => {
                tracing::warn!(%error, "durable plan projection failed; keeping the live plan");
                live_plan
            }
        };
        let active_background_tasks: Vec<leveler_client_protocol::UiActiveBackgroundTask> =
            background_snapshots
                .into_iter()
                .filter(|task| task.owner_scope.as_deref() == Some(session_id.as_str()))
                .map(|task| leveler_client_protocol::UiActiveBackgroundTask {
                    task_id: task.id,
                    program: task.program,
                    args: task.args,
                    elapsed_ms: task.duration_ms,
                    pid: task.pid,
                    ready_evidence: task.ready.map(|ready| ready.evidence),
                    lifetime: Some(task.lifetime.as_str().to_string()),
                })
                .collect();
        let facts = match db.session_facts_for(session_id).await {
            Ok(Some(facts)) => facts,
            Ok(None) | Err(_) => return false,
        };
        let probe = crate::runtime_boot::StateDirBootLiveness::new(&app.layout.state_dir);
        let Ok((task_status, task_terminal)) =
            crate::session_projection::project_task(&facts, &probe, false, false)
        else {
            return false;
        };
        let Ok(interrupted_tool_calls) =
            project_interrupted_tool_calls(&db, &facts, &probe, session_id, false).await
        else {
            return false;
        };
        let hosted_terminal = hosted_terminal_facts(app, session_id);
        let owner_lost = hosted_owner_lost_facts(app, session_id);
        let Ok(background_tasks) = project_background_tasks(
            &db,
            &probe,
            session_id,
            &active_background_tasks,
            &hosted_terminal,
            &owner_lost,
        )
        .await
        else {
            return false;
        };
        let foreign_origin = project_foreign_origin(&db, session_id).await;
        let _ = events.send(RuntimeEvent::SessionOpened {
            session: UiSessionSnapshot {
                history: None,
                id: session_id.clone(),
                repository: record.repository,
                task_status: Some(task_status),
                task_terminal,
                goal: record.goal,
                model: model.clone(),
                mode: protocol_mode(mode),
                branch: app.layout.primary_workspace().and_then(detect_branch_label),
                status: session_status_wire(record.status, task_status).into(),
                finalization_stage: live.finalization_stage,
                messages,
                // Compact is admitted like a turn, so no turn is running and
                // no interaction can be pending; checkpoints are about to be
                // dropped by the caller. The LIVE view (plan/diff/report)
                // must ride along though — hardcoding it empty
                // made this snapshot lie relative to `snapshot()`, and a
                // reconnect would resurrect state this push had cleared.
                pending_interactions: vec![],
                available_models,
                vision,
                last_sequence,
                latest_turn_ordinal: facts.turns.last().map(|turn| turn.ordinal),
                active_tools: live.active_tools,
                interrupted_tool_calls,
                active_background_tasks,
                background_tasks,
                plan,
                diff: live.diff,
                checkpoints: Vec::new(),
                recaps: Vec::new(),
                user_shells: Vec::new(),
                completion_report: live.completion_report,
                thinking,
                work_profile: Some(crate::canonical_work_profile(&record.work_profile)),
                collaboration: Some(record.collaboration.clone()),
                // This push is a same-session resync while the live turn keeps
                // running. The client keeps its execution-task state; deriving
                // it here would need a live-turn reading this projection does
                // not have.
                execution_task: None,
                children: Vec::new(),
                foreign_origin,
            },
        });
    }
    // The same observation the automatic compaction publishes, and in this
    // order for a reason: the snapshot above REPLACES the client's view, so an
    // event sent before it would be thrown away. Live clients get it here;
    // clients that rebuild from the durable log read it from the `Compacted`
    // row the cut committed.
    let _ = events.send(RuntimeEvent::ContextCompacted {
        from: compacted_from as u32,
        to: 1,
    });
    // Clear the spinner and confirm.
    let _ = events.send(RuntimeEvent::TurnCompleted);
    notify(NotificationLevel::Info, "✓ 已压缩历史为摘要".to_string());
    true
}

/// Persist a terminal Progress + empty Plan + empty Evidence so the next turn
/// does not inherit post-cut task state (after /clear, /compact, restore).
///
/// Also writes a **new** `ContextSnapshot` so `latest_context_snapshot` no longer
/// returns a pre-cut transcript (empty for clear/restore; summary for compact).
async fn reset_session_task_epoch(
    app: &Application,
    session_id: &SessionId,
) -> Result<(), anyhow::Error> {
    let db = app.open_database().await?;
    reset_session_task_epoch_db(&db, session_id, Vec::new()).await
}

/// DB-level epoch cut (testable without a full Application).
///
/// `model_visible` becomes the sole model-visible snapshot after the cut
/// (summary-only after compact; empty after clear/restore).
/// The four epoch-cut events as `(type, payload)` rows, in the canonical
/// order (snapshot first, so a partial legacy write cannot leave a fresh
/// Progress with a stale ContextSnapshot still "latest").
/// Commit a `/compact` as ONE transaction: the summary replaces the
/// transcript, the epoch events land in order, and the destroyed epoch's
/// derived goal checkpoints are invalidated (HCH-FIX-1 — the old shape could
/// leave a rewritten transcript under old-epoch task state with nothing on
/// restart able to notice).
///
/// The first of those events is `Compacted`, the same durable fact the
/// automatic compaction writes from the agent loop. Without it a manual
/// compaction left no trace anywhere but a notification that fades: the TUI
/// rebuilds its conversation by replaying this log, so the one place the user
/// could learn that history had been collapsed said nothing about it.
async fn commit_compaction_epoch(
    db: &leveler_storage::Database,
    session_id: &SessionId,
    summary: Message,
    compacted_from: usize,
) -> Result<(), anyhow::Error> {
    // Make the manual fold carry the same durable breadcrumb as the automatic
    // one. It is the marker the post-compaction context uses to inject the
    // deterministic facts block, so a `/compact` does not degrade to a summary
    // with no execution facts.
    //
    // The rows this transaction deletes are also the only record that some
    // background task results were already delivered. Rebuilding the origin
    // must carry the identities the caller computed from the history being
    // replaced, or the next admission injects every one of them again.
    let delivered_background_tasks = summary
        .origin
        .as_ref()
        .map(|origin| origin.delivered_background_task_ids().to_vec())
        .unwrap_or_default();
    let summary = leveler_model::Message {
        origin: Some(TranscriptOrigin::CompactionSummary {
            delivered_background_tasks,
        }),
        role: leveler_model::Role::User,
        content: vec![leveler_model::ContentPart::Text {
            text: format!(
                "{} to fit the window: `/compact` folded the earlier history.]\n\n{}",
                leveler_model::COMPACTION_BREADCRUMB_MARKER,
                summary.text_content()
            ),
        }],
        route: None,
    };
    let payload =
        serde_json::to_string(&summary).map_err(|e| anyhow::anyhow!("序列化摘要出错: {e}"))?;
    let mut events = vec![
        leveler_engine::EngineEvent::Compacted {
            from: compacted_from,
            to: 1, // the summary row
        }
        .to_row()
        .map_err(|e| anyhow::anyhow!("序列化压缩事件出错: {e}"))?,
    ];
    events.extend(
        epoch_event_rows(vec![summary], 1 /* the summary row */)
            .map_err(|e| anyhow::anyhow!("序列化任务状态出错: {e}"))?,
    );
    db.cut_context_epoch(session_id, &[payload], &events, leveler_core::now())
        .await?;
    Ok(())
}

fn epoch_event_rows(
    model_visible: Vec<Message>,
    through_ordinal: u64,
) -> Result<Vec<leveler_storage::EpochEventRow>, anyhow::Error> {
    let events = [
        leveler_engine::EngineEvent::ContextSnapshot {
            messages: model_visible,
            through_ordinal: Some(through_ordinal),
        },
        leveler_engine::EngineEvent::ProgressUpdated {
            ledger: leveler_lifecycle::ProgressLedger::new_context_epoch(),
        },
        leveler_engine::EngineEvent::PlanUpdated {
            steps: vec![],
            created: false,
            call_id: None,
        },
        leveler_engine::EngineEvent::EvidenceLedgerUpdated {
            ledger: leveler_lifecycle::EvidenceLedger::default(),
        },
    ];
    events.into_iter().map(|e| Ok(e.to_row()?)).collect()
}

async fn reset_session_task_epoch_db(
    db: &leveler_storage::Database,
    session_id: &SessionId,
    model_visible: Vec<Message>,
) -> Result<(), anyhow::Error> {
    let repo = leveler_storage::EventRepository::new(db);
    let now = leveler_core::now();
    // The epoch-cut snapshot supersedes the WHOLE post-cut transcript, so its
    // watermark is the live message count (all callers mutate messages before
    // resetting: clear/restore truncated them; /compact no longer comes
    // through here — it uses the atomic `cut_context_epoch`).
    let through_ordinal = MessageRepository::new(db).count(session_id).await?;
    for (tag, payload) in epoch_event_rows(model_visible, through_ordinal)? {
        repo.append(session_id, None, &tag, &payload, now).await?;
    }
    Ok(())
}

/// Deserialize a persisted `Message` payload into a `UiMessage`, or `None` for
/// tool/empty messages that have nothing to render.
fn ui_message(payload: &str) -> Option<UiMessage> {
    ui_message_at(payload, None, None)
}

/// `ordinal` is the persisted append position of this payload in the message
/// log — carried on snapshots so a client can interleave durable goal recaps
/// at the transcript position their checkpoint represents (long-goal P3).
/// How many images a set of attachments carries — the only kind a message can
/// show, and what a client needs to name them.
fn image_count(attachments: &[AttachmentRef]) -> usize {
    attachments
        .iter()
        .filter(|a| a.kind == AttachmentKind::Image)
        .count()
}

/// The durable source identity of an imported session, for a client snapshot.
///
/// The origin table is the only authority. A read failure is reported as
/// "not imported" rather than blocking the whole snapshot: losing the badge
/// must not cost the client the session it is opening.
async fn project_foreign_origin(
    db: &leveler_storage::Database,
    session_id: &SessionId,
) -> Option<UiForeignOrigin> {
    let record = match ForeignSessionOriginRepository::new(db)
        .get(session_id.as_str())
        .await
    {
        Ok(record) => record?,
        Err(error) => {
            tracing::warn!(
                session_id = %session_id.as_str(),
                %error,
                "foreign-session origin could not be read"
            );
            return None;
        }
    };
    let Some(tool) = UiForeignTool::parse(&record.tool) else {
        tracing::warn!(
            session_id = %session_id.as_str(),
            tool = %record.tool,
            "foreign-session origin names an unknown tool"
        );
        return None;
    };
    Some(UiForeignOrigin {
        tool,
        native_id: record.native_id,
        source_path: record.source_path,
        source_cwd: record.source_cwd,
        imported_at: record.imported_at,
    })
}

fn ui_message_at(payload: &str, ordinal: Option<u64>, input_id: Option<&str>) -> Option<UiMessage> {
    let message: leveler_model::Message = serde_json::from_str(payload).ok()?;
    // Same rule as history replay: a protocol repair is model context, not a
    // snapshot row. Filtering the sentence would also hide user-authored English.
    if message.is_protocol_repair() {
        return None;
    }
    let role = match message.role {
        Role::User => UiRole::User,
        Role::Assistant => UiRole::Assistant,
        Role::System => UiRole::System,
        Role::Tool => UiRole::Tool,
    };
    let text = message.text_content();
    let images = message
        .content
        .iter()
        .filter(|p| matches!(p, ContentPart::Image { .. }))
        .count();
    // A message that was only a picture has no text, and dropping it here is
    // how a reopened session lost the turn entirely.
    if text.trim().is_empty() && images == 0 {
        return None;
    }
    // Only the runtime's own notices: origin says so for every row written
    // with a notice origin; the header list still covers legacy rows written
    // before origins were recorded.
    let kind = (role == UiRole::User
        && (message
            .origin
            .as_ref()
            .is_some_and(|origin| matches!(origin, TranscriptOrigin::RuntimeNotice { .. }))
            || text.lines().next().is_some_and(|first| {
                leveler_agent::RUNTIME_NOTICE_HEADERS.contains(&first.trim_end())
            })))
    .then_some(leveler_client_protocol::UiMessageKind::RuntimeNotice);
    // A row written from the accepted-input journal keeps the identity the
    // client saw at admission. That is what makes a Resumed row the same row,
    // not a second one with a fresh uuid.
    let kind = match input_id {
        Some(_) if role == UiRole::User => Some(UiMessageKind::DeliveredInput),
        _ => kind,
    };
    Some(UiMessage {
        id: input_id
            .map(MessageId::new)
            .unwrap_or_else(|| MessageId::new(leveler_core::new_uuid_string())),
        role,
        text,
        ordinal,
        kind,
        images,
    })
}

/// The ordinal a new checkpoint records, or `None` to skip creating one.
/// A load failure must NEVER fall back to 0: restoring a 0-ordinal checkpoint
/// truncates the whole transcript.
fn checkpoint_ordinal(loaded: Result<usize, String>) -> Option<usize> {
    loaded.ok()
}

/// Take the cancellation handle of one running child or tool call, if it is
/// still running.
fn take_cancel(
    children: &ChildCancels,
    session_id: &SessionId,
    child_id: &str,
) -> Option<CancellationToken> {
    children
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get_mut(session_id)
        .and_then(|running| running.remove(child_id))
}

#[cfg(test)]
mod side_question_tests {
    use super::*;

    #[test]
    fn side_question_keeps_the_mode_contract_and_is_not_a_runtime_fact() {
        let message = side_question_message(
            "why this design?",
            "【当前运行时状态 · 只读快照】\n主任务: 空闲",
        );
        let text = message.text_content();
        // The contract bounds side effects, not investigation.
        assert!(text.contains("可以使用只读工具"), "{text}");
        assert!(!text.contains("不要调用任何工具"), "{text}");
        assert!(text.contains("不要修改工作区"), "{text}");
        assert!(text.contains("不要推进或改变主任务"), "{text}");
        // The observer projection travels with the question but is not the
        // contract: the message stays CoreContract, not a runtime fact row.
        assert!(text.contains("只读快照"), "{text}");
        assert!(text.contains("why this design?"), "{text}");
        assert_ne!(message.origin, Some(TranscriptOrigin::UserInput));
        let class = leveler_model::classify_transcript_origin(message.origin.as_ref());
        assert_ne!(class.authority, leveler_model::PromptAuthority::RuntimeFact);
        assert_eq!(
            class.authority,
            leveler_model::PromptAuthority::CoreContract
        );
        assert!(!class.authority_mismatch);
        assert_ne!(class.authority, leveler_model::PromptAuthority::UserIntent);
    }
}

#[cfg(test)]
mod runtime_notice_tests {
    use super::*;

    fn persisted(role: Role, text: &str) -> String {
        serde_json::to_string(&leveler_model::Message::text(role, text)).unwrap()
    }

    /// A settlement notice is a user-role message in the transcript, but the
    /// runtime wrote it. The snapshot says so; a user's own message stays
    /// ordinary.
    #[test]
    fn a_runtime_notice_is_marked_and_a_user_message_is_not() {
        let notice = ui_message(&persisted(
            Role::User,
            "## Background sub-agent settled\nEuclid (c1, role=explorer) has finished.",
        ))
        .unwrap();
        assert_eq!(
            notice.kind,
            Some(leveler_client_protocol::UiMessageKind::RuntimeNotice)
        );
        let typed = ui_message(&persisted(
            Role::User,
            "## Background sub-agent settled? what is that",
        ))
        .unwrap();
        let plain = ui_message(&persisted(Role::User, "please fix the parser")).unwrap();
        assert_eq!(plain.kind, None);
        assert_eq!(
            typed.kind, None,
            "a notice header is a whole first line, not a prefix a user can type into"
        );
    }

    /// The goal closeout row uses the user transport role so the model sees it.
    /// The snapshot is what a person sees, so that row is absent. English the
    /// person actually wrote stays a user message, origin or not.
    #[test]
    fn a_protocol_repair_is_absent_from_the_snapshot() {
        let nudge = serde_json::to_string(&leveler_model::Message::user(
            "Goal remains active. Continue working toward the original goal, and resolve it \
             with update_goal(complete|blocked) when the work is finished or cannot proceed.",
            TranscriptOrigin::ProtocolRepair {
                repair: leveler_model::ProtocolRepairKind::GoalUnresolved,
            },
        ))
        .unwrap();
        assert!(
            ui_message(&nudge).is_none(),
            "a protocol repair must not become a snapshot user row"
        );

        let authored = serde_json::to_string(&leveler_model::Message::user_input(
            "Goal remains active. I wrote this myself.",
        ))
        .unwrap();
        let shown = ui_message(&authored).unwrap();
        assert_eq!(shown.role, UiRole::User);
        assert_eq!(shown.kind, None);
        assert!(shown.text.contains("I wrote this myself."));

        let historical = ui_message(&persisted(Role::User, "please continue")).unwrap();
        assert_eq!(historical.role, UiRole::User);
        assert_eq!(historical.text, "please continue");

        let other_repair = serde_json::to_string(&leveler_model::Message::user(
            "Continue from where the answer was cut off.",
            TranscriptOrigin::ProtocolRepair {
                repair: leveler_model::ProtocolRepairKind::LengthContinuation,
            },
        ))
        .unwrap();
        assert!(
            ui_message(&other_repair).is_none(),
            "every protocol repair is harness control, not a user turn"
        );
    }
}

#[cfg(test)]
mod child_cancel_tests {
    use super::*;
    use leveler_agent::SteeringSource;

    /// A started child can be cancelled alone, exactly once; an ended child is
    /// not running, and asking to cancel it finds nothing to stop.
    #[test]
    fn a_running_child_can_be_cancelled_and_an_ended_one_cannot() {
        let children: ChildCancels = Arc::new(Mutex::new(HashMap::new()));
        let session = SessionId::new("s1");
        let steering = SessionSteering {
            session_id: session.clone(),
            queues: Arc::new(Mutex::new(HashMap::new())),
            events: tokio::sync::broadcast::channel(32).0.into(),
            seed: None,
            goal: Arc::new(Mutex::new(GoalSteer::default())),
            children: children.clone(),
            tool_calls: Arc::new(Mutex::new(HashMap::new())),
            tool_yields: Arc::new(Mutex::new(HashMap::new())),
        };
        let running = CancellationToken::new();
        steering.child_started("c1", running.clone());
        steering.child_started("c2", CancellationToken::new());
        steering.child_ended("c2");

        take_cancel(&children, &session, "c1")
            .expect("a running child has a handle")
            .cancel();
        assert!(running.is_cancelled());
        assert!(
            take_cancel(&children, &session, "c1").is_none(),
            "a handle is used once"
        );
        assert!(
            take_cancel(&children, &session, "c2").is_none(),
            "an ended child is not running"
        );
        assert!(take_cancel(&children, &SessionId::new("other"), "c1").is_none());
    }
}

#[cfg(test)]
mod checkpoint_ordinal_tests {
    use super::checkpoint_ordinal;

    #[test]
    fn load_failure_skips_the_checkpoint_instead_of_recording_zero() {
        assert_eq!(
            checkpoint_ordinal(Err("db unavailable".to_string())),
            None,
            "a failed transcript load must skip the checkpoint — a 0-ordinal \
             fallback wipes the conversation on restore"
        );
    }

    #[test]
    fn successful_load_records_the_transcript_length() {
        assert_eq!(checkpoint_ordinal(Ok(7)), Some(7));
        // An actually-empty transcript is a legitimate ordinal 0.
        assert_eq!(checkpoint_ordinal(Ok(0)), Some(0));
    }
}

#[cfg(test)]
mod title_tests {
    use super::title_from_first_message;

    #[test]
    fn first_sentence_of_first_line_wins() {
        assert_eq!(
            title_from_first_message("帮我修复登录超时的 bug。另外看下日志。").as_deref(),
            Some("帮我修复登录超时的 bug")
        );
        assert_eq!(
            title_from_first_message("\n\n  fix the login bug! then logs\n").as_deref(),
            Some("fix the login bug")
        );
    }

    #[test]
    fn dots_in_paths_and_versions_do_not_split() {
        assert_eq!(
            title_from_first_message("升级 v1.2 后 src/main.rs 编译不过").as_deref(),
            Some("升级 v1.2 后 src/main.rs 编译不过")
        );
    }

    #[test]
    fn long_titles_cap_at_40_chars_and_blank_is_none() {
        let long = "这".repeat(80);
        assert_eq!(title_from_first_message(&long).unwrap().chars().count(), 40);
        assert_eq!(title_from_first_message("   \n  "), None);
        assert_eq!(title_from_first_message("？？？"), None);
    }
}

#[cfg(test)]
mod approval_policy_tests {
    use super::{
        Arc, HashMap, ModelRef, Mutex, PermissionProfile, SessionId, SessionRuntimeConfig,
        install_persisted_config,
    };
    #[test]
    fn an_explicit_attach_during_hydration_is_not_overwritten() {
        use std::sync::Barrier;
        let session_id = SessionId::new("concurrent-policy");
        let initial = SessionRuntimeConfig {
            model: Some(ModelRef::new("mock", "m")),
            mode: PermissionProfile::Assisted,
            sandbox: false,
            collaboration: "chat".into(),
            approval_policy: ApprovalPolicy::Interactive,
            thinking: None,
        };
        let configs = Arc::new(Mutex::new(HashMap::from([(session_id.clone(), initial)])));
        let durable_read = Arc::new(Barrier::new(2));
        let attached = Arc::new(Barrier::new(2));
        let worker = {
            let configs = configs.clone();
            let session_id = session_id.clone();
            let durable_read = durable_read.clone();
            let attached = attached.clone();
            std::thread::spawn(move || {
                let mut hydrated = configs.lock().unwrap()[&session_id].clone();
                hydrated.model = Some(ModelRef::new("mock", "selected"));
                durable_read.wait();
                attached.wait();
                install_persisted_config(&configs, &session_id, hydrated)
            })
        };
        durable_read.wait();
        configs
            .lock()
            .unwrap()
            .get_mut(&session_id)
            .unwrap()
            .approval_policy = ApprovalPolicy::AutoApprove;
        attached.wait();
        let hydrated = worker.join().unwrap();
        assert_eq!(hydrated.approval_policy, ApprovalPolicy::AutoApprove);
        let stored = configs.lock().unwrap();
        assert_eq!(
            stored[&session_id].approval_policy,
            ApprovalPolicy::AutoApprove
        );
        assert_eq!(
            stored[&session_id].model,
            Some(ModelRef::new("mock", "selected"))
        );
    }

    use super::should_auto_approve;
    use leveler_client_protocol::ApprovalPolicy;

    #[test]
    fn per_session_auto_approve_needs_no_global_flag() {
        // §6.B — on a plain daemon (global off), an unattended goal session
        // auto-approves while an interactive session on the SAME daemon prompts.
        assert!(should_auto_approve(
            Some(ApprovalPolicy::AutoApprove),
            false
        ));
        assert!(!should_auto_approve(
            Some(ApprovalPolicy::Interactive),
            false
        ));
        // No stored config (e.g. a restored session) prompts unless the global
        // fallback applies.
        assert!(!should_auto_approve(None, false));
    }

    #[test]
    fn global_auto_approve_stays_a_backward_compatible_fallback() {
        // `serve --auto-approve` keeps auto-approving every session regardless of
        // per-session policy.
        assert!(should_auto_approve(Some(ApprovalPolicy::Interactive), true));
        assert!(should_auto_approve(None, true));
        assert!(should_auto_approve(Some(ApprovalPolicy::AutoApprove), true));
    }
}

#[cfg(test)]
mod collab_route_tests {
    use super::collaboration_routes_submit_to_goal;

    #[test]
    fn goal_collaboration_routes_plain_submit_to_goal_profile() {
        assert!(collaboration_routes_submit_to_goal("goal"));
        assert!(collaboration_routes_submit_to_goal("Goal"));
    }

    #[test]
    fn chat_and_plan_stay_on_content_turn() {
        assert!(!collaboration_routes_submit_to_goal("chat"));
        assert!(!collaboration_routes_submit_to_goal("plan"));
        assert!(!collaboration_routes_submit_to_goal(""));
    }
}

#[cfg(test)]
mod resumable_task_tests {
    use super::task_outcome_is_resumable;
    use leveler_lifecycle::{StopReason, TaskOutcome};
    use leveler_model::{ModelError, ModelErrorKind};

    fn failed(kind: ModelErrorKind) -> bool {
        task_outcome_is_resumable(
            &TaskOutcome::Failed,
            None,
            Some(&ModelError::new(kind, "x")),
        )
    }

    #[test]
    fn interruption_and_budget_windows_are_resumable() {
        assert!(task_outcome_is_resumable(
            &TaskOutcome::Interrupted,
            None,
            None
        ));
        assert!(task_outcome_is_resumable(
            &TaskOutcome::BudgetLimited,
            None,
            None
        ));
    }

    #[test]
    fn recoverable_provider_failures_are_resumable() {
        for kind in [
            ModelErrorKind::ProviderUnavailable,
            ModelErrorKind::Transport,
            ModelErrorKind::StreamInterrupted,
            ModelErrorKind::Timeout,
            ModelErrorKind::RateLimit,
        ] {
            assert!(failed(kind), "{kind:?} must be resumable");
        }
    }

    #[test]
    fn non_recoverable_failures_are_not_resumable() {
        for kind in [
            ModelErrorKind::InvalidRequest,
            ModelErrorKind::Auth,
            ModelErrorKind::Decode,
            ModelErrorKind::ConversationProtocol,
            ModelErrorKind::Other,
        ] {
            assert!(!failed(kind), "{kind:?} must not be resumable");
        }
        assert!(
            !task_outcome_is_resumable(&TaskOutcome::Failed, None, None),
            "an untyped failure has no proven resume path"
        );
    }

    #[test]
    fn a_declared_completion_is_terminal_but_an_answer_is_not() {
        // `StopReason::Completed` is the model's own declaration.
        assert!(!task_outcome_is_resumable(
            &TaskOutcome::Completed,
            Some(StopReason::Completed),
            None
        ));
        // A legacy row with no stop carries no declaration either, so it is
        // treated as the old meaning: terminal.
        assert!(!task_outcome_is_resumable(
            &TaskOutcome::Completed,
            None,
            None
        ));
        // The bad-dogfood shape: the session row says `completed`, but the
        // durable stop is `answered` — the model answered a partial summary
        // and the runtime never received a goal terminal. This must be
        // resumable, or the remaining work is unreachable.
        assert!(
            task_outcome_is_resumable(&TaskOutcome::Completed, Some(StopReason::Answered), None),
            "an answered terminal is not a proven completion"
        );
        assert!(
            task_outcome_is_resumable(&TaskOutcome::Blocked, Some(StopReason::Blocked), None),
            "a blocked goal is retryable through its own recorded objective"
        );
    }

    #[test]
    fn an_explicitly_cancelled_task_is_not_resumable() {
        assert!(
            !task_outcome_is_resumable(&TaskOutcome::Cancelled, None, None),
            "an explicit task cancel must not be resumable"
        );
    }
}

#[cfg(test)]
mod context_ops_tests {
    use super::*;
    use leveler_lifecycle::{ProgressLedger, TurnPhase};
    use leveler_model::{Message, Role};
    use leveler_storage::{Database, EventRepository, SessionRecord, SessionRepository};

    #[tokio::test]
    async fn reset_task_epoch_makes_progress_terminal_for_inheritance() {
        let db = Database::connect_in_memory().await.unwrap();
        let session = SessionRecord::new("/r", "g", "m", leveler_core::now());
        SessionRepository::new(&db).create(&session).await.unwrap();
        let id = SessionId::new(session.id.clone());

        // Simulate an open in-flight progress ledger.
        let open = ProgressLedger {
            cumulative_commands: 9,
            phase: TurnPhase::Active,
            ..Default::default()
        };
        let (tag, payload) = leveler_engine::EngineEvent::ProgressUpdated {
            ledger: open.clone(),
        }
        .to_row()
        .unwrap();
        EventRepository::new(&db)
            .append(&id, None, &tag, &payload, leveler_core::now())
            .await
            .unwrap();

        // Pre-cut snapshot must not remain latest after epoch reset.
        let (tag, payload) = leveler_engine::EngineEvent::ContextSnapshot {
            messages: vec![Message::text(Role::User, "old bulky history line")],
            through_ordinal: None,
        }
        .to_row()
        .unwrap();
        EventRepository::new(&db)
            .append(&id, None, &tag, &payload, leveler_core::now())
            .await
            .unwrap();

        reset_session_task_epoch_db(&db, &id, Vec::new())
            .await
            .unwrap();

        // Last ProgressUpdated must be terminal (same gate engine uses for seed).
        let rows = EventRepository::new(&db).load(&id).await.unwrap();
        let mut last_progress = None;
        let mut last_snapshot = None;
        for row in rows {
            match leveler_engine::EngineEvent::from_payload(&row.payload) {
                Ok(leveler_engine::EngineEvent::ProgressUpdated { ledger }) => {
                    last_progress = Some(ledger);
                }
                Ok(leveler_engine::EngineEvent::ContextSnapshot { messages, .. }) => {
                    last_snapshot = Some(messages);
                }
                _ => {}
            }
        }
        let last = last_progress.expect("progress event");
        assert!(
            last.is_terminal_for_inheritance(),
            "clear/compact/restore epoch must not re-seed task state: {last:?}"
        );
        assert_eq!(last.cumulative_commands, 0);
        let snap = last_snapshot.expect("context snapshot");
        assert!(
            snap.is_empty(),
            "epoch cut must supersede pre-cut ContextSnapshot; got {snap:?}"
        );
    }

    #[tokio::test]
    async fn compact_epoch_reset_installs_summary_snapshot() {
        let db = Database::connect_in_memory().await.unwrap();
        let session = SessionRecord::new("/r", "g", "m", leveler_core::now());
        SessionRepository::new(&db).create(&session).await.unwrap();
        let id = SessionId::new(session.id.clone());
        let (tag, payload) = leveler_engine::EngineEvent::ContextSnapshot {
            messages: vec![Message::text(Role::User, "pre-compact long history")],
            through_ordinal: None,
        }
        .to_row()
        .unwrap();
        EventRepository::new(&db)
            .append(&id, None, &tag, &payload, leveler_core::now())
            .await
            .unwrap();
        let summary = Message::text(Role::User, "COMPACTION SUMMARY: done");
        reset_session_task_epoch_db(&db, &id, vec![summary.clone()])
            .await
            .unwrap();
        let rows = EventRepository::new(&db).load(&id).await.unwrap();
        let mut last_snapshot = None;
        for row in rows {
            if let Ok(leveler_engine::EngineEvent::ContextSnapshot { messages, .. }) =
                leveler_engine::EngineEvent::from_payload(&row.payload)
            {
                last_snapshot = Some(messages);
            }
        }
        let snap = last_snapshot.expect("snapshot");
        assert_eq!(snap.len(), 1);
        assert!(
            snap[0].text_content().contains("COMPACTION SUMMARY"),
            "compact must pin summary as model-visible snapshot: {snap:?}"
        );
    }

    /// `/compact` must record the SAME durable fact the automatic compaction
    /// records. It wrote only the epoch-reset rows, so the one observation a
    /// user got was a notification that fades — and the TUI's own replay of
    /// the durable log (triggered by the snapshot /compact pushes) then
    /// rebuilt the conversation with nothing in it saying history had been
    /// collapsed.
    #[tokio::test]
    async fn manual_compact_persists_the_canonical_compaction_fact() {
        let db = Database::connect_in_memory().await.unwrap();
        let session = SessionRecord::new("/r", "g", "m", leveler_core::now());
        SessionRepository::new(&db).create(&session).await.unwrap();
        let id = SessionId::new(session.id.clone());
        let summary = Message::text(Role::User, "对话摘要（已压缩历史）：done");

        commit_compaction_epoch(&db, &id, summary, 12)
            .await
            .unwrap();

        let compacted: Vec<(usize, usize)> = EventRepository::new(&db)
            .load(&id)
            .await
            .unwrap()
            .iter()
            .filter_map(|row| {
                match leveler_engine::EngineEvent::from_payload(&row.payload).ok()? {
                    leveler_engine::EngineEvent::Compacted { from, to } => Some((from, to)),
                    _ => None,
                }
            })
            .collect();
        assert_eq!(
            compacted,
            vec![(12, 1)],
            "one durable compaction fact, counting what it replaced"
        );
        let snapshot = EventRepository::new(&db)
            .load(&id)
            .await
            .unwrap()
            .into_iter()
            .rev()
            .find_map(
                |row| match leveler_engine::EngineEvent::from_payload(&row.payload).ok()? {
                    leveler_engine::EngineEvent::ContextSnapshot { messages, .. } => Some(messages),
                    _ => None,
                },
            )
            .expect("context snapshot");
        assert!(
            snapshot.iter().any(|message| message
                .text_content()
                .contains(leveler_model::COMPACTION_BREADCRUMB_MARKER)),
            "the manual fold must be recognizable as a compaction context: {snapshot:?}"
        );
    }

    /// The `/compact` transaction deletes the rows that recorded a background
    /// task result. Their task identities are the only durable evidence that
    /// the fact already reached this transcript, so the rebuilt breadcrumb must
    /// keep the ones the caller computed. Dropping them makes the next
    /// admission inject the same terminal result again.
    #[tokio::test]
    async fn manual_compact_keeps_delivered_background_facts() {
        let db = Database::connect_in_memory().await.unwrap();
        let session = SessionRecord::new("/r", "g", "m", leveler_core::now());
        SessionRepository::new(&db).create(&session).await.unwrap();
        let id = SessionId::new(session.id.clone());
        let summary = Message::user(
            "对话摘要（已压缩历史）：done",
            TranscriptOrigin::CompactionSummary {
                delivered_background_tasks: vec!["bg-1".to_string()],
            },
        );

        commit_compaction_epoch(&db, &id, summary, 12)
            .await
            .unwrap();

        let rows = MessageRepository::new(&db).load(&id).await.unwrap();
        assert_eq!(rows.len(), 1, "the summary is the whole new transcript");
        let stored: Message = serde_json::from_str(&rows[0]).unwrap();
        assert_eq!(
            stored
                .origin
                .as_ref()
                .map(TranscriptOrigin::delivered_background_task_ids),
            Some(&["bg-1".to_string()][..]),
            "the manual fold must keep the delivered background-task identities: {stored:?}"
        );
    }

    /// The marker has to survive the path the TUI actually reads on a reopen
    /// or a post-compact resync: the durable log projected back into runtime
    /// events. Manual and automatic compaction must arrive as the same event.
    #[tokio::test]
    async fn manual_and_automatic_compaction_replay_the_same_observation() {
        use leveler_client_protocol::RuntimeEvent;

        async fn replayed_compactions(db: &Database, id: &SessionId) -> Vec<(u32, u32)> {
            crate::session_history::load_session_history(db, id)
                .await
                .unwrap()
                .0
                .into_iter()
                .filter_map(|entry| match entry.event {
                    RuntimeEvent::ContextCompacted { from, to } => Some((from, to)),
                    _ => None,
                })
                .collect()
        }

        async fn session_with_a_turn(db: &Database) -> SessionId {
            let session = SessionRecord::new("/r", "g", "m", leveler_core::now());
            SessionRepository::new(db).create(&session).await.unwrap();
            let id = SessionId::new(session.id.clone());
            // The real shape a compaction lands in: a turn that has already
            // settled. The replay bridge publishes one terminal and drops
            // everything after it, which is exactly what buried the marker.
            for event in [
                leveler_engine::EngineEvent::TurnStarted {
                    turn_id: leveler_core::TurnId::generate(),
                    kind: leveler_engine::TurnKind::Chat,
                },
                leveler_engine::EngineEvent::TaskFinished {
                    outcome: leveler_lifecycle::TaskOutcome::Completed,
                    reason: None,
                    failure: None,
                    stop: Some(leveler_lifecycle::StopReason::Answered),
                    warnings: Vec::new(),
                    verification: None,
                },
            ] {
                let (tag, payload) = event.to_row().unwrap();
                EventRepository::new(db)
                    .append(&id, None, &tag, &payload, leveler_core::now())
                    .await
                    .unwrap();
            }
            id
        }

        // Manual: the /compact transaction.
        let db = Database::connect_in_memory().await.unwrap();
        let manual = session_with_a_turn(&db).await;
        commit_compaction_epoch(
            &db,
            &manual,
            Message::text(Role::User, "对话摘要（已压缩历史）：done"),
            12,
        )
        .await
        .unwrap();

        // Automatic: what the agent loop appends on its own.
        let auto = session_with_a_turn(&db).await;
        let (tag, payload) = leveler_engine::EngineEvent::Compacted { from: 12, to: 1 }
            .to_row()
            .unwrap();
        EventRepository::new(&db)
            .append(&auto, None, &tag, &payload, leveler_core::now())
            .await
            .unwrap();

        assert_eq!(
            replayed_compactions(&db, &manual).await,
            vec![(12, 1)],
            "a manual compaction is observable after a replay"
        );
        assert_eq!(
            replayed_compactions(&db, &manual).await,
            replayed_compactions(&db, &auto).await,
            "manual and automatic compaction converge on one observation"
        );

        // An automatic compaction happens INSIDE a running turn, before its
        // terminal. Grouping an epoch cut separately must not split that turn
        // or cost it its terminal.
        let db = Database::connect_in_memory().await.unwrap();
        let session = SessionRecord::new("/r", "g", "m", leveler_core::now());
        SessionRepository::new(&db).create(&session).await.unwrap();
        let mid_turn = SessionId::new(session.id.clone());
        for event in [
            leveler_engine::EngineEvent::TurnStarted {
                turn_id: leveler_core::TurnId::generate(),
                kind: leveler_engine::TurnKind::Chat,
            },
            leveler_engine::EngineEvent::Compacted { from: 30, to: 1 },
            leveler_engine::EngineEvent::TaskFinished {
                outcome: leveler_lifecycle::TaskOutcome::Completed,
                reason: None,
                failure: None,
                stop: Some(leveler_lifecycle::StopReason::Answered),
                warnings: Vec::new(),
                verification: None,
            },
        ] {
            let (tag, payload) = event.to_row().unwrap();
            EventRepository::new(&db)
                .append(&mid_turn, None, &tag, &payload, leveler_core::now())
                .await
                .unwrap();
        }
        let entries = crate::session_history::load_session_history(&db, &mid_turn)
            .await
            .unwrap()
            .0;
        assert_eq!(
            entries
                .iter()
                .filter(|e| matches!(e.event, RuntimeEvent::ContextCompacted { .. }))
                .count(),
            1,
            "the automatic compaction still replays"
        );
        assert_eq!(
            entries.iter().filter(|e| e.turn_start).count(),
            1,
            "and it does not open a turn of its own: {entries:#?}"
        );
        assert_eq!(
            entries
                .iter()
                .filter(|e| matches!(
                    e.event,
                    RuntimeEvent::TurnAnswered
                        | RuntimeEvent::TurnCompleted
                        | RuntimeEvent::TurnCompletedWithWarnings { .. }
                ))
                .count(),
            1,
            "the turn keeps its terminal: {entries:#?}"
        );
    }

    #[test]
    fn compact_parse_rejects_corrupt_row_without_dropping_siblings() {
        let good = serde_json::to_string(&Message::text(Role::User, "ok")).unwrap();
        let payloads = vec![
            good.clone(),
            good.clone(),
            "{not-json".to_string(),
            good.clone(),
        ];
        let err = parse_history_messages_strict(&payloads).unwrap_err();
        assert!(
            err.contains("第 3 条"),
            "must name the corrupt ordinal: {err}"
        );
        // All-or-nothing: no partial Vec is returned on error.
        assert!(parse_history_messages_strict(&[good.clone(), good]).is_ok());
    }
}

#[cfg(test)]
mod in_flight_tests {
    use super::*;

    fn registered(registry: &InFlightCommands, command_id: &CommandId) -> bool {
        registry.lock().unwrap().contains_key(command_id.as_str())
    }

    /// The local ambiguity window opens before the receipt exists: whenever the
    /// receipt write runs, the command is already registered.
    #[tokio::test]
    async fn a_command_is_in_flight_before_its_receipt_is_written() {
        let registry: InFlightCommands = Arc::default();
        let command_id = CommandId::new("cmd-window");
        let (entry, seen_during_write) = InFlightDelivery::admit(&registry, &command_id, async {
            registered(&registry, &command_id)
        })
        .await;
        assert!(
            seen_during_write,
            "registration must precede the receipt write"
        );
        assert!(registered(&registry, &command_id));
        drop(entry);
        assert!(!registered(&registry, &command_id));
    }

    /// One delivery leaving must not unregister another still handling the
    /// same command; a kept registration outlives its delivery.
    #[test]
    fn registrations_are_counted_and_a_kept_one_stays() {
        let registry: InFlightCommands = Arc::default();
        let command_id = CommandId::new("cmd-twice");
        let first = InFlightDelivery::enter(&registry, &command_id);
        let second = InFlightDelivery::enter(&registry, &command_id);
        drop(second);
        assert!(registered(&registry, &command_id));
        first.keep_for_this_boot();
        assert!(registered(&registry, &command_id));
    }
}

#[cfg(test)]
mod terminal_assist_tests {
    use super::*;
    use leveler_test_support::{MockResponse, MockServer};
    use std::time::Duration;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_single_terminal_prediction_survives_the_admission_release_boundary() {
        let server = MockServer::start_one(MockResponse::json_ok(
            r#"{"id":"assist","choices":[{"message":{"role":"assistant","content":"运行测试"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":3,"total_tokens":13}}"#,
        )).await;
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("configs/providers")).unwrap();
        std::fs::create_dir_all(tmp.path().join("configs/models")).unwrap();
        std::fs::write(
            tmp.path().join("configs/providers/mock.yaml"),
            format!(
                "id: mock\nprotocol: openai_chat\nbase_url: {}\n",
                server.base_url()
            ),
        )
        .unwrap();
        std::fs::write(tmp.path().join("configs/models/m.yaml"), r#"
id: m
provider: mock
model_id: mock-model
protocol: openai_chat
capabilities: { streaming: true, tool_calling: true, parallel_tool_calls: false, structured_output: true, reasoning: false, vision: false }
limits: { context_window: 8192, reliable_context: 4096, max_output_tokens: 1024, max_tool_schema_bytes: 8192, max_parallel_tool_calls: 1 }
compatibility: { synthesize_tool_call_ids: true, drop_unsupported_fields: true }
"#).unwrap();
        let layout = leveler_project::Layout::from_parts(
            tmp.path().to_path_buf(),
            tmp.path().join("configs"),
            tmp.path().join("state"),
        );
        let app = Arc::new(Application::assemble(layout).unwrap());
        let model = ModelRef::new("mock", "m");
        let session = app
            .create_session(&model, "prediction boundary")
            .await
            .unwrap();
        let db = app.open_database().await.unwrap();
        let history = [
            Message::text(Role::User, "完成修复后运行测试"),
            Message::text(Role::Assistant, "修复已完成，等待运行测试。"),
        ]
        .iter()
        .map(|message| serde_json::to_string(message).unwrap())
        .collect::<Vec<_>>();
        MessageRepository::new(&db)
            .append(&session, &history, leveler_core::now())
            .await
            .unwrap();
        let client = Arc::new(InProcessRuntimeClient::new(
            app.clone(),
            model,
            PermissionProfile::Assisted,
            false,
        ));
        let active = client.active.clone();
        let lease = active.admit(&session).unwrap();
        let events = client.events_for(&session);
        let mut rx = events.subscribe();
        let (published_tx, published_rx) = oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let producer_active = active.clone();
        let producer_events = events.clone();
        let producer = std::thread::spawn(move || {
            let mut bridge =
                EventBridge::new(producer_events.clone()).with_terminal_publisher(move |event| {
                    producer_active.finish_with(&lease, || {
                        producer_events.send(event).unwrap();
                        published_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                    });
                });
            bridge.forward(leveler_engine::EngineEvent::TaskFinished {
                outcome: leveler_lifecycle::TaskOutcome::Completed,
                reason: None,
                failure: None,
                stop: Some(leveler_agent::StopReason::Completed),
                warnings: Vec::new(),
                verification: None,
            });
        });
        published_rx.await.unwrap();
        assert!(matches!(
            rx.recv().await.unwrap(),
            RuntimeEvent::TurnCompleted
        ));
        let (attempt_tx, attempt_rx) = oneshot::channel();
        let (done_tx, mut done_rx) = oneshot::channel();
        let handle = tokio::runtime::Handle::current();
        let command_client = client.clone();
        let consumer = std::thread::spawn(move || {
            attempt_tx.send(()).unwrap();
            handle
                .block_on(command_client.send(ClientCommand::RequestPromptSuggestion {
                    session_id: session,
                }))
                .unwrap();
            done_tx.send(()).unwrap();
        });
        attempt_rx.await.unwrap();
        let returned_before_release =
            tokio::time::timeout(Duration::from_millis(100), &mut done_rx)
                .await
                .is_ok();
        release_tx.send(()).unwrap();
        producer.join().unwrap();
        consumer.join().unwrap();
        assert!(
            !returned_before_release,
            "the terminal consumer discarded its single request before admission release; provider requests: {}",
            server.request_count()
        );
        let event = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(event, RuntimeEvent::PromptSuggestion { text } if text == "运行测试"));
        assert_eq!(
            server.request_count(),
            1,
            "one command must cause one provider request"
        );
    }
}

/// Start the existing short-lived dispatch command for an explicit run-now.
///
/// The action itself is still claimed and executed by the dispatcher through
/// this runtime; the child only performs one due pass, exactly as an OS wake
/// would. Its output is discarded because a run's observed outcome belongs to
/// the schedule store, never to an unobserved pipe.
fn spawn_schedule_dispatch(app: &Application) -> Result<(), String> {
    crate::schedule_tools::dispatch_trigger(&app.layout)()
}

#[cfg(test)]
mod session_declaration_tests {
    use super::*;
    use leveler_client_protocol::UiTaskDeclaration;
    use leveler_lifecycle::{AgentState, SessionStatus, StopReason, TaskOutcome};

    #[tokio::test]
    async fn session_list_projects_latest_durable_declaration_without_rewriting_lifecycle() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("configs/models")).unwrap();
        std::fs::create_dir_all(tmp.path().join("configs/providers")).unwrap();
        let app = Arc::new(
            Application::assemble(leveler_project::Layout::from_parts(
                tmp.path().to_path_buf(),
                tmp.path().join("configs"),
                tmp.path().join("state"),
            ))
            .unwrap(),
        );
        let db = app.open_database().await.unwrap();
        let row = leveler_storage::SessionRecord::new(
            "/repo",
            "a question",
            "mock/m",
            leveler_core::now(),
        );
        SessionRepository::new(&db).create(&row).await.unwrap();
        let id = SessionId::new(row.id);
        SessionRepository::new(&db)
            .update_status(
                &id,
                SessionStatus::Completed,
                AgentState::Complete,
                leveler_core::now(),
            )
            .await
            .unwrap();
        let client = InProcessRuntimeClient::new(
            app,
            ModelRef::new("mock", "m"),
            PermissionProfile::Assisted,
            false,
        );
        let summaries = client.list_sessions().await.unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(
            summaries[0].declaration, None,
            "legacy completion must remain unknown"
        );
        for (stop, declaration) in [
            (
                Some(StopReason::Answered),
                Some(UiTaskDeclaration::Answered),
            ),
            (
                Some(StopReason::Completed),
                Some(UiTaskDeclaration::Completed),
            ),
            (Some(StopReason::BudgetExhausted), None),
            (None, None),
        ] {
            let event = leveler_engine::EngineEvent::TaskFinished {
                outcome: TaskOutcome::Completed,
                reason: None,
                stop,
                failure: None,
                warnings: Vec::new(),
                verification: None,
            };
            let (tag, payload) = event.to_row().unwrap();
            leveler_storage::EventRepository::new(&db)
                .append(&id, None, &tag, &payload, leveler_core::now())
                .await
                .unwrap();
            let summaries = client.list_sessions().await.unwrap();
            assert_eq!(
                summaries[0].declaration, declaration,
                "latest stop is the only declaration source"
            );
            assert_eq!(
                summaries[0].status, "completed",
                "projection must not rewrite session lifecycle"
            );
        }
    }
}

#[cfg(test)]
mod recovered_continuation_tests {
    use super::*;
    use leveler_lifecycle::TaskOutcome;

    async fn fixture() -> (
        tempfile::TempDir,
        InProcessRuntimeClient,
        SessionId,
        leveler_storage::Database,
    ) {
        let temporary = tempfile::tempdir().unwrap();
        let app = Arc::new(
            Application::assemble(leveler_project::Layout::from_parts(
                temporary.path().to_path_buf(),
                temporary.path().join("configs"),
                temporary.path().join("state"),
            ))
            .unwrap(),
        );
        let db = app.open_database().await.unwrap();
        let row = leveler_storage::SessionRecord::new(
            "/repo",
            "original objective",
            "mock/m",
            leveler_core::now(),
        );
        SessionRepository::new(&db).create(&row).await.unwrap();
        let session = SessionId::new(row.id);
        leveler_storage::TaskStore::ensure_for_session(&db, &session, leveler_core::now())
            .await
            .unwrap();
        let client = InProcessRuntimeClient::new(
            app,
            ModelRef::new("mock", "m"),
            PermissionProfile::Assisted,
            false,
        );
        (temporary, client, session, db)
    }

    #[tokio::test]
    async fn a_genuinely_later_interrupted_input_can_supersede_old_closed_terminal() {
        let (_temporary, client, session, db) = fixture().await;
        for outcome in [TaskOutcome::Cancelled, TaskOutcome::Completed] {
            let first = leveler_storage::TurnRepository::new(&db)
                .start(&session, "chat", None, leveler_core::now())
                .await
                .unwrap();
            let event = leveler_engine::EngineEvent::TaskFinished {
                outcome,
                reason: None,
                stop: None,
                failure: None,
                warnings: Vec::new(),
                verification: None,
            };
            let (kind, payload) = event.to_row().unwrap();
            leveler_storage::EventRepository::new(&db)
                .append(
                    &session,
                    Some(&leveler_core::TurnId::new(first.id)),
                    &kind,
                    &payload,
                    leveler_core::now(),
                )
                .await
                .unwrap();
            let message = Message::user_input("a distinct admitted objective");
            let payload = serde_json::json!({"version":1,"initiating_message":message}).to_string();
            let later = leveler_storage::TurnRepository::new(&db)
                .start(&session, "chat", Some(&payload), leveler_core::now())
                .await
                .unwrap();
            leveler_storage::TurnRepository::new(&db)
                .finish(
                    &leveler_core::TurnId::new(later.id),
                    "interrupted",
                    leveler_core::now(),
                )
                .await
                .unwrap();
            MessageRepository::new(&db)
                .append(
                    &session,
                    &[serde_json::to_string(&message).unwrap()],
                    leveler_core::now(),
                )
                .await
                .unwrap();
            assert_eq!(
                client.continuation_state(&session).await.unwrap(),
                ContinuationState::Resumable
            );
        }
    }

    #[tokio::test]
    async fn corrupt_latest_recovered_input_is_an_error_not_an_older_goal_fallback() {
        let (_temporary, client, session, db) = fixture().await;
        for payload in [
            None,
            Some(r#"{"version":1,"initiating_message":{"role":"assistant","content":[]}}"#),
            Some(r#"{"version":99}"#),
        ] {
            let turn = leveler_storage::TurnRepository::new(&db)
                .start(&session, "chat", payload, leveler_core::now())
                .await
                .unwrap();
            leveler_storage::TurnRepository::new(&db)
                .finish(
                    &leveler_core::TurnId::new(turn.id),
                    "interrupted",
                    leveler_core::now(),
                )
                .await
                .unwrap();
            assert!(
                client.continuation_state(&session).await.is_err(),
                "corrupt latest input must fail explicitly: {payload:?}"
            );
        }
    }

    #[tokio::test]
    async fn late_finish_of_closed_turn_cannot_supersede_cancelled_or_completed() {
        let (_temporary, client, session, db) = fixture().await;
        let turn = leveler_storage::TurnRepository::new(&db).start(&session, "chat", Some(r#"{"version":1,"initiating_message":{"role":"user","content":[{"type":"text","text":"original objective"}]}}"#), leveler_core::now()).await.unwrap();
        let turn_id = leveler_core::TurnId::new(turn.id);
        leveler_storage::TurnRepository::new(&db)
            .finish(&turn_id, "interrupted", leveler_core::now())
            .await
            .unwrap();
        for (outcome, expected) in [
            (TaskOutcome::Cancelled, ContinuationState::Cancelled),
            (TaskOutcome::Completed, ContinuationState::NotResumable),
        ] {
            let event = leveler_engine::EngineEvent::TaskFinished {
                outcome,
                reason: None,
                stop: None,
                failure: None,
                warnings: Vec::new(),
                verification: None,
            };
            let (kind, payload) = event.to_row().unwrap();
            leveler_storage::EventRepository::new(&db)
                .append(
                    &session,
                    Some(&turn_id),
                    &kind,
                    &payload,
                    leveler_core::now(),
                )
                .await
                .unwrap();
            let late = leveler_engine::EngineEvent::TurnFinished {
                stop: None,
                turn_id: turn_id.clone(),
                outcome: leveler_lifecycle::TurnOutcome::Interrupted,
                stop_reason: "unclean process exit".into(),
                model_steps: 0,
                modified_files: Vec::new(),
                usage: None,
            };
            let (kind, payload) = late.to_row().unwrap();
            leveler_storage::EventRepository::new(&db)
                .append(
                    &session,
                    Some(&turn_id),
                    &kind,
                    &payload,
                    leveler_core::now(),
                )
                .await
                .unwrap();
            assert_eq!(client.continuation_state(&session).await.unwrap(), expected);
        }
    }
}

#[cfg(test)]
mod session_stream_tests {
    use super::*;

    fn runtime(root: &Path) -> InProcessRuntimeClient {
        let app = Application::assemble(leveler_project::Layout::from_parts(
            root.to_path_buf(),
            root.join("configs"),
            root.join("state"),
        ))
        .unwrap();
        InProcessRuntimeClient::new(
            Arc::new(app),
            ModelRef::new("unused", "m"),
            PermissionProfile::Assisted,
            false,
        )
    }

    #[tokio::test]
    async fn running_snapshot_carries_existing_bounded_execution_history() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = runtime(tmp.path());
        let db = runtime.app.open_database().await.unwrap();
        let record = leveler_storage::SessionRecord::new(
            runtime
                .app
                .layout
                .primary_workspace()
                .unwrap()
                .to_string_lossy()
                .as_ref(),
            "resume",
            "unused/m",
            leveler_core::now(),
        );
        SessionRepository::new(&db).create(&record).await.unwrap();
        let session = SessionId::new(record.id);
        let payload =
            serde_json::json!({"version":1,"initiating_message":Message::user_input("resume")})
                .to_string();
        let turn = leveler_storage::TurnRepository::new(&db)
            .start(&session, "chat", Some(&payload), leveler_core::now())
            .await
            .unwrap();
        let turn_id = leveler_core::TurnId::new(turn.id);
        let mut bridge = EventBridge::new(runtime.events_for(&session));
        for event in [
            leveler_engine::EngineEvent::TurnStarted {
                turn_id: turn_id.clone(),
                kind: leveler_engine::TurnKind::Chat,
            },
            leveler_engine::EngineEvent::ToolCallStarted {
                call_id: "a".into(),
                name: "run_command".into(),
                arguments: "{}".into(),
                parallel: false,
                risk: None,
                agent_id: None,
                model_step: Some(1),
            },
            leveler_engine::EngineEvent::ToolExecutionTiming {
                clock_id: "turn".into(),
                call_id: "a".into(),
                agent_id: None,
                started_ms: 5000,
                elapsed_ms: 9000,
                finished: true,
            },
            leveler_engine::EngineEvent::ToolCallFinished {
                call_id: "a".into(),
                name: "run_command".into(),
                is_error: true,
                preview: "failed".into(),
                agent_id: None,
                applied_diff: None,
                exit_code: Some(1),
                stop: None,
            },
            leveler_engine::EngineEvent::ToolCallStarted {
                call_id: "b".into(),
                name: "run_command".into(),
                arguments: "{}".into(),
                parallel: false,
                risk: None,
                agent_id: None,
                model_step: Some(1),
            },
        ] {
            let (kind, payload) = event.to_row().unwrap();
            leveler_storage::EventRepository::new(&db)
                .append(
                    &session,
                    Some(&turn_id),
                    &kind,
                    &payload,
                    leveler_core::now(),
                )
                .await
                .unwrap();
            bridge.forward(event);
        }
        let lease = runtime.active.admit(&session).unwrap();
        let snapshot = runtime.snapshot(&session).await.unwrap();
        let history = snapshot.history.unwrap();
        assert!(history.entries.iter().any(|entry| matches!(&entry.event,
            RuntimeEvent::ToolCallCompleted {id,ok:false,..} if id.as_str()=="a")));
        assert!(history.entries.iter().any(|entry| matches!(&entry.event,
            RuntimeEvent::ToolTimingUpdated {timing,..} if timing.elapsed_ms==9000 && timing.finished)));
        assert_eq!(snapshot.active_tools.len(), 1);
        assert_eq!(snapshot.active_tools[0].id.as_str(), "b");
        runtime.active.finish(&lease);
        assert!(runtime.snapshot(&session).await.unwrap().history.is_none());
    }

    #[tokio::test]
    async fn live_view_is_current_before_broadcast_consumers_run() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = runtime(tmp.path());
        let id = SessionId::new("stream");
        let sender = runtime.events_for(&id);
        let _receiver = sender.subscribe();
        sender
            .send(RuntimeEvent::ToolCallStarted {
                id: leveler_core::ToolCallId::new("tool"),
                name: "run_command".into(),
                arguments: "{}".into(),
                parallel: false,
                model_step: None,
                answer_effect: None,
            })
            .unwrap();
        assert_eq!(runtime.live_views.view(&id).active_tools.len(), 1);
        sender.send(RuntimeEvent::TurnCancelled).unwrap();
        for _ in 0..2048 {
            sender
                .send(RuntimeEvent::Notification {
                    level: NotificationLevel::Info,
                    message: "overflow".into(),
                })
                .unwrap();
        }
        assert!(runtime.live_views.view(&id).active_tools.is_empty());
    }

    #[tokio::test]
    async fn idle_unwatched_session_streams_stay_bounded() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = runtime(tmp.path());
        let mut watched = None;
        for index in 0..(SESSION_STREAM_CACHE_CAPACITY + 10) {
            let id = SessionId::new(format!("bounded-{index}"));
            let sender = runtime.events_for(&id);
            if index == 0 {
                watched = Some((id.clone(), sender.subscribe()));
            }
        }
        assert_eq!(
            runtime.session_events.streams.lock().unwrap().len(),
            SESSION_STREAM_CACHE_CAPACITY + 10,
            "a fresh stream is never evicted inside the grace period"
        );
        let now = std::time::Instant::now()
            + SESSION_STREAM_IDLE_GRACE
            + std::time::Duration::from_secs(1);
        runtime.evict_idle_session_streams(now);
        let streams = runtime.session_events.streams.lock().unwrap();
        assert_eq!(streams.len(), SESSION_STREAM_CACHE_CAPACITY);
        let (watched_id, mut receiver) = watched.unwrap();
        assert!(
            streams.contains_key(&watched_id),
            "a subscribed stream is not evicted"
        );
        assert!(
            !streams.contains_key(&SessionId::new("bounded-1")),
            "the oldest unwatched stream is reclaimed first"
        );
        drop(streams);
        let sender = runtime.events_for(&watched_id);
        sender
            .send(RuntimeEvent::Notification {
                level: NotificationLevel::Info,
                message: "still live".into(),
            })
            .unwrap();
        assert!(receiver.try_recv().is_ok());
    }

    #[tokio::test]
    async fn an_active_turn_keeps_its_session_stream() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = runtime(tmp.path());
        for index in 0..(SESSION_STREAM_CACHE_CAPACITY + 4) {
            runtime.events_for(&SessionId::new(format!("busy-{index}")));
        }
        let busy = SessionId::new("busy-0");
        let lease = runtime.active.admit(&busy).unwrap();
        let now = std::time::Instant::now()
            + SESSION_STREAM_IDLE_GRACE
            + std::time::Duration::from_secs(1);
        runtime.evict_idle_session_streams(now);
        assert!(
            runtime
                .session_events
                .streams
                .lock()
                .unwrap()
                .contains_key(&busy),
            "a running turn keeps its events"
        );
        runtime.active.finish(&lease);
    }

    #[tokio::test]
    async fn an_evicted_session_stream_is_recreated_on_the_next_publish() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = runtime(tmp.path());
        for index in 0..(SESSION_STREAM_CACHE_CAPACITY + 2) {
            runtime.events_for(&SessionId::new(format!("recreate-{index}")));
        }
        let now = std::time::Instant::now()
            + SESSION_STREAM_IDLE_GRACE
            + std::time::Duration::from_secs(1);
        runtime.evict_idle_session_streams(now);
        let id = SessionId::new("recreate-0");
        assert!(
            !runtime
                .session_events
                .streams
                .lock()
                .unwrap()
                .contains_key(&id)
        );
        let sender = runtime.events_for(&id);
        assert!(
            runtime
                .session_events
                .streams
                .lock()
                .unwrap()
                .contains_key(&id)
        );
        assert!(sender.subscribe().try_recv().is_err());
    }

    #[tokio::test]
    async fn dropping_runtime_closes_session_streams() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = runtime(tmp.path());
        let mut receiver = runtime.subscribe_session(&SessionId::new("stream"));
        drop(runtime);
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_millis(100), receiver.recv()).await,
            Ok(Err(broadcast::error::RecvError::Closed))
        ));
    }

    #[tokio::test]
    async fn deleting_session_closes_stream_and_clears_projection() {
        delete_session(false).await;
        delete_session(true).await;
    }

    async fn delete_session(scoped: bool) {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = runtime(tmp.path());
        let db = runtime.app.open_database().await.unwrap();
        let record =
            leveler_storage::SessionRecord::new("/repo", "delete", "unused/m", leveler_core::now());
        SessionRepository::new(&db).create(&record).await.unwrap();
        let id = SessionId::new(record.id);
        runtime.app.memory().record_activity(
            id.as_str(),
            crate::memory::MemoryActivity {
                kind: crate::memory::MemoryActivityKind::Capturing,
                active: true,
                last_captured_range: Some((1, 2)),
                ..Default::default()
            },
        );
        let cancel = CancellationToken::new();
        runtime.btw.lock().unwrap().insert(
            id.clone(),
            BtwSession {
                cancel: Some(cancel.clone()),
                ..Default::default()
            },
        );
        let shell_cancel = CancellationToken::new();
        runtime.user_shells.begin(
            &id,
            leveler_core::UserShellId::new("shell"),
            "echo test".into(),
            "/repo".into(),
            None,
            shell_cancel.clone(),
            CancellationToken::new(),
        );
        let sender = runtime.events_for(&id);
        let mut receiver = sender.subscribe();
        sender
            .send(RuntimeEvent::ToolCallStarted {
                id: leveler_core::ToolCallId::new("tool"),
                name: "run_command".into(),
                arguments: "{}".into(),
                parallel: false,
                model_step: None,
                answer_effect: None,
            })
            .unwrap();
        receiver.recv().await.unwrap();
        runtime
            .send(if scoped {
                ClientCommand::DeleteSessionFor {
                    requester_session_id: id.clone(),
                    session_id: id.clone(),
                }
            } else {
                ClientCommand::DeleteSession {
                    session_id: id.clone(),
                }
            })
            .await
            .unwrap();
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_millis(100), receiver.recv()).await,
            Ok(Err(broadcast::error::RecvError::Closed))
        ));
        assert!(runtime.live_views.view(&id).active_tools.is_empty());
        assert!(
            !runtime
                .session_events
                .streams
                .lock()
                .unwrap()
                .contains_key(&id)
        );
        assert!(runtime.user_shells.snapshot(&id).is_empty());
        assert!(!runtime.btw.lock().unwrap().contains_key(&id));
        assert!(cancel.is_cancelled());
        assert!(shell_cancel.is_cancelled());
        assert_eq!(
            runtime.app.memory().activity(id.as_str()),
            crate::memory::MemoryActivity::default(),
            "a deleted session must not leave a cached memory index or activity"
        );
        assert!(
            sender.send(RuntimeEvent::TurnCancelled).is_err(),
            "stale producers must stay closed"
        );
    }
}
