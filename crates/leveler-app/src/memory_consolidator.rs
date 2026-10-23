//! Runtime-owned asynchronous consolidation of durable memory inbox turns.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use leveler_agent::{
    BatchSourceTurn, DEFAULT_EXTRACTION_TIMEOUT, ExperienceEvent, ExperienceExtraction,
    ExperienceExtractor, ExtractionCall, ExtractionContext, KnownIdentity, MAX_BATCH_INPUT_CHARS,
    MAX_BATCH_TURNS, MAX_EXTRACTOR_INPUT_CHARS, ModelExperienceExtractor, ModelSemanticExtractor,
    SemanticExtractor, build_evidence_pack, evidence_fingerprint, validate_batch_candidates,
    validate_experience_candidates,
};
use leveler_core::{BootId, BootLiveness};
use leveler_memory::{MemoryCandidate, MemoryService, ServiceScope, WriteOutcome};
use leveler_model::{ModelRef, ModelRuntime};
use leveler_storage::{
    Database, EventRepository, ExperienceInboxItem, ExperienceInboxRepository, MemoryInboxItem,
    MemoryInboxReadyState, MemoryInboxRepository, ModelRequestRecord, ModelRequestRepository,
};
use tokio::sync::{Notify, Semaphore};
use tokio_util::sync::CancellationToken;

/// Enough turns to consolidate immediately rather than waiting for the idle
/// debounce. This is batching policy, not a provider scheduling framework.
pub const IMMEDIATE_BATCH_THRESHOLD: usize = 4;
/// Claim no more turns than the extractor contract permits. A claim whose
/// clipped texts exceed the aggregate input bound is split below without
/// losing or further truncating any source turn.
pub const MAX_WORKER_BATCH_TURNS: u32 = MAX_BATCH_TURNS as u32;
/// Sparse conversations still converge without turning every turn into a
/// provider request. Notifications never shorten this maximum-age window
/// unless enough work exists for a real batch.
const MAX_AGE_FLUSH: Duration = Duration::from_secs(60);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(300);
/// Settlement time an exit flush may spend beyond the extraction call it
/// already bounds with `extraction_timeout`.
const FLUSH_SETTLE_MARGIN: Duration = Duration::from_secs(30);
const PROCESSED_RETENTION_DAYS: i64 = 7;
const PRUNE_LIMIT: u32 = 128;
/// Experience work is one whole task per row and one provider call at a time.
/// Processing two at once would double a burst's cost for no scheduling gain.
const EXPERIENCE_CLAIM_LIMIT: u32 = 1;

static GLOBAL_MEMORY_CALL: OnceLock<Semaphore> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryConsolidationEvent {
    pub session_id: String,
    pub operation: String,
    pub id: String,
    pub title: String,
    pub authority: String,
    /// Number of lifecycle changes represented by this event. Candidate
    /// proposals are batched so the conversation gets one calm summary.
    pub count: usize,
}

pub type MemoryEventSink = Arc<dyn Fn(MemoryConsolidationEvent) + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConsolidationOutcome {
    pub claimed: usize,
    pub accepted: usize,
    pub rejected: usize,
}

/// What an explicit exit flush actually settled.
///
/// Durable queue state stays the truth: a flush that cannot finish leaves its
/// rows `pending` or `failed_retryable`, never in a hand-off limbo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MemoryFlushOutcome {
    /// Rows settled by this flush (processed or deterministically skipped).
    pub settled: usize,
    /// Rows still durable in an open lane when the flush returned.
    pub remaining: u64,
    /// A lane failed; its rows are parked for a durable retry.
    pub failed: bool,
    /// The flush stopped at its time budget instead of draining.
    pub timed_out: bool,
}

impl MemoryFlushOutcome {
    /// True only when every row in an open lane was settled.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.remaining == 0 && !self.failed && !self.timed_out
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct StagedTurnResult {
    accepted: Vec<MemoryCandidate>,
    rejected: usize,
    #[serde(default)]
    applied: usize,
    /// Turns or candidates deliberately not written: secret-gated, duplicate,
    /// suppressed, or a lifecycle skip. This is the "filtered" half the status
    /// surface must distinguish from "no candidate at all".
    #[serde(default)]
    skipped: usize,
}

/// The durable result of one experience-work row. Written before any store
/// mutation, so a retry replays the same decision rather than re-asking the
/// model.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct StagedExperienceResult {
    /// `extracted` or `skipped`. Skipped is a settled outcome, not an error.
    #[serde(default)]
    outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(default)]
    accepted: Vec<MemoryCandidate>,
    #[serde(default)]
    rejected: usize,
    #[serde(default)]
    applied: usize,
    #[serde(default)]
    skipped: usize,
    #[serde(default)]
    created: usize,
    #[serde(default)]
    updated: usize,
    #[serde(default)]
    pending: usize,
}

/// The application/runtime-owned worker for one project database.
pub struct MemoryConsolidator {
    db: Database,
    boot: BootId,
    state_dir: PathBuf,
    /// The unified write path. Legacy by default; the host may install the
    /// configured Markdown service before the worker starts.
    memory: MemoryService,
    /// Which lane(s) this project has open. A closed lane is never claimed,
    /// even if its durable queue still holds rows from before the switch.
    lanes: leveler_memory::MemoryLanes,
    runtime: Arc<dyn ModelRuntime>,
    extraction_timeout: Duration,
    /// Upper bound on an explicit exit flush: one full extraction attempt
    /// plus settlement. Work beyond it stays durably queued for retry.
    flush_budget: Duration,
    /// Serialises the worker loop with an explicit exit flush. Both call
    /// `run_once` / `run_experience_once`, which recover this boot's own
    /// unsettled claims on entry; two concurrent callers would steal each
    /// other's in-flight work and extract the same turn twice.
    work: tokio::sync::Mutex<()>,
    notify: Notify,
    cancel: CancellationToken,
    event_sink: MemoryEventSink,
}

impl MemoryConsolidator {
    pub fn new(
        db: Database,
        boot: BootId,
        state_dir: PathBuf,
        memory_dir: PathBuf,
        runtime: Arc<dyn ModelRuntime>,
        event_sink: MemoryEventSink,
    ) -> Arc<Self> {
        Arc::new(Self {
            db,
            boot,
            state_dir,
            memory: MemoryService::legacy(memory_dir),
            lanes: leveler_memory::MemoryLanes::ALL,
            runtime,
            extraction_timeout: DEFAULT_EXTRACTION_TIMEOUT,
            flush_budget: DEFAULT_EXTRACTION_TIMEOUT + FLUSH_SETTLE_MARGIN,
            work: tokio::sync::Mutex::new(()),
            notify: Notify::new(),
            cancel: CancellationToken::new(),
            event_sink,
        })
    }

    /// Restrict the worker to the project's effective lanes.
    #[must_use]
    pub fn with_lanes(mut self: Arc<Self>, lanes: leveler_memory::MemoryLanes) -> Arc<Self> {
        Arc::get_mut(&mut self)
            .expect("lanes are configured before the worker is shared")
            .lanes = lanes;
        self
    }

    /// Write through the host's configured backend. The service is installed
    /// before the worker is shared, like the lane policy.
    #[must_use]
    pub fn with_memory_service(mut self: Arc<Self>, service: MemoryService) -> Arc<Self> {
        Arc::get_mut(&mut self)
            .expect("the memory service is configured before the worker is shared")
            .memory = service;
        self
    }

    #[cfg(test)]
    fn with_extraction_timeout(mut self: Arc<Self>, timeout: Duration) -> Arc<Self> {
        Arc::get_mut(&mut self)
            .expect("a fresh consolidator has no other owners")
            .extraction_timeout = timeout;
        self
    }

    #[cfg(test)]
    fn with_flush_budget(mut self: Arc<Self>, budget: Duration) -> Arc<Self> {
        Arc::get_mut(&mut self)
            .expect("a fresh consolidator has no other owners")
            .flush_budget = budget;
        self
    }

    /// Wake the worker after a turn reaches a terminal state. This never waits
    /// for extraction and therefore cannot affect turn success or latency.
    pub fn notify_turn_terminal(&self) {
        tracing::debug!(
            event = "memory_turn_terminal",
            "terminal turn may have made memory work ready"
        );
        self.notify.notify_one();
    }

    /// Request prompt shutdown. Dropping a running extraction is safe: its
    /// processing claim remains fenced to this boot and startup recovery will
    /// return it to the retry queue after the boot lease ends.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Drain ready memory work before a headless process exits.
    ///
    /// The idle debounce exists to batch provider calls while a session is
    /// live; it must not outlive the process. This runs the worker's own
    /// passes, serially with the loop (see [`Self::work`]), under a bounded
    /// budget. Anything that does not settle stays `pending` or
    /// `failed_retryable` and is reported by [`MemoryFlushOutcome`].
    pub async fn flush(&self) -> MemoryFlushOutcome {
        let deadline = tokio::time::Instant::now() + self.flush_budget;
        let mut settled = 0usize;
        let mut failed = false;
        let timed_out = loop {
            let ready = self.ready_count().await + self.experience_ready_count().await;
            if ready == 0 {
                break false;
            }
            let permit = match tokio::time::timeout_at(deadline, self.work.lock()).await {
                Ok(permit) => permit,
                Err(_) => break true,
            };
            let mut progressed = false;
            if self.lanes.experience {
                match self.run_experience_once().await {
                    Ok(claimed) => {
                        progressed |= claimed;
                        settled += usize::from(claimed);
                    }
                    Err(error) => {
                        failed = true;
                        tracing::warn!(
                            %error,
                            "experience consolidation failed during the exit flush"
                        );
                    }
                }
            }
            if self.lanes.user {
                match self.run_once().await {
                    Ok(outcome) => {
                        progressed |= outcome.claimed > 0;
                        settled += outcome.claimed;
                    }
                    Err(error) => {
                        failed = true;
                        tracing::warn!(%error, "memory consolidation failed during the exit flush");
                    }
                }
            }
            drop(permit);
            if !progressed {
                break false;
            }
        };
        let remaining = self.outstanding().await;
        if remaining > 0 || failed || timed_out {
            tracing::warn!(
                event = "memory_flush_incomplete",
                settled,
                remaining,
                failed,
                timed_out,
                "memory work remains durably queued after the exit flush"
            );
        } else {
            tracing::debug!(
                event = "memory_flush_complete",
                settled,
                "memory work settled before exit"
            );
        }
        MemoryFlushOutcome {
            settled,
            remaining,
            failed,
            timed_out,
        }
    }

    /// Rows still durable in lanes this worker owns. Processed rows are
    /// retained diagnostics, not outstanding work.
    async fn outstanding(&self) -> u64 {
        let mut total = 0u64;
        if self.lanes.user {
            match MemoryInboxRepository::new(&self.db).counts().await {
                Ok(counts) => {
                    total += counts.pending + counts.failed_retryable + counts.processing;
                }
                Err(error) => tracing::warn!(%error, "could not count outstanding memory work"),
            }
        }
        if self.lanes.experience {
            match ExperienceInboxRepository::new(&self.db).counts().await {
                Ok(counts) => {
                    total += counts.pending + counts.failed_retryable + counts.processing;
                }
                Err(error) => {
                    tracing::warn!(%error, "could not count outstanding experience work");
                }
            }
        }
        total
    }

    /// Start the single project worker. The returned task is deliberately not
    /// awaited during shutdown; durable claims, not task joining, provide the
    /// completion guarantee.
    pub fn spawn(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let this = self.clone();
        tokio::spawn(async move { this.run().await })
    }

    async fn run(&self) {
        if let Err(error) = self.recover_dead_claims().await {
            tracing::warn!(%error, "memory consolidation startup recovery failed");
        }
        if let Err(error) = self.recover_dead_experience_claims().await {
            tracing::warn!(%error, "experience consolidation startup recovery failed");
        }
        // Startup is itself a flush signal: pending work from an earlier clean
        // shutdown must not require another user turn to become visible.
        // Experience work is one durable row per completed task and is
        // processed as soon as it is ready. The USER-TURN batch keeps its own
        // batching policy: a single new turn must not become an immediate
        // provider call merely because an experience row woke the worker.
        let mut flush_memory = true;
        loop {
            // The pass guard is released before the idle wait: an exit flush
            // must be able to run its own pass while the worker sleeps.
            let (experience_claimed, memory_claimed) = {
                let _pass = self.work.lock().await;
                let experience_claimed = if self.lanes.experience {
                    match self.run_experience_once().await {
                        Ok(claimed) => claimed,
                        Err(error) => {
                            tracing::warn!(%error, "experience consolidation failed");
                            false
                        }
                    }
                } else {
                    false
                };
                let mut memory_claimed = 0usize;
                if flush_memory {
                    match self.run_once().await {
                        Ok(outcome) => {
                            memory_claimed = outcome.claimed;
                            tracing::debug!(
                                claimed = outcome.claimed,
                                accepted = outcome.accepted,
                                rejected = outcome.rejected,
                                "memory consolidation batch settled"
                            );
                            flush_memory = outcome.claimed == MAX_WORKER_BATCH_TURNS as usize
                                || self.ready_count().await >= IMMEDIATE_BATCH_THRESHOLD as u64;
                        }
                        Err(error) => tracing::warn!(%error, "memory consolidation batch failed"),
                    }
                }
                (experience_claimed, memory_claimed)
            };
            if (experience_claimed && self.experience_ready_count().await > 0)
                || (flush_memory && memory_claimed > 0)
            {
                continue;
            }

            let delay = self.next_wake_delay().await;
            if delay.is_zero() {
                // Zero only because experience is ready. Process it on the
                // next loop without promoting the memory batch.
                flush_memory = self.ready_count().await >= IMMEDIATE_BATCH_THRESHOLD as u64;
                continue;
            }
            tokio::select! {
                _ = self.cancel.cancelled() => return,
                _ = self.notify.notified() => {
                    flush_memory = self.ready_count().await >= IMMEDIATE_BATCH_THRESHOLD as u64;
                }
                _ = tokio::time::sleep(delay) => flush_memory = true,
            }
        }
    }

