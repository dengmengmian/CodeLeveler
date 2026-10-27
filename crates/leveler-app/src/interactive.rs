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

use leveler_core::{CheckpointId, CommandId, SessionId};
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
use leveler_storage::{MessageRepository, SessionRepository};

use leveler_client_protocol::{
    ApprovalDecision as UiApprovalDecision, ApprovalPolicy, AttachmentId, AttachmentKind,
    AttachmentRef, ClientCommand, ClientError, CommandEnvelope, InteractiveRuntimeClient,
    MessageId, NotificationLevel, RuntimeEvent, UiCheckpoint, UiMessage, UiPlan, UiPlanState,
    UiRole, UiSessionSnapshot, UiSessionSummary, UiThinkingAccess, UiThinkingState,
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
        Err(error) => ClientError::Runtime(error.to_string()),
        Ok(_) => ClientError::Runtime("turn ended before durable admission".to_string()),
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

/// The user's own listing actor. These commands arrive through an in-process
/// interaction surface, so the trusted invocation is minted here, once.
fn user_listing_actor(query_id: Option<&leveler_core::CommandId>) -> leveler_memory::v2::Actor {
    leveler_memory::v2::Actor::User {
        invocation: leveler_memory::v2::UserInvocationId::mint(format!(
            "ui:memory-list:{}",
            query_id
                .map(leveler_core::CommandId::as_str)
                .unwrap_or("open")
        )),
    }
}

/// A bounded label for a write outcome, for a one-line status message.
fn outcome_title(outcome: &leveler_memory::WriteOutcome) -> String {
    match outcome {
        leveler_memory::WriteOutcome::Applied { id, title, .. } if title.is_empty() => id.clone(),
        leveler_memory::WriteOutcome::Applied { title, .. } => title.clone(),
        leveler_memory::WriteOutcome::Pending { title, .. } => title.clone(),
        leveler_memory::WriteOutcome::Forgotten { title, .. } => title.clone(),
        leveler_memory::WriteOutcome::Skipped { reason, .. }
        | leveler_memory::WriteOutcome::Rejected { reason }
        | leveler_memory::WriteOutcome::NotStored { reason } => reason.clone(),
        leveler_memory::WriteOutcome::Conflict { target, .. } => target.clone(),
    }
}

/// The status line for a trusted direct write.
///
/// Only `Applied` changed current memory. Pending, skipped, rejected,
/// conflicting and unstored writes did not, so they must not be reported as
/// saved. The write itself already happened through the service; this only
/// tells the person what it did.
fn direct_write_notification(
    outcome: &leveler_memory::WriteOutcome,
    title: &str,
    busy: bool,
) -> (leveler_client_protocol::NotificationLevel, String) {
    use leveler_client_protocol::NotificationLevel;
    use leveler_memory::WriteOutcome;
    match outcome {
        WriteOutcome::Applied { id, .. } => (
            NotificationLevel::Info,
            if busy {
                // This turn's context was assembled already, so say when the
                // memory actually starts being recalled.
                format!("已保存记忆 [{id}]：{title}。将从下一次用户回合开始参与自动召回。")
            } else {
                format!("已保存记忆 [{id}]：{title}")
            },
        ),
        WriteOutcome::Pending { id, reason, .. } => (
            NotificationLevel::Info,
            format!("记忆待确认 [{id}]：{title}；确认后才会生效（{reason}）"),
        ),
        WriteOutcome::Skipped { reason, .. }
        | WriteOutcome::Rejected { reason }
        | WriteOutcome::NotStored { reason } => (
            NotificationLevel::Warning,
            format!("未保存记忆：{title}（{reason}）"),
        ),
        WriteOutcome::Conflict { target, .. } => (
            NotificationLevel::Warning,
            format!("未保存记忆：{title} 与 [{target}] 的当前版本冲突，请刷新后重试"),
        ),
        WriteOutcome::Forgotten { id, .. } => (
            NotificationLevel::Info,
            format!("已删除记忆 [{id}]：{title}"),
        ),
    }
}

fn ui_memory_kind(
    view: &leveler_memory::MemoryView,
) -> Option<leveler_client_protocol::UiMemoryKind> {
    use leveler_client_protocol::UiMemoryKind;
    match view.kind.as_str() {
        "preference" => Some(UiMemoryKind::Preference),
        "decision" => Some(UiMemoryKind::Decision),
        "note" | "user_fact" => Some(UiMemoryKind::Note),
        "experience" => Some(UiMemoryKind::Experience),
        // Legacy and derived entries have no product kind to claim.
        _ => None,
    }
}

/// The wire scope for a domain scope. The two enums stay separate because the
/// wire must not gain lifecycle semantics the domain owns.
fn ui_memory_scope(scope: leveler_memory::ServiceScope) -> leveler_client_protocol::UiMemoryScope {
    match scope {
        leveler_memory::ServiceScope::Global => leveler_client_protocol::UiMemoryScope::Global,
        leveler_memory::ServiceScope::Project => leveler_client_protocol::UiMemoryScope::Project,
        leveler_memory::ServiceScope::Local => leveler_client_protocol::UiMemoryScope::Local,
    }
}

fn service_memory_scope(
    scope: leveler_client_protocol::UiMemoryScope,
) -> leveler_memory::ServiceScope {
    match scope {
        leveler_client_protocol::UiMemoryScope::Global => leveler_memory::ServiceScope::Global,
        leveler_client_protocol::UiMemoryScope::Project => leveler_memory::ServiceScope::Project,
        leveler_client_protocol::UiMemoryScope::Local => leveler_memory::ServiceScope::Local,
    }
}

/// One listing row. A credential-shaped record keeps its row so the user can
/// find and archive it, but never its text.
fn memory_row_from_view(
    view: &leveler_memory::MemoryView,
    scope: leveler_memory::ServiceScope,
) -> leveler_client_protocol::UiMemoryEntry {
    leveler_client_protocol::UiMemoryEntry {
        id: view.id.clone(),
        scope: ui_memory_scope(scope),
        title: if view.sensitive {
            leveler_memory::REDACTED_TITLE.to_string()
        } else {
            view.title.clone()
        },
        body: if view.sensitive {
            leveler_memory::REDACTED_BODY.to_string()
        } else {
            view.body.clone()
        },
        kind: ui_memory_kind(view),
        sensitive: view.sensitive,
        status: Some(view.status.clone()),
        authority: Some(view.authority.clone()),
        revision: view.revision,
        updated_at: view.updated_at.clone(),
        pinned: view.pinned_by_user,
    }
}

/// Pending candidates include the body, kind and source needed for informed
/// consent. Preserve service errors rather than presenting them as no
/// candidates.
fn pending_entries(
    service: &leveler_memory::MemoryService,
    actor: &leveler_memory::v2::Actor,
    scope: leveler_memory::ServiceScope,
) -> Result<Vec<leveler_client_protocol::UiMemoryCandidate>, leveler_memory::ServiceError> {
    Ok(service
        .pending(actor, scope)?
        .into_iter()
        .map(|view| leveler_client_protocol::UiMemoryCandidate {
            id: view.id,
            scope: ui_memory_scope(scope),
            title: if view.sensitive {
                leveler_memory::REDACTED_TITLE.to_string()
            } else {
                view.title
            },
            body: if view.sensitive {
                leveler_memory::REDACTED_BODY.to_string()
            } else {
                view.body
            },
            kind: view.kind,
            source: view.authority,
        })
        .collect())
}

fn project_memory_consolidation_event(
    event: crate::memory_consolidator::MemoryConsolidationEvent,
) -> RuntimeEvent {
    if event.operation == "proposed" {
        RuntimeEvent::Notification {
            level: leveler_client_protocol::NotificationLevel::Info,
            message: if event.count == 1 {
                "我发现一条可能值得记住的内容，先放在待确认里了。用 /memory review 查看。"
                    .to_string()
            } else {
                format!(
                    "我发现 {} 条可能值得记住的内容，先放在待确认里了。用 /memory review 查看。",
                    event.count
                )
            },
        }
    } else {
        RuntimeEvent::MemoryChanged {
            operation: event.operation,
            id: event.id,
            title: event.title,
            authority: Some(event.authority),
        }
    }
}

#[cfg(test)]
#[test]
fn proposed_memory_projects_to_truthful_pending_notification() {
    let event = crate::memory_consolidator::MemoryConsolidationEvent {
        session_id: "session".to_string(),
        operation: "proposed".to_string(),
        id: "cand-output".to_string(),
        title: "输出保持紧凑".to_string(),
        authority: "explicit_user".to_string(),
        count: 1,
    };
    match project_memory_consolidation_event(event) {
        RuntimeEvent::Notification { message, .. } => {
            assert_eq!(
                message,
                "我发现一条可能值得记住的内容，先放在待确认里了。用 /memory review 查看。"
            );
        }
        other => panic!("pending candidate became a durable change: {other:?}"),
    }
}

/// A complete listing is evidence only when every requested read succeeds.
/// The configured backend decides which store is read: the same one writes go
/// to, never a second database.
fn send_memory_list(
    events: &tokio::sync::broadcast::Sender<RuntimeEvent>,
    service: &leveler_memory::MemoryService,
    lanes: leveler_memory::MemoryLanes,
    memory_dir: &std::path::Path,
    include_archived: bool,
    query_id: Option<leveler_core::CommandId>,
) {
    let actor = user_listing_actor(query_id.as_ref());
    let listing = (|| -> Result<RuntimeEvent, leveler_memory::ServiceError> {
        let mut active = Vec::new();
        let mut archived = Vec::new();
        let mut pending = Vec::new();
        // Every configured scope is a separate store. A failure in any one of
        // them fails the whole listing: a partial list would look complete
        // while a pending record stayed invisible.
        for scope in service.configured_scopes() {
            active.extend(
                service
                    .listing_for_user(scope, lanes)?
                    .iter()
                    .map(|view| memory_row_from_view(view, scope)),
            );
            if include_archived {
                archived.extend(
                    service
                        .archived(&actor, scope, lanes, None)?
                        .iter()
                        .map(|view| memory_row_from_view(view, scope)),
                );
            }
            pending.extend(pending_entries(service, &actor, scope)?);
        }
        Ok(RuntimeEvent::MemoryList {
            query_id,
            memory_dir: memory_dir.display().to_string(),
            active,
            archived,
            pending,
        })
    })();
    let event = match listing {
        Ok(event) => event,
        Err(error) => RuntimeEvent::Notification {
            level: leveler_client_protocol::NotificationLevel::Warning,
            message: format!("memory list failed: {error}"),
        },
    };
    let _ = events.send(event);
}

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
    events: broadcast::Sender<RuntimeEvent>,
    kind: AssistKind,
) {
    if active.is_running(&session_id) {
        return;
    }
    let (generation, cancellation) = assists.begin(&session_id, kind);
    tokio::spawn(async move {
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
            AssistKind::AwaySummary => RuntimeEvent::AwaySummary {
                text: generated.text,
            },
        };
        let _ = events.send(event);
    });
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

