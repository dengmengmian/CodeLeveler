//! Automatic memory capture.
//!
//! After a top-level turn settles, the runtime enqueues the newly covered turn
//! range in the durable inbox and runs one worker per session:
//!
//! ```text
//! settled turns -> durable capture job -> extraction model call -> commit
//! ```
//!
//! The job row, not this worker, is the source of truth. A crash at any point
//! leaves either a pending job or an orphan file set that
//! [`CaptureStore::reconcile`] repairs on the next claim, so restarting the
//! process never loses or duplicates a turn. Extraction failures never fail the
//! user's turn: they are recorded on the job and surfaced as memory activity.

use std::sync::Arc;
use std::time::Duration;

use leveler_core::SessionId;
use leveler_memory::capture_model::{
    CAPTURE_SYSTEM_PROMPT, CAPTURE_TOOL_NAME, extraction_schema, extraction_user_message,
    parse_model_outcome,
};
use leveler_memory::capture_transcript::{TranscriptBudget, condense_turn_transcript};
use leveler_memory::{
    CaptureAdmission, CaptureOutcomeDraft, CapturePlanStep, CaptureRange, CaptureStore,
    ClaimRequest, MemoryScope, plan_capture_admissions,
};
use leveler_model::{
    ContentPart, FinishReason, ModelRef, ModelRequest, ModelRuntime, Role, ToolChoice,
    ToolDefinition,
};
use leveler_storage::{
    Database, MemoryCaptureDisposition, MessageRepository, TurnRecord, TurnRepository,
};
use tokio_util::sync::CancellationToken;

use super::{MemoryActivity, MemoryActivityKind, MemoryRuntime};
use crate::config_manager::ConfigManager;

/// Matches the reference lease: long enough for a large transcript, short
/// enough that a crashed worker's job is reclaimable in one sitting.
const CAPTURE_LEASE: Duration = Duration::from_secs(5 * 60);
const EXTRACTION_TIMEOUT: Duration = Duration::from_secs(2 * 60);
const EXTRACTION_MAX_OUTPUT_TOKENS: u32 = 16 * 1024;
/// Bounded retries. A terminal failure advances the captured cursor so one
/// unlucky range cannot block every later turn forever.
const MAX_CAPTURE_ATTEMPTS: u32 = 3;

/// The tool schema the extractor must answer with.
fn capture_tool() -> ToolDefinition {
    ToolDefinition {
        name: CAPTURE_TOOL_NAME.to_owned(),
        description: "Record the durable observations extracted from the supplied turn range. \
                      Call exactly once with the full outcome."
            .to_owned(),
        input_schema: extraction_schema(),
    }
}

/// A settled top-level conversation turn. Internal node/repair turns are part
/// of a larger turn's mechanics, not separate user prompts.
fn is_capture_eligible(turn: &TurnRecord) -> bool {
    matches!(turn.kind.as_str(), "user" | "chat") && turn.status != "running"
}

impl crate::Application {
    /// Queue a settled turn and make one attempt to drain the queue.
    ///
    /// Called after `run_collaboration_turn` on every entry point. It never
    /// changes the turn's outcome: a memory failure is logged and exposed as
    /// activity, because the user's work already succeeded.
    pub(crate) async fn memory_after_turn(&self, session_id: &SessionId, model: &ModelRef) {
        let runtime = self.memory.clone();
        // A temporary workspace and a paused or globally disabled session do
        // not capture. The session row still records that decision so a later
        // enable cannot backfill the turn. That write stays on the session
        // database and does not create a memory store.
        let capture_allowed = runtime.config_for(Some(session_id.as_str())).can_capture()
            && !runtime.is_workspace_ephemeral();
        let db = match self.open_database().await {
            Ok(db) => db,
            Err(error) => {
                tracing::warn!(%error, "memory capture could not open the session database");
                return;
            }
        };
        if let Err(error) = runtime
            .freeze_latest_capture_admission(&db, session_id, capture_allowed)
            .await
        {
            tracing::warn!(%error, "memory capture could not record turn eligibility");
            return;
        }
        if !capture_allowed {
            return;
        }
        match runtime.enqueue_missing(&db, session_id).await {
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(%error, "memory capture could not queue the settled turn");
                runtime.record_activity(
                    session_id.as_str(),
                    MemoryActivity {
                        kind: MemoryActivityKind::Failed,
                        last_error: Some(error),
                        ..Default::default()
                    },
                );
                return;
            }
        }
        runtime.kick_capture_worker(db, session_id.clone(), model.clone(), self.config.clone());
    }

    /// The memory runtime this application owns.
    pub fn memory(&self) -> &MemoryRuntime {
        &self.memory
    }
}