    async fn next_wake_delay(&self) -> Duration {
        let memory = if self.lanes.user {
            self.memory_wake_delay().await
        } else {
            MAX_AGE_FLUSH
        };
        let experience = if !self.lanes.experience {
            MAX_AGE_FLUSH
        } else {
            match ExperienceInboxRepository::new(&self.db)
                .ready_state(leveler_core::now())
                .await
            {
                Ok(state) if state.count > 0 => Duration::ZERO,
                Ok(_) => MAX_AGE_FLUSH,
                Err(error) => {
                    tracing::warn!(%error, "could not schedule pending experience work");
                    MAX_AGE_FLUSH
                }
            }
        };
        memory.min(experience)
    }

    async fn memory_wake_delay(&self) -> Duration {
        match MemoryInboxRepository::new(&self.db)
            .ready_state(leveler_core::now())
            .await
        {
            Ok(state) => {
                let now = leveler_core::now();
                let oldest_age_ms = state
                    .oldest_created_at
                    .map(|oldest| (now - oldest).num_milliseconds().max(0));
                tracing::debug!(
                    event = "memory_inbox_pending",
                    pending = state.count,
                    oldest_age_ms = ?oldest_age_ms,
                    "memory inbox scheduling state"
                );
                wake_delay(&state, now)
            }
            Err(error) => {
                tracing::warn!(%error, "could not schedule pending memory work");
                MAX_AGE_FLUSH
            }
        }
    }

    async fn ready_count(&self) -> u64 {
        if !self.lanes.user {
            return 0;
        }
        match MemoryInboxRepository::new(&self.db)
            .ready_state(leveler_core::now())
            .await
        {
            Ok(state) => state.count,
            Err(error) => {
                tracing::warn!(%error, "could not inspect ready memory work");
                0
            }
        }
    }

    async fn experience_ready_count(&self) -> u64 {
        if !self.lanes.experience {
            return 0;
        }
        match ExperienceInboxRepository::new(&self.db)
            .ready_state(leveler_core::now())
            .await
        {
            Ok(state) => state.count,
            Err(error) => {
                tracing::warn!(%error, "could not inspect ready experience work");
                0
            }
        }
    }