fn emit_project_rules(events: &broadcast::Sender<RuntimeEvent>, repo: Option<&Path>) {
    let Some(repo) = repo else {
        return;
    };
    let sources = leveler_context::load_rules(repo)
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
    use leveler_lifecycle::{StopReason, TaskOutcome};
    match outcome {
        TaskOutcome::Interrupted | TaskOutcome::BudgetLimited => true,
        TaskOutcome::Failed => failure.is_some_and(|error| {
            matches!(
                error.kind,
                leveler_model::ModelErrorKind::ProviderUnavailable
                    | leveler_model::ModelErrorKind::Transport
                    | leveler_model::ModelErrorKind::StreamInterrupted
                    | leveler_model::ModelErrorKind::Timeout
                    | leveler_model::ModelErrorKind::RateLimit
            )
        }),
        // An answer is not a completion declaration. Every other Completed
        // terminal (a declared `completed` stop, or a legacy row with no stop)
        // is one.
        TaskOutcome::Completed => stop == Some(StopReason::Answered),
        // A blocked goal still owes its work. `继续` retries that same goal;
        // the alternative — treating it as an ordinary goal-mode message —
        // opens a second goal whose objective is the word the user typed,
        // which loses the original intent on the exact path they chose to
        // resume it.
        TaskOutcome::Blocked => true,
        TaskOutcome::Cancelled => false,
    }
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

/// An in-process runtime client backed by an [`Application`].
pub struct InProcessRuntimeClient {
    app: Arc<Application>,
    /// Defaults for sessions created by this runtime service.
    default_runtime: SessionRuntimeConfig,
    /// Model/mode/path selected independently by each live or restored session.
    session_runtime: Mutex<HashMap<SessionId, SessionRuntimeConfig>>,
    // Serialize committed settings with their live permission projection.
    settings_edits: tokio::sync::Mutex<()>,
    /// When true, skip the approval overlay (AutoApprove) so unattended TUI
    /// PTY drivers and CI dogfood can run interactive turns.
    auto_approve: bool,
    /// Root of the content-addressed image store (`<state_dir>/media`, under the
    /// global home — not the project).
    media_root: PathBuf,
    /// Compatibility stream containing events from every session.
    events: broadcast::Sender<RuntimeEvent>,
    /// Session-scoped streams used by daemon/socket clients.
    session_events: Arc<Mutex<HashMap<SessionId, broadcast::Sender<RuntimeEvent>>>>,
    pending: PendingApprovals,
    pending_clarify: PendingClarifications,
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
    retiring_reason: std::sync::Mutex<Option<leveler_client_protocol::RestartReason>>,
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
    /// Text the user sent while a turn was already running, per session.
    /// Drained by the agent loop at the top of each round.
    steering: Arc<Mutex<HashMap<SessionId, Vec<String>>>>,
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
    /// Optional prompt/recap generation, cancelled before a real turn starts.
    auxiliary_assists: Arc<AuxiliaryAssists>,
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
    queues: Arc<Mutex<HashMap<SessionId, Vec<String>>>>,
    goal: Arc<Mutex<GoalSteer>>,
    children: ChildCancels,
    tool_calls: ChildCancels,
}

impl leveler_agent::SteeringSource for SessionSteering {
    fn take_pending(&self) -> Vec<String> {
        self.queues
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(&self.session_id)
            .map(std::mem::take)
            .unwrap_or_default()
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

    fn tool_call_started(&self, id: &str, cancel: CancellationToken) {
        self.tool_calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(self.session_id.clone())
            .or_default()
            .insert(id.to_string(), cancel);
    }

    fn tool_call_ended(&self, id: &str) {
        release_cancel(&self.tool_calls, &self.session_id, id);
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
    model: ModelRef,
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

    /// Like [`Self::new`], with an explicit auto-approve switch for unattended
    /// interactive TUI sessions.
    pub fn new_with_options(
        app: Arc<Application>,
        model: ModelRef,
        mode: PermissionProfile,
        sandbox: bool,
        auto_approve: bool,
    ) -> Self {
        let (events, _) = broadcast::channel(2048);
        let session_events: Arc<Mutex<HashMap<SessionId, broadcast::Sender<RuntimeEvent>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let mut memory_events = app.subscribe_memory_events();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let all_events = events.clone();
            let scoped_events = session_events.clone();
            handle.spawn(async move {
                loop {
                    let event = match memory_events.recv().await {
                        Ok(event) => event,
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            tracing::warn!(skipped, "memory event bridge lagged");
                            continue;
                        }
                        Err(broadcast::error::RecvError::Closed) => return,
                    };
                    let session_id = event.session_id.clone();
                    let runtime_event = project_memory_consolidation_event(event);
                    let _ = all_events.send(runtime_event.clone());
                    if let Some(sender) = scoped_events
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .get(&SessionId::new(session_id))
                        .cloned()
                    {
                        let _ = sender.send(runtime_event);
                    }
                }
            });
        }
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
            session_runtime: Mutex::new(HashMap::new()),
            settings_edits: tokio::sync::Mutex::new(()),
            auto_approve,
            media_root,
            events,
            session_events,
            pending: Arc::new(Mutex::new(HashMap::new())),
            pending_clarify: Arc::new(Mutex::new(HashMap::new())),
            steering: Arc::new(Mutex::new(HashMap::new())),
            goal_steering: Arc::new(Mutex::new(HashMap::new())),
            child_cancels: Arc::new(Mutex::new(HashMap::new())),
            tool_call_cancels: Arc::new(Mutex::new(HashMap::new())),
            auxiliary_assists: Arc::new(AuxiliaryAssists::default()),
            user_shells: Arc::new(crate::user_shell::UserShellStore::default()),
            checkpoints: Arc::new(crate::checkpoints::CheckpointStore::default()),
            live_views: Arc::new(crate::live_view::LiveViews::default()),
            btw: Arc::new(Mutex::new(HashMap::new())),
            active: Arc::new(ActiveTurns::with_retiring(shutting_down.clone())),
            shutting_down: shutting_down.clone(),
            retiring_reason: std::sync::Mutex::new(None),
            process_shutdown: None,
            client_presence: None,
            in_flight,
        }
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
        Ok(self.runtime_config(session_id).await?.model)
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

    fn events_for(&self, session_id: &SessionId) -> broadcast::Sender<RuntimeEvent> {
        let mut session_events = self.session_events.lock().unwrap();
        if let Some(events) = session_events.get(session_id).cloned() {
            return events;
        }
        let (events, mut forward) = broadcast::channel(2048);
        session_events.insert(session_id.clone(), events.clone());
        drop(session_events);
        let all_events = self.events.clone();
        let live_views = self.live_views.clone();
        let owned_session_id = session_id.clone();
        tokio::spawn(async move {
            loop {
                match forward.recv().await {
                    Ok(event) => {
                        live_views.apply(&owned_session_id, &event);
                        let _ = all_events.send(event);
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "session event compatibility stream lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
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
                match background_events.recv().await {
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
                        let snapshots = match background_registry.try_active_snapshots().await {
                            Ok(tasks) => tasks,
                            Err(error) => {
                                let _ = background_sender.send(RuntimeEvent::Notification {
                                    level: NotificationLevel::Error,
                                    message: format!("无法读取后台任务状态: {error}"),
                                });
                                continue;
                            }
                        };
                        let tasks = snapshots
                            .into_iter()
                            .filter(|task| {
                                task.owner_scope.as_deref() == Some(background_session_id.as_str())
                            })
                            .map(|task| leveler_client_protocol::UiActiveBackgroundTask {
                                task_id: task.id,
                                program: task.program,
                                args: task.args,
                                elapsed_ms: task.duration_ms,
                                pid: task.pid,
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
        Self::decode_settings(session_id, settings, approval_policy)
    }

    fn decode_settings(
        session_id: &SessionId,
        settings: &leveler_storage::SessionSettingsSnapshot,
        approval_policy: ApprovalPolicy,
    ) -> Result<SessionRuntimeConfig, ClientError> {
        let record = &settings.session;
        let model = ModelRef::parse(&record.model).ok_or_else(|| {
            ClientError::Runtime(format!(
                "session {session_id} stores invalid model reference `{}`",
                record.model
            ))
        })?;
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
        sessions
            .update_model(session_id, &config.model.to_string(), leveler_core::now())
            .await
            .map_err(|error| ClientError::Runtime(error.to_string()))?;
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

    /// Run one explicit user shell command (`!command`). USER-ORIGINATED
    /// DIRECT EXECUTION: no model, no agent loop, no tool registry. The
    /// ActiveTurns slot enforces the Idle/AgentTurn/UserShell foreground
    /// mutex; host safety (confinement, sandbox, env scrub, tree kill) rides
    /// the same substrate as agent shell execution.
    async fn handle_run_user_shell(
        &self,
        session_id: SessionId,
        command: String,
    ) -> Result<(), ClientError> {
        let trimmed = command.trim();
        if trimmed.is_empty() {
            self.notify_error(&session_id, "在 ! 后输入要执行的命令".to_string());
            return Ok(());
        }
        // The hang guard protects the user too (backgrounding via `&`,
        // credential-file reads, comment-swallowed commands) — same rules as
        // the agent's shell tool, stated to the user instead of the model.
        if let Some(reason) = leveler_tools::tools::refuse_shell_script(trimmed) {
            self.notify_error(&session_id, format!("命令被拒绝: {reason}"));
            return Ok(());
        }
        let config = self.runtime_config(&session_id).await?;
        let admission = match self.active.admit(&session_id) {
            Ok(admission) => admission,
            Err(crate::active_turns::TurnAdmissionError::Busy(_)) => {
                self.notify_error(
                    &session_id,
                    "Agent 正在运行 · 等待完成或先取消当前任务".to_string(),
                );
                return Ok(());
            }
            Err(error) => {
                self.notify_error(&session_id, error.to_string());
                return Ok(());
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
                    self.notify_error(&session_id, format!("无法构造执行环境: {error}"));
                    return Ok(());
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
                        crate::user_shell::started_event(&id, &command_text, &cwd_display),
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
                        let spec = AdoptedTaskSpec {
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
                            initial_output: initial_output.clone(),
                            observer: Some(observer),
                            on_adopted: Some(on_adopted),
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
                    // error where the user will look for it.
                    store.append_output(&session_id, &id, &format!("{error}\n"));
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
    ) -> Result<crate::active_turns::TurnLease, ClientError> {
        self.auxiliary_assists.cancel(session_id);
        let admission = self
            .active
            .admit(session_id)
            .map_err(|error| ClientError::Runtime(error.to_string()))?;
        if retitle {
            self.retitle_placeholder_session(session_id, text).await;
        }
        self.checkpoint_before_turn(session_id, text).await;
        let _ = self
            .events_for(session_id)
            .send(RuntimeEvent::UserMessageAdded {
                message: UiMessage {
                    id: MessageId::new(leveler_core::new_uuid_string()),
                    role: UiRole::User,
                    text: text.to_string(),
                    ordinal: None,
                    kind: None,
                    images,
                },
            });
        Ok(admission)
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
            goal: self.goal_state_for(session_id),
            children: self.child_cancels.clone(),
            tool_calls: self.tool_call_cancels.clone(),
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
            .registry
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
                model: r.model,
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
        self.spawn_content_turn(session_id, parts, admission, config)
    }

    fn spawn_goal_turn(
        &self,
        session_id: SessionId,
        content: String,
        admission: crate::active_turns::TurnLease,
        config: SessionRuntimeConfig,
    ) -> oneshot::Receiver<Result<(), ClientError>> {
        // Single interactive path: direct goal loop (update_goal + tools +
        // spawn_agent). Orchestrate is not used for sessions.
        self.spawn_direct_goal_turn(session_id, content, admission, config)
    }

    fn spawn_direct_goal_turn(
        &self,
        session_id: SessionId,
        content: String,
        admission: crate::active_turns::TurnLease,
        config: SessionRuntimeConfig,
    ) -> oneshot::Receiver<Result<(), ClientError>> {
        let app = self.app.clone();
        let events = self.events_for(&session_id);
        let active = self.active.clone();
        let model = config.model;
        let mode = config.mode;
        let sandbox = config.sandbox;
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
                    app.run_in_session_with_clarifier(
                        &session_id,
                        &model,
                        mode,
                        &content,
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
                if let Some(tx) = accepted_tx
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
                {
                    let _ = tx.send(Err(turn_rejection(&result)));
                }
                // TaskFinished is the normal path's one terminal authority and
                // EventBridge projects it immediately. Only failures that never
                // reached that commit need the wrapper fallback.
                if !bridge.terminal_published() {
                    active.finish_with(&admission, || {
                        let _ = events.send(turn_runtime_event(result));
                    });
                } else {
                    active.finish(&admission);
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
    ) -> Result<(), ClientError> {
        // A client that sends the explicit command while a turn already runs
        // must get the same upgrade as a text steer. Refusing with "session is
        // busy" would make Goal entry depend on which command shape a client
        // happened to send.
        if self.active.is_running(&session_id) {
            return self.steer_into_goal(session_id, content).await;
        }
        let config = self.runtime_config(&session_id).await?;
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
        let cancel = self.stage_turn(&session_id, &content, true, 0).await?;
        let accepted = self.spawn_goal_turn(session_id, content, cancel, config);
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

    /// Route an ordinary user message: memory commands, the vision gate, then
    /// a chat/content or goal turn chosen by the session's collaboration axis.
    async fn handle_submit_message(
        &self,
        session_id: SessionId,
        content: String,
        attachments: Vec<AttachmentRef>,
        origin: Option<&CommandId>,
    ) -> Result<(), ClientError> {
        // A message that is ONLY a memory command is the user's own write, not
        // a task. Classified HERE, before `stage_turn`, so TUI, Web, mobile and
        // remote all get the same answer — doing it in a client reducer would
        // mean one client saving and another handing the same sentence to the
        // model. `origin` is the delivery identity, so a retried command
        // replays its write instead of becoming a second one.
        if attachments.is_empty()
            && self.handle_direct_memory_message(&session_id, &content, origin)
        {
            return Ok(());
        }
        // `/goal` is product semantics, not a client feature. A client that
        // does not parse slash commands (Desktop, an older build, a steering
        // message) still reaches the same durable axis and the same executor
        // profile through this one parse.
        if attachments.is_empty()
            && let Some(command) = leveler_client_protocol::parse_goal_command(&content)
        {
            use leveler_client_protocol::GoalCommand;
            return match command {
                GoalCommand::Run(task) if self.active.is_running(&session_id) => {
                    self.steer_into_goal(session_id, task).await
                }
                GoalCommand::Run(task) => self.run_goal_turn(session_id, task).await,
                GoalCommand::Clear => self.clear_goal_axis(session_id).await,
                GoalCommand::Status => self.goal_status(session_id).await,
                GoalCommand::Usage => Err(ClientError::Runtime(
                    "用法: /goal <任务>（/goal clear 返回 Chat，/goal status 查看模式）".into(),
                )),
            };
        }
        let config = self.runtime_config(&session_id).await?;
        // The TUI asks the user about this first, but the reducer is one
        // client. Every other one — web, headless, a model changed between
        // staging and sending — arrives here, and here the answer is the same:
        // a model that cannot read an image is told so, never handed the text
        // with the picture removed.
        self.refuse_unreadable_images(&config.model, &attachments)
            .await?;
        let cancel = self
            .stage_turn(&session_id, &content, true, image_count(&attachments))
            .await?;
        // CollaborationMode is the single source of turn profile:
        // goal → goal_mode / update_goal path; chat|plan → content turn.
        // Plan read_only is applied inside engine from session.collaboration.
        let accepted = if collaboration_routes_submit_to_goal(&config.collaboration) {
            if !attachments.is_empty() {
                // The content entry resolves the same session-owned Goal
                // profile while preserving every attachment.
                self.spawn_turn(session_id, content, attachments, cancel, config)
            } else {
                self.spawn_goal_turn(session_id, content, cancel, config)
            }
        } else {
            self.spawn_turn(session_id, content, attachments, cancel, config)
        };
        await_turn_acceptance(accepted).await
    }

    /// Route a continuation intent (`继续`, `继续，但是先不要跑测试`, …).
    ///
    /// The runtime owns the mapping: continuation intent AND a resumable task
    /// must both hold before this becomes a resume. Otherwise the message is an
    /// ordinary submit, so a `继续` in a fresh session, or one that is really a
    /// new request, reaches the model exactly as before.
    async fn handle_resume_task(
        &self,
        session_id: SessionId,
        content: String,
        origin: Option<&CommandId>,
    ) -> Result<(), ClientError> {
        // No continuation intent: the client mislabelled an ordinary message.
        let Some(continuation) = leveler_client_protocol::parse_continuation(&content) else {
            return self
                .handle_submit_message(session_id, content, Vec::new(), origin)
                .await;
        };
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
                    .handle_submit_message(session_id, content, Vec::new(), origin)
                    .await;
            }
        }
        // The instruction is the user's own words. The original objective stays
        // the task's first user message and the resume path seeds the prior
        // plan/ledger; `continuation.amendment` travels inside that text, so an
        // amendment is an addition and never a replacement.
        if let Some(amendment) = &continuation.amendment {
            tracing::info!(
                session = %session_id.as_str(),
                phrase = %continuation.phrase,
                amendment = %amendment,
                "continuation intent resumes the session's logical task"
            );
        }
        let instruction = Message::user_input(content.clone());
        let cancel = self.stage_turn(&session_id, &content, false, 0).await?;
        let accepted = self.spawn_resume_turn(session_id, instruction, cancel);
        await_turn_acceptance(accepted).await
    }

    /// What a continuation intent (`继续`) means for this session.
    ///
    /// The persisted terminal is the authority: an `interrupted` turn (user
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
            let payload = turn.payload.as_deref().ok_or_else(|| {
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
    ) -> oneshot::Receiver<Result<(), ClientError>> {
        let app = self.app.clone();
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
                if let Some(tx) = accepted_tx
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
                {
                    let _ = tx.send(Err(turn_rejection(&result)));
                }
                if !bridge.terminal_published() {
                    active.finish_with(&admission, || {
                        let _ = events.send(turn_runtime_event(result));
                    });
                } else {
                    active.finish(&admission);
                }
            });
        });
        accepted_rx
    }

    /// `/develop`: one goal through `Analyze → Coding → Verify → Review`.
    ///
    /// Identical plumbing to [`Self::spawn_direct_goal_turn`] — same admission,
    /// same approver, clarifier and steering, same terminal publication — so
    /// cancelling, approving and resuming behave exactly as they do for any
    /// other turn. Only the harness entry point differs.
    fn spawn_develop_turn(
        &self,
        session_id: SessionId,
        content: String,
        admission: crate::active_turns::TurnLease,
        config: SessionRuntimeConfig,
    ) -> oneshot::Receiver<Result<(), ClientError>> {
        let app = self.app.clone();
        let events = self.events_for(&session_id);
        let active = self.active.clone();
        let model = config.model;
        let mode = config.mode;
        let sandbox = config.sandbox;
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
                    app.run_develop_in_session(
                        &session_id,
                        &model,
                        mode,
                        &content,
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
                if let Some(tx) = accepted_tx
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
                {
                    let _ = tx.send(Err(turn_rejection(&result)));
                }
                if !bridge.terminal_published() {
                    active.finish_with(&admission, || {
                        let _ = events.send(turn_runtime_event(result));
                    });
                } else {
                    active.finish(&admission);
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
    fn spawn_recap(&self, session_id: SessionId, config: SessionRuntimeConfig) {
        let app = self.app.clone();
        let events = self.events_for(&session_id);
        let model = config.model;
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
                .registry
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
    fn spawn_btw(&self, session_id: SessionId, question: String, config: SessionRuntimeConfig) {
        let app = self.app.clone();
        let events = self.events_for(&session_id);
        let model = config.model;
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
                        .registry
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
                            app.registry.clone(),
                            Arc::new(registry.clone()),
                            tool_context.clone(),
                            model.clone(),
                            1,
                        )
                        .with_approver(Arc::new(AutoApprove))
                    });
                    let route = app.registry.route_identity(&model);
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
                            app.registry.as_ref(),
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
                        let outcome = app.registry.generate(summary_request, cancel.clone()).await;
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
                        let outcome = app.registry.generate(request, cancel.clone()).await;
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

    /// Save a message that is ONLY a memory command, and report it.
    ///
    /// Returns true when the message was consumed as a write, so no turn is
    /// staged, no model request is made, and the running turn (if any) is not
    /// steered. Anything ambiguous returns false and stays an ordinary
    /// message: failing to save is recoverable, swallowing a real task is not.
    fn handle_direct_memory_message(
        &self,
        session_id: &SessionId,
        content: &str,
        origin: Option<&CommandId>,
    ) -> bool {
        let Some(body) = leveler_memory::parse_direct_memory_command(content) else {
            return false;
        };
        let events = self.events_for(session_id);
        let title = leveler_memory::title_from_body(&body);
        // The message arrived through this process's user input surface, so
        // the invocation is minted HERE and never derived from the text. The
        // delivery id is the operation identity: a retry replays, while two
        // independent identical commands remain two operations.
        let invocation = Self::memory_invocation("direct", session_id, origin);
        let request = leveler_memory::RememberRequest {
            op_id: leveler_memory::v2::OpId::derived("direct", invocation.as_str()),
            scope: leveler_memory::ServiceScope::Project,
            title: title.clone(),
            body: body.clone(),
            // A commanded save defaults to a lasting preference: "remember
            // this" almost always means "apply it from now on".
            kind: "preference".to_string(),
            subject: None,
            value: None,
            tags: Vec::new(),
            // The exact span the direct write owns. The same turn's extractor
            // matches this structurally instead of duplicating the fact.
            evidence: Some(body),
            expires_at: None,
            replaces_id: None,
        };
        match self
            .app
            .memory_service()
            .remember(&leveler_memory::v2::Actor::User { invocation }, request)
        {
            Ok(outcome) => {
                let busy = self.active.is_running(session_id);
                let (level, message) = direct_write_notification(&outcome, &title, busy);
                let _ = events.send(RuntimeEvent::Notification { level, message });
                // Deliberately no list refresh here: the confirmation above
                // IS the feedback, and a follow-up listing notification would
                // overwrite it on a one-line status bar. Clients that show a
                // panel re-list on their own after a write.
                true
            }
            Err(error) => {
                // Refused (a credential, say). Report it and do NOT fall
                // through to the model, which would hand it the same text.
                let _ = events.send(RuntimeEvent::Notification {
                    level: leveler_client_protocol::NotificationLevel::Warning,
                    message: format!("保存记忆失败：{error}"),
                });
                true
            }
        }
    }

    fn spawn_content_turn(
        &self,
        session_id: SessionId,
        parts: Vec<ContentPart>,
        admission: crate::active_turns::TurnLease,
        config: SessionRuntimeConfig,
    ) -> oneshot::Receiver<Result<(), ClientError>> {
        let app = self.app.clone();
        let events = self.events_for(&session_id);
        let active = self.active.clone();
        let model = config.model;
        let mode = config.mode;
        let sandbox = config.sandbox;
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
                if let Some(tx) = accepted_tx
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
                {
                    let _ = tx.send(Err(turn_rejection(&result)));
                }
                if !bridge.terminal_published() {
                    active.finish_with(&admission, || {
                        let _ = events.send(turn_runtime_event(result));
                    });
                } else {
                    active.finish(&admission);
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
                });
                self.notify_error(&session_id, format!("压缩失败：{error}"));
                return Ok(());
            }
        };
        let model = config.model;
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
    /// Identity of one trusted user memory operation.
    ///
    /// A delivered command carries a durable `command_id`; that is the
    /// operation identity, so a retry replays and two independent identical
    /// commands stay distinct. The raw `send` seam has no delivery identity,
    /// so it mints a fresh one per call.
    fn memory_invocation(
        prefix: &str,
        session_id: &SessionId,
        origin: Option<&CommandId>,
    ) -> leveler_memory::v2::UserInvocationId {
        let operation = match origin {
            Some(command_id) => command_id.as_str().to_string(),
            None => format!("raw:{}:{}", session_id, leveler_core::new_uuid_string()),
        };
        leveler_memory::v2::UserInvocationId::mint(format!("{prefix}:{operation}"))
    }

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
                if attachments.is_empty()
                    && leveler_client_protocol::parse_continuation(&content).is_some()
                {
                    self.handle_resume_task(session_id, content, Some(origin))
                        .await
                } else {
                    self.handle_submit_message(session_id, content, attachments, Some(origin))
                        .await
                }
            }
            ClientCommand::ResumeTask {
                session_id,
                content,
            } => {
                self.handle_resume_task(session_id, content, Some(origin))
                    .await
            }
            ClientCommand::AcceptMemory {
                session_id,
                scope,
                id,
            } => {
                self.accept_memory(session_id, scope, id, Some(origin))
                    .await
            }
            ClientCommand::RejectMemory {
                session_id,
                scope,
                id,
            } => {
                self.reject_memory(session_id, scope, id, Some(origin))
                    .await
            }
            ClientCommand::RememberMemory {
                session_id,
                scope,
                body,
                kind,
            } => {
                self.remember_memory(session_id, scope, body, kind, Some(origin))
                    .await
            }
            ClientCommand::ForgetMemory {
                session_id,
                scope,
                id,
            } => {
                self.forget_memory(session_id, scope, id, Some(origin))
                    .await
            }
            ClientCommand::SetMemoryPin {
                session_id,
                scope,
                id,
                pinned,
            } => {
                self.set_memory_pin(session_id, scope, id, pinned, Some(origin))
                    .await
            }
            ClientCommand::DeprecateMemory {
                session_id,
                scope,
                id,
                revision,
            } => {
                self.deprecate_memory(session_id, scope, id, revision, Some(origin))
                    .await
            }
            ClientCommand::UpdateMemory {
                session_id,
                scope,
                id,
                revision,
                title,
                body,
            } => {
                self.update_memory(session_id, scope, id, revision, title, body, Some(origin))
                    .await
            }
            command => self.send(command).await,
        }
    }

    async fn accept_memory(
        &self,
        session_id: SessionId,
        scope: leveler_client_protocol::UiMemoryScope,
        id: String,
        origin: Option<&CommandId>,
    ) -> Result<(), ClientError> {
        // User-authoritative promotion: this command arrived through an
        // in-process review surface, which is why the model can never reach
        // it. The invocation is minted here, not passed in.
        let events = self.events_for(&session_id);
        let invocation = Self::memory_invocation("ui:accept", &session_id, origin);
        let actor = leveler_memory::v2::Actor::User { invocation };
        match self.app.memory_service().review(
            &actor,
            service_memory_scope(scope),
            &id,
            leveler_memory::ReviewRequest::Accept,
        ) {
            Ok(outcome) => {
                let _ = events.send(RuntimeEvent::Notification {
                    level: leveler_client_protocol::NotificationLevel::Info,
                    message: format!("已采纳记忆 [{id}]: {}", outcome_title(&outcome)),
                });
            }
            Err(err) => {
                let _ = events.send(RuntimeEvent::Notification {
                    level: leveler_client_protocol::NotificationLevel::Warning,
                    message: format!("accept failed [{id}]: {err}"),
                });
            }
        }
        Ok(())
    }

    /// Pin or unpin one record through the memory service.
    ///
    /// A user pin is a standing constraint, so the invocation is minted here
    /// from the user's own command; the model has no path to it.
    async fn set_memory_pin(
        &self,
        session_id: SessionId,
        scope: leveler_client_protocol::UiMemoryScope,
        id: String,
        pinned: bool,
        origin: Option<&CommandId>,
    ) -> Result<(), ClientError> {
        let events = self.events_for(&session_id);
        let invocation = Self::memory_invocation(
            if pinned { "ui:pin" } else { "ui:unpin" },
            &session_id,
            origin,
        );
        let actor = leveler_memory::v2::Actor::User { invocation };
        match self
            .app
            .memory_service()
            .pin(&actor, service_memory_scope(scope), &id, pinned)
        {
            Ok(outcome) => {
                let _ = events.send(RuntimeEvent::Notification {
                    level: leveler_client_protocol::NotificationLevel::Info,
                    message: if pinned {
                        format!("已固定记忆 [{id}]: {}", outcome_title(&outcome))
                    } else {
                        format!("已取消固定记忆 [{id}]: {}", outcome_title(&outcome))
                    },
                });
            }
            Err(error) => {
                let _ = events.send(RuntimeEvent::Notification {
                    level: leveler_client_protocol::NotificationLevel::Warning,
                    message: format!("pin failed [{id}]: {error}"),
                });
            }
        }
        Ok(())
    }

    /// Retire one record as no longer true, keeping its history.
    ///
    /// The revision the user was looking at is required: a newer record is a
    /// conflict to show, not a state to overwrite silently.
    async fn deprecate_memory(
        &self,
        session_id: SessionId,
        scope: leveler_client_protocol::UiMemoryScope,
        id: String,
        revision: u64,
        origin: Option<&CommandId>,
    ) -> Result<(), ClientError> {
        let events = self.events_for(&session_id);
        let invocation = Self::memory_invocation("ui:deprecate", &session_id, origin);
        let actor = leveler_memory::v2::Actor::User { invocation };
        match self.app.memory_service().deprecate(
            &actor,
            service_memory_scope(scope),
            &id,
            revision,
            "the user marked this memory as no longer true".into(),
        ) {
            Ok(outcome) => {
                let _ = events.send(RuntimeEvent::Notification {
                    level: leveler_client_protocol::NotificationLevel::Info,
                    message: format!("已标记过期记忆 [{id}]: {}", outcome_title(&outcome)),
                });
            }
            Err(error) => {
                let _ = events.send(RuntimeEvent::Notification {
                    level: leveler_client_protocol::NotificationLevel::Warning,
                    message: format!("deprecate failed [{id}]: {error}"),
                });
            }
        }
        Ok(())
    }

    /// Rewrite one record from the user's own /memory page.
    ///
    /// The command carries the revision the page opened, so a record another
    /// process changed in the meantime is a conflict the user must resolve —
    /// never a silent overwrite. The invocation is minted from the user's
    /// command id: the model has no path to this handler.
    #[allow(clippy::too_many_arguments)]
    async fn update_memory(
        &self,
        session_id: SessionId,
        scope: leveler_client_protocol::UiMemoryScope,
        id: String,
        revision: u64,
        title: String,
        body: String,
        origin: Option<&CommandId>,
    ) -> Result<(), ClientError> {
        let events = self.events_for(&session_id);
        let invocation = Self::memory_invocation("ui:update", &session_id, origin);
        let actor = leveler_memory::v2::Actor::User { invocation };
        let request = leveler_memory::UpdateRequest {
            title: Some(title),
            body: Some(body),
            value: None,
            tags: None,
            expires_at: None,
            subject: None,
        };
        match self.app.memory_service().update(
            &actor,
            service_memory_scope(scope),
            &id,
            revision,
            request,
        ) {
            Ok(outcome) => {
                let _ = events.send(RuntimeEvent::Notification {
                    level: leveler_client_protocol::NotificationLevel::Info,
                    message: format!("已更新记忆 [{id}]: {}", outcome_title(&outcome)),
                });
            }
            Err(error) => {
                // A conflict is a specific, actionable answer: the record is
                // at a newer revision and the caller's text was NOT written.
                // The message says which, so the page can keep the edit.
                let message = match &error {
                    leveler_memory::ServiceError::Conflict(_) => {
                        format!("update conflict [{id}]: {error}")
                    }
                    leveler_memory::ServiceError::NotCurrent(_) => {
                        format!("update rejected [{id}]: {error}")
                    }
                    _ => format!("update failed [{id}]: {error}"),
                };
                let _ = events.send(RuntimeEvent::Notification {
                    level: leveler_client_protocol::NotificationLevel::Warning,
                    message,
                });
            }
        }
        Ok(())
    }

    async fn reject_memory(
        &self,
        session_id: SessionId,
        scope: leveler_client_protocol::UiMemoryScope,
        id: String,
        origin: Option<&CommandId>,
    ) -> Result<(), ClientError> {
        // Declines a PENDING candidate and suppresses that signal, so the
        // same proposal does not return on the next turn.
        let events = self.events_for(&session_id);
        let invocation = Self::memory_invocation("ui:reject", &session_id, origin);
        let actor = leveler_memory::v2::Actor::User { invocation };
        match self.app.memory_service().review(
            &actor,
            service_memory_scope(scope),
            &id,
            leveler_memory::ReviewRequest::Reject {
                reason: "rejected by the user".to_string(),
            },
        ) {
            Ok(_) => {
                let _ = events.send(RuntimeEvent::Notification {
                    level: leveler_client_protocol::NotificationLevel::Info,
                    message: format!("已拒绝候选 [{id}]，不再重复提示"),
                });
            }
            Err(err) => {
                let _ = events.send(RuntimeEvent::Notification {
                    level: leveler_client_protocol::NotificationLevel::Warning,
                    message: format!("reject failed [{id}]: {err}"),
                });
            }
        }
        Ok(())
    }

    async fn remember_memory(
        &self,
        session_id: SessionId,
        scope: leveler_client_protocol::UiMemoryScope,
        body: String,
        kind: Option<leveler_client_protocol::UiMemoryKind>,
        origin: Option<&CommandId>,
    ) -> Result<(), ClientError> {
        // The user's own write: no model, no agent turn, no candidate. The
        // command IS the authorization; the invocation identity is minted
        // here from the trusted in-process command.
        use leveler_client_protocol::UiMemoryKind;
        let events = self.events_for(&session_id);
        let body = body.trim().to_string();
        if body.is_empty() {
            let _ = events.send(RuntimeEvent::Notification {
                level: leveler_client_protocol::NotificationLevel::Warning,
                message: "记忆内容不能为空".to_string(),
            });
            return Ok(());
        }
        let domain_kind = match kind.unwrap_or(UiMemoryKind::Preference) {
            UiMemoryKind::Preference => "preference",
            UiMemoryKind::Decision => "decision",
            UiMemoryKind::Note => "note",
            // An experience needs the structured evidence a task records. A
            // hand-typed /remember has none, and storing it as one would
            // claim a verification that never happened.
            UiMemoryKind::Experience => {
                let _ = events.send(RuntimeEvent::Notification {
                    level: leveler_client_protocol::NotificationLevel::Warning,
                    message: "经验条目由已完成任务的运行证据自动生成，不能用 /remember 直接写入"
                        .to_string(),
                });
                return Ok(());
            }
        };
        // The title is derived HERE, once, so TUI / Web / CLI cannot drift
        // into three conventions. Never model-generated.
        let title = leveler_memory::title_from_body(&body);
        let invocation = Self::memory_invocation("ui:remember", &session_id, origin);
        let request = leveler_memory::RememberRequest {
            op_id: leveler_memory::v2::OpId::derived("ui:remember", invocation.as_str()),
            scope: service_memory_scope(scope),
            title: title.clone(),
            body: body.clone(),
            kind: domain_kind.to_string(),
            subject: None,
            value: None,
            tags: Vec::new(),
            evidence: Some(body),
            expires_at: None,
            replaces_id: None,
        };
        match self
            .app
            .memory_service()
            .remember(&leveler_memory::v2::Actor::User { invocation }, request)
        {
            Ok(outcome) => {
                let busy = self.active.is_running(&session_id);
                let (level, message) = direct_write_notification(&outcome, &title, busy);
                let _ = events.send(RuntimeEvent::Notification { level, message });
            }
            Err(err) => {
                let _ = events.send(RuntimeEvent::Notification {
                    level: leveler_client_protocol::NotificationLevel::Warning,
                    message: format!("保存记忆失败：{err}"),
                });
            }
        }
        Ok(())
    }

    async fn forget_memory(
        &self,
        session_id: SessionId,
        scope: leveler_client_protocol::UiMemoryScope,
        id: String,
        origin: Option<&CommandId>,
    ) -> Result<(), ClientError> {
        // Archives an ACTIVE entry. A pending id used to land here and
        // silently do nothing — the Web "忽略" button's bug — so a candidate
        // id is refused with a pointer to the right action.
        let events = self.events_for(&session_id);
        let invocation = Self::memory_invocation("ui:forget", &session_id, origin);
        let actor = leveler_memory::v2::Actor::User { invocation };
        let service = self.app.memory_service();
        let service_scope = service_memory_scope(scope);
        let is_pending = match service.pending(&actor, service_scope) {
            Ok(pending) => pending.iter().any(|view| view.id == id),
            Err(error) => {
                let _ = events.send(RuntimeEvent::Notification {
                    level: leveler_client_protocol::NotificationLevel::Warning,
                    message: format!("forget failed [{id}]: {error}"),
                });
                return Ok(());
            }
        };
        if is_pending {
            let _ = events.send(RuntimeEvent::Notification {
                level: leveler_client_protocol::NotificationLevel::Warning,
                message: format!("[{id}] 是待确认候选，不是已保存记忆；请改用拒绝"),
            });
            return Ok(());
        }
        match service.forget(&actor, service_scope, &id) {
            Ok(leveler_memory::WriteOutcome::Forgotten { id, title }) => {
                let _ = events.send(RuntimeEvent::Notification {
                    level: leveler_client_protocol::NotificationLevel::Info,
                    message: format!("已归档记忆 [{id}]: {title}"),
                });
            }
            Ok(outcome) => {
                let _ = events.send(RuntimeEvent::Notification {
                    level: leveler_client_protocol::NotificationLevel::Warning,
                    message: format!(
                        "forget was queued, not applied: {}",
                        outcome_title(&outcome)
                    ),
                });
            }
            Err(err) => {
                let _ = events.send(RuntimeEvent::Notification {
                    level: leveler_client_protocol::NotificationLevel::Warning,
                    message: format!("forget failed: {err}"),
                });
            }
        }
        Ok(())
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
                            let waiting = self
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
                // Continuation is an App intent, shared by every client. Keep
                // attachments on the content path rather than discarding them
                // when invoking the existing attachment-free Resume command.
                if attachments.is_empty()
                    && leveler_client_protocol::parse_continuation(&content).is_some()
                {
                    self.handle_resume_task(session_id, content, None).await
                } else {
                    self.handle_submit_message(session_id, content, attachments, None)
                        .await
                }
            }
            ClientCommand::ResumeTask {
                session_id,
                content,
            } => self.handle_resume_task(session_id, content, None).await,
            ClientCommand::RunGoal {
                session_id,
                content,
            } => self.run_goal_turn(session_id, content).await,
            ClientCommand::RunDevelop {
                session_id,
                content,
            } => {
                let config = self.runtime_config(&session_id).await?;
                let cancel = self.stage_turn(&session_id, &content, true, 0).await?;
                let accepted = self.spawn_develop_turn(session_id, content, cancel, config);
                await_turn_acceptance(accepted).await
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
            ClientCommand::AnswerClarification { request_id, answer } => {
                resolve_clarification(&self.pending_clarify, &request_id, answer)
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
                self.persist_goal_declaration(&session_id, &goal).await?;
                let config = self.runtime_config(&session_id).await?;
                let cancel = self.stage_turn(&session_id, &goal, false, 0).await?;
                let accepted = self.spawn_goal_turn(session_id, goal, cancel, config);
                await_turn_acceptance(accepted).await
            }
            ClientCommand::ListMemory {
                session_id,
                include_archived,
                query_id,
            } => {
                // The location shown is the current Markdown root: the store
                // this process actually reads.
                let memory_dir = self
                    .app
                    .layout
                    .markdown_memory_dir()
                    .unwrap_or_else(|| self.app.layout.state_dir.clone());
                let events = self.events_for(&session_id);
                send_memory_list(
                    &events,
                    &self.app.memory_service(),
                    self.app.memory_lanes(),
                    &memory_dir,
                    include_archived,
                    query_id,
                );
                Ok(())
            }
            ClientCommand::MemoryStatus {
                session_id,
                query_id,
            } => {
                let events = self.events_for(&session_id);
                match crate::memory_status::report(&self.app).await {
                    Ok(status) => {
                        let _ = events.send(RuntimeEvent::MemoryStatus { query_id, status });
                    }
                    Err(error) => {
                        let _ = events.send(RuntimeEvent::Notification {
                            level: leveler_client_protocol::NotificationLevel::Warning,
                            message: format!("memory status failed: {error}"),
                        });
                    }
                }
                Ok(())
            }
            ClientCommand::SteerCurrentTurn {
                session_id,
                content,
            } => {
                if self.active.is_running(&session_id) {
                    // A steer is also an input surface for `/goal`. The
                    // terminal already forwards it here when the user types
                    // `/goal` during a turn; every other client reaches the
                    // same branch by sending the text as a steer.
                    if let Some(command) = leveler_client_protocol::parse_goal_command(&content) {
                        use leveler_client_protocol::GoalCommand;
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
                    self.steering
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .entry(session_id.clone())
                        .or_default()
                        .push(content);
                    return Ok(());
                }
                // The turn ended between the client deciding to steer and this
                // arriving — a real race, because the client's "busy" view lags
                // the runtime by one event. Dropping the text here would lose
                // what the user typed, so fall through to an ordinary
                // submission and start a new turn with it.
                return Box::pin(self.send(ClientCommand::SubmitMessage {
                    session_id,
                    content,
                    attachments: Vec::new(),
                }))
                .await;
            }
            ClientCommand::AcceptMemory {
                session_id,
                scope,
                id,
            } => self.accept_memory(session_id, scope, id, None).await,
            ClientCommand::RejectMemory {
                session_id,
                scope,
                id,
            } => self.reject_memory(session_id, scope, id, None).await,
            ClientCommand::RememberMemory {
                session_id,
                scope,
                body,
                kind,
            } => {
                self.remember_memory(session_id, scope, body, kind, None)
                    .await
            }
            ClientCommand::ForgetMemory {
                session_id,
                scope,
                id,
            } => self.forget_memory(session_id, scope, id, None).await,
            ClientCommand::SetMemoryPin {
                session_id,
                scope,
                id,
                pinned,
            } => {
                self.set_memory_pin(session_id, scope, id, pinned, None)
                    .await
            }
            ClientCommand::DeprecateMemory {
                session_id,
                scope,
                id,
                revision,
            } => {
                self.deprecate_memory(session_id, scope, id, revision, None)
                    .await
            }
            ClientCommand::UpdateMemory {
                session_id,
                scope,
                id,
                revision,
                title,
                body,
            } => {
                self.update_memory(session_id, scope, id, revision, title, body, None)
                    .await
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
                // A new conversation inherits configured model defaults, not
                // the previous conversation's explicit Thinking intent.
                config.thinking = None;
                match self
                    .app
                    .create_daemon_session(
                        &config.model,
                        PLACEHOLDER_GOAL,
                        config.mode,
                        // `/clear` opens a sibling conversation under the
                        // caller's own axis; an unreadable value falls back to
                        // the product default rather than guessing chat.
                        config.collaboration.parse().unwrap_or_default(),
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
                crate::user_shell::remove_session_output(&self.app.layout.state_dir, &session_id);
                let _ = self
                    .events_for(&requester_session_id)
                    .send(RuntimeEvent::SessionList {
                        sessions: self.list_sessions().await?,
                    });
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
                self.spawn_btw(session_id, question, config);
                Ok(())
            }
            ClientCommand::CancelBtw { session_id } => {
                self.cancel_btw(&session_id);
                Ok(())
            }
            ClientCommand::CancelCurrentTurn { session_id } => {
                if !self.cancel_active(&session_id) {
                    // No owned live turn: recover a possible orphan left by an
                    // earlier process. Never race the active executor's own
                    // terminal transition with the reaper.
                    self.cancel_orphaned(&session_id).await?;
                }
                Ok(())
            }
            ClientCommand::CancelTask { session_id } => {
                // A running turn carries the flag to its terminal, so the
                // engine records `cancelled` (not resumable `interrupted`).
                // With nothing running, commit that terminal here.
                let delivered = self
                    .app
                    .request_active_task_cancel(&session_id)
                    .await
                    .map_err(|error| ClientError::Runtime(error.to_string()))?;
                if delivered {
                    self.active.cancel_task(&session_id);
                } else {
                    match self.app.cancel_task(&session_id).await {
                        Ok(Some(event)) => {
                            let mut bridge = EventBridge::new(self.events_for(&session_id));
                            bridge.forward(event);
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
            ClientCommand::RunUserShell {
                session_id,
                command,
            } => self.handle_run_user_shell(session_id, command).await,
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
                self.spawn_recap(session_id, config);
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
                self.app.browser().shutdown().await;
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
        let db = self
            .app
            .open_database()
            .await
            .map_err(|e| ClientError::Runtime(e.to_string()))?;
        let settings = SessionRepository::new(&db)
            .settings_snapshot(session_id)
            .await
            .map_err(|e| ClientError::Runtime(e.to_string()))?
            .ok_or_else(|| ClientError::SessionNotFound(session_id.clone()))?;
        let config = self.config_from_settings(session_id, &settings)?;
        let last_sequence = settings.sequence;
        let record = settings.session;

        let payloads = MessageRepository::new(&db)
            .load(session_id)
            .await
            .map_err(|e| ClientError::Runtime(e.to_string()))?;
        let messages = payloads
            .iter()
            .enumerate()
            .filter_map(|(i, p)| ui_message_at(p, Some(i as u64)))
            .collect();

        let mut available_models = self.app.model_refs();
        available_models.sort_by_key(|m| m.to_string());

        let model = config.model.clone();
        let profile = self.app.registry.profile(&model).await.ok();
        // Whether the current model accepts images (spec §42).
        let vision = profile
            .as_ref()
            .map(|p| p.capabilities.vision)
            .unwrap_or(false);
        let thinking = ui_thinking_state(profile.as_ref(), config.thinking);

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
        pending_interactions.sort_by_key(|item| match item {
            leveler_client_protocol::UiPendingInteraction::Approval(request) => {
                request.id.as_str().to_string()
            }
            leveler_client_protocol::UiPendingInteraction::Clarification(request) => {
                request.id.as_str().to_string()
            }
        });

        let live = self.live_views.view(session_id);
        let checkpoints = self.checkpoints.list(session_id);

        // Long-goal P3: durable goal recaps survive the process, so a
        // reopened client re-renders them from the persisted rows — same
        // projection as the live event, oldest boundary first.
        let mut recaps = Vec::new();
        let stores = leveler_storage::EngineStores::from_database(&db);

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
            execution_task = goals
                .iter()
                .find(|goal| goal.state == leveler_storage::GoalState::Running)
                .map(|goal| leveler_client_protocol::UiExecutionTask {
                    goal_id: goal.id.to_string(),
                    objective: goal.objective.clone(),
                    running: runtime_active,
                });
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

        // Unlike the best-effort fields above, a child row that cannot be
        // decoded fails the snapshot: it is canonical history, and every other
        // reader of it (restart reconciliation, observability) refuses corrupt
        // rows rather than showing a partial truth as the whole one.
        let children = crate::children::project_children(&db, session_id, runtime_active)
            .await
            .map_err(|e| ClientError::Runtime(e.to_string()))?;
        let active_background_tasks = self
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
            })
            .collect();

        Ok(UiSessionSnapshot {
            id: session_id.clone(),
            repository: record.repository,
            task_status: Some(task_status),
            task_terminal,
            goal: record.goal,
            model: Some(model),
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
            active_tools: live.active_tools,
            active_background_tasks,
            plan,
            diff: live.diff,
            checkpoints,
            recaps,
            user_shells: {
                let live = self.user_shells.snapshot(session_id);
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
        })
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
        let config = self.runtime_config(&request.session_id).await?;
        let id = format!("host-{}", request.terminal_id.replace('-', ""));
        let owner = request.session_id.as_str();
        let registry = self.app.background_tasks();
        let observation = match request.operation {
            TerminalOperation::Create { cols, rows } => {
                let (_, mut process, _) = self
                    .app
                    .user_shell_execution(config.mode, config.sandbox, "")
                    .map_err(|e| ClientError::Runtime(e.to_string()))?;
                let shell = self
                    .app
                    .environment
                    .var("SHELL")
                    .filter(|s| {
                        std::path::Path::new(s).is_absolute() && std::path::Path::new(s).is_file()
                    })
                    .unwrap_or_else(|| "/bin/zsh".into());
                process.program = shell;
                process.args = vec!["-l".into()];
                process.pty = Some(PtySize { cols, rows });
                process.unrestricted_execution =
                    config.mode == leveler_execution::PermissionProfile::FullAccess;
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
                let model = session
                    .model
                    .as_ref()
                    .ok_or_else(|| ClientError::Runtime("created session has no model".into()))?;
                let context_window = self
                    .app
                    .registry
                    .profile(model)
                    .await
                    .map_err(|e| ClientError::Runtime(e.to_string()))?
                    .limits
                    .context_window;
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
                explicit
            }
            None => {
                let preferred = self.default_runtime.model.clone();
                if self.app.model_refs().contains(&preferred) {
                    preferred
                } else {
                    let fallback = self.app.fallback_default_model().ok_or_else(|| {
                        ClientError::Runtime(format!(
                            "model `{preferred}` is not configured, and this environment has no\
                             other configured model to fall back to"
                        ))
                    })?;
                    substitution = Some((fallback.clone(), preferred));
                    fallback
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
                        model: model.to_string(),
                        mode: execution_mode(request.mode).as_str().to_string(),
                        sandbox: self.default_runtime.sandbox,
                        kind: leveler_engine::ExecutionKind::Direct,
                        axes: Some(leveler_engine::NewSessionAxes {
                            collaboration: request.collaboration.as_str().to_string(),
                        }),
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
                        &model,
                        &request.goal,
                        execution_mode(request.mode),
                        request.collaboration,
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
        let context_window =
            self.app
                .registry
                .profile(
                    session.model.as_ref().ok_or_else(|| {
                        ClientError::Runtime("created session has no model".into())
                    })?,
                )
                .await
                .map(|profile| profile.limits.context_window)
                .map_err(|error| ClientError::Runtime(error.to_string()))?;
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
        leveler_engine::EngineEvent::PlanUpdated { steps } => steps,
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
    events: &broadcast::Sender<RuntimeEvent>,
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
    request_messages.push(Message::text(Role::User, COMPACT_PROMPT));
    let mut request = ModelRequest::new(model.clone(), request_messages);
    let requested_effort = leveler_engine::ModelCallKind::Compaction.default_reasoning_effort();
    let profile = match app.registry.profile(model).await {
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
        app.registry.route_identity(model).as_ref(),
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
    let generate = app.registry.generate(request, call_cancel.clone());
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
        TranscriptOrigin::CompactionSummary,
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
        let profile = app.registry.profile(&model).await.ok();
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
        let active_background_tasks = background_snapshots
            .into_iter()
            .filter(|task| task.owner_scope.as_deref() == Some(session_id.as_str()))
            .map(|task| leveler_client_protocol::UiActiveBackgroundTask {
                task_id: task.id,
                program: task.program,
                args: task.args,
                elapsed_ms: task.duration_ms,
                pid: task.pid,
            })
            .collect();
        let task_projection = match db.session_facts_for(session_id).await {
            Ok(facts) => facts.and_then(|facts| {
                crate::session_projection::project_task(
                    &facts,
                    &crate::runtime_boot::StateDirBootLiveness::new(&app.layout.state_dir),
                    false,
                    false,
                )
                .ok()
            }),
            Err(_) => None,
        };
        let Some((task_status, task_terminal)) = task_projection else {
            return false;
        };
        let _ = events.send(RuntimeEvent::SessionOpened {
            session: UiSessionSnapshot {
                id: session_id.clone(),
                repository: record.repository,
                task_status: Some(task_status),
                task_terminal,
                goal: record.goal,
                model: Some(model.clone()),
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
                active_tools: live.active_tools,
                active_background_tasks,
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
        leveler_engine::EngineEvent::PlanUpdated { steps: vec![] },
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
    ui_message_at(payload, None)
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

fn ui_message_at(payload: &str, ordinal: Option<u64>) -> Option<UiMessage> {
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
    // Only the runtime's own notices: a user-role message whose first line is
    // exactly one of the headers the runtime writes.
    let kind = (role == UiRole::User
        && text.lines().next().is_some_and(|first| {
            leveler_agent::RUNTIME_NOTICE_HEADERS.contains(&first.trim_end())
        }))
    .then_some(leveler_client_protocol::UiMessageKind::RuntimeNotice);
    Some(UiMessage {
        id: MessageId::new(leveler_core::new_uuid_string()),
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
            goal: Arc::new(Mutex::new(GoalSteer::default())),
            children: children.clone(),
            tool_calls: Arc::new(Mutex::new(HashMap::new())),
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
            model: ModelRef::new("mock", "m"),
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
                hydrated.model = ModelRef::new("mock", "selected");
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
        assert_eq!(stored[&session_id].model, ModelRef::new("mock", "selected"));
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
        let mut app = Application::assemble(layout).unwrap();
        app.config.assist_prompt_suggestions = true;
        let app = Arc::new(app);
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
        let consumer = std::thread::spawn(move || {
            attempt_tx.send(()).unwrap();
            handle
                .block_on(client.send(ClientCommand::RequestPromptSuggestion {
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
mod memory_listing_tests {
    use super::*;

    fn project_service(dir: &std::path::Path) -> leveler_memory::MemoryService {
        leveler_memory::MemoryService::new(leveler_memory::MemoryRoots::new().with_project(
            leveler_memory::v2::ScopeRoots::new(
                leveler_memory::v2::MemoryScope::Project,
                dir.join("memory"),
                dir.join("memory-v2"),
            ),
        ))
    }

    fn user() -> leveler_memory::v2::Actor {
        leveler_memory::v2::Actor::User {
            invocation: leveler_memory::v2::UserInvocationId::mint("listing-test"),
        }
    }

    #[test]
    fn memory_listing_real_empty_preserves_shape_and_query_identity() {
        let directory = tempfile::tempdir().unwrap();
        let (events, mut receiver) = tokio::sync::broadcast::channel(4);
        for include_archived in [false, true] {
            send_memory_list(
                &events,
                &project_service(directory.path()),
                leveler_memory::MemoryLanes::ALL,
                directory.path(),
                include_archived,
                Some("memory-query".into()),
            );
            match receiver.try_recv().unwrap() {
                RuntimeEvent::MemoryList {
                    query_id,
                    active,
                    archived,
                    pending,
                    ..
                } => {
                    assert_eq!(query_id.unwrap().as_str(), "memory-query");
                    assert!(active.is_empty() && archived.is_empty() && pending.is_empty());
                }
                other => panic!("expected genuine empty listing, got {other:?}"),
            }
        }
    }

    /// A current record that cannot be read is a broken store, not an empty
    /// one: the listing must say so instead of presenting a short list.
    #[test]
    fn memory_listing_a_broken_record_is_reported_not_shown_as_empty() {
        let directory = tempfile::tempdir().unwrap();
        let service = project_service(directory.path());
        // A malformed document in the current root.
        std::fs::create_dir_all(directory.path().join("memory")).unwrap();
        std::fs::write(
            directory.path().join("memory").join("broken.md"),
            b"---\nid: [unterminated\n---\nbody",
        )
        .unwrap();
        let (events, mut receiver) = tokio::sync::broadcast::channel(4);
        send_memory_list(
            &events,
            &service,
            leveler_memory::MemoryLanes::ALL,
            directory.path(),
            true,
            Some("memory-query".into()),
        );
        assert!(
            matches!(
                receiver.try_recv().unwrap(),
                RuntimeEvent::Notification {
                    level: leveler_client_protocol::NotificationLevel::Warning,
                    ..
                }
            ),
            "an unreadable store must be visible"
        );
        assert!(
            receiver.try_recv().is_err(),
            "failed listing must not emit MemoryList"
        );
    }

    /// A credential-shaped record cannot enter the store at all, so no listing
    /// row can carry one. The refusal is the protection; the listing proves it.
    #[test]
    fn a_credential_shaped_write_never_reaches_a_listing_row() {
        let directory = tempfile::tempdir().unwrap();
        let service = project_service(directory.path());
        let secret = "ghp_abcdefghijklmnopqrstuvwxyz0123";
        let refused = service.remember(
            &user(),
            leveler_memory::RememberRequest::new(
                leveler_memory::v2::OpId::derived("listing", "secret"),
                leveler_memory::ServiceScope::Project,
                "Deploy token",
                format!("token={secret}"),
                "note",
            ),
        );
        assert!(refused.is_err(), "a credential must not be stored");

        let (events, mut receiver) = tokio::sync::broadcast::channel(4);
        send_memory_list(
            &events,
            &service,
            leveler_memory::MemoryLanes::ALL,
            directory.path(),
            true,
            Some("query".into()),
        );
        match receiver.try_recv().unwrap() {
            RuntimeEvent::MemoryList {
                active, pending, ..
            } => {
                let wire = format!("{active:?} {pending:?}");
                assert!(active.is_empty() && pending.is_empty(), "{wire}");
                assert!(!wire.contains(secret), "listing leaked the credential");
            }
            other => panic!("expected memory listing, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn memory_listing_directory_read_errors_are_not_empty_results() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let service = project_service(directory.path());
        // One current record so the store is initialized, then make the
        // current root unreadable.
        service
            .remember(
                &user(),
                leveler_memory::RememberRequest::new(
                    leveler_memory::v2::OpId::derived("listing", "one"),
                    leveler_memory::ServiceScope::Project,
                    "Deploy preference",
                    "Use foreground service startup.",
                    "note",
                ),
            )
            .unwrap();
        let path = directory.path().join("memory");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o0)).unwrap();
        let (events, mut receiver) = tokio::sync::broadcast::channel(4);
        send_memory_list(
            &events,
            &service,
            leveler_memory::MemoryLanes::ALL,
            directory.path(),
            true,
            Some("directory-query".into()),
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            matches!(
                receiver.try_recv().unwrap(),
                RuntimeEvent::Notification {
                    level: leveler_client_protocol::NotificationLevel::Warning,
                    ..
                }
            ),
            "a directory read failure must be visible"
        );
        assert!(receiver.try_recv().is_err());
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
