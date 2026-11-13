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
//! [`V2CaptureStore::reconcile`] repairs on the next claim, so restarting the
//! process never loses or duplicates a turn. Extraction failures never fail the
//! user's turn: they are recorded on the job and surfaced as memory activity.

use std::sync::Arc;
use std::time::Duration;

use leveler_core::SessionId;
use leveler_memory::capture_model::{
    CAPTURE_SYSTEM_PROMPT, CAPTURE_TOOL_NAME, extraction_schema, extraction_user_message,
    missing_range, parse_model_outcome,
};
use leveler_memory::capture_transcript::{TranscriptBudget, condense_turn_transcript};
use leveler_memory::{
    CaptureOutcomeDraft, CaptureRange, ClaimRequest, V2CaptureStore, V2MemoryScope,
};
use leveler_model::{
    ContentPart, FinishReason, ModelRef, ModelRequest, ModelRuntime, Role, ToolChoice,
    ToolDefinition,
};
use leveler_storage::{Database, MessageRepository, TurnRecord, TurnRepository};
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
        if !runtime.config_for(Some(session_id.as_str())).can_capture() {
            return;
        }
        // A temporary working directory owns no durable workspace memory, so
        // there is no scope to capture into.
        if runtime.is_workspace_ephemeral() {
            return;
        }
        let db = match self.open_database().await {
            Ok(db) => db,
            Err(error) => {
                tracing::warn!(%error, "memory capture could not open the session database");
                return;
            }
        };
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
    /// Enqueue every eligible turn the persisted cursor has not requested.    ///
    /// Idempotent: `requested_cursor` only moves forward, and a duplicate
    /// range returns the same job row. This is also the crash-recovery entry
    /// point — the next turn after a restart reconciles the whole gap.
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
        let settled = turns
            .iter()
            .filter(|turn| is_capture_eligible(turn))
            .count();
        let settled = u32::try_from(settled)
            .ok()
            .filter(|count| *count <= 999_999)
            .ok_or_else(|| "session exceeds the durable capture turn limit".to_owned())?;
        let storage = self.storage.clone();
        let session_id = session_id.as_str().to_owned();
        tokio::task::spawn_blocking(move || {
            storage
                .ensure_initialized()
                .map_err(|error| error.to_string())?;
            let store = V2CaptureStore::open(storage.workspace_dir(), V2MemoryScope::Workspace)
                .map_err(|error| error.to_string())?;
            store
                .ensure_session(&session_id)
                .map_err(|error| error.to_string())?;
            let cursors = store
                .cursors(&session_id)
                .map_err(|error| error.to_string())?;
            let Some(range) =
                missing_range(cursors.requested, settled).map_err(|error| error.to_string())?
            else {
                return Ok(None);
            };
            store
                .enqueue_for_prompt_with_visibility(
                    &session_id,
                    range,
                    range.through_turn().saturating_sub(1),
                    config.rollout.exposes_memory(),
                )
                .map_err(|error| error.to_string())?;
            Ok(Some(range))
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
            if !cancel.is_cancelled() {
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
        if !cancel.is_cancelled() {
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
                Ok(count) => tracing::info!(count, "memory-v2 promoted hidden observations"),
                Err(error) => {
                    tracing::warn!(%error, "memory-v2 observation promotion failed")
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
    async fn run_maintenance(&self, config: leveler_memory::config::MemoryV2Config) {
        let storage = self.storage.clone();
        let retention = leveler_memory::RetentionPolicy {
            archived_observation_days: config.archived_retention_days,
            terminal_job_days: config.job_retention_days,
        };
        let result = tokio::task::spawn_blocking(move || {
            let store = leveler_memory::V2MaintenanceStore::open(
                storage.workspace_dir(),
                V2MemoryScope::Workspace,
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
                        "memory-v2 retention gc completed"
                    );
                }
            }
            Ok(Err(error)) => tracing::warn!(%error, "memory-v2 retention gc failed"),
            Err(error) => tracing::warn!(%error, "memory-v2 retention gc task failed"),
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
            V2CaptureStore::open(storage.workspace_dir(), V2MemoryScope::Workspace)
                .map_err(|error| error.to_string())
        })
        .await
        {
            Ok(Ok(store)) => store,
            Ok(Err(error)) => {
                tracing::warn!(%error, "memory-v2 capture store unavailable");
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
                tracing::warn!(%error, "memory-v2 capture store task failed");
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
                tracing::warn!(%error, "memory-v2 capture claim failed");
                break;
            }
            Err(error) => {
                tracing::warn!(%error, "memory-v2 capture claim task failed");
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
                            "memory-v2 capture committed"
                        );
                    }
                    Ok(Err(error)) => {
                        tracing::warn!(%error, "memory-v2 capture commit failed");
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
                        let detail = format!("memory-v2 capture commit task failed: {error}");
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
                    "memory-v2 capture extraction failed"
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
    let eligible: Vec<_> = turns.into_iter().filter(is_capture_eligible).collect();
    let from = usize::try_from(job.range.start_turn().saturating_sub(1)).unwrap_or(usize::MAX);
    let through = usize::try_from(job.range.through_turn()).unwrap_or(usize::MAX);
    let Some(selected) = eligible.get(from..through) else {
        return Err(ExtractionFailure {
            detail: "capture range no longer maps to settled turns".to_owned(),
            terminal: true,
        });
    };
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
    store: &V2CaptureStore,
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
        Ok(Err(error)) => tracing::warn!(%error, "memory-v2 capture lease release failed"),
        Err(error) => tracing::warn!(%error, "memory-v2 capture lease release task failed"),
    }
}