    async fn recover_dead_claims(&self) -> Result<(), String> {
        let repo = MemoryInboxRepository::new(&self.db);
        for boot in repo.processing_boots().await.map_err(|e| e.to_string())? {
            if boot != self.boot
                && crate::runtime_boot::boot_liveness(&self.state_dir, &boot) == BootLiveness::Dead
            {
                let recovered = repo
                    .recover_claims_for_boot(
                        &boot,
                        "worker boot ended before claim settlement",
                        leveler_core::now(),
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                tracing::info!(
                    dead_boot = boot.as_str(),
                    recovered,
                    "recovered memory claims"
                );
            }
        }
        Ok(())
    }

    async fn recover_dead_experience_claims(&self) -> Result<(), String> {
        let repo = ExperienceInboxRepository::new(&self.db);
        for boot in repo.processing_boots().await.map_err(|e| e.to_string())? {
            if boot != self.boot
                && crate::runtime_boot::boot_liveness(&self.state_dir, &boot) == BootLiveness::Dead
            {
                let recovered = repo
                    .recover_claims_for_boot(
                        &boot,
                        "worker boot ended before claim settlement",
                        leveler_core::now(),
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                tracing::info!(
                    dead_boot = boot.as_str(),
                    recovered,
                    "recovered experience claims"
                );
            }
        }
        Ok(())
    }

    /// Process one completed task's experience work item.
    ///
    /// Returns whether a row was claimed. A deterministic skip (task not
    /// completed, no failure signal) is a settled result, not an error: it is
    /// recorded in `result_json` and never retried.
    async fn run_experience_once(&self) -> Result<bool, String> {
        if !self.lanes.experience {
            return Ok(false);
        }
        let repo = ExperienceInboxRepository::new(&self.db);
        repo.recover_claims_for_boot(
            &self.boot,
            "previous worker batch ended before claim settlement",
            leveler_core::now(),
        )
        .await
        .map_err(|e| e.to_string())?;
        self.recover_dead_experience_claims().await?;
        let claimed = repo
            .claim_batch(&self.boot, leveler_core::now(), EXPERIENCE_CLAIM_LIMIT)
            .await
            .map_err(|e| e.to_string())?;
        let Some(item) = claimed.into_iter().next() else {
            return Ok(false);
        };
        let result = self.process_experience_item(&item).await;
        let now = leveler_core::now();
        match result {
            Ok(staged) => {
                repo.mark_processed(&self.boot, item.inbox_id, now)
                    .await
                    .map_err(|e| e.to_string())?;
                tracing::info!(
                    event = "experience_consolidation_completed",
                    turn_id = %item.turn_id,
                    outcome = %staged.outcome,
                    reason = ?staged.reason,
                    accepted = staged.accepted.len(),
                    rejected = staged.rejected,
                    created = staged.created,
                    updated = staged.updated,
                    pending = staged.pending,
                    skipped = staged.skipped,
                    "experience consolidation completed"
                );
                if let Err(error) = repo
                    .prune_processed(
                        now - chrono::Duration::days(PROCESSED_RETENTION_DAYS),
                        PRUNE_LIMIT,
                    )
                    .await
                {
                    tracing::warn!(%error, event = "experience_prune_failed", "could not prune processed experience rows");
                }
                Ok(true)
            }
            Err(error) => {
                let next = leveler_core::now() + retry_delay(item.attempt.max(1));
                repo.mark_retryable(&self.boot, item.inbox_id, next, &error, leveler_core::now())
                    .await
                    .map_err(|settle| {
                        format!("{error}; failed to persist experience retry: {settle}")
                    })?;
                tracing::warn!(
                    event = "experience_consolidation_retried",
                    turn_id = %item.turn_id,
                    %error,
                    "experience extraction failed; durable retry scheduled"
                );
                Err(error)
            }
        }
    }

    /// Persist a deterministic skip before settling the claim. A skip is a
    /// real outcome: the status surface must be able to say "looked, found
    /// nothing" instead of "never processed".
    async fn settle_skipped_experience(
        &self,
        item: &ExperienceInboxItem,
        reason: impl Into<String>,
    ) -> Result<StagedExperienceResult, String> {
        let staged = skipped_experience(reason);
        let json = serde_json::to_string(&staged).map_err(|error| error.to_string())?;
        ExperienceInboxRepository::new(&self.db)
            .stage_result(&self.boot, item.inbox_id, &json, leveler_core::now())
            .await
            .map_err(|error| error.to_string())?;
        Ok(staged)
    }

    async fn process_experience_item(
        &self,
        item: &ExperienceInboxItem,
    ) -> Result<StagedExperienceResult, String> {
        // A staged result is a model answer already paid for. Replay it even
        // when the turn changed state since (it cannot: claims are only made
        // after the turn is terminal).
        if let Some(raw) = item.result_json.as_deref() {
            let staged: StagedExperienceResult = serde_json::from_str(raw).map_err(|error| {
                format!(
                    "turn {} has invalid staged experience result: {error}",
                    item.turn_id
                )
            })?;
            return self.apply_staged_experience(item, staged).await;
        }
        if item.turn_status != "completed" {
            return self
                .settle_skipped_experience(item, format!("task_{}", item.turn_status))
                .await;
        }
        // A fresh turn carries the initiating message; a resumed continuation
        // carries only the objective anchor. Both are user-authored goals and
        // both can be the goal of an experience.
        let initiation = leveler_engine::decode_turn_initiating_message_opt(&item.turn_payload)
            .map_err(|error| {
                format!(
                    "turn {} has invalid initiating payload: {error}",
                    item.turn_id
                )
            })?;
        let is_continuation = initiation.is_none();
        let goal = match initiation {
            Some(message) => message.text_content(),
            None => {
                leveler_engine::decode_turn_continuation(&item.turn_payload)
                    .map_err(|error| {
                        format!(
                            "turn {} has invalid continuation payload: {error}",
                            item.turn_id
                        )
                    })?
                    .objective
                    .text
            }
        };
        let session = leveler_core::SessionId::new(item.session_id.clone());
        let turn_id = leveler_core::TurnId::new(item.turn_id.clone());
        let events = EventRepository::new(&self.db);
        let Some(turn_finished) = events
            .load_last_by_type(&session, "turn_finished", Some(&turn_id))
            .await
            .map_err(|error| error.to_string())?
        else {
            return self
                .settle_skipped_experience(item, "turn_terminal_missing")
                .await;
        };
        // `task_finished` is a session-level terminal with no turn id. The
        // turn's own terminal sequence is the boundary that attributes the
        // first one after it to this turn. A crash between finish_turn and
        // finish_task leaves no terminal for THIS turn; the next turn's
        // terminal must not be inherited as this turn's outcome.
        let next_turn_started = events
            .first_after(&session, "turn_started", turn_finished.sequence)
            .await
            .map_err(|error| error.to_string())?;
        let Some(task_terminal) = events
            .first_after(&session, "task_finished", turn_finished.sequence)
            .await
            .map_err(|error| error.to_string())?
        else {
            return self
                .settle_skipped_experience(item, "task_outcome_missing")
                .await;
        };
        if next_turn_started
            .as_ref()
            .is_some_and(|next| task_terminal.sequence > next.sequence)
        {
            return self
                .settle_skipped_experience(item, "task_outcome_unattributed")
                .await;
        }
        let outcome = serde_json::from_str::<serde_json::Value>(&task_terminal.payload)
            .ok()
            .and_then(|document| {
                document
                    .get("payload")?
                    .get("outcome")?
                    .as_str()
                    .map(str::to_string)
            });
        if outcome.as_deref() != Some("completed") {
            return self
                .settle_skipped_experience(
                    item,
                    format!("task_outcome_{}", outcome.as_deref().unwrap_or("unknown")),
                )
                .await;
        }
        // A resumed continuation inherits the lineage's earlier facts: a
        // failure from before a crash can pair with the final pass. A fresh
        // turn reads only itself, so an unrelated older task cannot leak in.
        let turn_events = if is_continuation {
            let turns = leveler_storage::TurnStore::list_for_session(&self.db, &session)
                .await
                .map_err(|error| error.to_string())?;
            let current = turns
                .iter()
                .find(|turn| turn.id == item.turn_id)
                .ok_or_else(|| format!("turn {} has no durable row", item.turn_id))?;
            let root = leveler_engine::decode_turn_continuation(&item.turn_payload)
                .map_err(|error| error.to_string())?
                .root_turn_id;
            let min_ordinal = root
                .as_ref()
                .and_then(|root| {
                    turns
                        .iter()
                        .find(|turn| turn.id == root.as_str())
                        .map(|turn| turn.ordinal)
                })
                .unwrap_or(current.ordinal);
            let lineage: BTreeSet<String> = turns
                .into_iter()
                .filter(|turn| turn.ordinal >= min_ordinal)
                .map(|turn| turn.id)
                .collect();
            events
                .load_by_types(
                    &session,
                    &["tool_call_started", "tool_call_finished", "turn_finished"],
                )
                .await
                .map_err(|error| error.to_string())?
                .into_iter()
                .filter(|record| {
                    record
                        .turn_id
                        .as_deref()
                        .is_some_and(|turn| lineage.contains(turn))
                })
                .collect()
        } else {
            events
                .load_by_types_in_turn(
                    &session,
                    &turn_id,
                    &["tool_call_started", "tool_call_finished", "turn_finished"],
                )
                .await
                .map_err(|error| error.to_string())?
        };
        let mut experience_events: Vec<ExperienceEvent<'_>> = turn_events
            .iter()
            .map(|record| ExperienceEvent {
                turn_id: record.turn_id.as_deref(),
                payload: record.payload.as_str(),
            })
            .collect();
        experience_events.push(ExperienceEvent {
            turn_id: None,
            payload: task_terminal.payload.as_str(),
        });
        let Some(pack) = build_evidence_pack(&item.turn_id, &goal, &experience_events) else {
            return self
                .settle_skipped_experience(item, "no_experience_signal")
                .await;
        };
        // Identical evidence cannot teach anything the active store does not
        // already hold: the same failure, fix location and post-fix receipt
        // produced the lesson sitting there now. Skip before paying for a
        // provider call. A changed receipt, signature or file changes the
        // fingerprint and still extracts.
        let evidence_tag = format!("evidence:{}", evidence_fingerprint(&pack));
        if experience_evidence_learned(&self.memory, &evidence_tag)? {
            tracing::info!(
                event = "experience_extraction_skipped",
                turn_id = %item.turn_id,
                reason = "duplicate_evidence",
                "experience extraction skipped"
            );
            return self
                .settle_skipped_experience(item, "duplicate_evidence")
                .await;
        }
        tracing::info!(
            event = "experience_extraction_started",
            turn_id = %item.turn_id,
            failures = pack.failures.len(),
            resolutions = pack.resolutions.len(),
            mutations = pack.mutations.len(),
            observations = pack.observations.len(),
            "experience extraction started"
        );
        let model = ModelRef::parse(&item.model)
            .ok_or_else(|| format!("invalid persisted model reference `{}`", item.model))?;
        let extractor = ModelExperienceExtractor::new(self.runtime.clone(), model)
            .with_timeout(self.extraction_timeout);
        let ExperienceExtraction { candidates, call } =
            extractor.extract(&pack, &self.cancel).await;
        if let Some(call) = call.as_ref() {
            self.record_experience_call(&item.session_id, call).await;
        }
        let candidates = candidates.map_err(|error| error.to_string())?;
        let mut validated = validate_experience_candidates(&candidates, &pack);
        for candidate in &mut validated.accepted {
            // The tag is what makes a later identical evidence pack
            // recognisable as already learned. It is generated, never model
            // supplied.
            candidate.tags.push(evidence_tag.clone());
        }
        for (index, rejection) in &validated.rejected {
            tracing::debug!(
                event = "experience_candidate_rejected",
                turn_id = %item.turn_id,
                index = *index,
                code = rejection.code(),
                reason = %rejection.reason(),
                "experience candidate rejected"
            );
        }
        let staged = StagedExperienceResult {
            outcome: "extracted".to_string(),
            reason: None,
            rejected: validated.rejected.len(),
            accepted: validated.accepted,
            applied: 0,
            skipped: 0,
            created: 0,
            updated: 0,
            pending: 0,
        };
        let staged_json = serde_json::to_string(&staged).map_err(|error| error.to_string())?;
        ExperienceInboxRepository::new(&self.db)
            .stage_result(&self.boot, item.inbox_id, &staged_json, leveler_core::now())
            .await
            .map_err(|error| error.to_string())?;
        self.apply_staged_experience(item, staged).await
    }

    async fn apply_staged_experience(
        &self,
        item: &ExperienceInboxItem,
        mut staged: StagedExperienceResult,
    ) -> Result<StagedExperienceResult, String> {
        if staged.outcome != "extracted" {
            return Ok(staged);
        }
        if staged.applied > staged.accepted.len() {
            return Err(format!(
                "turn {} staged experience apply cursor is out of bounds",
                item.turn_id
            ));
        }
        let repo = ExperienceInboxRepository::new(&self.db);
        // Runtime extraction writes under a task identity derived from the
        // durable inbox row: the authority is the runtime's, never the model's.
        let actor = leveler_memory::v2::Actor::Runtime {
            task: leveler_core::TaskId::new(format!("memory-inbox:{}", item.inbox_id)),
        };
        while staged.applied < staged.accepted.len() {
            let candidate = staged.accepted[staged.applied].clone();
            // The cursor is persisted after each write, so a retry replays the
            // same operation identity instead of creating a second record.
            let op_seed = format!("experience:{}:{}", item.inbox_id, staged.applied);
            let outcome = self
                .memory
                .admit_candidate(&actor, ServiceScope::Project, &candidate, &op_seed)
                .map_err(|error| error.to_string())?;
            staged.applied += 1;
            let event = match outcome {
                WriteOutcome::Applied {
                    id,
                    title,
                    operation,
                    ..
                } => match operation.as_str() {
                    "created" => {
                        staged.created += 1;
                        Some(("experience_created", id, title))
                    }
                    "merged" => {
                        staged.updated += 1;
                        Some(("experience_merged", id, title))
                    }
                    _ => {
                        staged.updated += 1;
                        Some(("experience_updated", id, title))
                    }
                },
                WriteOutcome::Pending { id, title, .. } => {
                    staged.pending += 1;
                    (!id.is_empty()).then_some(("experience_queued", id, title))
                }
                WriteOutcome::Skipped { reason, .. } => {
                    staged.skipped += 1;
                    tracing::debug!(
                        event = "experience_write_skipped",
                        turn_id = %item.turn_id,
                        %reason,
                        "experience write skipped"
                    );
                    None
                }
                WriteOutcome::NotStored { reason } | WriteOutcome::Rejected { reason } => {
                    staged.skipped += 1;
                    tracing::info!(
                        event = "experience_write_not_stored",
                        turn_id = %item.turn_id,
                        %reason,
                        "experience write not stored"
                    );
                    None
                }
                WriteOutcome::Conflict { target, .. } => {
                    return Err(format!(
                        "experience write for `{target}` raced a concurrent change"
                    ));
                }
                WriteOutcome::Forgotten { .. } => {
                    return Err("experience admission cannot forget a record".to_string());
                }
            };
            let persisted = serde_json::to_string(&staged).map_err(|error| error.to_string())?;
            repo.update_staged_result(&self.boot, item.inbox_id, &persisted, leveler_core::now())
                .await
                .map_err(|error| error.to_string())?;
            if let Some((operation, id, title)) = event {
                (self.event_sink)(MemoryConsolidationEvent {
                    session_id: item.session_id.clone(),
                    operation: operation.to_string(),
                    id,
                    title,
                    authority: "tool_observation".to_string(),
                    count: 1,
                });
            }
        }
        Ok(staged)
    }

    async fn run_once(&self) -> Result<ConsolidationOutcome, String> {
        if !self.lanes.user {
            return Ok(ConsolidationOutcome {
                claimed: 0,
                accepted: 0,
                rejected: 0,
            });
        }
        let repo = MemoryInboxRepository::new(&self.db);
        // The worker calls this method serially. A claim still owned by this
        // boot therefore belongs to an earlier invocation whose settlement
        // write failed; return it to the queue and replay idempotently.
        repo.recover_claims_for_boot(
            &self.boot,
            "previous worker batch ended before claim settlement",
            leveler_core::now(),
        )
        .await
        .map_err(|e| e.to_string())?;
        // A competing process can die after our startup pass, so dead-boot
        // recovery is part of the work loop rather than a startup-only repair.
        self.recover_dead_claims().await?;
        let items = repo
            .claim_batch(&self.boot, leveler_core::now(), MAX_WORKER_BATCH_TURNS)
            .await
            .map_err(|e| e.to_string())?;
        if items.is_empty() {
            return Ok(ConsolidationOutcome {
                claimed: 0,
                accepted: 0,
                rejected: 0,
            });
        }
        let ids = items.iter().map(|item| item.inbox_id).collect::<Vec<_>>();
        let model = ModelRef::parse(&items[0].model)
            .ok_or_else(|| format!("invalid persisted model reference `{}`", items[0].model));
        let result = match model {
            Ok(model) => {
                let extractor = ModelSemanticExtractor::new(self.runtime.clone(), model)
                    .with_timeout(self.extraction_timeout);
                self.process_claimed(&items, &extractor).await
            }
            Err(error) => Err(error),
        };
        match result {
            Ok(outcome) => {
                repo.mark_processed(&self.boot, &ids, leveler_core::now())
                    .await
                    .map_err(|e| e.to_string())?;
                tracing::info!(
                    event = "memory_consolidation_completed",
                    batch_size = outcome.claimed,
                    candidate_count = outcome.accepted + outcome.rejected,
                    accepted = outcome.accepted,
                    rejected = outcome.rejected,
                    "memory consolidation completed"
                );
                tracing::debug!(
                    event = "memory_inbox_processed",
                    processed = ids.len(),
                    "memory inbox claims settled"
                );
                if let Err(error) = repo
                    .prune_processed(
                        leveler_core::now() - chrono::Duration::days(PROCESSED_RETENTION_DAYS),
                        PRUNE_LIMIT,
                    )
                    .await
                {
                    tracing::warn!(%error, event = "memory_prune_failed", "could not prune processed memory inbox rows");
                }
                Ok(outcome)
            }
            Err(error) => {
                tracing::warn!(
                    event = "memory_consolidation_failed",
                    batch_size = items.len(),
                    %error,
                    "memory consolidation batch failed"
                );
                let next = leveler_core::now()
                    + retry_delay(items.iter().map(|i| i.attempt).max().unwrap_or(1));
                repo.mark_retryable(&self.boot, &ids, next, &error, leveler_core::now())
                    .await
                    .map_err(|settle| format!("{error}; failed to persist retry: {settle}"))?;
                tracing::warn!(
                    event = "memory_consolidation_retried",
                    batch_size = items.len(),
                    pending = items.len(),
                    %error,
                    "memory consolidation failed; durable retry scheduled"
                );
                Err(error)
            }
        }
    }

    async fn process_claimed(
        &self,
        items: &[MemoryInboxItem],
        extractor: &dyn SemanticExtractor,
    ) -> Result<ConsolidationOutcome, String> {
        let staged_count = items
            .iter()
            .filter(|item| item.result_json.is_some())
            .count();
        if staged_count != 0 && staged_count != items.len() {
            return Err("memory inbox batch has partially staged results".to_string());
        }
        let staged = if staged_count == items.len() {
            items
                .iter()
                .map(|item| {
                    serde_json::from_str::<StagedTurnResult>(
                        item.result_json.as_deref().expect("all results are staged"),
                    )
                    .map_err(|error| {
                        format!(
                            "turn {} has invalid staged memory result: {error}",
                            item.turn_id
                        )
                    })
                })
                .collect::<Result<Vec<_>, _>>()?
        } else {
            self.extract_and_stage(items, extractor).await?
        };

        self.apply_staged(items, staged).await
    }

    async fn extract_and_stage(
        &self,
        items: &[MemoryInboxItem],
        extractor: &dyn SemanticExtractor,
    ) -> Result<Vec<StagedTurnResult>, String> {
        let mut sources = Vec::with_capacity(items.len());
        // Session per source turn, index-aligned with `sources`. A provider
        // request never spans two sessions, so every recorded call has one
        // owning session and cost attribution stays exact.
        let mut source_sessions: Vec<String> = Vec::with_capacity(items.len());
        let mut staged = items
            .iter()
            .map(|_| StagedTurnResult {
                accepted: Vec::new(),
                rejected: 0,
                applied: 0,
                skipped: 0,
            })
            .collect::<Vec<_>>();
        tracing::info!(
            event = "memory_consolidation_started",
            batch_size = items.len(),
            model = %items[0].model,
            "memory consolidation started"
        );
        for (index, item) in items.iter().enumerate() {
            let message = leveler_engine::decode_turn_initiating_message(&item.turn_payload)
                .map_err(|e| {
                    format!("turn {} has invalid initiating payload: {e}", item.turn_id)
                })?;
            let user_text: String = message
                .text_content()
                .chars()
                .take(MAX_EXTRACTOR_INPUT_CHARS)
                .collect();
            if user_text.trim().is_empty() {
                continue;
            }
            // Deterministic pre-provider secret gate. A turn that appears to
            // carry credential material is not sent to the extractor at all:
            // the model is never asked "is this a secret?", and the turn
            // produces no candidate. This affects ONLY auto-memory extraction —
            // the user's conversation, the main coding request, and the
            // authoritative turn payload are untouched.
            if leveler_memory::looks_like_secret(&user_text) {
                staged[index].skipped += 1;
                tracing::info!(
                    event = "memory_secret_gate_skipped",
                    turn_id = %item.turn_id,
                    "turn withheld from memory extraction by the secret gate"
                );
                continue;
            }
            sources.push(BatchSourceTurn {
                source_turn_id: item.turn_id.clone(),
                user_text,
            });
            source_sessions.push(item.session_id.clone());
        }

        let mut candidates = Vec::new();
        // The bounded identity list below is context, not an allowlist: a real
        // older identity outside the window must still be replaceable, so such
        // a target is verified against the store instead of being rejected.
        let mut known_ids = BTreeSet::new();
        if !sources.is_empty() {
            // Structured identity retrieval, not authority: the extractor may
            // reuse a real subject or name a real id as a replacement target,
            // but the fact still has to be present in the user's own turn.
            let identities = self
                .memory
                .identity_context(
                    ServiceScope::Project,
                    leveler_memory::v2::IDENTITY_CONTEXT_LIMIT,
                )
                .map_err(|error| error.to_string())?;
            known_ids.extend(identities.iter().map(|identity| identity.id.clone()));
            let context = ExtractionContext {
                known_identities: identities
                    .into_iter()
                    .map(|identity| KnownIdentity {
                        memory_id: identity.id,
                        subject: identity.subject_key,
                        title: identity.title,
                        value: identity.value,
                    })
                    .collect(),
            };
            let semaphore = GLOBAL_MEMORY_CALL.get_or_init(|| Semaphore::new(1));
            let _permit = tokio::select! {
                _ = self.cancel.cancelled() => return Err("memory consolidation cancelled".to_string()),
                permit = semaphore.acquire() => {
                    permit.map_err(|_| "global memory extraction semaphore closed".to_string())?
                }
            };
            let mut batch_start = 0;
            while batch_start < sources.len() {
                let mut batch_end = batch_start;
                let mut input_chars = 0;
                let batch_session = source_sessions[batch_start].as_str();
                while batch_end < sources.len() && batch_end - batch_start < MAX_BATCH_TURNS {
                    // A batch is one session: a single provider request must
                    // have one owning session for cost attribution.
                    if source_sessions[batch_end] != batch_session {
                        break;
                    }
                    let next_chars = sources[batch_end].user_text.chars().count();
                    if batch_end > batch_start && input_chars + next_chars > MAX_BATCH_INPUT_CHARS {
                        break;
                    }
                    input_chars += next_chars;
                    batch_end += 1;
                }
                let (result, call) = extractor
                    .extract_batch_recorded_with_context(
                        &sources[batch_start..batch_end],
                        &context,
                        &self.cancel,
                    )
                    .await;
                if let Some(call) = call {
                    self.record_extraction_call(batch_session, &call).await;
                }
                candidates.extend(result.map_err(|e| e.to_string())?);
                batch_start = batch_end;
            }
        }
        tracing::debug!(
            event = "memory_candidate_count",
            batch_size = items.len(),
            candidate_count = candidates.len(),
            "memory candidates extracted"
        );
        let mut validated = validate_batch_candidates(candidates, &sources);
        validated.sort_by_key(|candidate| {
            items
                .iter()
                .position(|item| item.turn_id == candidate.source_turn_id)
                .unwrap_or(usize::MAX)
        });

        for candidate in validated {
            let index = items
                .iter()
                .position(|item| item.turn_id == candidate.source_turn_id);
            match (index, candidate.result) {
                (Some(index), Ok(memory)) => {
                    let replacement_verified = match memory.replaces_id.as_deref() {
                        None => true,
                        Some(target) if known_ids.contains(target) => true,
                        Some(target) => match self.memory.read(
                            &leveler_memory::v2::Actor::External,
                            ServiceScope::Project,
                            target,
                        ) {
                            Ok(_) => true,
                            // A guessed, already-superseded, or malformed id is
                            // not a real replacement target.
                            Err(leveler_memory::ServiceError::NotFound(_))
                            | Err(leveler_memory::ServiceError::NotCurrent(_))
                            | Err(leveler_memory::ServiceError::Invalid(_)) => false,
                            // The store could not answer. Rejecting here would
                            // silently drop a possibly valid replacement, so the
                            // row stays unprocessed and retries.
                            Err(error) => {
                                return Err(format!(
                                    "memory replacement target check failed: {error}"
                                ));
                            }
                        },
                    };
                    if !replacement_verified {
                        // The model named a target that is neither in this
                        // request's context nor current in the store. A guessed
                        // id must not reach the lifecycle as if it were
                        // verified.
                        staged[index].rejected += 1;
                        tracing::info!(
                            event = "memory_candidate_rejected",
                            turn_id = %candidate.source_turn_id,
                            reason = "unknown_replacement_target",
                            "memory candidate named an identity that is not current in the store"
                        );
                    } else {
                        staged[index].accepted.push(memory);
                    }
                }
                (Some(index), Err(_)) => staged[index].rejected += 1,
                (None, _) => staged[0].rejected += 1,
            }
        }
        let serialized = items
            .iter()
            .zip(&staged)
            .map(|(item, result)| {
                serde_json::to_string(result)
                    .map(|json| (item.inbox_id, json))
                    .map_err(|error| error.to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        MemoryInboxRepository::new(&self.db)
            .stage_results(&self.boot, &serialized, leveler_core::now())
            .await
            .map_err(|error| error.to_string())?;
        Ok(staged)
    }

    /// Record ONE memory-extraction provider call in the session's model-call
    /// ledger. `MemoryExtraction` is its own lane, so a user asking "why was
    /// there another model call after my turn?" can read the answer, and
    /// auxiliary spend is separable from the coding work.
    ///
    /// Never records user text or candidate bodies — only the shape of the
    /// exchange, exactly like every other row in this table.
    async fn record_extraction_call(&self, session_id: &str, call: &ExtractionCall) {
        self.record_auxiliary_call(
            session_id,
            call,
            leveler_storage::ModelCallKind::MemoryExtraction,
        )
        .await
    }

    async fn record_experience_call(&self, session_id: &str, call: &ExtractionCall) {
        self.record_auxiliary_call(
            session_id,
            call,
            leveler_storage::ModelCallKind::ExperienceExtraction,
        )
        .await
    }

    async fn record_auxiliary_call(
        &self,
        session_id: &str,
        call: &ExtractionCall,
        kind: leveler_storage::ModelCallKind,
    ) {
        let session = leveler_core::SessionId::new(session_id);
        // Pricing is optional: an unpriced model records `None` rather than a
        // fabricated zero cost.
        let pricing = self
            .runtime
            .profile(&call.model)
            .await
            .ok()
            .and_then(|profile| profile.pricing);
        let usage = call.usage.unwrap_or_default();
        let cost_usd_micros = pricing
            .filter(|_| usage.total() > 0)
            .and_then(|p| p.cost_for_usage(&usage));
        let record = ModelRequestRecord {
            budget_scope: None,
            estimated_tokens: None,
            id: leveler_core::EventId::generate().into_inner(),
            provider_request_id: call.provider_request_id.clone(),
            session_id: session,
            provider: call.model.provider.clone(),
            model: call.model.model.clone(),
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cached_input_tokens: call.usage.map(|u| u.cached_input_tokens),
            reasoning_tokens: usage.reasoning_tokens,
            // The memory-extraction call builds its own request; no projection
            // was measured for it, which is an absence, not a zero.
            projected_input_tokens: None,
            projected_reasoning_tokens: None,
            cost_usd_micros,
            agent_id: None,
            finish_reason: call.finish_reason.map(|reason| {
                serde_json::to_value(reason)
                    .ok()
                    .and_then(|value| value.as_str().map(ToOwned::to_owned))
                    .unwrap_or_else(|| format!("{reason:?}").to_ascii_lowercase())
            }),
            // The class of failure, not the provider's message: a diagnostics
            // row must not carry raw upstream text.
            error_kind: call
                .error
                .as_deref()
                .map(|_| format!("{}_failed", kind.as_str())),
            // The consolidator keeps the failure class only; it holds no
            // transport error to observe.
            error_detail: None,
            latency_ms: Some(call.latency_ms),
            attempt_ms: None,
            connect_ms: None,
            ttft_ms: None,
            max_event_gap_ms: None,
            retry_count: 0,
            kind,
            created_at: leveler_core::now(),
            reasoning_effort: call
                .reasoning_effort
                .map(|effort| effort.as_wire().to_string()),
        };
        if let Err(error) = ModelRequestRepository::new(&self.db).insert(&record).await {
            // A diagnostics row must never fail the consolidation it describes.
            tracing::warn!(
                %error,
                event = "auxiliary_extraction_record_failed",
                lane = kind.as_str(),
                "could not record the auxiliary extraction model call"
            );
        }
        tracing::info!(
            event = "auxiliary_extraction_call",
            lane = kind.as_str(),
            model = %call.model,
            source_turn_count = call.source_turn_count,
            input_tokens = usage.input_tokens,
            output_tokens = usage.output_tokens,
            reasoning_tokens = ?usage.reasoning_tokens,
            latency_ms = call.latency_ms,
            cost_usd_micros = ?cost_usd_micros,
            error = ?call.error,
            "auxiliary extraction model call completed"
        );
    }

    async fn apply_staged(
        &self,
        items: &[MemoryInboxItem],
        mut staged: Vec<StagedTurnResult>,
    ) -> Result<ConsolidationOutcome, String> {
        let rejected = staged.iter().map(|result| result.rejected).sum();
        let accepted = staged.iter().map(|result| result.accepted.len()).sum();
        let repo = MemoryInboxRepository::new(&self.db);
        let mut proposed_by_session = BTreeMap::<String, BTreeSet<String>>::new();
        for (item, result) in items.iter().zip(&mut staged) {
            if result.applied > result.accepted.len() {
                return Err(format!(
                    "turn {} staged apply cursor is out of bounds",
                    item.turn_id
                ));
            }
            // Runtime extraction writes under a task identity derived from the
            // durable inbox row; the candidate cannot claim user authority.
            let actor = leveler_memory::v2::Actor::Runtime {
                task: leveler_core::TaskId::new(format!("memory-inbox:{}", item.inbox_id)),
            };
            while result.applied < result.accepted.len() {
                let candidate = result.accepted[result.applied].clone();
                let authority = candidate.authority.as_str().to_string();
                // The cursor is persisted after each write, so a retry replays
                // the same operation identity instead of duplicating a record.
                let op_seed = format!("inbox:{}:{}", item.inbox_id, result.applied);
                let outcome = self
                    .memory
                    .admit_candidate(&actor, ServiceScope::Project, &candidate, &op_seed)
                    .map_err(|e| e.to_string())?;
                result.applied += 1;
                let result_json = serde_json::to_string(result).map_err(|e| e.to_string())?;
                repo.update_staged_result(
                    &self.boot,
                    item.inbox_id,
                    &result_json,
                    leveler_core::now(),
                )
                .await
                .map_err(|e| e.to_string())?;
                let event = match outcome {
                    WriteOutcome::Applied {
                        id,
                        title,
                        operation,
                        ..
                    } => {
                        let operation = match operation.as_str() {
                            "created" | "superseded" | "merged" => operation.as_str(),
                            // v2 names a lifecycle update directly; the
                            // transcript has no separate verb for it.
                            _ => "superseded",
                        };
                        Some((operation.to_string(), id, title))
                    }
                    WriteOutcome::Pending { id, reason, .. } => {
                        if reason == "awaiting_user_review" || reason == "already_pending" {
                            proposed_by_session
                                .entry(item.session_id.clone())
                                .or_default()
                                .insert(id);
                        } else {
                            result.skipped += 1;
                        }
                        None
                    }
                    WriteOutcome::Skipped { reason, .. } | WriteOutcome::NotStored { reason } => {
                        result.skipped += 1;
                        tracing::debug!(
                            event = "memory_candidate_not_applied",
                            turn_id = %item.turn_id,
                            %reason,
                            "memory candidate not applied"
                        );
                        None
                    }
                    WriteOutcome::Rejected { reason } => {
                        result.skipped += 1;
                        tracing::info!(
                            event = "memory_candidate_rejected_by_store",
                            turn_id = %item.turn_id,
                            %reason,
                            "memory candidate rejected by the store"
                        );
                        None
                    }
                    WriteOutcome::Conflict { target, .. } => {
                        return Err(format!(
                            "memory write for `{target}` raced a concurrent change"
                        ));
                    }
                    WriteOutcome::Forgotten { .. } => {
                        return Err("memory admission cannot forget a record".to_string());
                    }
                };
                if let Some((operation, id, title)) = event {
                    (self.event_sink)(MemoryConsolidationEvent {
                        session_id: item.session_id.clone(),
                        operation: operation.to_string(),
                        id,
                        title,
                        authority: authority.clone(),
                        count: 1,
                    });
                }
            }
        }
        for (session_id, candidate_ids) in proposed_by_session {
            (self.event_sink)(MemoryConsolidationEvent {
                session_id,
                operation: "proposed".to_string(),
                id: String::new(),
                title: String::new(),
                authority: "explicit_user".to_string(),
                count: candidate_ids.len(),
            });
        }
        Ok(ConsolidationOutcome {
            claimed: items.len(),
            accepted,
            rejected,
        })
    }
}

fn skipped_experience(reason: impl Into<String>) -> StagedExperienceResult {
    StagedExperienceResult {
        outcome: "skipped".to_string(),
        reason: Some(reason.into()),
        ..Default::default()
    }
}

/// Whether an ACTIVE lesson already carries this evidence identity. Archived
/// and superseded entries do not count: once a lesson is invalidated, the same
/// evidence may be worth re-learning against the current environment.
fn experience_evidence_learned(memory: &MemoryService, tag: &str) -> Result<bool, String> {
    let active = memory
        .list(
            &leveler_memory::v2::Actor::External,
            ServiceScope::Project,
            leveler_memory::MemoryLanes::ALL,
            None,
        )
        .map_err(|error| error.to_string())?;
    Ok(active
        .iter()
        .any(|entry| entry.is_experience() && entry.tags.iter().any(|candidate| candidate == tag)))
}

fn retry_delay(attempt: i64) -> chrono::Duration {
    let exponent = u32::try_from(attempt.saturating_sub(1))
        .unwrap_or(u32::MAX)
        .min(62);
    let seconds = 1_u64
        .checked_shl(exponent)
        .unwrap_or(u64::MAX)
        .min(MAX_RETRY_DELAY.as_secs());
    chrono::Duration::seconds(seconds as i64)
}

fn wake_delay(state: &MemoryInboxReadyState, now: leveler_core::Timestamp) -> Duration {
    if state.count >= IMMEDIATE_BATCH_THRESHOLD as u64 {
        return Duration::ZERO;
    }
    let Some(oldest) = state.oldest_created_at else {
        // Also provides a bounded poll for a retry becoming due or another
        // process dying while it owns a claim.
        return MAX_AGE_FLUSH;
    };
    let deadline = oldest
        + chrono::Duration::from_std(MAX_AGE_FLUSH)
            .expect("memory max-age duration is representable");
    (deadline - now).to_std().unwrap_or(Duration::ZERO)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use leveler_agent::{BatchSemanticCandidate, ExtractionError};
    use leveler_memory::{
        AdmitOutcome, AgentAdmitOutcome, CandidateDurability, CandidateScope, MemoryAuthority,
        MemoryStore, OperationHint, SemanticCandidate,
    };
    use leveler_model::{
        FinishReason, Message, ModelError, ModelErrorKind, ModelEventStream, ModelProfile,
        ModelRequest, ModelResponse, Role, TokenUsage,
    };
    use leveler_storage::{SessionRecord, SessionRepository, TurnRepository};

    use super::*;

    struct UnusedRuntime;

    struct HangingRuntime;

    #[async_trait]
    impl ModelRuntime for UnusedRuntime {
        async fn stream(
            &self,
            _: ModelRequest,
            _: CancellationToken,
        ) -> Result<ModelEventStream, ModelError> {
            panic!("test supplies its extractor directly")
        }

        async fn generate(
            &self,
            _: ModelRequest,
            _: CancellationToken,
        ) -> Result<ModelResponse, ModelError> {
            panic!("test supplies its extractor directly")
        }

        async fn profile(&self, _: &ModelRef) -> Result<ModelProfile, ModelError> {
            Err(ModelError::new(
                leveler_model::ModelErrorKind::ProviderUnavailable,
                "no profile in tests",
            ))
        }
    }

    #[async_trait]
    impl ModelRuntime for HangingRuntime {
        async fn stream(
            &self,
            _: ModelRequest,
            _: CancellationToken,
        ) -> Result<ModelEventStream, ModelError> {
            unreachable!()
        }

        async fn generate(
            &self,
            _: ModelRequest,
            _: CancellationToken,
        ) -> Result<ModelResponse, ModelError> {
            futures::future::pending().await
        }

        async fn profile(&self, _: &ModelRef) -> Result<ModelProfile, ModelError> {
            Err(ModelError::new(
                leveler_model::ModelErrorKind::ProviderUnavailable,
                "no profile in tests",
            ))
        }
    }

    struct ReversedExtractor;

    struct OneCandidateExtractor;

    /// Counts provider-side invocations so a test can prove the extractor was
    /// never asked to read a turn.
    struct CountingExtractor {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl SemanticExtractor for CountingExtractor {
        async fn extract(
            &self,
            _: &str,
            _: &CancellationToken,
        ) -> Result<Vec<SemanticCandidate>, ExtractionError> {
            unreachable!()
        }

        async fn extract_batch(
            &self,
            _: &[BatchSourceTurn],
            _: &CancellationToken,
        ) -> Result<Vec<BatchSemanticCandidate>, ExtractionError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Vec::new())
        }
    }

    struct BackgroundCorrectionExtractor;

    struct ScriptedRuntime {
        calls: AtomicUsize,
        replies: Mutex<VecDeque<Result<String, ModelError>>>,
    }

    impl ScriptedRuntime {
        fn new(replies: Vec<Result<&str, ModelError>>) -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                replies: Mutex::new(
                    replies
                        .into_iter()
                        .map(|reply| reply.map(str::to_string))
                        .collect(),
                ),
            })
        }
    }

    #[async_trait]
    impl ModelRuntime for ScriptedRuntime {
        async fn stream(
            &self,
            _: ModelRequest,
            _: CancellationToken,
        ) -> Result<ModelEventStream, ModelError> {
            unreachable!()
        }

        async fn generate(
            &self,
            request: ModelRequest,
            _: CancellationToken,
        ) -> Result<ModelResponse, ModelError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let text = self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok("[]".into()))?;
            Ok(ModelResponse {
                request_id: request.request_id,
                message: Message::text(Role::Assistant, text),
                finish_reason: FinishReason::Stop,
                usage: TokenUsage::default(),
            })
        }