impl MemoryRuntime {
    /// Record capture eligibility for the latest settled user/chat turn.
    ///
    /// The first decision sticks. A turn that settled while this session was
    /// paused, or while memory was globally off, stays ineligible after memory
    /// comes back. Turns that finished before this process started are left
    /// unmarked so they are not backfilled.
    pub(crate) async fn freeze_latest_capture_admission(
        &self,
        db: &Database,
        session_id: &SessionId,
        capture_allowed: bool,
    ) -> Result<(), String> {
        let turns = TurnRepository::new(db)
            .list(session_id)
            .await
            .map_err(|error| error.to_string())?;
        let Some(latest) = turns.iter().rev().find(|turn| is_capture_eligible(turn)) else {
            return Ok(());
        };
        if !turn_finished_during_process(latest.finished_at.as_deref(), self.process_started) {
            return Ok(());
        }
        let repo = TurnRepository::new(db);
        if repo
            .memory_capture_disposition(&latest.id)
            .await
            .map_err(|error| error.to_string())?
            .is_some()
        {
            return Ok(());
        }
        let disposition = if capture_allowed {
            MemoryCaptureDisposition::Eligible
        } else {
            MemoryCaptureDisposition::Ineligible
        };
        repo.admit_memory_capture(session_id, &latest.id, disposition, leveler_core::now())
            .await
            .map_err(|error| error.to_string())
    }

    /// Enqueue capture jobs for turns that were eligible when they settled.
    ///
    /// Paused and globally disabled turns advance the cursor as an explicit
    /// skip, so they are not captured later and do not stall the turns that
    /// follow. A settled turn with no persisted decision is not eligible:
    /// historical sessions are not backfilled, and a lost decision does not
    /// stall the cursor. Already queued jobs stay recoverable through the
    /// claim lease.
    pub(crate) async fn enqueue_missing(
        &self,
        db: &Database,
        session_id: &SessionId,
    ) -> Result<Option<CaptureRange>, String> {
        let config = self.config_for(Some(session_id.as_str()));
        if !config.can_capture() {
            return Ok(None);
        }
        let turns = TurnRepository::new(db)
            .list(session_id)
            .await
            .map_err(|error| error.to_string())?;
        let dispositions = TurnRepository::new(db)
            .memory_capture_dispositions(session_id)
            .await
            .map_err(|error| error.to_string())?;
        let mut admissions = Vec::new();
        for turn in turns.iter().filter(|turn| is_capture_eligible(turn)) {
            // A missing row is not eligible. That covers turns from before
            // this ledger and any turn whose decision was lost. The latest
            // turn is admitted just before this plan, so a turn that settled
            // while memory was on still has an eligible row.
            let admission = match dispositions.get(&turn.id) {
                Some(MemoryCaptureDisposition::Eligible) => CaptureAdmission::Eligible,
                Some(MemoryCaptureDisposition::Ineligible) | None => CaptureAdmission::NotEligible,
            };
            admissions.push(admission);
        }
        let exposes = config.rollout.exposes_memory();
        let storage = self.storage.clone();
        let session_id = session_id.as_str().to_owned();
        tokio::task::spawn_blocking(move || {
            apply_capture_plan(&storage, &session_id, &admissions, exposes)
        })
        .await
        .map_err(|error| format!("memory capture enqueue task failed: {error}"))?
    }

