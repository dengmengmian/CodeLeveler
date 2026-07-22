//! [`InProcessRuntimeClient`] — the first [`InteractiveRuntimeClient`], bridging
//! the runtime's synchronous observer callbacks, cancellation tokens, and
//! approver into the async, broadcast-shaped client protocol the TUI consumes
//! .
//!
//! The runtime exposes progress as a synchronous `&mut dyn FnMut(AgentEvent)`
//! observer with no channel. This client wraps that callback in a closure that
//! forwards each event into a `tokio::sync::broadcast`, and drives the turn on a
//! blocking thread (the turn future is not `Send`) so `send` returns
//! immediately.
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
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use leveler_agent::AgentEvent;
use leveler_core::{CheckpointId, SessionId};
use leveler_execution::{Approver, AutoApprove, PermissionProfile};
use leveler_media::MediaStore;
use leveler_model::{
    ContentPart, ImageSource, Message, ModelRef, ModelRequest, ModelRuntime, Role, ToolChoice,
};
use leveler_storage::{MessageRepository, SessionRepository};

use leveler_client_protocol::{
    ApprovalDecision as UiApprovalDecision, AttachmentId, AttachmentKind, AttachmentRef,
    ClientCommand, ClientError, CommandEnvelope, InteractiveRuntimeClient, MessageId,
    NotificationLevel, RuntimeEvent, UiActiveToolCall, UiCheckpoint, UiCompletionReport, UiDiff,
    UiMessage, UiPlan, UiRole, UiSessionSnapshot, UiSessionSummary, UiVerification,
};

fn execution_decision(value: UiApprovalDecision) -> leveler_execution::ApprovalDecision {
    match value {
        UiApprovalDecision::ApproveOnce => leveler_execution::ApprovalDecision::ApproveOnce,
        UiApprovalDecision::ApproveSession => leveler_execution::ApprovalDecision::ApproveSession,
        UiApprovalDecision::ApproveAlways => leveler_execution::ApprovalDecision::ApproveAlways,
        UiApprovalDecision::Deny => leveler_execution::ApprovalDecision::Deny,
    }
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

use crate::event_bridge::{EventBridge, OrchestratorBridge, turn_runtime_event};
use crate::prompt_bridge::{
    ChannelApprover, ChannelClarifier, PendingApprovals, PendingClarifications, resolve_approval,
    resolve_clarification, validate_pending_session,
};
use crate::workspace_view::{build_report, compute_diff, detect_branch_label};

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
    let line = content.trim().lines().find(|l| !l.trim().is_empty())?.trim();
    let sentence = line
        .split(['。', '？', '！', '?', '!', '；', ';'])
        .next()
        .unwrap_or(line)
        .trim();
    let title: String = sentence.chars().take(40).collect();
    (!title.is_empty()).then_some(title)
}