        async fn profile(&self, _: &ModelRef) -> Result<ModelProfile, ModelError> {
            Err(ModelError::new(
                leveler_model::ModelErrorKind::ProviderUnavailable,
                "no profile in tests",
            ))
        }
    }

    #[async_trait]
    impl SemanticExtractor for ReversedExtractor {
        async fn extract(
            &self,
            _: &str,
            _: &CancellationToken,
        ) -> Result<Vec<SemanticCandidate>, ExtractionError> {
            unreachable!()
        }

        async fn extract_batch(
            &self,
            turns: &[BatchSourceTurn],
            _: &CancellationToken,
        ) -> Result<Vec<BatchSemanticCandidate>, ExtractionError> {
            Ok(vec![
                envelope(
                    &turns[1],
                    "项目默认模型是 Flash",
                    "Flash",
                    OperationHint::Update,
                ),
                envelope(
                    &turns[0],
                    "项目默认模型是 Pro",
                    "Pro",
                    OperationHint::Create,
                ),
            ])
        }
    }

    #[async_trait]
    impl SemanticExtractor for OneCandidateExtractor {
        async fn extract(
            &self,
            _: &str,
            _: &CancellationToken,
        ) -> Result<Vec<SemanticCandidate>, ExtractionError> {
            unreachable!()
        }

        async fn extract_batch(
            &self,
            turns: &[BatchSourceTurn],
            _: &CancellationToken,
        ) -> Result<Vec<BatchSemanticCandidate>, ExtractionError> {
            Ok(vec![envelope(
                &turns[0],
                "turn 0",
                "Pro",
                OperationHint::Create,
            )])
        }
    }

    /// Names one exact replacement target, as a model answering with an id.
    struct ReplacementExtractor {
        target: String,
    }

    #[async_trait]
    impl SemanticExtractor for ReplacementExtractor {
        async fn extract(
            &self,
            _: &str,
            _: &CancellationToken,
        ) -> Result<Vec<SemanticCandidate>, ExtractionError> {
            unreachable!()
        }

        async fn extract_batch(
            &self,
            turns: &[BatchSourceTurn],
            _: &CancellationToken,
        ) -> Result<Vec<BatchSemanticCandidate>, ExtractionError> {
            Ok(vec![BatchSemanticCandidate {
                source_turn_id: turns[0].source_turn_id.clone(),
                candidate: SemanticCandidate {
                    fact: "项目默认模型改成 Flash".to_string(),
                    subject: "project.default_model".to_string(),
                    related_subject: None,
                    replaces_memory_id: Some(self.target.clone()),
                    value: Some("Flash".to_string()),
                    statement: leveler_memory::StatementKind::Assertion,
                    scope: CandidateScope::Project,
                    durability: CandidateDurability::Durable,
                    memory_type: leveler_memory::SemanticMemoryType::Project,
                    authority: MemoryAuthority::ExplicitUser,
                    operation_hint: OperationHint::Update,
                    evidence_span: "项目默认模型改成 Flash".to_string(),
                    confidence: Some(1.0),
                },
            }])
        }
    }

    #[async_trait]
    impl SemanticExtractor for BackgroundCorrectionExtractor {
        async fn extract(
            &self,
            _: &str,
            _: &CancellationToken,
        ) -> Result<Vec<SemanticCandidate>, ExtractionError> {
            unreachable!()
        }

        async fn extract_batch(
            &self,
            turns: &[BatchSourceTurn],
            _: &CancellationToken,
        ) -> Result<Vec<BatchSemanticCandidate>, ExtractionError> {
            Ok(vec![BatchSemanticCandidate {
                source_turn_id: turns[0].source_turn_id.clone(),
                candidate: SemanticCandidate {
                    fact: "以后不要后台启动服务".to_string(),
                    subject: "service.background_start".to_string(),
                    related_subject: None,
                    replaces_memory_id: None,
                    value: Some("disabled".to_string()),
                    statement: leveler_memory::StatementKind::Assertion,
                    scope: CandidateScope::Project,
                    durability: CandidateDurability::Durable,
                    memory_type: leveler_memory::SemanticMemoryType::Feedback,
                    authority: MemoryAuthority::ExplicitUser,
                    operation_hint: OperationHint::Update,
                    evidence_span: "以后不要后台启动服务了".to_string(),
                    confidence: Some(1.0),
                },
            }])
        }
    }

    fn envelope(
        turn: &BatchSourceTurn,
        fact: &str,
        value: &str,
        operation_hint: OperationHint,
    ) -> BatchSemanticCandidate {
        BatchSemanticCandidate {
            source_turn_id: turn.source_turn_id.clone(),
            candidate: SemanticCandidate {
                fact: fact.to_string(),
                subject: "project.default_model".to_string(),
                related_subject: None,
                replaces_memory_id: None,
                value: Some(value.to_string()),
                statement: leveler_memory::StatementKind::Assertion,
                scope: CandidateScope::Project,
                durability: CandidateDurability::Durable,
                memory_type: leveler_memory::SemanticMemoryType::Project,
                authority: MemoryAuthority::ExplicitUser,
                operation_hint,
                evidence_span: fact.to_string(),
                confidence: Some(1.0),
            },
        }
    }

    async fn admitted_messages(
        db: &Database,
        messages: impl IntoIterator<Item = String>,
    ) -> Vec<(String, leveler_core::Timestamp)> {
        let record = SessionRecord::new("/repo", "goal", "mock/model", leveler_core::now());
        let session = leveler_core::SessionId::new(record.id.clone());
        SessionRepository::new(db).create(&record).await.unwrap();
        let mut ids = Vec::new();
        for message in messages {
            let admitted_at = leveler_core::now();
            let payload = serde_json::json!({
                "version": 1,
                "initiating_message": {
                    "role": "user",
                    "content": [{"type": "text", "text": message}]
                }
            });
            let turn = TurnRepository::new(db)
                .start(&session, "user", Some(&payload.to_string()), admitted_at)
                .await
                .unwrap();
            ids.push((turn.id.clone(), admitted_at));
            TurnRepository::new(db)
                .finish(
                    &leveler_core::TurnId::new(turn.id),
                    "completed",
                    leveler_core::now(),
                )
                .await
                .unwrap();
        }
        ids
    }

    async fn admitted_turns(db: &Database, count: usize) -> Vec<leveler_core::Timestamp> {
        admitted_messages(db, (0..count).map(|index| format!("turn {index}")))
            .await
            .into_iter()
            .map(|(_, admitted_at)| admitted_at)
            .collect()
    }

    /// Admit one fresh user turn for a session whose memory flag is `enabled`.
    async fn admit_with_memory_flag(db: &Database, enabled: bool, text: &str) -> i64 {
        let record = SessionRecord::new("/repo", "goal", "mock/model", leveler_core::now());
        let session = leveler_core::SessionId::new(record.id.clone());
        SessionRepository::new(db).create(&record).await.unwrap();
        SessionRepository::new(db)
            .set_memory_enabled(&session, enabled)
            .await
            .unwrap();
        let payload = serde_json::json!({
            "version": 1,
            "initiating_message": {
                "role": "user",
                "content": [{"type": "text", "text": text}]
            }
        });
        TurnRepository::new(db)
            .start(
                &session,
                "user",
                Some(&payload.to_string()),
                leveler_core::now(),
            )
            .await
            .unwrap();
        MemoryInboxRepository::new(db)
            .counts()
            .await
            .unwrap()
            .pending as i64
    }

    #[tokio::test]
    async fn a_disabled_memory_policy_admits_no_turn() {
        let db = Database::connect_in_memory().await.unwrap();
        assert_eq!(admit_with_memory_flag(&db, false, "记住用 Pro").await, 0);
        assert_eq!(admit_with_memory_flag(&db, true, "记住用 Flash").await, 1);
    }

    #[tokio::test]
    async fn a_closed_user_lane_claims_no_user_work() {
        let temp = tempfile::tempdir().unwrap();
        let db = Database::connect_in_memory().await.unwrap();
        admit_with_memory_flag(&db, true, "记住用 Flash").await;
        let worker = MemoryConsolidator::new(
            db.clone(),
            BootId::generate(),
            temp.path().to_path_buf(),
            temp.path().join("memory"),
            Arc::new(UnusedRuntime),
            Arc::new(|_| {}),
        )
        .with_lanes(leveler_memory::MemoryLanes::EXPERIENCE_ONLY);

        let outcome = worker.run_once().await.unwrap();
        assert_eq!(outcome.claimed, 0);
        assert_eq!(worker.ready_count().await, 0);
        let pending = MemoryInboxRepository::new(&db)
            .counts()
            .await
            .unwrap()
            .pending;
        assert_eq!(
            pending, 1,
            "the row stays durable for when the lane reopens"
        );
    }

    #[tokio::test]
    async fn a_closed_experience_lane_claims_no_experience_work() {
        let temp = tempfile::tempdir().unwrap();
        let db = Database::connect_in_memory().await.unwrap();
        admitted_messages(&db, ["fix the build".to_string()]).await;
        let worker = MemoryConsolidator::new(
            db.clone(),
            BootId::generate(),
            temp.path().to_path_buf(),
            temp.path().join("memory"),
            Arc::new(UnusedRuntime),
            Arc::new(|_| {}),
        )
        .with_lanes(leveler_memory::MemoryLanes::USER_ONLY);

        assert!(!worker.run_experience_once().await.unwrap());
        assert_eq!(worker.experience_ready_count().await, 0);
        let pending = ExperienceInboxRepository::new(&db)
            .counts()
            .await
            .unwrap()
            .pending;
        assert_eq!(
            pending, 1,
            "the row stays durable for when the lane reopens"
        );
    }

    #[tokio::test]
    async fn a_secret_bearing_turn_never_reaches_the_extractor() {
        let temp = tempfile::tempdir().unwrap();
        let db = Database::connect_in_memory().await.unwrap();
        admitted_messages(
            &db,
            ["我的 api_key = sk-abcdefghijklmnopqrstuvwxyz012345".to_string()],
        )
        .await;
        let boot = BootId::generate();
        let items = MemoryInboxRepository::new(&db)
            .claim_batch(&boot, leveler_core::now(), 8)
            .await
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let extractor = CountingExtractor {
            calls: calls.clone(),
        };
        let worker = MemoryConsolidator::new(
            db,
            boot,
            temp.path().to_path_buf(),
            temp.path().join("memory"),
            Arc::new(UnusedRuntime),
            Arc::new(|_| {}),
        );

        let outcome = worker.process_claimed(&items, &extractor).await.unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 0, "secret turn was extracted");
        assert_eq!(outcome.accepted, 0);
        assert_eq!(outcome.rejected, 0);
        assert!(
            MemoryStore::open(temp.path().join("memory"))
                .unwrap()
                .list_pending()
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn model_output_is_applied_in_durable_inbox_order() {
        let temp = tempfile::tempdir().unwrap();
        let db = Database::connect_in_memory().await.unwrap();
        admitted_messages(
            &db,
            [
                "项目默认模型是 Pro".to_string(),
                "项目默认模型是 Flash".to_string(),
            ],
        )
        .await;
        let boot = BootId::generate();
        let items = MemoryInboxRepository::new(&db)
            .claim_batch(&boot, leveler_core::now(), 8)
            .await
            .unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();
        let worker = MemoryConsolidator::new(
            db,
            boot,
            temp.path().to_path_buf(),
            temp.path().join("memory"),
            Arc::new(UnusedRuntime),
            Arc::new(move |event| captured.lock().unwrap().push(event)),
        );

        let outcome = worker
            .process_claimed(&items, &ReversedExtractor)
            .await
            .unwrap();

        assert_eq!(outcome.accepted, 2);
        let store = MemoryStore::open(temp.path().join("memory")).unwrap();
        let active = store.effective_active().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].body, "项目默认模型是 Flash");
        assert!(store.list_pending().unwrap().is_empty());
        // The extractor answered newest-turn-first; the durable inbox order
        // must still win, so Pro is created before Flash supersedes it.
        let events = events.lock().unwrap();
        let changes: Vec<(&str, &str)> = events
            .iter()
            .map(|event| (event.operation.as_str(), event.title.as_str()))
            .collect();
        assert_eq!(
            changes,
            [
                ("created", "项目默认模型是 Pro"),
                ("superseded", "项目默认模型是 Flash")
            ]
        );
        assert!(!events.iter().any(|event| event.operation == "proposed"));
    }

    #[tokio::test]
    async fn an_update_never_guesses_which_keyless_direct_save_it_replaces() {
        let temp = tempfile::tempdir().unwrap();
        let db = Database::connect_in_memory().await.unwrap();
        admitted_messages(&db, ["以后不要后台启动服务了".to_string()]).await;
        let boot = BootId::generate();
        let items = MemoryInboxRepository::new(&db)
            .claim_batch(&boot, leveler_core::now(), 1)
            .await
            .unwrap();
        let store = MemoryStore::open(temp.path().join("memory")).unwrap();
        let old_id = store
            .activate(
                "以后后台启动服务",
                "以后后台启动服务",
                leveler_memory::MemoryKind::Preference,
                Vec::new(),
            )
            .unwrap()
            .id;
        let worker = MemoryConsolidator::new(
            db,
            boot,
            temp.path().to_path_buf(),
            temp.path().join("memory"),
            Arc::new(UnusedRuntime),
            Arc::new(|_| {}),
        );

        worker
            .process_claimed(&items, &BackgroundCorrectionExtractor)
            .await
            .unwrap();

        // The direct save has no structured identity. The model's update names
        // no active subject, so the runtime parks it instead of guessing that
        // the prose describes the same decision.
        assert_eq!(store.list_pending().unwrap().len(), 1);
        let active = store.effective_active().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, old_id);
        assert!(store.list_archived().unwrap().is_empty());
    }

    #[tokio::test]
    async fn flush_settles_ready_turns_before_a_headless_exit() {
        let temp = tempfile::tempdir().unwrap();
        let db = Database::connect_in_memory().await.unwrap();
        admitted_messages(&db, ["项目默认模型是 Flash".to_string()]).await;
        let runtime = ScriptedRuntime::new(vec![Ok("[]")]);
        let worker = MemoryConsolidator::new(
            db.clone(),
            BootId::generate(),
            temp.path().to_path_buf(),
            temp.path().join("memory"),
            runtime.clone(),
            Arc::new(|_| {}),
        )
        .with_lanes(leveler_memory::MemoryLanes::USER_ONLY);

        let outcome = worker.flush().await;

        assert_eq!(
            outcome.settled, 1,
            "a ready turn must be extracted before the process exits"
        );
        assert_eq!(outcome.remaining, 0);
        assert!(outcome.is_complete(), "{outcome:?}");
        assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);
        let counts = MemoryInboxRepository::new(&db).counts().await.unwrap();
        assert_eq!(counts.processed, 1);
        assert_eq!(counts.pending, 0);
    }

    #[tokio::test]
    async fn a_flushed_turn_is_not_consumed_twice() {
        let temp = tempfile::tempdir().unwrap();
        let db = Database::connect_in_memory().await.unwrap();
        admitted_messages(&db, ["项目默认模型是 Flash".to_string()]).await;
        let runtime = ScriptedRuntime::new(vec![Ok("[]")]);
        let worker = MemoryConsolidator::new(
            db.clone(),
            BootId::generate(),
            temp.path().to_path_buf(),
            temp.path().join("memory"),
            runtime.clone(),
            Arc::new(|_| {}),
        )
        .with_lanes(leveler_memory::MemoryLanes::USER_ONLY);
        assert_eq!(worker.flush().await.settled, 1);

        // The next process (a fresh boot) must find nothing left to extract.
        let next = MemoryConsolidator::new(
            db,
            BootId::generate(),
            temp.path().to_path_buf(),
            temp.path().join("memory"),
            runtime.clone(),
            Arc::new(|_| {}),
        )
        .with_lanes(leveler_memory::MemoryLanes::USER_ONLY);
        assert_eq!(next.run_once().await.unwrap().claimed, 0);
        assert_eq!(
            runtime.calls.load(Ordering::SeqCst),
            1,
            "one turn is one extraction, whoever drains it"
        );
    }

    #[tokio::test]
    async fn concurrent_flushes_settle_each_turn_once() {
        let temp = tempfile::tempdir().unwrap();
        let db = Database::connect_in_memory().await.unwrap();
        admitted_messages(&db, ["项目默认模型是 Flash".to_string()]).await;
        let runtime = ScriptedRuntime::new(vec![Ok("[]")]);
        let worker = MemoryConsolidator::new(
            db.clone(),
            BootId::generate(),
            temp.path().to_path_buf(),
            temp.path().join("memory"),
            runtime.clone(),
            Arc::new(|_| {}),
        )
        .with_lanes(leveler_memory::MemoryLanes::USER_ONLY);

        let (first, second) = tokio::join!(worker.flush(), worker.flush());

        assert_eq!(first.settled + second.settled, 1);
        assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            MemoryInboxRepository::new(&db)
                .counts()
                .await
                .unwrap()
                .processed,
            1
        );
    }

    #[tokio::test]
    async fn a_failed_flush_parks_its_rows_for_a_durable_retry() {
        let temp = tempfile::tempdir().unwrap();
        let db = Database::connect_in_memory().await.unwrap();
        admitted_messages(&db, ["项目默认模型是 Flash".to_string()]).await;
        let runtime = ScriptedRuntime::new(vec![Err(ModelError::new(
            ModelErrorKind::Timeout,
            "timed out",
        ))]);
        let worker = MemoryConsolidator::new(
            db.clone(),
            BootId::generate(),
            temp.path().to_path_buf(),
            temp.path().join("memory"),
            runtime,
            Arc::new(|_| {}),
        )
        .with_extraction_timeout(Duration::from_millis(100))
        .with_lanes(leveler_memory::MemoryLanes::USER_ONLY);

        let outcome = worker.flush().await;

        assert!(outcome.failed, "{outcome:?}");
        assert_eq!(outcome.remaining, 1);
        assert!(!outcome.timed_out);
        assert!(!outcome.is_complete());
        let counts = MemoryInboxRepository::new(&db).counts().await.unwrap();
        assert_eq!(counts.failed_retryable, 1, "the row stays durable");
        assert_eq!(counts.processing, 0, "the claim must not be left stuck");
    }

    #[tokio::test]
    async fn flush_never_claims_work_from_a_closed_lane() {
        let temp = tempfile::tempdir().unwrap();
        let db = Database::connect_in_memory().await.unwrap();
        admitted_messages(&db, ["项目默认模型是 Flash".to_string()]).await;
        let runtime = ScriptedRuntime::new(vec![Ok("[]")]);
        let worker = MemoryConsolidator::new(
            db.clone(),
            BootId::generate(),
            temp.path().to_path_buf(),
            temp.path().join("memory"),
            runtime.clone(),
            Arc::new(|_| {}),
        )
        .with_lanes(leveler_memory::MemoryLanes::USER_ONLY);

        let outcome = worker.flush().await;

        assert_eq!(outcome.settled, 1);
        assert!(outcome.is_complete(), "{outcome:?}");
        let experience = ExperienceInboxRepository::new(&db).counts().await.unwrap();
        assert_eq!(
            experience.pending, 1,
            "the closed experience lane keeps its durable row"
        );
        assert_eq!(experience.processed, 0);
    }

    #[tokio::test]
    async fn flush_is_bounded_when_another_pass_holds_the_worker() {
        let temp = tempfile::tempdir().unwrap();
        let db = Database::connect_in_memory().await.unwrap();
        admitted_messages(&db, ["项目默认模型是 Flash".to_string()]).await;
        let runtime = ScriptedRuntime::new(vec![Ok("[]")]);
        let worker = MemoryConsolidator::new(
            db.clone(),
            BootId::generate(),
            temp.path().to_path_buf(),
            temp.path().join("memory"),
            runtime.clone(),
            Arc::new(|_| {}),
        )
        .with_flush_budget(Duration::from_millis(50))
        .with_lanes(leveler_memory::MemoryLanes::USER_ONLY);

        let _pass = worker.work.lock().await;
        let outcome = worker.flush().await;

        assert!(outcome.timed_out, "{outcome:?}");
        assert_eq!(outcome.remaining, 1);
        assert_eq!(
            runtime.calls.load(Ordering::SeqCst),
            0,
            "the flush must never run a pass beside the worker"
        );
        assert_eq!(
            MemoryInboxRepository::new(&db)
                .counts()
                .await
                .unwrap()
                .pending,
            1,
            "the durable row is still there for the next process"
        );
    }

    #[test]
    fn retry_backoff_is_bounded() {
        assert_eq!(retry_delay(1), chrono::Duration::seconds(1));
        assert_eq!(retry_delay(100), chrono::Duration::seconds(300));
    }

    #[test]
    fn sparse_flush_deadline_is_anchored_to_oldest_turn_not_worker_phase() {
        let now = leveler_core::now();
        let fresh = MemoryInboxReadyState {
            count: 1,
            oldest_created_at: Some(now),
        };
        let old = MemoryInboxReadyState {
            count: 1,
            oldest_created_at: Some(now - chrono::Duration::seconds(59)),
        };
        assert_eq!(wake_delay(&fresh, now), Duration::from_secs(60));
        assert_eq!(wake_delay(&old, now), Duration::from_secs(1));
        assert_eq!(
            wake_delay(
                &MemoryInboxReadyState {
                    count: 4,
                    oldest_created_at: Some(now),
                },
                now,
            ),
            Duration::ZERO
        );
    }

    #[tokio::test]
    async fn five_short_turns_are_consolidated_in_one_model_call() {
        let temp = tempfile::tempdir().unwrap();
        let db = Database::connect_in_memory().await.unwrap();
        admitted_turns(&db, 5).await;
        let runtime = ScriptedRuntime::new(vec![Ok("[]")]);
        let worker = MemoryConsolidator::new(
            db,
            BootId::generate(),
            temp.path().to_path_buf(),
            temp.path().join("memory"),
            runtime.clone(),
            Arc::new(|_| {}),
        );

        assert_eq!(worker.run_once().await.unwrap().claimed, 5);
        assert_eq!(worker.run_once().await.unwrap().claimed, 0);
        assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn textless_turn_is_processed_without_poisoning_other_turns() {
        let temp = tempfile::tempdir().unwrap();
        let db = Database::connect_in_memory().await.unwrap();
        admitted_messages(&db, [String::new(), "a normal text turn".to_string()]).await;
        let runtime = ScriptedRuntime::new(vec![Ok("[]")]);
        let worker = MemoryConsolidator::new(
            db.clone(),
            BootId::generate(),
            temp.path().to_path_buf(),
            temp.path().join("memory"),
            runtime.clone(),
            Arc::new(|_| {}),
        );

        assert_eq!(worker.run_once().await.unwrap().claimed, 2);
        assert_eq!(
            MemoryInboxRepository::new(&db)
                .counts()
                .await
                .unwrap()
                .processed,
            2
        );
        assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn one_hundred_short_turns_use_thirteen_bounded_batches() {
        let temp = tempfile::tempdir().unwrap();
        let db = Database::connect_in_memory().await.unwrap();
        let admitted_at = admitted_turns(&db, 100).await;
        let now = leveler_core::now();
        let mut pending_ages_ms = admitted_at
            .into_iter()
            .map(|created| (now - created).num_milliseconds().max(0))
            .collect::<Vec<_>>();
        pending_ages_ms.sort_unstable();
        let runtime = ScriptedRuntime::new(Vec::new());
        let worker = MemoryConsolidator::new(
            db,
            BootId::generate(),
            temp.path().to_path_buf(),
            temp.path().join("memory"),
            runtime.clone(),
            Arc::new(|_| {}),
        );

        let mut batch_sizes = Vec::new();
        loop {
            let outcome = worker.run_once().await.unwrap();
            if outcome.claimed == 0 {
                break;
            }
            batch_sizes.push(outcome.claimed);
        }

        assert_eq!(runtime.calls.load(Ordering::SeqCst), 13);
        assert_eq!(
            batch_sizes,
            [8; 12].into_iter().chain([4]).collect::<Vec<_>>()
        );
        let mut batch_distribution = batch_sizes.clone();
        batch_distribution.sort_unstable();
        println!(
            "turns=100 extraction_requests=13 average_batch_size={:.2} p50_batch_size={} p95_batch_size={} pending_age_p50_ms={} pending_age_p95_ms={}",
            100.0 / batch_sizes.len() as f64,
            batch_distribution[batch_distribution.len() / 2],
            batch_distribution
                [(batch_distribution.len() * 95 / 100).min(batch_distribution.len() - 1)],
            pending_ages_ms[pending_ages_ms.len() / 2],
            pending_ages_ms[(pending_ages_ms.len() * 95 / 100).min(pending_ages_ms.len() - 1)],
        );
    }

    #[tokio::test]
    async fn separate_project_databases_never_share_claims() {
        let temp_a = tempfile::tempdir().unwrap();
        let temp_b = tempfile::tempdir().unwrap();
        let db_a = Database::connect(&temp_a.path().join("sessions.db"))
            .await
            .unwrap();
        let db_b = Database::connect(&temp_b.path().join("sessions.db"))
            .await
            .unwrap();
        admitted_turns(&db_a, 2).await;
        admitted_turns(&db_b, 3).await;
        let runtime = ScriptedRuntime::new(vec![Ok("[]")]);
        let worker_a = MemoryConsolidator::new(
            db_a,
            BootId::generate(),
            temp_a.path().to_path_buf(),
            temp_a.path().join("memory"),
            runtime,
            Arc::new(|_| {}),
        );

        assert_eq!(worker_a.run_once().await.unwrap().claimed, 2);
        assert_eq!(
            MemoryInboxRepository::new(&db_b)
                .ready_state(leveler_core::now())
                .await
                .unwrap()
                .count,
            3
        );
    }

    /// There is no deterministic parser left. Every durable statement goes to
    /// the model extractor, which owns meaning; the runtime only validates the
    /// structured result.
    #[tokio::test]
    async fn every_durable_statement_goes_through_the_model_extractor() {
        let temp = tempfile::tempdir().unwrap();
        let db = Database::connect_in_memory().await.unwrap();
        let statement = "以后本项目默认模型固定为 Pro";
        let turn_ids = admitted_messages(&db, [statement.to_string()]).await;
        let store = MemoryStore::open(temp.path().join("memory")).unwrap();
        let raw = serde_json::json!({"candidates": [{
            "source_turn_id": turn_ids[0].0.clone(),
            "candidate": {
                "fact": "本项目默认模型固定为 Pro",
                "subject": "project.default_model",
                "value": "Pro",
                "scope": "project",
                "durability": "durable",
                "memory_type": "project",
                "authority": "explicit_user",
                "statement": "assertion",
                "operation_hint": "reaffirm",
                "evidence_span": "本项目默认模型固定为 Pro",
                "confidence": 1.0
            }
        }]})
        .to_string();
        let runtime = ScriptedRuntime::new(vec![Ok(raw.as_str())]);
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();
        let worker = MemoryConsolidator::new(
            db,
            BootId::generate(),
            temp.path().to_path_buf(),
            temp.path().join("memory"),
            runtime.clone(),
            Arc::new(move |event| captured.lock().unwrap().push(event)),
        );

        worker.run_once().await.unwrap();

        let active = store.effective_active().unwrap();
        assert_eq!(active.len(), 1);
        assert!(active[0].body.contains("Pro"));
        assert!(store.list_pending().unwrap().is_empty());
        assert_eq!(
            runtime.calls.load(Ordering::SeqCst),
            1,
            "the extractor is the only path from prose to memory"
        );
        assert!(
            events
                .lock()
                .unwrap()
                .iter()
                .any(|event| event.operation == "created"),
            "an admitted fact must be reported as a durable change"
        );
    }

    #[tokio::test]
    async fn provider_failure_is_retryable_and_later_settles() {
        let temp = tempfile::tempdir().unwrap();
        let db = Database::connect_in_memory().await.unwrap();
        admitted_turns(&db, 1).await;
        let runtime = ScriptedRuntime::new(vec![
            Err(ModelError::new(ModelErrorKind::Timeout, "timed out")),
            Ok("[]"),
        ]);
        let worker = MemoryConsolidator::new(
            db,
            BootId::generate(),
            temp.path().to_path_buf(),
            temp.path().join("memory"),
            runtime.clone(),
            Arc::new(|_| {}),
        );

        assert!(worker.run_once().await.is_err());
        tokio::time::sleep(Duration::from_millis(1_050)).await;
        assert_eq!(worker.run_once().await.unwrap().claimed, 1);
        assert_eq!(worker.run_once().await.unwrap().claimed, 0);
        assert_eq!(runtime.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_memory_extraction_call_lands_in_the_model_call_ledger() {
        let temp = tempfile::tempdir().unwrap();
        let db = Database::connect_in_memory().await.unwrap();
        admitted_turns(&db, 1).await;
        let runtime = ScriptedRuntime::new(vec![Ok("[]")]);
        let worker = MemoryConsolidator::new(
            db.clone(),
            BootId::generate(),
            temp.path().to_path_buf(),
            temp.path().join("memory"),
            runtime,
            Arc::new(|_| {}),
        );

        worker.run_once().await.unwrap();

        // The extraction is visible as its OWN lane, with a low effort, so
        // "why was there another model call after my turn?" has an answer and
        // auxiliary spend is separable from the coding work.
        // Query every session in this in-memory database: `admitted_turns`
        // names its session from a fresh random id, so the assertion must not
        // depend on that id.
        let mut rows = Vec::new();
        for session in SessionRepository::new(&db).list().await.unwrap() {
            rows.extend(
                leveler_storage::ModelRequestRepository::new(&db)
                    .load_for_session(&leveler_core::SessionId::new(session.id))
                    .await
                    .unwrap(),
            );
        }
        let memory_rows: Vec<_> = rows
            .iter()
            .filter(|row| row.kind == leveler_storage::ModelCallKind::MemoryExtraction)
            .collect();
        assert_eq!(memory_rows.len(), 1, "one batch, one ledger row");
        assert_eq!(memory_rows[0].reasoning_effort.as_deref(), Some("low"));
        assert!(memory_rows[0].error_kind.is_none());
    }

    #[tokio::test]
    async fn extractor_deadline_keeps_the_claim_retryable() {
        let temp = tempfile::tempdir().unwrap();
        let db = Database::connect_in_memory().await.unwrap();
        admitted_turns(&db, 1).await;
        let worker = MemoryConsolidator::new(
            db.clone(),
            BootId::generate(),
            temp.path().to_path_buf(),
            temp.path().join("memory"),
            Arc::new(HangingRuntime),
            Arc::new(|_| {}),
        )
        .with_extraction_timeout(Duration::from_millis(1));

        let error = worker.run_once().await.unwrap_err();
        let counts = MemoryInboxRepository::new(&db).counts().await.unwrap();

        assert!(error.contains("timed out"));
        assert_eq!(counts.failed_retryable, 1);
    }

    #[tokio::test]
    async fn startup_recovers_claims_owned_by_a_dead_boot() {
        let temp = tempfile::tempdir().unwrap();
        let db = Database::connect_in_memory().await.unwrap();
        admitted_turns(&db, 1).await;
        let dead = BootId::generate();
        assert_eq!(
            MemoryInboxRepository::new(&db)
                .claim_batch(&dead, leveler_core::now(), 1)
                .await
                .unwrap()
                .len(),
            1
        );
        let runtime = ScriptedRuntime::new(vec![Ok("[]")]);
        let worker = MemoryConsolidator::new(
            db,
            BootId::generate(),
            temp.path().to_path_buf(),
            temp.path().join("memory"),
            runtime,
            Arc::new(|_| {}),
        );

        worker.recover_dead_claims().await.unwrap();
        assert_eq!(worker.run_once().await.unwrap().claimed, 1);
    }

    #[tokio::test]
    async fn spawned_worker_drains_pending_work_after_database_reopen() {
        let temp = tempfile::tempdir().unwrap();
        let database_path = temp.path().join("sessions.db");
        let first_process = Database::connect(&database_path).await.unwrap();
        admitted_turns(&first_process, 1).await;
        drop(first_process);

        let reopened = Database::connect(&database_path).await.unwrap();
        let runtime = ScriptedRuntime::new(vec![Ok("[]")]);
        let worker = MemoryConsolidator::new(
            reopened.clone(),
            BootId::generate(),
            temp.path().to_path_buf(),
            temp.path().join("memory"),
            runtime.clone(),
            Arc::new(|_| {}),
        );
        let task = worker.spawn();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let counts = MemoryInboxRepository::new(&reopened)
                    .counts()
                    .await
                    .unwrap();
                if counts.processed == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("startup worker drains the reopened inbox");
        worker.cancel();
        task.await.unwrap();

        assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn next_pass_recovers_an_unsettled_claim_owned_by_the_same_boot() {
        let temp = tempfile::tempdir().unwrap();
        let db = Database::connect_in_memory().await.unwrap();
        admitted_turns(&db, 1).await;
        let boot = BootId::generate();
        assert_eq!(
            MemoryInboxRepository::new(&db)
                .claim_batch(&boot, leveler_core::now(), 1)
                .await
                .unwrap()
                .len(),
            1
        );
        let runtime = ScriptedRuntime::new(vec![Ok("[]")]);
        let worker = MemoryConsolidator::new(
            db,
            boot,
            temp.path().to_path_buf(),
            temp.path().join("memory"),
            runtime,
            Arc::new(|_| {}),
        );

        assert_eq!(worker.run_once().await.unwrap().claimed, 1);
    }

    #[tokio::test]
    async fn crash_after_admission_replays_without_duplicate_memory() {
        let temp = tempfile::tempdir().unwrap();
        let db = Database::connect_in_memory().await.unwrap();
        admitted_turns(&db, 1).await;
        let dead = BootId::generate();
        let claimed = MemoryInboxRepository::new(&db)
            .claim_batch(&dead, leveler_core::now(), 1)
            .await
            .unwrap();
        let first = MemoryConsolidator::new(
            db.clone(),
            dead,
            temp.path().to_path_buf(),
            temp.path().join("memory"),
            Arc::new(UnusedRuntime),
            Arc::new(|_| {}),
        );
        first
            .process_claimed(&claimed, &OneCandidateExtractor)
            .await
            .unwrap();
        // Simulate process death before mark_processed. Even if a second model
        // call would drift, the staged validated result must be replayed and
        // the provider must not be called again.
        let drifted = serde_json::json!([{
            "source_turn_id": claimed[0].turn_id,
            "candidate": {
                "fact": "a different fact",
                "subject": "project.default_model",
                "value": "Different",
                "scope": "project",
                "durability": "durable",
                "memory_type": "project",
                "authority": "explicit_user",
                "operation_hint": "create",
                "evidence_span": "turn 0",
                "confidence": 1.0
            }
        }])
        .to_string();
        let runtime = ScriptedRuntime::new(vec![Ok(drifted.as_str())]);
        let second = MemoryConsolidator::new(
            db,
            BootId::generate(),
            temp.path().to_path_buf(),
            temp.path().join("memory"),
            runtime.clone(),
            Arc::new(|_| {}),
        );
        second.recover_dead_claims().await.unwrap();
        second.run_once().await.unwrap();
        assert_eq!(runtime.calls.load(Ordering::SeqCst), 0);

        let store = MemoryStore::open(temp.path().join("memory")).unwrap();
        let active = store.effective_active().unwrap();
        assert_eq!(active.len(), 1, "the replay must not duplicate the fact");
        assert_eq!(active[0].body, "turn 0");
        assert!(store.list_pending().unwrap().is_empty());
    }

    #[tokio::test]
    async fn staged_apply_cursor_resumes_same_key_updates_as_latest_current_value() {
        let temp = tempfile::tempdir().unwrap();
        let db = Database::connect_in_memory().await.unwrap();
        admitted_turns(&db, 1).await;
        let old_boot = BootId::generate();
        let claimed = MemoryInboxRepository::new(&db)
            .claim_batch(&old_boot, leveler_core::now(), 1)
            .await
            .unwrap();
        let key = leveler_memory::semantic_key_of("default_model");
        let mut pro = leveler_memory::MemoryCandidate::new(
            "默认模型：Pro",
            "默认模型：Pro",
            leveler_memory::CandidateKind::Free,
            Some(key.clone()),
            leveler_memory::CandidateSource::UserExplicit,
            vec![],
        )
        .unwrap();
        pro.semantic_key = Some(key.clone());
        pro.authority = leveler_memory::MemoryAuthority::ExplicitUser;
        let mut flash = leveler_memory::MemoryCandidate::new(
            "默认模型：Flash",
            "默认模型：Flash",
            leveler_memory::CandidateKind::Free,
            Some(key.clone()),
            leveler_memory::CandidateSource::UserExplicit,
            vec![],
        )
        .unwrap();
        flash.semantic_key = Some(key);
        flash.authority = leveler_memory::MemoryAuthority::ExplicitUser;
        let store = MemoryStore::open(temp.path().join("memory")).unwrap();
        // The cursor says candidate 0 was already applied before the crash, so
        // the fixture commits it. Proposing instead would be a sequence the
        // worker can never produce: a committed fact never leaves its own
        // proposal behind.
        store.propose(pro.clone()).unwrap();
        assert!(matches!(
            store.admit_candidate(pro.clone()).unwrap(),
            AdmitOutcome::Applied(_)
        ));
        let staged = StagedTurnResult {
            accepted: vec![pro, flash],
            rejected: 0,
            applied: 1,
            skipped: 0,
        };
        MemoryInboxRepository::new(&db)
            .stage_results(
                &old_boot,
                &[(claimed[0].inbox_id, serde_json::to_string(&staged).unwrap())],
                leveler_core::now(),
            )
            .await
            .unwrap();
        MemoryInboxRepository::new(&db)
            .recover_claims_for_boot(
                &old_boot,
                "crash after first candidate",
                leveler_core::now(),
            )
            .await
            .unwrap();
        let runtime = ScriptedRuntime::new(vec![Ok("this must not be called")]);
        let worker = MemoryConsolidator::new(
            db,
            BootId::generate(),
            temp.path().to_path_buf(),
            temp.path().join("memory"),
            runtime.clone(),
            Arc::new(|_| {}),
        );

        worker.run_once().await.unwrap();

        assert_eq!(runtime.calls.load(Ordering::SeqCst), 0);
        let active = store.effective_active().unwrap();
        assert_eq!(active.len(), 1);
        assert!(active[0].body.contains("Flash"));
        // The superseded proposal for the same identity is stale.
        let pending = store.list_pending().unwrap();
        assert!(
            pending.is_empty(),
            "stale proposals: {:?}",
            pending.iter().map(|c| &c.id).collect::<Vec<_>>()
        );
        // The value the resumed cursor replaced is history, not current truth.
        let archived = store.list_archived().unwrap();
        assert_eq!(archived.len(), 1);
        assert!(archived[0].body.contains("Pro"));
    }

    /// A staged experience result is a model answer already paid for. A crash
    /// that returns the claim to the queue must replay that answer, not ask
    /// the model again. `UnusedRuntime` panics on any provider call.
    #[tokio::test]
    async fn a_staged_experience_result_is_replayed_without_a_second_model_call() {
        use leveler_core::{SessionId, TurnId};

        let temp = tempfile::tempdir().unwrap();
        let db = Database::connect_in_memory().await.unwrap();
        let record = SessionRecord::new("/repo", "goal", "mock/model", leveler_core::now());
        let session = SessionId::new(record.id.clone());
        SessionRepository::new(&db).create(&record).await.unwrap();
        let payload = serde_json::json!({
            "version": 1,
            "initiating_message": {
                "role": "user",
                "content": [{"type": "text", "text": "fix the check"}]
            }
        });
        let turn = TurnRepository::new(&db)
            .start(
                &session,
                "user",
                Some(&payload.to_string()),
                leveler_core::now(),
            )
            .await
            .unwrap();
        let turn_id = TurnId::new(turn.id.clone());
        TurnRepository::new(&db)
            .finish(&turn_id, "completed", leveler_core::now())
            .await
            .unwrap();
        let events = EventRepository::new(&db);
        events
            .append(
                &session,
                Some(&turn_id),
                "turn_finished",
                &serde_json::json!({
                    "type": "turn_finished",
                    "payload": {
                        "turn_id": turn.id,
                        "outcome": "completed",
                        "stop_reason": "Completed",
                        "rounds": 1,
                        "modified_files": []
                    }
                })
                .to_string(),
                leveler_core::now(),
            )
            .await
            .unwrap();
        events
            .append(
                &session,
                None,
                "task_finished",
                &serde_json::json!({
                    "type": "task_finished",
                    "payload": {"outcome": "completed", "reason": null, "warnings": []}
                })
                .to_string(),
                leveler_core::now(),
            )
            .await
            .unwrap();

        let old_boot = BootId::generate();
        let repo = ExperienceInboxRepository::new(&db);
        let claimed = repo
            .claim_batch(&old_boot, leveler_core::now(), 1)
            .await
            .unwrap();
        assert_eq!(claimed.len(), 1);
        let staged = r#"{"outcome":"skipped","reason":"crash_replay","accepted":[],"rejected":0,"applied":0,"skipped":0,"created":0,"updated":0,"pending":0}"#;
        repo.stage_result(&old_boot, claimed[0].inbox_id, staged, leveler_core::now())
            .await
            .unwrap();
        repo.recover_claims_for_boot(&old_boot, "dead boot", leveler_core::now())
            .await
            .unwrap();

        let worker = MemoryConsolidator::new(
            db.clone(),
            BootId::generate(),
            temp.path().to_path_buf(),
            temp.path().join("memory"),
            Arc::new(UnusedRuntime),
            Arc::new(|_| {}),
        );
        assert!(worker.run_experience_once().await.unwrap());
        let processed = repo
            .last_processed()
            .await
            .unwrap()
            .expect("the recovered claim settled");
        assert!(
            processed
                .result_json
                .as_deref()
                .is_some_and(|json| json.contains("crash_replay")),
            "the staged decision must be replayed: {processed:?}"
        );
    }

    /// Only an ACTIVE lesson proves its evidence is already learned. A
    /// different evidence identity is new, and an archived lesson no longer
    /// suppresses a later re-learning.
    #[test]
    fn duplicate_evidence_is_recognised_only_while_its_lesson_is_active() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let tag = "evidence:0123456789abcdef";
        let mut candidate = MemoryCandidate::new(
            "engine profile lesson",
            "fix the profile in engine.config",
            leveler_memory::CandidateKind::Free,
            Some("experience.engine.profile".to_string()),
            leveler_memory::CandidateSource::RuntimeObserved,
            vec!["experience".to_string(), tag.to_string()],
        )
        .unwrap();
        candidate.authority = MemoryAuthority::ToolObservation;
        candidate.experience = Some(leveler_memory::ExperienceRecord {
            category: leveler_memory::ExperienceCategory::Fix,
            verification: leveler_memory::ExperienceVerification::Verified,
            applies_when: "the verifier rejects the profile".to_string(),
            invalid_if: None,
            evidence: vec![
                leveler_memory::ExperienceEvidence {
                    kind: "failure".to_string(),
                    reference: "step 2: ./verify.sh (exit 1)".to_string(),
                },
                leveler_memory::ExperienceEvidence {
                    kind: "resolution".to_string(),
                    reference: "step 4: ./verify.sh (exit 0)".to_string(),
                },
            ],
        });
        let outcome = store.admit_experience_candidate(candidate).unwrap();
        assert!(
            matches!(outcome, AgentAdmitOutcome::Applied(_)),
            "fixture lesson was not activated: {outcome:?}"
        );

        let service = MemoryService::legacy(dir.path());
        assert!(experience_evidence_learned(&service, tag).unwrap());
        assert!(!experience_evidence_learned(&service, "evidence:ffffffffffffffff").unwrap());
    }

    /// Seed one active identity with a distinct structured subject key.
    ///
    /// Returns the active entry id: a keyed candidate is stored under the
    /// slug of its key, not under the pending candidate id.
    fn seed_active_identity(store: &MemoryStore, key: &str) -> String {
        let candidate = MemoryCandidate::new(
            format!("{key} is Pro"),
            format!("{key} must stay Pro"),
            leveler_memory::CandidateKind::Free,
            Some(key.to_string()),
            leveler_memory::CandidateSource::SystemInferred,
            Vec::new(),
        )
        .unwrap();
        let pending_id = candidate.id.clone();
        store.propose(candidate).unwrap();
        store.accept(&pending_id).unwrap().id
    }

    #[tokio::test]
    async fn a_replacement_outside_the_identity_window_is_verified_against_the_store() {
        let temp = tempfile::tempdir().unwrap();
        let memory_dir = temp.path().join("memory");
        let store = MemoryStore::open(&memory_dir).unwrap();
        let seeded = (0..30)
            .map(|index| seed_active_identity(&store, &format!("project.setting.{index}")))
            .collect::<Vec<_>>();
        // The extractor context is bounded at 24, so at least one real identity
        // in this store is deliberately not offered to the model.
        let offered = MemoryService::legacy(memory_dir.as_path())
            .identity_context(
                ServiceScope::Project,
                leveler_memory::v2::IDENTITY_CONTEXT_LIMIT,
            )
            .unwrap()
            .into_iter()
            .map(|identity| identity.id)
            .collect::<BTreeSet<_>>();
        let target = seeded
            .iter()
            .find(|id| !offered.contains(*id))
            .expect("30 identities exceed the 24-entry context")
            .clone();
        let probe = MemoryService::legacy(memory_dir.as_path()).read(
            &leveler_memory::v2::Actor::External,
            ServiceScope::Project,
            &target,
        );
        assert!(
            probe.is_ok(),
            "the chosen target {target} must be real; offered={} seeded={} error={probe:?}",
            offered.len(),
            seeded.len(),
        );

        let db = Database::connect_in_memory().await.unwrap();
        admitted_messages(&db, ["项目默认模型改成 Flash".to_string()]).await;
        let boot = BootId::generate();
        let claimed = MemoryInboxRepository::new(&db)
            .claim_batch(&boot, leveler_core::now(), 1)
            .await
            .unwrap();
        let worker = MemoryConsolidator::new(
            db,
            boot,
            temp.path().to_path_buf(),
            memory_dir,
            Arc::new(UnusedRuntime),
            Arc::new(|_| {}),
        );

        let outcome = worker
            .process_claimed(
                &claimed,
                &ReplacementExtractor {
                    target: target.clone(),
                },
            )
            .await
            .unwrap();

        assert_eq!(
            outcome.accepted, 1,
            "a real identity outside the window must still be replaceable"
        );
        assert_eq!(outcome.rejected, 0);
        assert!(
            store.read_active(&target).is_err(),
            "the named current identity must be superseded"
        );
    }

    #[tokio::test]
    async fn a_guessed_replacement_target_is_rejected_before_the_lifecycle() {
        let temp = tempfile::tempdir().unwrap();
        let memory_dir = temp.path().join("memory");
        let store = MemoryStore::open(&memory_dir).unwrap();
        seed_active_identity(&store, "project.setting.0");

        let db = Database::connect_in_memory().await.unwrap();
        admitted_messages(&db, ["项目默认模型改成 Flash".to_string()]).await;
        let boot = BootId::generate();
        let claimed = MemoryInboxRepository::new(&db)
            .claim_batch(&boot, leveler_core::now(), 1)
            .await
            .unwrap();
        let worker = MemoryConsolidator::new(
            db,
            boot,
            temp.path().to_path_buf(),
            memory_dir,
            Arc::new(UnusedRuntime),
            Arc::new(|_| {}),
        );

        let outcome = worker
            .process_claimed(
                &claimed,
                &ReplacementExtractor {
                    target: "mem-not-a-real-id".to_string(),
                },
            )
            .await
            .unwrap();

        assert_eq!(outcome.accepted, 0);
        assert_eq!(outcome.rejected, 1);
        assert_eq!(
            store.list_active().unwrap().len(),
            1,
            "a guessed target must not change the store"
        );
    }
}