    /// Start the session's capture worker unless one is already running.
    pub(crate) fn kick_capture_worker(
        &self,
        db: Database,
        session_id: SessionId,
        model: ModelRef,
        config: Arc<ConfigManager>,
    ) {
        let key = session_id.as_str().to_owned();
        let Some(cancel) = self.try_begin_capture_worker(&key) else {
            // A running worker claims newly queued jobs before it exits.
            return;
        };
        let runtime = self.clone();
        tokio::spawn(async move {
            let produced =
                run_capture_worker(&runtime, &config, &db, &session_id, &model, cancel.clone())
                    .await;
            runtime.finish_capture_worker(&key);
            // A pause or global disable that landed while this job was in
            // flight must not start another automatic model call. The job
            // already claimed is allowed to finish; Dream waits until capture
            // is allowed again.
            if !cancel.is_cancelled() && runtime.config_for(Some(session_id.as_str())).can_capture()
            {
                runtime
                    .run_capture_followups(&config, &db, &session_id, &model, produced)
                    .await;
            }
        });
    }

    /// Run this session's capture queue to completion now (`/flush`).
    ///
    /// A background worker for the same session owns the slot: two workers on
    /// one cursor would fight over the same job, so the command reports busy
    /// instead of waiting for a queue it cannot observe.
    pub(crate) async fn flush_capture(
        &self,
        config: &Arc<ConfigManager>,
        db: &Database,
        session_id: &SessionId,
        model: &ModelRef,
    ) -> FlushOutcome {
        if !self.config_for(Some(session_id.as_str())).can_capture() {
            return FlushOutcome::Disabled;
        }
        if self.is_workspace_ephemeral() {
            return FlushOutcome::WorkspaceEphemeral;
        }
        let key = session_id.as_str().to_owned();
        let Some(cancel) = self.try_begin_capture_worker(&key) else {
            return FlushOutcome::Busy;
        };
        if let Err(error) = self.enqueue_missing(db, session_id).await {
            self.finish_capture_worker(&key);
            return FlushOutcome::Failed(error);
        }
        let produced =
            run_capture_worker(self, config, db, session_id, model, cancel.clone()).await;
        self.finish_capture_worker(&key);
        if !cancel.is_cancelled() && self.config_for(Some(session_id.as_str())).can_capture() {
            self.run_capture_followups(config, db, session_id, model, produced)
                .await;
        }
        FlushOutcome::Flushed { produced }
    }

    /// Post-capture work, shared by the background worker and `/flush`:
    /// promotion first (so observations hidden by an earlier rollout become
    /// eligible now), retention GC, then automatic Dream.
    async fn run_capture_followups(
        &self,
        config: &Arc<ConfigManager>,
        db: &Database,
        session_id: &SessionId,
        model: &ModelRef,
        produced: bool,
    ) {
        let memory_config = self.config_for(Some(session_id.as_str()));
        if memory_config.can_expose_memory() {
            match self.promote_hidden_observations().await {
                Ok(0) => {}
                Ok(count) => tracing::info!(count, "memory promoted hidden observations"),
                Err(error) => {
                    tracing::warn!(%error, "memory observation promotion failed")
                }
            }
        }
        if memory_config.can_run_maintenance() {
            self.run_maintenance(memory_config).await;
        }
        if produced {
            super::dream::after_capture(self, config, db, session_id, model).await;
        }
    }

    /// Delete observations and job metadata past their retention windows.
    /// Failures are logged, never surfaced as a turn failure.
    async fn run_maintenance(&self, config: leveler_memory::config::MemoryConfig) {
        let storage = self.storage.clone();
        let retention = leveler_memory::RetentionPolicy {
            archived_observation_days: config.archived_retention_days,
            terminal_job_days: config.job_retention_days,
        };
        let result = tokio::task::spawn_blocking(move || {
            let store = leveler_memory::MaintenanceStore::open(
                storage.workspace_dir(),
                MemoryScope::Workspace,
                storage.global_dir(),
                storage.workspace_dir(),
            )
            .map_err(|error| error.to_string())?;
            store.gc_now(retention).map_err(|error| error.to_string())
        })
        .await;
        match result {
            Ok(Ok(removed)) => {
                if removed.archived_observations_removed > 0 || removed.terminal_jobs_removed > 0 {
                    tracing::info!(
                        archived_observations = removed.archived_observations_removed,
                        terminal_jobs = removed.terminal_jobs_removed,
                        "memory retention gc completed"
                    );
                }
            }
            Ok(Err(error)) => tracing::warn!(%error, "memory retention gc failed"),
            Err(error) => tracing::warn!(%error, "memory retention gc task failed"),
        }
    }
}