fn emit_project_rules(events: &broadcast::Sender<RuntimeEvent>, repo: &Path) {
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
/// Pure policy: single mapping table for TUI, remote clients, and tests.
pub(crate) fn collaboration_routes_submit_to_goal(collaboration: &str) -> bool {
    collaboration.eq_ignore_ascii_case("goal")
}

use crate::Application;
use crate::active_turns::ActiveTurns;

/// An in-process runtime client backed by an [`Application`].
pub struct InProcessRuntimeClient {
    app: Arc<Application>,
    /// Defaults for sessions created by this runtime service.
    default_runtime: SessionRuntimeConfig,
    /// Model/mode/path selected independently by each live or restored session.
    session_runtime: Mutex<HashMap<SessionId, SessionRuntimeConfig>>,
    /// When true, skip the approval overlay (AutoApprove) so unattended TUI
    /// PTY drivers and CI dogfood can run interactive turns.
    auto_approve: bool,
    /// When true, turns run the full orchestrated state machine instead of the
    /// direct tool loop (spec §54).
    /// Root of the content-addressed image store (`<state_dir>/media`, under the
    /// global home — not the project).
    media_root: PathBuf,
    /// Compatibility stream containing events from every session.
    events: broadcast::Sender<RuntimeEvent>,
    /// Session-scoped streams used by daemon/socket clients.
    session_events: Mutex<HashMap<SessionId, broadcast::Sender<RuntimeEvent>>>,
    pending: PendingApprovals,
    pending_clarify: PendingClarifications,
    /// Conversation checkpoints, isolated by owning session (spec §68).
    /// Arc so the async compact worker can drop them after a successful rewrite.
    checkpoints: Arc<Mutex<HashMap<SessionId, Vec<UiCheckpoint>>>>,
    /// Workspace snapshots captured with each checkpoint (git repos only), so
    /// restore rolls back files, not just the transcript (plan B9).
    checkpoint_snapshots: Arc<Mutex<HashMap<CheckpointId, leveler_execution::SnapshotId>>>,
    /// Live client-facing state that is not part of the message transcript.
    /// A reconnecting UI receives this through `snapshot()`.
    live_views: Arc<Mutex<HashMap<SessionId, LiveSessionView>>>,
    /// Per-session ownership and cancellation of active main turns.
    active: Arc<ActiveTurns>,
}

#[derive(Debug, Clone)]
struct SessionRuntimeConfig {
    model: ModelRef,
    mode: PermissionProfile,
    sandbox: bool,
    orchestrate: bool,
    work_profile: String,
    collaboration: String,
}

#[derive(Debug, Clone, Default)]
struct LiveSessionView {
    active_tools: Vec<UiActiveToolCall>,
    plan: Option<UiPlan>,
    verification: Option<UiVerification>,
    diff: Option<UiDiff>,
    completion_report: Option<UiCompletionReport>,
}

fn update_live_view(
    views: &Mutex<HashMap<SessionId, LiveSessionView>>,
    session_id: &SessionId,
    event: &RuntimeEvent,
) {
    let mut views = views.lock().unwrap();
    let view = views.entry(session_id.clone()).or_default();
    match event {
        RuntimeEvent::ToolCallStarted {
            id,
            name,
            arguments,
            ..
        } => {
            view.active_tools.retain(|tool| tool.id != *id);
            view.active_tools.push(UiActiveToolCall {
                id: id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
            });
        }
        RuntimeEvent::ToolCallCompleted { id, .. } => {
            view.active_tools.retain(|tool| tool.id != *id);
        }
        RuntimeEvent::PlanUpdated { plan } => view.plan = Some(plan.clone()),
        RuntimeEvent::VerificationUpdated { verification } => {
            view.verification = Some(verification.clone());
        }
        RuntimeEvent::DiffUpdated { diff } => view.diff = Some(diff.clone()),
        RuntimeEvent::SessionCompleted { report } => {
            view.completion_report = Some(report.clone());
        }
        RuntimeEvent::UserMessageAdded { .. } => {
            view.completion_report = None;
        }
        RuntimeEvent::TurnCompleted
        | RuntimeEvent::TurnAnswered
        | RuntimeEvent::TurnTruncated { .. }
        | RuntimeEvent::TurnIncomplete { .. }
        | RuntimeEvent::TurnCompletedUnverified { .. }
        | RuntimeEvent::TurnFailed { .. }
        | RuntimeEvent::TurnCancelled => view.active_tools.clear(),
        _ => {}
    }
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
        let media_root = app.layout.state_dir.join("media");
        Self {
            app,
            default_runtime: SessionRuntimeConfig {
                model,
                mode,
                sandbox,
                orchestrate: false,
                work_profile: "balanced".into(),
                // Default: plain conversation (no update_goal gate).
                collaboration: "chat".into(),
            },
            session_runtime: Mutex::new(HashMap::new()),
            auto_approve,
            media_root,
            events,
            session_events: Mutex::new(HashMap::new()),
            pending: Arc::new(Mutex::new(HashMap::new())),
            pending_clarify: Arc::new(Mutex::new(HashMap::new())),
            checkpoints: Arc::new(Mutex::new(HashMap::new())),
            checkpoint_snapshots: Arc::new(Mutex::new(HashMap::new())),
            live_views: Arc::new(Mutex::new(HashMap::new())),
            active: Arc::new(ActiveTurns::default()),
        }
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
                        update_live_view(&live_views, &owned_session_id, &event);
                        let _ = all_events.send(event);
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "session event compatibility stream lagged");
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
    pub fn attach_session(&self, session_id: SessionId) {
        self.session_runtime
            .lock()
            .unwrap()
            .insert(session_id, self.default_runtime.clone());
    }

    async fn runtime_config(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionRuntimeConfig, ClientError> {
        if let Some(config) = self
            .session_runtime
            .lock()
            .unwrap()
            .get(session_id)
            .cloned()
        {
            return Ok(config);
        }
        let db = self
            .app
            .open_database()
            .await
            .map_err(|error| ClientError::Runtime(error.to_string()))?;
        let record = SessionRepository::new(&db)
            .get(session_id)
            .await
            .map_err(|error| ClientError::Runtime(error.to_string()))?
            .ok_or_else(|| ClientError::SessionNotFound(session_id.clone()))?;
        let model = ModelRef::parse(&record.model).ok_or_else(|| {
            ClientError::Runtime(format!(
                "session {} stores invalid model reference `{}`",
                session_id.as_str(),
                record.model
            ))
        })?;
        let (mode, sandbox, kind, _) = SessionRepository::new(&db)
            .execution(session_id)
            .await
            .map_err(|error| ClientError::Runtime(error.to_string()))?
            .ok_or_else(|| ClientError::SessionNotFound(session_id.clone()))?;
        let mode = crate::session::mode_from_str(&mode).ok_or_else(|| {
            ClientError::Runtime(format!(
                "session {} stores invalid execution mode `{mode}`",
                session_id.as_str()
            ))
        })?;
        let orchestrate = match kind.as_str() {
            "direct" => false,
            "orchestrate" | "orchestrated" => true,
            other => {
                return Err(ClientError::Runtime(format!(
                    "session {} stores invalid execution kind `{other}`",
                    session_id.as_str()
                )));
            }
        };
        let config = SessionRuntimeConfig {
            model,
            mode,
            sandbox,
            orchestrate,
            work_profile: record.work_profile.clone(),
            collaboration: record.collaboration.clone(),
        };
        self.session_runtime
            .lock()
            .unwrap()
            .insert(session_id.clone(), config.clone());
        Ok(config)
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
        sessions
            .update_model(session_id, &config.model.to_string(), leveler_core::now())
            .await
            .map_err(|error| ClientError::Runtime(error.to_string()))?;
        sessions
            .set_execution(
                session_id,
                config.mode.as_str(),
                config.sandbox,
                if config.orchestrate {
                    "orchestrate"
                } else {
                    "direct"
                },
                leveler_core::now(),
            )
            .await
            .map_err(|error| ClientError::Runtime(error.to_string()))?;
        sessions
            .set_axes(
                session_id,
                &config.collaboration,
                &config.work_profile,
                leveler_core::now(),
            )
            .await
            .map_err(|error| ClientError::Runtime(error.to_string()))?;
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
                .load(session_id)
                .await
                .map(|m| m.len())
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
        match leveler_execution::WorkspaceSnapshot::capture(&self.app.layout.repo_root).await {
            Ok(Some(snapshot)) => {
                self.checkpoint_snapshots
                    .lock()
                    .unwrap()
                    .insert(checkpoint.id.clone(), snapshot);
            }
            Ok(None) => {} // not a git repo: transcript-only checkpoints
            Err(error) => {
                tracing::warn!("checkpoint workspace snapshot failed: {error}");
            }
        }
        self.checkpoints
            .lock()
            .unwrap()
            .entry(session_id.clone())
            .or_default()
            .push(checkpoint.clone());
        let _ = self
            .events_for(session_id)
            .send(RuntimeEvent::CheckpointCreated { checkpoint });
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
    /// `None` reaps every still-running turn (process quit).
    async fn reap_running_turns(&self, session: Option<&SessionId>) {
        let Ok(db) = self.app.open_database().await else {
            return;
        };
        let result = leveler_engine::reap_running_turns(&db, session).await;
        match result {
            Ok(events) if events.is_empty() => {}
            Ok(events) => {
                let reaped = events.len();
                tracing::warn!(
                    reaped,
                    session = session.map(|s| s.as_str()),
                    "reaped zombie running turns"
                );
            }
            Err(error) => {
                tracing::warn!("failed to reap running turns: {error}");
            }
        }
    }

    /// Build the first user message's content parts from text and image
    /// attachments, loading (and base64-encoding) each image from the store.
    fn content_parts(&self, text: &str, attachments: &[AttachmentRef]) -> Vec<ContentPart> {
        let store = MediaStore::new(&self.media_root);
        let mut parts = Vec::new();
        if !text.trim().is_empty() {
            parts.push(ContentPart::Text {
                text: text.to_string(),
            });
        }
        for att in attachments {
            if att.kind == AttachmentKind::Image
                && let Ok((media_type, data)) = store.load_base64(&att.sha256)
            {
                parts.push(ContentPart::Image {
                    source: ImageSource::Base64 { media_type, data },
                });
            }
        }
        parts
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
    ) -> Result<CancellationToken, ClientError> {
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
        drop_session_checkpoints_maps(&self.checkpoints, &self.checkpoint_snapshots, session_id);
    }

    /// After restore to `ordinal`, drop later checkpoints (and their snapshots)
    /// so the UI cannot re-restore a point that no longer exists in the transcript.
    fn prune_checkpoints_after_restore(&self, session_id: &SessionId, restored_ordinal: u32) {
        let discarded = {
            let mut map = self.checkpoints.lock().unwrap();
            let Some(list) = map.get_mut(session_id) else {
                return;
            };
            let mut keep = Vec::new();
            let mut drop = Vec::new();
            for checkpoint in list.drain(..) {
                if checkpoint.ordinal <= restored_ordinal {
                    keep.push(checkpoint);
                } else {
                    drop.push(checkpoint);
                }
            }
            *list = keep;
            drop
        };
        if discarded.is_empty() {
            return;
        }
        let mut snaps = self.checkpoint_snapshots.lock().unwrap();
        for checkpoint in discarded {
            snaps.remove(&checkpoint.id);
        }
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
    async fn list_sessions(&self) -> Vec<UiSessionSummary> {
        let Ok(db) = self.app.open_database().await else {
            return Vec::new();
        };
        let first_texts = MessageRepository::new(&db)
            .first_user_texts()
            .await
            .unwrap_or_default();
        SessionRepository::new(&db)
            .list()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|r| {
                let goal = if r.goal == PLACEHOLDER_GOAL || r.goal.trim().is_empty() {
                    first_texts
                        .get(&r.id)
                        .and_then(|text| title_from_first_message(text))
                        .unwrap_or(r.goal)
                } else {
                    r.goal
                };
                UiSessionSummary {
                    id: SessionId::new(r.id),
                    goal,
                    status: r.status.as_str().to_string(),
                    model: r.model,
                    updated_at: r.updated_at,
                    repository: Some(self.app.layout.repo_root.display().to_string()),
                }
            })
            .collect()
    }

    fn approver(&self, session_id: &SessionId, cancel: CancellationToken) -> Arc<dyn Approver> {
        if self.auto_approve {
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
        cancel: CancellationToken,
        config: SessionRuntimeConfig,
    ) {
        let parts = self.content_parts(&content, &attachments);
        self.spawn_content_turn(session_id, parts, cancel, config);
    }

    fn spawn_goal_turn(
        &self,
        session_id: SessionId,
        content: String,
        cancel: CancellationToken,
        config: SessionRuntimeConfig,
    ) {
        if config.orchestrate {
            self.spawn_orchestrated_turn(session_id, content, cancel, config);
        } else {
            self.spawn_direct_goal_turn(session_id, content, cancel, config);
        }
    }

    fn spawn_direct_goal_turn(
        &self,
        session_id: SessionId,
        content: String,
        cancel: CancellationToken,
        config: SessionRuntimeConfig,
    ) {
        let app = self.app.clone();
        let events = self.events_for(&session_id);
        let active = self.active.clone();
        let model = config.model;
        let mode = config.mode;
        let sandbox = config.sandbox;
        let repo = self.app.layout.repo_root.clone();
        let approver = self.approver(&session_id, cancel.clone());
        let clarifier = self.clarifier(&session_id, cancel.clone());

        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            handle.block_on(async move {
                emit_project_rules(&events, &repo);
                let mut bridge = EventBridge::new(events.clone());
                let mut observer = |event: AgentEvent| bridge.forward(event);
                let result = app
                    .run_in_session_with_clarifier(
                        &session_id,
                        &model,
                        mode,
                        &content,
                        approver,
                        clarifier,
                        sandbox,
                        &mut observer,
                        cancel,
                    )
                    .await;
                let outcome = turn_runtime_event(result);
                let _ = events.send(outcome);
                active.finish(&session_id);
            });
        });
    }

    /// Side question: one generate call over a fork of the session transcript.
    /// Does not take the main turn cancel token, so a running agent turn keeps
    /// going; does not append to MessageRepository.
    fn spawn_btw(&self, session_id: SessionId, question: String, config: SessionRuntimeConfig) {
        let app = self.app.clone();
        let events = self.events_for(&session_id);
        let model = config.model;
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            handle.block_on(async move {
                let _ = events.send(RuntimeEvent::BtwStarted {
                    question: question.clone(),
                });

                let result: Result<String, String> = async {
                    let db = app.open_database().await.map_err(|e| e.to_string())?;
                    let payloads = MessageRepository::new(&db)
                        .load(&session_id)
                        .await
                        .map_err(|e| e.to_string())?;
                    let raw: Vec<Message> = payloads
                        .iter()
                        .filter_map(|p| serde_json::from_str(p).ok())
                        .collect();
                    // Same budgeted path as main turns — never dump unbounded raw history.
                    let snapshot = load_latest_context_snapshot(&db, &session_id).await;
                    // Advisory side-question: bare fold, no model summary call.
                    let (mut messages, _) = leveler_engine::budget_prior_messages(
                        raw,
                        snapshot,
                        None,
                        Some(question.as_str()),
                        leveler_agent::PRE_REQUEST_COMPACT_THRESHOLD,
                    );
                    messages.push(Message::text(
                        Role::User,
                        format!(
                            "【旁问 / btw】请用当前对话上下文简短回答下面的问题。\
                             不要调用任何工具，不要修改文件，不要继续主任务。\n\n{question}"
                        ),
                    ));
                    let mut request = ModelRequest::new(model, messages);
                    request.tool_choice = ToolChoice::None;
                    let resp = app
                        .registry
                        .generate(request, CancellationToken::new())
                        .await
                        .map_err(|e| e.to_string())?;
                    Ok(resp.message.text_content())
                }
                .await;

                match result {
                    Ok(text) => {
                        if !text.is_empty() {
                            let _ = events.send(RuntimeEvent::BtwTextDelta { delta: text });
                        }
                        let _ = events.send(RuntimeEvent::BtwCompleted);
                    }
                    Err(error) => {
                        let _ = events.send(RuntimeEvent::BtwFailed { error });
                    }
                }
            });
        });
    }

    fn spawn_content_turn(
        &self,
        session_id: SessionId,
        parts: Vec<ContentPart>,
        cancel: CancellationToken,
        config: SessionRuntimeConfig,
    ) {
        let app = self.app.clone();
        let events = self.events_for(&session_id);
        let active = self.active.clone();
        let model = config.model;
        let mode = config.mode;
        let sandbox = config.sandbox;
        let repo = self.app.layout.repo_root.clone();
        let approver = self.approver(&session_id, cancel.clone());
        let clarifier = self.clarifier(&session_id, cancel.clone());

        // The runtime's observer is `&mut dyn FnMut`, so the turn future is not
        // `Send` and cannot be `tokio::spawn`ed. Drive it on a blocking thread
        // with the current runtime handle — provider/DB clients stay on their
        // home runtime.
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            handle.block_on(async move {
                emit_project_rules(&events, &repo);
                let mut bridge = EventBridge::new(events.clone());
                let mut observer = |event: AgentEvent| bridge.forward(event);
                let result = app
                    .run_in_session_with_content(
                        &session_id,
                        &model,
                        mode,
                        parts,
                        approver,
                        clarifier,
                        sandbox,
                        &mut observer,
                        cancel,
                    )
                    .await;
                let outcome = turn_runtime_event(result);
                let _ = events.send(outcome);
                active.finish(&session_id);
            });
        });
    }

    /// Run a turn through the full orchestrator, surfacing plan, verification,
    /// diff, and a completion report (spec §20–§23, §54).
    fn spawn_orchestrated_turn(
        &self,
        session_id: SessionId,
        content: String,
        cancel: CancellationToken,
        config: SessionRuntimeConfig,
    ) {
        let cancel_probe = cancel.clone();
        let app = self.app.clone();
        let events = self.events_for(&session_id);
        let active = self.active.clone();
        let model = config.model;
        let mode = config.mode;
        let sandbox = config.sandbox;
        let approver = self.approver(&session_id, cancel.clone());
        let clarifier = self.clarifier(&session_id, cancel.clone());
        let repo = self.app.layout.repo_root.clone();

        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            handle.block_on(async move {
                emit_project_rules(&events, &repo);
                let mut bridge = OrchestratorBridge::new(events.clone());
                let mut observer = |event: leveler_engine::EngineEvent| bridge.forward(event);
                let result = app
                    .orchestrate_task(
                        &model,
                        mode,
                        &content,
                        approver,
                        clarifier,
                        sandbox,
                        &mut observer,
                        cancel,
                        Some(session_id.clone()),
                    )
                    .await;
                match result {
                    Ok((_session_id, report)) => {
                        // with_patch=true: the WebUI/TUI show the actual code diff
                        // (not just file counts) inline after each turn.
                        let diff = compute_diff(&repo, true);
                        let _ = events.send(RuntimeEvent::DiffUpdated { diff: diff.clone() });
                        if cancel_probe.is_cancelled() {
                            let _ = events.send(RuntimeEvent::TurnCancelled);
                        } else {
                            let _ = events.send(RuntimeEvent::SessionCompleted {
                                report: build_report(&report, &diff),
                            });
                            let _ = events.send(RuntimeEvent::TurnCompleted);
                        }
                    }
                    Err(_) if cancel_probe.is_cancelled() => {
                        let _ = events.send(RuntimeEvent::TurnCancelled);
                    }
                    Err(e) => {
                        let _ = events.send(RuntimeEvent::TurnFailed {
                            error: e.to_string(),
                        });
                    }
                }
                active.finish(&session_id);
            });
        });
    }
    /// Restore a checkpoint: roll back the transcript, task epoch, and (git)
    /// workspace to the checkpoint, surfacing any partial-failure honestly.
    async fn handle_restore_checkpoint(
        &self,
        session_id: SessionId,
        checkpoint_id: CheckpointId,
    ) -> Result<(), ClientError> {
        let _token = self.admit_context_op(&session_id, "恢复检查点")?;
        let ordinal = self
            .checkpoints
            .lock()
            .unwrap()
            .get(&session_id)
            .into_iter()
            .flatten()
            .find(|c| c.id == checkpoint_id)
            .map(|c| c.ordinal);
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
                    // Roll the workspace back to the checkpoint's
                    // snapshot (git repos). A failure is surfaced —
                    // the transcript rolled back but files did not.
                    let snapshot = self
                        .checkpoint_snapshots
                        .lock()
                        .unwrap()
                        .get(&checkpoint_id)
                        .cloned();
                    match snapshot {
                        Some(snapshot) => {
                            if let Err(error) = leveler_execution::WorkspaceSnapshot::restore(
                                &self.app.layout.repo_root,
                                &snapshot,
                            )
                            .await
                            {
                                self.notify_error(
                                    &session_id,
                                    format!("对话已回滚,但工作区文件回滚失败: {error}"),
                                );
                            } else {
                                let diff = compute_diff(&self.app.layout.repo_root, true);
                                let _ = self
                                    .events_for(&session_id)
                                    .send(RuntimeEvent::DiffUpdated { diff });
                            }
                        }
                        None => {
                            let _ = self
                                .events_for(&session_id)
                                .send(RuntimeEvent::Notification {
                                    level: NotificationLevel::Info,
                                    message: "已回滚对话;工作区非 git 仓库,文件未回滚".to_string(),
                                });
                        }
                    }
                    // Discard checkpoints that pointed past the restore.
                    self.prune_checkpoints_after_restore(&session_id, ordinal);
                    if let Ok(session) = self.snapshot(&session_id).await {
                        let _ = self
                            .events_for(&session_id)
                            .send(RuntimeEvent::SessionOpened { session });
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
        self.active.finish(&session_id);
        Ok(())
    }
    /// Compact the session's context off the request path, owning the
    /// session so Submit/clear/restore cannot race the transcript rewrite.
    async fn handle_compact_context(&self, session_id: SessionId) -> Result<(), ClientError> {
        // Own the session while compact runs so Submit/clear/restore
        // cannot race the transcript rewrite.
        let cancel = self.admit_context_op(&session_id, "压缩上下文")?;
        let app = self.app.clone();
        let events = self.events_for(&session_id);
        let config = match self.runtime_config(&session_id).await {
            Ok(config) => config,
            Err(error) => {
                self.active.finish(&session_id);
                // Surface the same way as in-flight compact failures.
                let _ = events.send(RuntimeEvent::TurnFailed {
                    error: format!("压缩失败：{error}"),
                });
                self.notify_error(&session_id, format!("压缩失败：{error}"));
                return Ok(());
            }
        };
        let model = config.model;
        let mode = config.mode;
        let active = self.active.clone();
        let checkpoints = self.checkpoints.clone();
        let checkpoint_snapshots = self.checkpoint_snapshots.clone();
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            handle.block_on(async move {
                let rewrote =
                    compact_conversation(&app, &events, &model, mode, &session_id, cancel).await;
                // Only wipe checkpoints when the transcript was actually
                // replaced — a short/no-op compact must keep them.
                if rewrote {
                    drop_session_checkpoints_maps(&checkpoints, &checkpoint_snapshots, &session_id);
                }
                active.finish(&session_id);
            });
        });
        Ok(())
    }
    /// Clear the conversation: drop stored messages and reset task epoch,
    /// surfacing a DB failure rather than showing a false-empty transcript.
    async fn handle_clear_conversation(&self, session_id: SessionId) -> Result<(), ClientError> {
        let _token = self.admit_context_op(&session_id, "清空会话")?;
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
                self.live_views.lock().unwrap().remove(&session_id);
                self.drop_session_checkpoints(&session_id);
                if let Ok(session) = self.snapshot(&session_id).await {
                    let _ = self
                        .events_for(&session_id)
                        .send(RuntimeEvent::SessionOpened { session });
                }
            }
            Err(error) => self.notify_error(&session_id, format!("清空会话失败: {error}")),
        }
        self.active.finish(&session_id);
        Ok(())
    }
}

