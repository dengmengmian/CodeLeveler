//! Runtime-owned asynchronous consolidation of durable memory inbox turns.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use leveler_agent::{
    BatchSourceTurn, DEFAULT_EXTRACTION_TIMEOUT, MAX_BATCH_INPUT_CHARS, MAX_BATCH_TURNS,
    MAX_EXTRACTOR_INPUT_CHARS, ModelSemanticExtractor, SemanticExtractor,
    validate_batch_candidates,
};
use leveler_core::{BootId, BootLiveness};
use leveler_memory::{
    AdmitOutcome, AppliedOperation, MemoryCandidate, MemoryStore, ProposeOutcome,
};
use leveler_model::{ModelRef, ModelRuntime};
use leveler_storage::{Database, MemoryInboxItem, MemoryInboxReadyState, MemoryInboxRepository};
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
const PROCESSED_RETENTION_DAYS: i64 = 7;
const PRUNE_LIMIT: u32 = 128;

static GLOBAL_MEMORY_CALL: OnceLock<Semaphore> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryConsolidationEvent {
    pub session_id: String,
    pub operation: String,
    pub id: String,
    pub title: String,
    pub authority: String,
}

pub type MemoryEventSink = Arc<dyn Fn(MemoryConsolidationEvent) + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConsolidationOutcome {
    pub claimed: usize,
    pub accepted: usize,
    pub rejected: usize,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct StagedTurnResult {
    accepted: Vec<MemoryCandidate>,
    rejected: usize,
    #[serde(default)]
    applied: usize,
}

/// The application/runtime-owned worker for one project database.
pub struct MemoryConsolidator {
    db: Database,
    boot: BootId,
    state_dir: PathBuf,
    memory_dir: PathBuf,
    runtime: Arc<dyn ModelRuntime>,
    extraction_timeout: Duration,
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
            memory_dir,
            runtime,
            extraction_timeout: DEFAULT_EXTRACTION_TIMEOUT,
            notify: Notify::new(),
            cancel: CancellationToken::new(),
            event_sink,
        })
    }

    #[cfg(test)]
    fn with_extraction_timeout(mut self: Arc<Self>, timeout: Duration) -> Arc<Self> {
        Arc::get_mut(&mut self)
            .expect("a fresh consolidator has no other owners")
            .extraction_timeout = timeout;
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
        // Startup is itself a flush signal: pending work from an earlier clean
        // shutdown must not require another user turn to become visible.
        let mut flush_now = true;
        loop {
            if flush_now {
                match self.run_once().await {
                    Ok(outcome) => {
                        tracing::debug!(
                            claimed = outcome.claimed,
                            accepted = outcome.accepted,
                            rejected = outcome.rejected,
                            "memory consolidation batch settled"
                        );
                        flush_now = outcome.claimed == MAX_WORKER_BATCH_TURNS as usize
                            || self.ready_count().await >= IMMEDIATE_BATCH_THRESHOLD as u64;
                        if flush_now {
                            continue;
                        }
                    }
                    Err(error) => tracing::warn!(%error, "memory consolidation batch failed"),
                }
            }

            let delay = self.next_wake_delay().await;
            if delay.is_zero() {
                flush_now = true;
                continue;
            }
            tokio::select! {
                _ = self.cancel.cancelled() => return,
                _ = self.notify.notified() => {
                    flush_now = self.ready_count().await >= IMMEDIATE_BATCH_THRESHOLD as u64;
                }
                _ = tokio::time::sleep(delay) => flush_now = true,
            }
        }
    }

    async fn next_wake_delay(&self) -> Duration {
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

    async fn run_once(&self) -> Result<ConsolidationOutcome, String> {
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
        let mut staged = items
            .iter()
            .map(|_| StagedTurnResult {
                accepted: Vec::new(),
                rejected: 0,
                applied: 0,
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
            // Deterministic durable wording avoids a model call, but is still
            // only a candidate. Only an explicit memory command may write
            // active memory without a separate accept decision.
            if let Some(candidate) = leveler_memory::parse_durable_fact(&user_text) {
                staged[index].accepted.push(candidate);
                continue;
            }
            if user_text.trim().is_empty() {
                continue;
            }
            sources.push(BatchSourceTurn {
                source_turn_id: item.turn_id.clone(),
                user_text,
            });
        }

        let mut candidates = Vec::new();
        if !sources.is_empty() {
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
                while batch_end < sources.len() && batch_end - batch_start < MAX_BATCH_TURNS {
                    let next_chars = sources[batch_end].user_text.chars().count();
                    if batch_end > batch_start && input_chars + next_chars > MAX_BATCH_INPUT_CHARS {
                        break;
                    }
                    input_chars += next_chars;
                    batch_end += 1;
                }
                candidates.extend(
                    extractor
                        .extract_batch(&sources[batch_start..batch_end], &self.cancel)
                        .await
                        .map_err(|e| e.to_string())?,
                );
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
                (Some(index), Ok(memory)) => staged[index].accepted.push(memory),
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

    async fn apply_staged(
        &self,
        items: &[MemoryInboxItem],
        mut staged: Vec<StagedTurnResult>,
    ) -> Result<ConsolidationOutcome, String> {
        let rejected = staged.iter().map(|result| result.rejected).sum();
        let accepted = staged.iter().map(|result| result.accepted.len()).sum();
        let store = MemoryStore::open(&self.memory_dir).map_err(|e| e.to_string())?;
        let repo = MemoryInboxRepository::new(&self.db);
        for (item, result) in items.iter().zip(&mut staged) {
            if result.applied > result.accepted.len() {
                return Err(format!(
                    "turn {} staged apply cursor is out of bounds",
                    item.turn_id
                ));
            }
            while result.applied < result.accepted.len() {
                let candidate = result.accepted[result.applied].clone();
                let authority = candidate.authority.as_str().to_string();
                let outcome = store
                    .admit_candidate(candidate)
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
                    AdmitOutcome::Proposed(ProposeOutcome::Pending(entry)) => {
                        Some(("proposed", entry.id, entry.title))
                    }
                    AdmitOutcome::Corrected(outcome) => outcome.entry.and_then(|entry| {
                        let operation = match outcome.operation {
                            AppliedOperation::Created => "created",
                            AppliedOperation::Superseded => "superseded",
                            AppliedOperation::Merged => "merged",
                            AppliedOperation::Skipped => return None,
                        };
                        Some((operation, entry.id, entry.title))
                    }),
                    AdmitOutcome::Proposed(_) => None,
                };
                if let Some((operation, id, title)) = event {
                    (self.event_sink)(MemoryConsolidationEvent {
                        session_id: item.session_id.clone(),
                        operation: operation.to_string(),
                        id,
                        title,
                        authority: authority.clone(),
                    });
                }
            }
        }
        Ok(ConsolidationOutcome {
            claimed: items.len(),
            accepted,
            rejected,
        })
    }
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
        CandidateDurability, CandidateScope, MemoryAuthority, OperationHint, SemanticCandidate,
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
            panic!("test supplies its extractor directly")
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
            unreachable!()
        }
    }

    struct ReversedExtractor;

    struct OneCandidateExtractor;

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
            unreachable!()
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
                    value: Some("disabled".to_string()),
                    scope: CandidateScope::Project,
                    durability: CandidateDurability::Durable,
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
                value: Some(value.to_string()),
                scope: CandidateScope::Project,
                durability: CandidateDurability::Durable,
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

    #[tokio::test]
    async fn model_output_is_proposed_in_durable_inbox_order() {
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
        let worker = MemoryConsolidator::new(
            db,
            boot,
            temp.path().to_path_buf(),
            temp.path().join("memory"),
            Arc::new(UnusedRuntime),
            Arc::new(|_| {}),
        );

        let outcome = worker
            .process_claimed(&items, &ReversedExtractor)
            .await
            .unwrap();

        assert_eq!(outcome.accepted, 2);
        let store = MemoryStore::open(temp.path().join("memory")).unwrap();
        assert!(store.list_active().unwrap().is_empty());
        let pending = store.list_pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].body, "项目默认模型是 Flash");
    }

    #[tokio::test]
    async fn semantic_correction_supersedes_a_keyless_direct_save() {
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

        worker
            .process_claimed(&items, &BackgroundCorrectionExtractor)
            .await
            .unwrap();

        assert!(store.list_pending().unwrap().is_empty());
        let active = store.effective_active().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].body, "以后不要后台启动服务");
        let history = store.list_archived().unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].id, old_id);
        assert!(
            store
                .search("后台启动服务", 10)
                .unwrap()
                .into_iter()
                .all(|(entry, _)| entry.id != old_id)
        );
        assert!(events.lock().unwrap().iter().any(|event| {
            event.operation == "superseded" && event.title.contains("不要后台启动")
        }));
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

    #[tokio::test]
    async fn deterministic_fact_is_proposed_without_a_model_call() {
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
                "authority": "explicit_user",
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

        assert!(store.effective_active().unwrap().is_empty());
        let pending = store.list_pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].body.contains("Pro"));
        assert_eq!(runtime.calls.load(Ordering::SeqCst), 0);
        assert!(
            events
                .lock()
                .unwrap()
                .iter()
                .any(|event| event.operation == "proposed"),
            "the bridge needs a distinct pending-candidate signal"
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
    async fn crash_after_propose_replays_without_duplicate_pending_memory() {
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
        assert!(store.list_active().unwrap().is_empty());
        let pending = store.list_pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].body, "turn 0");
    }

    #[tokio::test]
    async fn staged_apply_cursor_resumes_same_key_updates_as_latest_pending() {
        let temp = tempfile::tempdir().unwrap();
        let db = Database::connect_in_memory().await.unwrap();
        admitted_turns(&db, 1).await;
        let old_boot = BootId::generate();
        let claimed = MemoryInboxRepository::new(&db)
            .claim_batch(&old_boot, leveler_core::now(), 1)
            .await
            .unwrap();
        let pro = leveler_memory::parse_durable_fact("以后本项目默认模型固定为 Pro").unwrap();
        let flash = leveler_memory::parse_durable_fact("以后本项目默认模型改成 Flash").unwrap();
        let store = MemoryStore::open(temp.path().join("memory")).unwrap();
        store.propose(pro.clone()).unwrap();
        let staged = StagedTurnResult {
            accepted: vec![pro, flash],
            rejected: 0,
            applied: 1,
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
        assert!(store.effective_active().unwrap().is_empty());
        let pending = store.list_pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].body.contains("Flash"));
        assert!(store.list_archived().unwrap().is_empty());
    }
}