/// What `/flush` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FlushOutcome {
    Disabled,
    WorkspaceEphemeral,
    Busy,
    Flushed { produced: bool },
    Failed(String),
}

impl FlushOutcome {
    pub(crate) fn succeeded(&self) -> bool {
        matches!(self, Self::Flushed { .. })
    }

    pub(crate) fn message(&self) -> String {
        match self {
            Self::Disabled => "Memory capture is disabled by the current configuration.".to_owned(),
            Self::WorkspaceEphemeral => super::WORKSPACE_EPHEMERAL_MESSAGE.to_owned(),
            Self::Busy => "This session's memory capture worker is already running.".to_owned(),
            Self::Flushed { produced: true } => {
                "Captured the settled turns waiting in this session's queue.".to_owned()
            }
            Self::Flushed { produced: false } => {
                "Capture queue drained; no new observations were stored.".to_owned()
            }
            Self::Failed(error) => format!("Memory capture flush failed: {error}"),
        }
    }
}

/// True when `finished_at` is at or after this process began.
///
/// A turn that finished earlier is historical. It keeps whatever eligibility
/// row it already has, and a missing row is not backfilled.
pub(crate) fn turn_finished_during_process(
    finished_at: Option<&str>,
    process_started: leveler_core::Timestamp,
) -> bool {
    let Some(finished_at) = finished_at else {
        return false;
    };
    let Ok(finished) = chrono::DateTime::parse_from_rfc3339(finished_at) else {
        return false;
    };
    finished.with_timezone(&chrono::Utc) >= process_started
}

fn apply_capture_plan(
    storage: &leveler_memory::MemoryStorage,
    session_id: &str,
    admissions: &[CaptureAdmission],
    exposes: bool,
) -> Result<Option<CaptureRange>, String> {
    storage
        .ensure_initialized()
        .map_err(|error| error.to_string())?;
    let store = CaptureStore::open(storage.workspace_dir(), MemoryScope::Workspace)
        .map_err(|error| error.to_string())?;
    store
        .ensure_session(session_id)
        .map_err(|error| error.to_string())?;
    let requested = store
        .cursors(session_id)
        .map_err(|error| error.to_string())?
        .requested;
    let steps =
        plan_capture_admissions(requested, admissions).map_err(|error| error.to_string())?;
    if steps.is_empty() {
        return Ok(None);
    }
    let mut enqueued = None;
    for step in steps {
        match step {
            CapturePlanStep::Skip(range) => store
                .record_skipped_range(session_id, range)
                .map_err(|error| error.to_string())?,
            CapturePlanStep::Enqueue(range) => {
                store
                    .enqueue_for_prompt_with_visibility(
                        session_id,
                        range,
                        range.through_turn().saturating_sub(1),
                        exposes,
                    )
                    .map_err(|error| error.to_string())?;
                enqueued = Some(range);
            }
        }
    }
    Ok(enqueued)
}