#[async_trait]
impl InteractiveRuntimeClient for InProcessRuntimeClient {
    async fn send(&self, command: ClientCommand) -> Result<(), ClientError> {
        match command {
            ClientCommand::SubmitMessage {
                session_id,
                content,
                attachments,
            } => {
                let config = self.runtime_config(&session_id).await?;
                let cancel = self
                    .active
                    .admit(&session_id)
                    .map_err(|error| ClientError::Runtime(error.to_string()))?;
                self.retitle_placeholder_session(&session_id, &content).await;
                self.checkpoint_before_turn(&session_id, &content).await;
                let _ = self
                    .events_for(&session_id)
                    .send(RuntimeEvent::UserMessageAdded {
                        message: UiMessage {
                            id: MessageId::new(leveler_core::new_uuid_string()),
                            role: UiRole::User,
                            text: content.clone(),
                        },
                    });
                // CollaborationMode is the single source of turn profile:
                // goal → goal_mode / update_goal path; chat|plan → content turn.
                // Plan read_only is applied inside engine from session.collaboration.
                if collaboration_routes_submit_to_goal(&config.collaboration) {
                    if !attachments.is_empty() {
                        // Goal path is text-first; attachments still need the
                        // multimodal content turn (goal_mode stays false unless
                        // the user used /goal). Prefer content when media present.
                        self.spawn_turn(session_id, content, attachments, cancel, config);
                    } else {
                        self.spawn_goal_turn(session_id, content, cancel, config);
                    }
                } else {
                    self.spawn_turn(session_id, content, attachments, cancel, config);
                }
                Ok(())
            }
            ClientCommand::RunGoal {
                session_id,
                content,
            } => {
                let config = self.runtime_config(&session_id).await?;
                let cancel = self
                    .active
                    .admit(&session_id)
                    .map_err(|error| ClientError::Runtime(error.to_string()))?;
                self.retitle_placeholder_session(&session_id, &content).await;
                self.checkpoint_before_turn(&session_id, &content).await;
                let _ = self
                    .events_for(&session_id)
                    .send(RuntimeEvent::UserMessageAdded {
                        message: UiMessage {
                            id: MessageId::new(leveler_core::new_uuid_string()),
                            role: UiRole::User,
                            text: content.clone(),
                        },
                    });
                self.spawn_goal_turn(session_id, content, cancel, config);
                Ok(())
            }
            ClientCommand::AddAttachment { session_id, path } => {
                let media_root = self.media_root.clone();
                let events = self.events_for(&session_id);
                tokio::task::spawn_blocking(move || {
                    let store = MediaStore::new(&media_root);
                    let source = PathBuf::from(&path);
                    let name = source
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.clone());
                    match store.import_path(&source) {
                        Ok(stored) => {
                            let _ = events.send(RuntimeEvent::AttachmentAdded {
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
                                error: e.to_string(),
                            });
                        }
                    }
                });
                Ok(())
            }
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
                            let _ = events.send(RuntimeEvent::AttachmentAdded { attachment });
                        }
                        Err(e) => {
                            let _ = events.send(RuntimeEvent::AttachmentProcessingFailed {
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
            ClientCommand::SelectModel { session_id, model } => {
                if !self.app.model_refs().contains(&model) {
                    return Err(ClientError::Runtime(format!(
                        "model `{model}` is not configured"
                    )));
                }
                let mut config = self.runtime_config(&session_id).await?;
                config.model = model.clone();
                self.persist_runtime_config(&session_id, config).await?;
                // Persist as the global default so the next launch uses it.
                if let Err(err) =
                    crate::global_config::GlobalConfig::save_default_model(&model.to_string())
                {
                    tracing::warn!(%err, model = %model, "failed to persist default_model");
                }
                if let Ok(session) = self.snapshot(&session_id).await {
                    let _ = self
                        .events_for(&session_id)
                        .send(RuntimeEvent::SessionUpdated { session });
                }
                Ok(())
            }
            ClientCommand::SetPermissionProfile { session_id, mode } => {
                let mut config = self.runtime_config(&session_id).await?;
                config.mode = execution_mode(mode);
                self.persist_runtime_config(&session_id, config).await?;
                if let Ok(session) = self.snapshot(&session_id).await {
                    let _ = self
                        .events_for(&session_id)
                        .send(RuntimeEvent::SessionUpdated { session });
                }
                Ok(())
            }
            ClientCommand::SetAgentMode {
                session_id,
                orchestrate,
            } => {
                let mut config = self.runtime_config(&session_id).await?;
                config.orchestrate = orchestrate;
                self.persist_runtime_config(&session_id, config).await?;
                if let Ok(session) = self.snapshot(&session_id).await {
                    let _ = self
                        .events_for(&session_id)
                        .send(RuntimeEvent::SessionUpdated { session });
                }
                Ok(())
            }
            ClientCommand::SetProductAxes {
                session_id,
                work_profile,
                collaboration,
            } => {
                // Idle-only is enforced by the TUI; runtime still accepts while idle.
                let mut config = self.runtime_config(&session_id).await?;
                config.work_profile = work_profile;
                config.collaboration = collaboration.clone();
                // Collaboration::Plan forces Safe-only tools via ToolContext.read_only
                // (orthogonal to the permission profile).
                self.persist_runtime_config(&session_id, config).await?;
                if let Ok(session) = self.snapshot(&session_id).await {
                    let _ = self
                        .events_for(&session_id)
                        .send(RuntimeEvent::SessionUpdated { session });
                }
                Ok(())
            }
            ClientCommand::ConfirmPlanToGoal {
                session_id,
                content,
            } => {
                let mut config = self.runtime_config(&session_id).await?;
                config.collaboration = "goal".into();
                self.persist_runtime_config(&session_id, config.clone())
                    .await?;
                let goal = if content.trim().is_empty() {
                    "Execute the confirmed plan".to_string()
                } else {
                    content
                };
                let cancel = self
                    .active
                    .admit(&session_id)
                    .map_err(|error| ClientError::Runtime(error.to_string()))?;
                self.checkpoint_before_turn(&session_id, &goal).await;
                let _ = self
                    .events_for(&session_id)
                    .send(RuntimeEvent::UserMessageAdded {
                        message: UiMessage {
                            id: MessageId::new(leveler_core::new_uuid_string()),
                            role: UiRole::User,
                            text: goal.clone(),
                        },
                    });
                self.spawn_goal_turn(session_id, goal, cancel, config);
                Ok(())
            }
            ClientCommand::ListMemory {
                session_id,
                include_archived,
            } => {
                let memory_dir = self.app.layout.memory_dir();
                let events = self.events_for(&session_id);
                match leveler_memory::MemoryStore::open(&memory_dir) {
                    Ok(store) => {
                        let active = store
                            .list_active()
                            .unwrap_or_default()
                            .into_iter()
                            .map(|e| leveler_client_protocol::UiMemoryEntry {
                                id: e.id,
                                title: e.title,
                            })
                            .collect();
                        let archived = if include_archived {
                            store
                                .list_archived()
                                .unwrap_or_default()
                                .into_iter()
                                .map(|e| leveler_client_protocol::UiMemoryEntry {
                                    id: e.id,
                                    title: e.title,
                                })
                                .collect()
                        } else {
                            Vec::new()
                        };
                        let _ = events.send(RuntimeEvent::MemoryList {
                            memory_dir: memory_dir.display().to_string(),
                            active,
                            archived,
                        });
                    }
                    Err(err) => {
                        let _ = events.send(RuntimeEvent::Notification {
                            level: leveler_client_protocol::NotificationLevel::Warning,
                            message: format!("memory open failed: {err}"),
                        });
                    }
                }
                Ok(())
            }
            ClientCommand::ForgetMemory { session_id, id } => {
                let memory_dir = self.app.layout.memory_dir();
                let events = self.events_for(&session_id);
                match leveler_memory::MemoryStore::open(&memory_dir).and_then(|s| s.forget(&id)) {
                    Ok(entry) => {
                        let _ = events.send(RuntimeEvent::Notification {
                            level: leveler_client_protocol::NotificationLevel::Info,
                            message: format!("archived memory [{}]: {}", entry.id, entry.title),
                        });
                        // Refresh list after forget.
                        if let Ok(store) = leveler_memory::MemoryStore::open(&memory_dir) {
                            let active = store
                                .list_active()
                                .unwrap_or_default()
                                .into_iter()
                                .map(|e| leveler_client_protocol::UiMemoryEntry {
                                    id: e.id,
                                    title: e.title,
                                })
                                .collect();
                            let archived = store
                                .list_archived()
                                .unwrap_or_default()
                                .into_iter()
                                .map(|e| leveler_client_protocol::UiMemoryEntry {
                                    id: e.id,
                                    title: e.title,
                                })
                                .collect();
                            let _ = events.send(RuntimeEvent::MemoryList {
                                memory_dir: memory_dir.display().to_string(),
                                active,
                                archived,
                            });
                        }
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
            ClientCommand::RequestDiff { session_id } => {
                let repo = self.app.layout.repo_root.clone();
                let events = self.events_for(&session_id);
                tokio::task::spawn_blocking(move || {
                    let diff = compute_diff(&repo, true);
                    let _ = events.send(RuntimeEvent::DiffUpdated { diff });
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
                    sessions: self.list_sessions().await,
                });
                Ok(())
            }
            ClientCommand::RequestSessionListFor {
                requester_session_id,
            } => {
                let _ = self
                    .events_for(&requester_session_id)
                    .send(RuntimeEvent::SessionList {
                        sessions: self.list_sessions().await,
                    });
                Ok(())
            }
            ClientCommand::OpenSession { session_id } => {
                if let Ok(session) = self.snapshot(&session_id).await {
                    let _ = self.events.send(RuntimeEvent::SessionOpened { session });
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
                    SessionRepository::new(&db).delete(&session_id).await?;
                    Ok(())
                }
                .await;
                if let Err(error) = deleted {
                    let _ = self.events.send(RuntimeEvent::Notification {
                        level: NotificationLevel::Error,
                        message: format!("删除会话失败: {error}"),
                    });
                }
                let _ = self.events.send(RuntimeEvent::SessionList {
                    sessions: self.list_sessions().await,
                });
                Ok(())
            }
            ClientCommand::DeleteSessionFor {
                requester_session_id,
                session_id,
            } => {
                let deleted: Result<(), anyhow::Error> = async {
                    let db = self.app.open_database().await?;
                    SessionRepository::new(&db).delete(&session_id).await?;
                    Ok(())
                }
                .await;
                if let Err(error) = deleted {
                    self.notify_error(&requester_session_id, format!("删除会话失败: {error}"));
                }
                let _ = self
                    .events_for(&requester_session_id)
                    .send(RuntimeEvent::SessionList {
                        sessions: self.list_sessions().await,
                    });
                Ok(())
            }
            ClientCommand::RenameSession { session_id, name } => {
                let name = name.trim().to_string();
                let renamed: Result<(), anyhow::Error> = async {
                    if name.is_empty() {
                        anyhow::bail!("名称不能为空");
                    }
                    let db = self.app.open_database().await?;
                    SessionRepository::new(&db)
                        .update_goal(&session_id, &name)
                        .await?;
                    Ok(())
                }
                .await;
                if let Err(error) = renamed {
                    self.notify_error(&session_id, format!("重命名会话失败: {error}"));
                }
                let _ = self.events.send(RuntimeEvent::SessionList {
                    sessions: self.list_sessions().await,
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
                }
                let _ = self.events.send(RuntimeEvent::SessionList {
                    sessions: self.list_sessions().await,
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
                    let title = if record.goal == PLACEHOLDER_GOAL {
                        record.goal.clone()
                    } else {
                        format!("{} (分叉)", record.goal)
                    };
                    let fork =
                        leveler_storage::SessionRecord::new(
                            record.repository.clone(),
                            title,
                            record.model.clone(),
                            leveler_core::now(),
                        )
                        .with_axes(&record.collaboration, &record.work_profile);
                    sessions.create(&fork).await?;
                    let fork_id = SessionId::new(fork.id.clone());
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
                        let _ = self.events.send(RuntimeEvent::Notification {
                            level: NotificationLevel::Info,
                            message: format!("已分叉会话: {fork_id}"),
                        });
                    }
                    Err(error) => {
                        self.notify_error(&session_id, format!("分叉会话失败: {error}"));
                    }
                }
                let _ = self.events.send(RuntimeEvent::SessionList {
                    sessions: self.list_sessions().await,
                });
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
            ClientCommand::CancelCurrentTurn { session_id } => {
                if !self.cancel_active(&session_id) {
                    // No owned live turn: recover a possible orphan left by an
                    // earlier process. Never race the active executor's own
                    // terminal transition with the reaper.
                    self.reap_running_turns(Some(&session_id)).await;
                }
                Ok(())
            }
            ClientCommand::ForceCancelCurrentTurn { session_id } => {
                if !self.cancel_active(&session_id) {
                    self.reap_running_turns(Some(&session_id)).await;
                }
                Ok(())
            }
            ClientCommand::Quit => {
                self.active.cancel_all();
                // Process is exiting: reaper is the safety net for turns that
                // never got finish() because the OS killed the process mid-flight.
                self.reap_running_turns(None).await;
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
            Some(leveler_storage::Admission::Uncertain) => {
                return Err(ClientError::Runtime(format!(
                    "command {} died mid-dispatch on a prior attempt; its effect is uncertain — \
                     verify state and resubmit with a fresh command id",
                    envelope.command_id.as_str()
                )));
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

        // Optimistic concurrency: reject a command issued against a stale view
        // *before* consuming its id, so the client can resync and reissue with a
        // fresh id rather than have this one silently swallowed as a duplicate.
        if let Some(expected) = envelope.expected_version {
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
        match receipts
            .admit(
                &envelope.command_id,
                &envelope.session_id,
                &command_fingerprint,
                &envelope.issued_at,
                leveler_core::now(),
            )
            .await
            .map_err(|e| ClientError::Runtime(e.to_string()))?
        {
            leveler_storage::Admission::AlreadyCompleted => return Ok(()),
            leveler_storage::Admission::Uncertain => {
                return Err(ClientError::Runtime(format!(
                    "command {} died mid-dispatch on a prior attempt; its effect is uncertain — \
                     verify state and resubmit with a fresh command id",
                    envelope.command_id.as_str()
                )));
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
        match self.send(envelope.command).await {
            Ok(()) => {
                receipts
                    .mark_completed(&command_id)
                    .await
                    .map_err(|e| ClientError::Runtime(e.to_string()))?;
                Ok(())
            }
            Err(error) => {
                // A returned error does not prove the command had no partial
                // effect. Leave the durable receipt in `dispatching`, so a
                // retry is surfaced as Uncertain instead of blindly executing
                // the command again. Only a caller with positive evidence that
                // nothing started may explicitly mark a receipt retryable.
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
        let record = SessionRepository::new(&db)
            .get(session_id)
            .await
            .map_err(|e| ClientError::Runtime(e.to_string()))?
            .ok_or_else(|| ClientError::SessionNotFound(session_id.clone()))?;

        let payloads = MessageRepository::new(&db)
            .load(session_id)
            .await
            .map_err(|e| ClientError::Runtime(e.to_string()))?;
        let messages = payloads.iter().filter_map(|p| ui_message(p)).collect();

        let mut available_models = self.app.model_refs();
        available_models.sort_by_key(|m| m.to_string());

        let config = self.runtime_config(session_id).await?;
        let model = config.model.clone();
        // Whether the current model accepts images (spec §42).
        let vision = self
            .app
            .registry
            .profile(&model)
            .await
            .map(|p| p.capabilities.vision)
            .unwrap_or(false);

        let last_sequence = leveler_storage::EventRepository::new(&db)
            .latest_sequence(session_id)
            .await
            .map_err(|e| ClientError::Runtime(e.to_string()))?;

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

        let live = self
            .live_views
            .lock()
            .unwrap()
            .get(session_id)
            .cloned()
            .unwrap_or_default();
        let checkpoints = self
            .checkpoints
            .lock()
            .unwrap()
            .get(session_id)
            .cloned()
            .unwrap_or_default();

        Ok(UiSessionSnapshot {
            id: session_id.clone(),
            repository: record.repository,
            goal: record.goal,
            model: Some(model),
            mode: protocol_mode(config.mode),
            branch: detect_branch_label(&self.app.layout.repo_root),
            status: record.status.as_str().to_string(),
            messages,
            pending_interactions,
            available_models,
            vision,
            last_sequence,
            active_tools: live.active_tools,
            plan: live.plan,
            verification: live.verification,
            diff: live.diff,
            checkpoints,
            completion_report: live.completion_report,
        })
    }
}

#[async_trait]
impl leveler_local_transport::LocalRuntimeService for InProcessRuntimeClient {
    async fn create_session(
        &self,
        request: leveler_local_transport::CreateSessionRequest,
    ) -> Result<leveler_local_transport::SessionBootstrap, ClientError> {
        let model = request
            .model
            .unwrap_or_else(|| self.default_runtime.model.clone());
        if !self.app.model_refs().contains(&model) {
            return Err(ClientError::Runtime(format!(
                "model `{model}` is not configured"
            )));
        }
        let session_id = self
            .app
            .create_daemon_session(&model, &request.goal)
            .await
            .map_err(|error| ClientError::Runtime(error.to_string()))?;
        self.persist_runtime_config(
            &session_id,
            SessionRuntimeConfig {
                model: model.clone(),
                mode: execution_mode(request.mode),
                sandbox: self.default_runtime.sandbox,
                orchestrate: false,
                work_profile: self.default_runtime.work_profile.clone(),
                collaboration: self.default_runtime.collaboration.clone(),
            },
        )
        .await?;
        let session = self.snapshot(&session_id).await?;
        let context_window = self
            .app
            .registry
            .profile(&model)
            .await
            .map(|profile| profile.limits.context_window)
            .map_err(|error| ClientError::Runtime(error.to_string()))?;
        Ok(leveler_local_transport::SessionBootstrap {
            session,
            context_window,
        })
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

/// Drop UI checkpoints + workspace snapshots for a session (shared with the
/// async compact worker that only holds map handles).
fn drop_session_checkpoints_maps(
    checkpoints: &Mutex<HashMap<SessionId, Vec<UiCheckpoint>>>,
    checkpoint_snapshots: &Mutex<HashMap<CheckpointId, leveler_execution::SnapshotId>>,
    session_id: &SessionId,
) {
    // `&Mutex` so both Arc and owned Mutex can call via as_ref.
    let removed = checkpoints
        .lock()
        .unwrap()
        .remove(session_id)
        .unwrap_or_default();
    if removed.is_empty() {
        return;
    }
    let mut snaps = checkpoint_snapshots.lock().unwrap();
    for checkpoint in removed {
        snaps.remove(&checkpoint.id);
    }
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
    model: &ModelRef,
    mode: PermissionProfile,
    session_id: &SessionId,
    cancellation: CancellationToken,
) -> bool {
    let notify = |level: NotificationLevel, message: String| {
        let _ = events.send(RuntimeEvent::Notification { level, message });
    };
    let fail = |message: String| {
        let _ = events.send(RuntimeEvent::TurnFailed {
            error: message.clone(),
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
    request_messages.push(Message::text(Role::User, COMPACT_PROMPT));
    let request = ModelRequest::new(model.clone(), request_messages);
    // Show a spinner while the (blocking, non-streaming) summary is generated —
    // otherwise the whole briefing "appears out of nowhere" with no feedback.
    let _ = events.send(RuntimeEvent::AgentActivity {
        label: "正在压缩上下文…".to_string(),
    });
    let summary = match app.registry.generate(request, cancellation).await {
        Ok(resp) => resp.message.text_content(),
        Err(e) => {
            fail(format!("压缩失败：{e}"));
            return false;
        }
    };

    let summary_msg = Message::text(
        Role::User,
        format!(
            "{}：\n{summary}",
            leveler_client_protocol::COMPACTION_SUMMARY_PREFIX
        ),
    );
    // All-or-nothing: serialize first, then atomic replace. Never truncate
    // before a successful write (append failure must not wipe history).
    let payload = match serde_json::to_string(&summary_msg) {
        Ok(p) => p,
        Err(e) => {
            fail(format!("压缩失败：序列化摘要出错: {e}"));
            return false;
        }
    };
    if let Err(e) = repo
        .replace_all(session_id, &[payload], leveler_core::now())
        .await
    {
        fail(format!("压缩失败：写入摘要出错（原历史未改动）: {e}"));
        return false;
    }
    // Cut Plan/Evidence/Progress and replace ContextSnapshot with the summary
    // so later turns never re-merge a pre-compact snapshot.
    if let Err(e) = reset_session_task_epoch_db(&db, session_id, vec![summary_msg.clone()]).await {
        notify(
            NotificationLevel::Warning,
            format!("已压缩历史,但任务状态/快照重置失败: {e}"),
        );
    }

    if let Ok(Some(record)) = SessionRepository::new(&db).get(session_id).await {
        let messages = repo
            .load(session_id)
            .await
            .unwrap_or_default()
            .iter()
            .filter_map(|p| ui_message(p))
            .collect();
        let mut available_models = app.model_refs();
        available_models.sort_by_key(|m| m.to_string());
        let vision = app
            .registry
            .profile(model)
            .await
            .map(|p| p.capabilities.vision)
            .unwrap_or(false);
        let last_sequence = leveler_storage::EventRepository::new(&db)
            .latest_sequence(session_id)
            .await
            .ok()
            .flatten();
        let _ = events.send(RuntimeEvent::SessionOpened {
            session: UiSessionSnapshot {
                id: session_id.clone(),
                repository: record.repository,
                goal: record.goal,
                model: Some(model.clone()),
                mode: protocol_mode(mode),
                branch: detect_branch_label(&app.layout.repo_root),
                status: record.status.as_str().to_string(),
                messages,
                pending_interactions: vec![],
                available_models,
                vision,
                last_sequence,
                active_tools: Vec::new(),
                plan: None,
                verification: None,
                diff: None,
                checkpoints: Vec::new(),
                completion_report: None,
            },
        });
    }
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
async fn reset_session_task_epoch_db(
    db: &leveler_storage::Database,
    session_id: &SessionId,
    model_visible: Vec<Message>,
) -> Result<(), anyhow::Error> {
    let repo = leveler_storage::EventRepository::new(db);
    let now = leveler_core::now();
    // Order: invalidate snapshot first so a partial failure cannot leave a
    // fresh Progress with a stale ContextSnapshot still "latest".
    let events = [
        leveler_engine::EngineEvent::ContextSnapshot {
            messages: model_visible,
        },
        leveler_engine::EngineEvent::ProgressUpdated {
            ledger: leveler_lifecycle::ProgressLedger::new_context_epoch(),
        },
        leveler_engine::EngineEvent::PlanUpdated { steps: vec![] },
        leveler_engine::EngineEvent::EvidenceLedgerUpdated {
            ledger: leveler_lifecycle::EvidenceLedger::default(),
        },
    ];
    for event in events {
        let (tag, payload) = event.to_row()?;
        repo.append(session_id, None, &tag, &payload, now).await?;
    }
    Ok(())
}

/// Latest ContextSnapshot from the event log (same source engine turns use).
async fn load_latest_context_snapshot(
    db: &leveler_storage::Database,
    session_id: &SessionId,
) -> Option<Vec<Message>> {
    let rows = leveler_storage::EventRepository::new(db)
        .load(session_id)
        .await
        .ok()?;
    let mut last = None;
    for row in rows {
        if let Ok(leveler_engine::EngineEvent::ContextSnapshot { messages }) =
            leveler_engine::EngineEvent::from_payload(&row.payload)
        {
            last = Some(messages);
        }
    }
    last
}

/// Deserialize a persisted `Message` payload into a `UiMessage`, or `None` for
/// tool/empty messages that have nothing to render.
fn ui_message(payload: &str) -> Option<UiMessage> {
    let message: leveler_model::Message = serde_json::from_str(payload).ok()?;
    let role = match message.role {
        Role::User => UiRole::User,
        Role::Assistant => UiRole::Assistant,
        Role::System => UiRole::System,
        Role::Tool => UiRole::Tool,
    };
    let text = message.text_content();
    if text.trim().is_empty() {
        return None;
    }
    Some(UiMessage {
        id: MessageId::new(leveler_core::new_uuid_string()),
        role,
        text,
    })
}

/// The ordinal a new checkpoint records, or `None` to skip creating one.
/// A load failure must NEVER fall back to 0: restoring a 0-ordinal checkpoint
/// truncates the whole transcript.
fn checkpoint_ordinal(loaded: Result<usize, String>) -> Option<usize> {
    loaded.ok()
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
                Ok(leveler_engine::EngineEvent::ContextSnapshot { messages }) => {
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
            if let Ok(leveler_engine::EngineEvent::ContextSnapshot { messages }) =
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

    #[test]
    fn btw_history_builder_uses_budgeted_path_not_raw_dump() {
        // Same function /btw calls: oversized raw history is folded under threshold.
        let mut raw = Vec::new();
        for i in 0..80 {
            raw.push(Message::text(
                Role::User,
                format!("history line {i} with enough padding to burn tokens xxxxxxxx"),
            ));
            raw.push(Message::text(
                Role::Assistant,
                format!("reply {i} also padded so estimate_tokens exceeds a tiny budget"),
            ));
        }
        let before = raw.len();
        let (budgeted, _) = leveler_engine::budget_prior_messages(
            raw,
            None,
            None,
            Some("side question"),
            // Force compaction path with a tiny threshold.
            500,
        );
        assert!(
            budgeted.len() < before,
            "btw must not send full raw transcript: before={before} after={}",
            budgeted.len()
        );
        assert!(
            leveler_agent::estimate_tokens(&budgeted) <= 2_000
                || budgeted.len() <= leveler_agent::COMPACT_KEEP_RECENT + 4,
            "budgeted btw history must be bounded"
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

    #[test]
    fn drop_session_checkpoints_removes_snapshot_entries() {
        use leveler_core::CheckpointId;
        use leveler_execution::SnapshotId;

        let session = SessionId::new("s-drop");
        let id_a = CheckpointId::new("a");
        let id_b = CheckpointId::new("b");
        let other = SessionId::new("other");
        let id_other = CheckpointId::new("o");

        let checkpoints = Mutex::new(HashMap::from([
            (
                session.clone(),
                vec![
                    UiCheckpoint {
                        id: id_a.clone(),
                        label: "a".into(),
                        ordinal: 1,
                    },
                    UiCheckpoint {
                        id: id_b.clone(),
                        label: "b".into(),
                        ordinal: 2,
                    },
                ],
            ),
            (
                other.clone(),
                vec![UiCheckpoint {
                    id: id_other.clone(),
                    label: "o".into(),
                    ordinal: 0,
                }],
            ),
        ]));
        let snaps = Mutex::new(HashMap::from([
            (id_a.clone(), SnapshotId("snap-a".into())),
            (id_b.clone(), SnapshotId("snap-b".into())),
            (id_other.clone(), SnapshotId("snap-o".into())),
        ]));

        drop_session_checkpoints_maps(&checkpoints, &snaps, &session);
        assert!(!checkpoints.lock().unwrap().contains_key(&session));
        assert!(checkpoints.lock().unwrap().contains_key(&other));
        let snaps = snaps.lock().unwrap();
        assert!(!snaps.contains_key(&id_a));
        assert!(!snaps.contains_key(&id_b));
        assert!(snaps.contains_key(&id_other));
    }
}

#[cfg(test)]
mod live_view_tests {
    use super::*;
    use leveler_core::ToolCallId;

    #[test]
    fn live_view_tracks_only_tools_that_are_still_running() {
        let session_id = SessionId::new("s1");
        let views = Mutex::new(HashMap::new());
        let id = ToolCallId::new("tool-1");

        update_live_view(
            &views,
            &session_id,
            &RuntimeEvent::ToolCallStarted {
                id: id.clone(),
                name: "run_command".to_string(),
                arguments: r#"{"cmd":"cargo test"}"#.to_string(),
                parallel: false,
            },
        );
        assert_eq!(
            views
                .lock()
                .unwrap()
                .get(&session_id)
                .unwrap()
                .active_tools
                .len(),
            1
        );

        update_live_view(
            &views,
            &session_id,
            &RuntimeEvent::ToolCallCompleted {
                id,
                ok: true,
                preview: "ok".to_string(),
                duration_ms: 1,
            },
        );
        assert!(
            views
                .lock()
                .unwrap()
                .get(&session_id)
                .unwrap()
                .active_tools
                .is_empty()
        );
    }
}