/// Drain this session's claimable capture jobs. Returns whether any committed
/// outcome contained observations (the only condition that can make Dream
/// eligible).
async fn run_capture_worker(
    runtime: &MemoryRuntime,
    config: &Arc<ConfigManager>,
    db: &Database,
    session_id: &SessionId,
    model: &ModelRef,
    cancel: CancellationToken,
) -> bool {
    if !runtime.config_for(Some(session_id.as_str())).can_capture() {
        return false;
    }
    let storage = runtime.storage.clone();
    let scope = session_id.as_str().to_owned();
    let store = {
        let storage = storage.clone();
        let scope = scope.clone();
        match tokio::task::spawn_blocking(move || {
            storage
                .ensure_initialized()
                .map_err(|error| error.to_string())?;
            CaptureStore::open(storage.workspace_dir(), MemoryScope::Workspace)
                .map_err(|error| error.to_string())
        })
        .await
        {
            Ok(Ok(store)) => store,
            Ok(Err(error)) => {
                tracing::warn!(%error, "memory capture store unavailable");
                runtime.record_activity(
                    &scope,
                    MemoryActivity {
                        kind: MemoryActivityKind::Failed,
                        last_error: Some(error),
                        ..Default::default()
                    },
                );
                return false;
            }
            Err(error) => {
                tracing::warn!(%error, "memory capture store task failed");
                return false;
            }
        }
    };

    let owner = format!("{}-{}", std::process::id(), leveler_core::new_uuid_string());
    let mut produced = false;
    loop {
        if cancel.is_cancelled() {
            break;
        }
        // A page toggle pauses capture at the worker's next claim boundary.
        // The request already in flight is allowed to settle; nothing new is
        // claimed after the user turns memory off.
        if !runtime.config_for(Some(session_id.as_str())).can_capture() {
            break;
        }
        let claim_store = store.clone();
        let claim_session = scope.clone();
        let request = ClaimRequest {
            owner: owner.clone(),
            now: leveler_core::now().timestamp(),
            duration: CAPTURE_LEASE,
        };
        let lease = match tokio::task::spawn_blocking(move || {
            claim_store.claim_for_session(&claim_session, &request)
        })
        .await
        {
            Ok(Ok(Some(lease))) => lease,
            Ok(Ok(None)) => break,
            Ok(Err(error)) => {
                tracing::warn!(%error, "memory capture claim failed");
                break;
            }
            Err(error) => {
                tracing::warn!(%error, "memory capture claim task failed");
                break;
            }
        };
        runtime.record_activity(
            &scope,
            MemoryActivity {
                kind: MemoryActivityKind::Capturing,
                active: true,
                ..Default::default()
            },
        );
        let range = lease.job.range;
        let outcome =
            extract_range(runtime, config, db, session_id, model, &lease.job, &cancel).await;
        match outcome {
            Ok(outcome) => {
                let is_observations = matches!(outcome, CaptureOutcomeDraft::Observations(_));
                let observations = match &outcome {
                    CaptureOutcomeDraft::Observations(values) => values.len(),
                    CaptureOutcomeDraft::Noop => 0,
                };
                let commit_store = store.clone();
                let commit_lease = lease.clone();
                let now = leveler_core::now().timestamp();
                let committed = tokio::task::spawn_blocking(move || {
                    commit_store.commit(&commit_lease, &outcome, now)
                })
                .await;
                match committed {
                    Ok(Ok(_)) => {
                        produced |= is_observations;
                        runtime.record_activity(
                            &scope,
                            MemoryActivity {
                                kind: MemoryActivityKind::Capturing,
                                active: false,
                                last_captured_range: Some((
                                    range.start_turn(),
                                    range.through_turn(),
                                )),
                                ..Default::default()
                            },
                        );
                        tracing::info!(
                            from_turn = range.start_turn(),
                            through_turn = range.through_turn(),
                            observations,
                            "memory capture committed"
                        );
                    }
                    Ok(Err(error)) => {
                        tracing::warn!(%error, "memory capture commit failed");
                        fail_lease(&store, &lease, &error.to_string(), false).await;
                        runtime.record_activity(
                            &scope,
                            MemoryActivity {
                                kind: MemoryActivityKind::Failed,
                                active: false,
                                last_error: Some(error.to_string()),
                                ..Default::default()
                            },
                        );
                    }
                    Err(error) => {
                        let detail = format!("memory capture commit task failed: {error}");
                        tracing::warn!(%detail);
                        fail_lease(&store, &lease, &detail, false).await;
                    }
                }
            }
            Err(failure) => {
                tracing::warn!(
                    from_turn = range.start_turn(),
                    through_turn = range.through_turn(),
                    attempt = lease.job.attempt,
                    %failure.detail,
                    "memory capture extraction failed"
                );
                fail_lease(&store, &lease, &failure.detail, failure.terminal).await;
                runtime.record_activity(
                    &scope,
                    MemoryActivity {
                        kind: MemoryActivityKind::Failed,
                        active: false,
                        last_error: Some(failure.detail),
                        ..Default::default()
                    },
                );
            }
        }
    }
    runtime.record_activity(
        &scope,
        MemoryActivity {
            kind: MemoryActivityKind::Capturing,
            active: false,
            ..Default::default()
        },
    );
    produced
}

struct ExtractionFailure {
    detail: String,
    terminal: bool,
}

/// Extract one claimed range. A missing/oversized transcript and a malformed
/// model answer are retryable until the attempt bound; a corrupt stored
/// payload is terminal, because retrying cannot change the evidence.
async fn extract_range(
    runtime: &MemoryRuntime,
    config: &Arc<ConfigManager>,
    db: &Database,
    session_id: &SessionId,
    model: &ModelRef,
    job: &leveler_memory::CaptureJob,
    cancel: &CancellationToken,
) -> Result<CaptureOutcomeDraft, ExtractionFailure> {
    let registry = config.snapshot().registry.clone();
    let route = runtime.model_override().unwrap_or_else(|| model.clone());
    let turns = TurnRepository::new(db)
        .list(session_id)
        .await
        .map_err(|error| ExtractionFailure {
            detail: format!("could not list durable turns: {error}"),
            terminal: false,
        })?;
    let dispositions = TurnRepository::new(db)
        .memory_capture_dispositions(session_id)
        .await
        .map_err(|error| ExtractionFailure {
            detail: format!("could not read capture eligibility: {error}"),
            terminal: false,
        })?;
    let eligible: Vec<_> = turns.into_iter().filter(is_capture_eligible).collect();
    let from = usize::try_from(job.range.start_turn().saturating_sub(1)).unwrap_or(usize::MAX);
    let through = usize::try_from(job.range.through_turn()).unwrap_or(usize::MAX);
    let Some(selected) = eligible.get(from..through) else {
        return Err(ExtractionFailure {
            detail: "capture range no longer maps to settled turns".to_owned(),
            terminal: true,
        });
    };
    // A job must not hand a paused or disabled turn to the extraction model,
    // even if an older range index still names it.
    let selected: Vec<_> = selected
        .iter()
        .filter(|turn| {
            !matches!(
                dispositions.get(&turn.id),
                Some(MemoryCaptureDisposition::Ineligible)
            )
        })
        .collect();
    if selected.is_empty() {
        return Ok(CaptureOutcomeDraft::Noop);
    }
    let mut payloads = Vec::new();
    for turn in selected {
        let messages = MessageRepository::new(db)
            .load_for_turn(session_id, &leveler_core::TurnId::new(turn.id.clone()))
            .await
            .map_err(|error| ExtractionFailure {
                detail: format!("could not load the turn transcript: {error}"),
                terminal: false,
            })?;
        payloads.extend(messages);
    }
    let items =
        super::transcript::transcript_items(&payloads).map_err(|detail| ExtractionFailure {
            detail,
            terminal: true,
        })?;
    let condensed = condense_turn_transcript(items, TranscriptBudget::for_attempt(job.attempt))
        .map_err(|detail| ExtractionFailure {
            detail: format!("could not render the transcript: {detail}"),
            terminal: false,
        })?;
    let mut system = String::from(CAPTURE_SYSTEM_PROMPT);
    if let Some(note) = condensed.stats.prompt_note() {
        system.push(' ');
        system.push_str(&note);
    }
    let mut request = ModelRequest::new(
        route.clone(),
        vec![
            leveler_model::Message::text(Role::System, system),
            leveler_model::Message::text(
                Role::User,
                extraction_user_message(session_id.as_str(), job.range, &condensed.json),
            ),
        ],
    );
    request.tools = vec![capture_tool()];
    request.tool_choice = ToolChoice::Tool(CAPTURE_TOOL_NAME.to_owned());
    request.max_output_tokens = Some(EXTRACTION_MAX_OUTPUT_TOKENS);
    request.temperature = Some(0.0);
    // Extraction is mechanical and must return a forced tool call. Disabling
    // thinking is what makes that legal on every route: Messages rejects a
    // forced choice while thinking is on, and thinking-flag routes otherwise
    // spend the budget on reasoning.
    request.thinking_disabled = true;

    let started = std::time::Instant::now();
    let result = tokio::select! {
        biased;
        () = cancel.cancelled() => {
            return Err(ExtractionFailure {
                detail: "capture worker cancelled".to_owned(),
                terminal: false,
            });
        }
        result = tokio::time::timeout(
            EXTRACTION_TIMEOUT,
            registry.generate(request, cancel.clone()),
        ) => result,
    };
    let outcome = match result {
        Ok(result) => result,
        Err(_) => Err(leveler_model::ModelError::new(
            leveler_model::ModelErrorKind::Timeout,
            format!(
                "memory extraction timed out after {} seconds",
                EXTRACTION_TIMEOUT.as_secs()
            ),
        )),
    };
    let latency_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    crate::observability::record_auxiliary_call_in_registry(
        &registry,
        db,
        session_id,
        leveler_storage::ModelCallKind::MemoryCapture,
        &route.provider,
        &route.model,
        None,
        &outcome,
        latency_ms,
        None,
        None,
    )
    .await
    .map_err(|error| ExtractionFailure {
        detail: format!("could not record the extraction call: {error}"),
        terminal: false,
    })?;
    let response = outcome.map_err(|error| ExtractionFailure {
        detail: format!("extraction model failed: {error}"),
        terminal: false,
    })?;
    if response.finish_reason == FinishReason::Length {
        return Err(ExtractionFailure {
            detail: "extraction output was truncated".to_owned(),
            terminal: job.attempt >= MAX_CAPTURE_ATTEMPTS,
        });
    }
    let arguments = response
        .message
        .content
        .iter()
        .find_map(|part| match part {
            ContentPart::ToolCall { call } if call.name == CAPTURE_TOOL_NAME => {
                Some(call.arguments.to_string())
            }
            _ => None,
        })
        .ok_or_else(|| ExtractionFailure {
            detail: "extraction model did not call the required tool".to_owned(),
            terminal: false,
        })?;
    parse_model_outcome(&arguments, &route.model, leveler_core::now().timestamp()).map_err(
        |detail| ExtractionFailure {
            detail,
            terminal: job.attempt >= MAX_CAPTURE_ATTEMPTS,
        },
    )
}

/// Release a claim. The attempt bound is enforced here so no job can retry
/// forever without an operator.
async fn fail_lease(
    store: &CaptureStore,
    lease: &leveler_memory::CaptureLease,
    detail: &str,
    terminal: bool,
) {
    let store = store.clone();
    let lease = lease.clone();
    let detail = detail.to_owned();
    let terminal = terminal || lease.job.attempt >= MAX_CAPTURE_ATTEMPTS;
    let now = leveler_core::now().timestamp();
    let released = tokio::task::spawn_blocking(move || {
        if terminal {
            store.fail_terminal(&lease, now, &detail)
        } else {
            store.fail_retryable(&lease, now, &detail)
        }
    })
    .await;
    match released {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(%error, "memory capture lease release failed"),
        Err(error) => tracing::warn!(%error, "memory capture lease release task failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::turn_finished_during_process;

    fn at(rfc3339: &str) -> leveler_core::Timestamp {
        chrono::DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    #[test]
    fn only_a_turn_that_finished_in_this_process_can_be_admitted() {
        let started = at("2026-11-13T12:00:00Z");
        assert!(!turn_finished_during_process(None, started));
        assert!(!turn_finished_during_process(Some("not-a-time"), started));
        assert!(!turn_finished_during_process(
            Some("2026-11-13T11:59:59Z"),
            started
        ));
        assert!(turn_finished_during_process(
            Some("2026-11-13T12:00:00Z"),
            started
        ));
        assert!(turn_finished_during_process(
            Some("2026-11-13T12:00:01Z"),
            started
        ));
    }
}
