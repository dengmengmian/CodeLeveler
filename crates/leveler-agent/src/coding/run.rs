//! The Coding task strategy: what a coding agent does with an engine turn.
//!
//! This is the harness half of what used to be one fused `TaskEngine`. The
//! engine below owns session/turn/task lifecycle, persistence and resume; the
//! decisions here — what a Coding task IS, when it is concluded, what gets
//! verified, whether a reviewer runs — belong to the domain and live here.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio_util::sync::CancellationToken;

use leveler_core::{SessionId, ToolCallId, TurnId};
use leveler_engine::{
    EngineError, EngineEvent, EventLog, ExecutionKind, TaskEngine, TaskOutcome, TurnKind,
    TurnRunner,
};
use leveler_execution::{Approver, Clarifier, PermissionProfile};
use leveler_lifecycle::{
    AgentState, PlanState, ProgressLedger, SessionStatus, StopReason, VerificationReport,
};
use leveler_model::{
    ContentPart, Message, Role, RuntimeNoticeKind, ToolCall, ToolResultContent, TranscriptOrigin,
};
#[cfg(test)]
use leveler_storage::EventStore;

use crate::coding::factory::{ExecutorFactory, TurnProfile};
use crate::coding::policy::IndependentReviewPolicy;
use crate::coding::turn::{TurnInput, content_objective_text, drive_turn};
use crate::coding::workspace::GitWorkspace;
use crate::{ContinuationPolicy, StepLimits};

const FINALIZATION_REVIEW: &str = "review";
const FINALIZATION_RESOLVING_OUTCOME: &str = "resolving_outcome";
const FINALIZATION_CONTINUATION_CHECKPOINT: &str = "continuation_checkpoint";
const FINALIZATION_PUBLISHING_TERMINAL: &str = "publishing_terminal";

async fn begin_finalization_phase(
    log: &EventLog<'_>,
    phase: &str,
    observer: &mut (dyn FnMut(EngineEvent) + Send),
) -> Option<std::time::Instant> {
    if let Err(error) = log
        .append(
            None,
            EngineEvent::FinalizationPhaseStarted {
                phase: phase.to_string(),
                at: leveler_core::now(),
            },
            observer,
        )
        .await
    {
        tracing::warn!(%error, %phase, "could not persist finalization phase start");
        return None;
    }
    Some(std::time::Instant::now())
}

fn finish_finalization_phase(phase: &str, started: std::time::Instant) {
    tracing::debug!(
        %phase,
        elapsed_ms = started.elapsed().as_millis(),
        "finalization phase finished"
    );
}

/// The Coding harness: the composition root for a coding agent, over the
/// engine that gives it a lifecycle.
///
/// The engine is a field, not a base class. It knows nothing about anything
/// declared in this module.
pub struct CodingRuntime {
    pub engine: TaskEngine,
    pub factory: ExecutorFactory,
    pub approver: Arc<dyn Approver>,
    pub clarifier: Arc<dyn Clarifier>,
    /// Set while the user has asked to cancel the LOGICAL task. Read at
    /// terminal time so a cooperative stop is recorded as `cancelled` rather
    /// than resumable `interrupted`.
    pub task_cancel: Arc<AtomicBool>,
}

#[derive(Clone, Copy)]
enum ResumeTurnKind {
    Chat,
    Goal,
}

struct ResumeLineage {
    kind: ResumeTurnKind,
    objective: leveler_lifecycle::ObjectiveAnchor,
    root_turn_id: TurnId,
    turn_ids: Vec<TurnId>,
    goal_id: Option<leveler_core::GoalId>,
    /// The pending declaration this resume will consume, when it is the exact
    /// objective of the goal being continued.
    declaration: Option<String>,
    /// The newest user/chat turn itself reached `completed`. A finished
    /// question is not the work a later `继续` continues; a failed or
    /// interrupted turn is.
    latest_completed: bool,
}

/// A delivery the crashed terminal commit never published, read back from the
/// turn that produced it.
struct UncommittedDelivery {
    turn_id: TurnId,
    text: String,
    stop: StopReason,
    model_steps: u32,
    modified_files: Vec<String>,
    /// The transcript boundary (exclusive next ordinal) the accepted answer
    /// belongs to. `None` on deliveries written before the binding existed.
    transcript_ordinal: Option<u64>,
    /// The delivery's turn has no `TurnFinished`: the crash hit inside the
    /// turn's own terminal commit, so recovery must close that turn first.
    /// Otherwise only the task terminal is missing.
    turn_open: bool,
    /// What the durable log says about the closure review this delivery owes.
    /// A crash can hit before the review starts, during it, or after it
    /// settled but before the task terminal; recovery must resume exactly the
    /// remaining obligation.
    review: DeliveryReviewState,
}

/// The closure-review obligation recorded after a delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeliveryReviewState {
    /// Finalization never reached the review stage.
    Absent,
    /// A review was launched but its outcome is not durable. The obligation
    /// was created under `Required`, so recovery must not drop it.
    Unsettled,
    /// The review already settled; recovery must not run a second one.
    Settled(ClosureReview),
}

fn continuation_root_is_valid(
    turns: &[leveler_storage::TurnRecord],
    continuation: &leveler_storage::TurnRecord,
    root: &TurnId,
    continuation_goal: Option<&leveler_core::GoalId>,
) -> bool {
    turns.iter().any(|candidate| {
        if candidate.id != root.as_str()
            || candidate.session_id != continuation.session_id
            || candidate.ordinal >= continuation.ordinal
        {
            return false;
        }
        if candidate.kind == continuation.kind {
            return true;
        }
        // A Chat turn upgraded by `/goal` is the legitimate root of the Goal
        // windows that continue it. Its payload names the goal, which is what
        // distinguishes it from an unrelated Chat turn.
        candidate.kind == "chat"
            && continuation_goal.is_some()
            && candidate
                .payload
                .as_deref()
                .and_then(|payload| leveler_engine::decode_turn_continuation(payload).ok())
                .and_then(|lineage| lineage.goal_id)
                .as_ref()
                == continuation_goal
    })
}

/// Whether a persisted V1 turn carried one of the two bare continuation
/// words that version accepted. This reads old turn payloads to recover their
/// lineage; it never classifies a live user message. Current turns record an
/// explicit objective, and a live message's meaning is the model's declared
/// intent.
fn is_bare_legacy_continuation(message: &Message) -> bool {
    let text = message.text_content();
    let trimmed = text.trim();
    let lower = trimmed.to_ascii_lowercase();
    trimmed == "继续" || lower == "continue" || lower == "resume"
}

/// The domain-neutral half of a task: what to do and how long the runtime
/// may spend on it. Nothing here names Git, a repository, or a verification
/// plan — the engine's generic lifecycle machinery reads only this half.
#[derive(Clone)]
pub struct RuntimeTaskSpec {
    pub goal: String,
    pub kind: ExecutionKind,
    /// Top-level continuation is independent from model capability. Interactive
    /// tasks use `UntilTerminal`; evals may supply a fixed case budget.
    pub continuation: ContinuationPolicy,
    /// Optional top-level token/cost/duration limits. Defaults are unlimited.
    /// Evaluation may additionally supply an explicit case-wide round budget.
    pub limits: StepLimits,
}
/// The Coding-domain half of a task: where the work happens and how its
/// completion is proven. Verification and baseline attribution live here —
/// they are the Coding completion gate's inputs, not runtime lifecycle.
#[derive(Clone)]
pub struct CodingTaskSpec {
    pub repository: Option<PathBuf>,
    pub mode: PermissionProfile,
    pub sandbox: bool,
}

/// Everything needed to create a task: the runtime descriptor plus the Coding
/// execution spec. The split is the migration seam toward a domain-neutral
/// engine — while Coding is the only domain, the engine still receives both
/// halves together, but which half a code path reads is now explicit.
#[derive(Clone)]
pub struct TaskSpec {
    pub runtime: RuntimeTaskSpec,
    pub coding: CodingTaskSpec,
}

fn goal_profile(spec: &TaskSpec) -> TurnProfile {
    TurnProfile::Goal {
        continuation: spec.runtime.continuation,
        limits: spec.runtime.limits,
        continues_active_goal: false,
    }
}

/// Marker of the deterministic facts block. A block is rebuilt from durable
/// state on every request, so it is never accumulated across folds.
const COMPACTION_FACTS_MARKER: &str = "## Retained execution facts";
/// Hard bound on the block. The block is a projection, not history; when the
/// active facts exceed it, the least critical tail is truncated explicitly.
const MAX_COMPACTION_FACTS_CHARS: usize = 3_500;

struct CompactionFactsInput<'a> {
    objective: Option<&'a str>,
    effective: &'a [leveler_lifecycle::RequirementItem],
    amendments: &'a [String],
    plan: Option<&'a leveler_lifecycle::PlanState>,
    progress: Option<&'a leveler_lifecycle::ProgressLedger>,
    verification: &'a [crate::executor::goal_verification::VerificationRun],
    dangling: &'a [(String, String)],
    background: &'a [(String, String, String)],
}

/// Render the bounded deterministic facts a post-compaction request must keep.
///
/// The model summary remains the only carrier of free-form history; these
/// facts come from durable owners (goal row, plan/progress events, verification
/// runs, tool-event dangling calls, background registry) and are re-derived
/// every time, so a fold cannot silently drop them or make an old one linger.
fn render_compaction_facts(input: &CompactionFactsInput<'_>) -> Option<String> {
    let mut out = String::new();
    out.push_str(COMPACTION_FACTS_MARKER);
    out.push_str(" (deterministic, not the model summary)\n");
    if let Some(objective) = input.objective.map(str::trim).filter(|s| !s.is_empty()) {
        out.push_str(&format!("Task objective: {}\n", clip(objective, 400)));
    }
    let owed: Vec<&leveler_lifecycle::RequirementItem> = input
        .effective
        .iter()
        .filter(|item| item.is_owed())
        .collect();
    if !owed.is_empty() {
        out.push_str("Recorded user requirements (the Gate uses exactly this set):\n");
        for requirement in owed.iter().take(8) {
            out.push_str(&format!(
                "- {} [{}] {}\n",
                requirement.id,
                requirement.kind.label(),
                clip(&requirement.text, 200)
            ));
        }
        if owed.len() > 8 {
            out.push_str(&format!(
                "…and {} more recorded requirements the Gate still enforces; call get_goal_scope to read them.\n",
                owed.len() - 8
            ));
        }
    }
    if !input.amendments.is_empty() {
        out.push_str(
            "User instructions, oldest to newest (recorded verbatim; requirement changes are declared separately):\n",
        );
        let start = input.amendments.len().saturating_sub(8);
        for amendment in &input.amendments[start..] {
            out.push_str(&format!("- {}\n", clip(amendment, 240)));
        }
    }
    if let Some(plan) = input.plan {
        let open = plan.open_steps();
        if !open.is_empty() {
            out.push_str("Open plan steps (the runtime still expects these):\n");
            for step in open.iter().take(10) {
                out.push_str(&format!(
                    "- [{}] {}\n",
                    step.status.as_str(),
                    clip(&step.step, 180)
                ));
            }
        }
    }
    if let Some(progress) = input.progress {
        out.push_str(&format!(
            "Execution so far: phase={:?}, model_steps={}, commands={}, modified_files={}\n",
            progress.phase,
            progress.cumulative_model_steps,
            progress.cumulative_commands,
            progress.cumulative_modified_files
        ));
        if !progress.cumulative_modified_paths.is_empty() {
            out.push_str(&format!(
                "Modified paths: {}\n",
                progress
                    .cumulative_modified_paths
                    .iter()
                    .take(10)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    if !input.verification.is_empty() {
        out.push_str("Verification evidence (recorded mutation count matters):\n");
        let start = input.verification.len().saturating_sub(5);
        for run in &input.verification[start..] {
            out.push_str(&format!(
                "- `{}` {} (step {}, mutations {})\n",
                clip(&run.label, 160),
                if run.ok { "passed" } else { "failed" },
                run.model_step,
                run.mutation_ops
            ));
        }
    }
    if !input.dangling.is_empty() {
        out.push_str("Unknown tool outcomes (may have run; do not retry a side effect blind):\n");
        for (name, call_id) in input.dangling.iter().take(5) {
            out.push_str(&format!("- {name} ({call_id})\n"));
        }
    }
    if !input.background.is_empty() {
        out.push_str("Background tasks owned by this task:\n");
        for (id, status, lifetime) in input.background.iter().take(5) {
            out.push_str(&format!("- {id} {status} ({lifetime})\n"));
        }
    }
    if out.lines().count() <= 1 {
        return None;
    }
    if out.len() > MAX_COMPACTION_FACTS_CHARS {
        let cut = out
            .char_indices()
            .take_while(|(index, _)| *index <= MAX_COMPACTION_FACTS_CHARS)
            .last()
            .map(|(index, _)| index)
            .unwrap_or(0);
        out.truncate(cut);
        out.push_str("\n…[facts truncated to the context budget]\n");
    }
    Some(out)
}

fn clip(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let clipped: String = text.chars().take(max_chars).collect();
    format!("{clipped}…")
}

/// The Coding harness's answer to "is the prior domain state still open?".
///
/// This is the harness's judgement over its own vocabulary: a finished epoch is
/// a fully completed plan, or progress that reached Closing/Terminal. The
/// engine never computes it. Coding uses the answer when it decides whether
/// the next fresh turn inherits its prior workflow state.
///
/// Absence of prior state is OPEN, not closed: an empty epoch seeds harmlessly.
pub(crate) fn prior_epoch_open(
    plan: Option<&PlanState>,
    progress: Option<&ProgressLedger>,
) -> bool {
    if let Some(progress) = progress
        && progress.is_terminal_for_inheritance()
    {
        return false;
    }
    if let Some(plan) = plan
        && plan.is_fully_completed()
    {
        return false;
    }
    true
}

/// The session's lifecycle columns for a finished task.
///
/// Read these directly from how the run ended. User-requested tests, builds,
/// and linters remain ordinary tool calls and do not create a second terminal
/// status for the task.
pub(crate) fn terminal_status_for(report: &TaskReport) -> (SessionStatus, AgentState) {
    use StopReason as S;
    match report.stop_reason {
        S::Completed | S::Answered => (SessionStatus::Completed, AgentState::Complete),
        S::Incomplete | S::BudgetExhausted | S::TurnLimitReached | S::Stalled => {
            (SessionStatus::Incomplete, AgentState::Execute)
        }
        S::Blocked => (SessionStatus::Blocked, AgentState::Execute),
    }
}

/// Record on the terminal whether its goal still owes work.
///
/// `resume` re-enters a goal whose last window left work owed, so an owed goal
/// is exactly one a continuation can re-enter: [`task_outcome_is_resumable`].
/// A blocked, interrupted or recoverably-failed goal is owed; a delivered or
/// explicitly-cancelled one is not.
///
/// An INFRASTRUCTURE fault is not a verdict on the user's request. Nothing was
/// decided there, and a restart must still be able to find the goal, so only a
/// typed unrecoverable provider fault (`decided == false`, failure present)
/// closes the work.
fn settle_goal_from_terminal(terminal: &mut leveler_engine::TaskTerminal, decided: bool) {
    let reenterable =
        task_outcome_is_resumable(terminal.outcome, terminal.stop, terminal.failure.as_ref());
    let settled = if decided {
        !reenterable
    } else {
        terminal.failure.is_some() && !reenterable
    };
    if let Some(goal) = terminal.goal.as_mut() {
        goal.settle = settled;
    }
}

fn chat_profile(spec: &TaskSpec) -> TurnProfile {
    TurnProfile::Chat {
        continuation: spec.runtime.continuation,
        limits: spec.runtime.limits,
    }
}

/// The engine's terminal report for a task.
#[derive(Debug)]
pub struct TaskReport {
    pub outcome: TaskOutcome,
    pub final_text: String,
    pub modified_files: Vec<String>,
    /// The executor's stop reason (legacy status mapping needs its nuance).
    pub stop_reason: StopReason,
    /// The executor's concrete reason for a non-success stop, when available.
    pub stop_detail: Option<String>,
    /// Model steps this task's turn started — a mechanical iteration count,
    /// never a measure of how much of the task is done.
    pub model_steps: u32,
    /// Executor wall time used to preserve the required review's bounded tail.
    pub execution_duration_ms: u64,
    /// Turns this invocation ran for the goal. One: the engine no longer
    /// opens further windows on the model's behalf, so an invocation is one
    /// turn. Kept as the writer of the durable `goals.windows_run` count.
    pub windows: u32,
    /// Legacy review findings (unused; kept for report shape stability).
    pub review: Option<Vec<String>>,
    /// Completion-contract warnings orthogonal to project verification.
    pub completion_warnings: Vec<String>,
    /// How far the runtime could verify an accepted completion. `None` when
    /// the task declared no verification obligation.
    pub verification: Option<VerificationReport>,
}

impl TaskReport {
    /// A report with the always-present fields set and orchestration-only
    /// extras defaulted to `None`.
    /// Sites that produce those set them via `TaskReport { field: Some(..),
    /// ..TaskReport::new(..) }`, so a new optional field defaults in one place.
    pub(crate) fn new(
        outcome: TaskOutcome,
        final_text: String,
        modified_files: Vec<String>,
        stop_reason: StopReason,
        model_steps: u32,
    ) -> Self {
        Self {
            outcome,
            final_text,
            modified_files,
            stop_reason,
            stop_detail: None,
            model_steps,
            execution_duration_ms: 0,
            windows: 1,
            review: None,
            completion_warnings: Vec::new(),
            verification: None,
        }
    }

    fn with_stop_detail(mut self, stop_detail: Option<String>) -> Self {
        self.stop_detail = stop_detail;
        self
    }
}

fn report_from_agent_outcome(
    outcome: crate::AgentOutcome,
    task_outcome: TaskOutcome,
) -> TaskReport {
    let execution_duration_ms = outcome.progress.cumulative_duration_ms;
    let mut report = TaskReport::new(
        task_outcome,
        outcome.final_text,
        outcome.modified_files,
        outcome.stop_reason,
        outcome.model_steps,
    )
    .with_stop_detail(outcome.stop_detail);
    report.execution_duration_ms = execution_duration_ms;
    report.completion_warnings = outcome.completion_warnings;
    report.verification = outcome.verification;
    report
}

fn task_terminal_reason(report: &TaskReport) -> Option<String> {
    report.stop_detail.clone()
}

fn task_terminal_stop(report: &TaskReport) -> StopReason {
    report.stop_reason
}

pub fn mode_str(mode: PermissionProfile) -> &'static str {
    mode.as_str()
}

/// [`leveler_engine::ContextSummarizer`] backed by the engine's own model runtime:
/// one bounded, tool-free request over the messages about to be folded.
pub struct ModelSummarizer<'a> {
    runtime: &'a dyn leveler_model::ModelRuntime,
    model: &'a leveler_model::ModelRef,
    cancellation: &'a CancellationToken,
    engine: &'a TaskEngine,
    session_id: &'a SessionId,
    limits: StepLimits,
    budget_scope: String,
    overrides: Option<&'a crate::coding::ExecutionOverrides>,
}

#[async_trait::async_trait]
impl leveler_engine::ContextSummarizer for ModelSummarizer<'_> {
    async fn summarize(
        &self,
        messages: &[leveler_model::Message],
        keep_recent: usize,
        keep_recent_tokens: u64,
    ) -> Result<Option<String>, EngineError> {
        let profile = self
            .runtime
            .profile(self.model)
            .await
            .map_err(|error| EngineError::Config(error.to_string()))?;
        let policy = crate::coding::policy::resolve_execution_policy(
            &profile,
            crate::coding::policy::ExecutionRole::Main,
            &TurnProfile::Chat {
                continuation: ContinuationPolicy::UntilTerminal,
                limits: StepLimits::default(),
            },
            self.overrides,
        );
        let Some(request) = leveler_context::summary_request(
            self.runtime,
            self.model,
            crate::ModelCallKind::Compaction.default_reasoning_effort(),
            // The CALLER's retention, not a policy re-derived here: the
            // briefing must replace exactly the rounds the fold removes.
            messages,
            keep_recent,
            keep_recent_tokens,
            policy.max_output_tokens,
        )
        .await
        else {
            return Ok(None);
        };
        let progress = crate::coding::turn::persisted_budget_progress(
            self.engine.stores.events.as_ref(),
            self.session_id,
            &self.budget_scope,
        )
        .await?
        .unwrap_or_default();
        let progress = crate::coding::turn::reconcile_model_spend(
            progress,
            self.engine.stores.model_requests.as_ref(),
            self.session_id,
            &self.budget_scope,
        )
        .await?;
        if !crate::executor::auxiliary_budget_available(
            self.limits,
            &progress,
            &request,
            profile.pricing.as_ref(),
        ) {
            return Ok(None);
        }
        let mut recorder = PreTurnSummaryObserver {
            summarizer: self,
            pricing: profile.pricing,
            reasoning_effort: request.reasoning_effort,
            request: &request,
        };
        let round = crate::executor::run_auxiliary_round(
            self.runtime,
            request.clone(),
            self.cancellation,
            &mut recorder,
            self.limits.max_duration.map(|duration| {
                duration.saturating_sub(std::time::Duration::from_millis(
                    progress.cumulative_duration_ms,
                ))
            }),
        )
        .await;
        let round = match round {
            Ok(round) => round,
            // No auxiliary budget: no briefing. The lifecycle (not this
            // summarizer) decides what that costs — soft keeps the original
            // history, hard folds mechanically.
            Err(crate::AgentError::AuxiliaryBudgetUnavailable) => return Ok(None),
            // The harness cancelled the AUXILIARY child token when its own
            // deadline elapsed; the task's own token is still live. A real
            // task cancellation is NOT this case and propagates below.
            Err(crate::AgentError::Cancelled) if !self.cancellation.is_cancelled() => {
                return Ok(None);
            }
            Err(error) => {
                // A real task cancellation is not a summary failure.
                if self.cancellation.is_cancelled() {
                    return Err(EngineError::Cancelled);
                }
                // A provider fault or timeout produced no briefing. The
                // context lifecycle decides what that costs; the summarizer
                // does not.
                if matches!(error, crate::AgentError::Model(_)) {
                    return Ok(None);
                }
                // Ownership, persistence and configuration faults are not
                // recoverable by a fold and must not be hidden.
                return Err(EngineError::Config(error.to_string()));
            }
        };
        Ok(leveler_context::accepted_summary(
            &leveler_model::ModelResponse {
                request_id: leveler_core::RequestId::new(round.request_id),
                message: round.message,
                usage: round.usage,
                finish_reason: round.finish_reason,
            },
        ))
    }
}
struct PreTurnSummaryObserver<'a, 'b> {
    summarizer: &'a ModelSummarizer<'b>,
    pricing: Option<leveler_model::ModelPricing>,
    reasoning_effort: Option<leveler_model::ReasoningEffort>,
    request: &'a leveler_model::ModelRequest,
}
#[async_trait::async_trait]
impl leveler_agent_core::ModelRoundObserver for PreTurnSummaryObserver<'_, '_> {
    type Error = crate::AgentError;
    fn on_event(&mut self, _: leveler_agent_core::AgentEvent) {}
    async fn before_attempt(&mut self) -> Result<(), Self::Error> {
        let host = self.summarizer;
        let progress = crate::coding::turn::persisted_budget_progress(
            host.engine.stores.events.as_ref(),
            host.session_id,
            &host.budget_scope,
        )
        .await
        .map_err(|error| leveler_engine::PortError::Persistence(error.to_string()))?
        .unwrap_or_default();
        let progress = crate::coding::turn::reconcile_model_spend(
            progress,
            host.engine.stores.model_requests.as_ref(),
            host.session_id,
            &host.budget_scope,
        )
        .await
        .map_err(|error| leveler_engine::PortError::Persistence(error.to_string()))?;
        if !crate::executor::auxiliary_budget_available(
            host.limits,
            &progress,
            self.request,
            self.pricing.as_ref(),
        ) {
            return Err(crate::AgentError::AuxiliaryBudgetUnavailable);
        }
        Ok(())
    }
    async fn on_attempt(
        &mut self,
        attempt: leveler_model::ModelAttempt,
    ) -> Result<(), Self::Error> {
        let host = self.summarizer;
        let mut record = crate::executor::model_attempt_record(
            &attempt,
            host.model,
            crate::ModelCallKind::Compaction,
            self.reasoning_effort,
        )
        .priced(self.pricing.as_ref());
        record.budget_scope = Some(host.budget_scope.clone());
        host.engine
            .stores
            .model_requests
            .insert(&leveler_engine::storage_model_request(
                &record,
                host.session_id,
            ))
            .await
            .map_err(|error| leveler_engine::PortError::Persistence(error.to_string()))?;
        let progress = crate::coding::turn::persisted_budget_progress(
            host.engine.stores.events.as_ref(),
            host.session_id,
            &host.budget_scope,
        )
        .await
        .map_err(|error| leveler_engine::PortError::Persistence(error.to_string()))?
        .unwrap_or_default();
        let mut progress = crate::coding::turn::reconcile_model_spend(
            progress,
            host.engine.stores.model_requests.as_ref(),
            host.session_id,
            &host.budget_scope,
        )
        .await
        .map_err(|error| leveler_engine::PortError::Persistence(error.to_string()))?;
        progress.cumulative_duration_ms = progress
            .cumulative_duration_ms
            .saturating_add(attempt.latency_ms);
        EventLog::new(host.engine.stores.events.as_ref(), host.session_id.clone())
            .append(
                None,
                EngineEvent::ProgressUpdated { ledger: progress },
                &mut |_| {},
            )
            .await
            .map_err(|error| leveler_engine::PortError::Persistence(error.to_string()))?;
        if host.limits.max_cost_usd_micros.is_some() && record.cost_usd_micros.is_none() {
            return Err(crate::AgentError::Model(leveler_model::ModelError::new(
                leveler_model::ModelErrorKind::Other,
                "summary attempt has no auditable cost under active cap",
            )));
        }
        Ok(())
    }
}

impl CodingRuntime {
    async fn resume_lineage(
        &self,
        session_id: &SessionId,
        fallback_objective: &str,
    ) -> Result<ResumeLineage, EngineError> {
        let turns = self
            .engine
            .stores
            .turns
            .list_for_session(session_id)
            .await?;
        let mut resolved = None;
        let mut legacy_continuation_ids = Vec::new();
        for turn in turns.iter().rev() {
            if turn.kind != "user" && turn.kind != "chat" {
                continue;
            }
            let Some(payload) = turn.payload.as_deref() else {
                // With no WAL payload there is no evidence tying this turn to
                // an older objective. Downgrade explicitly to THIS real row;
                // a legacy user turn can use the persisted session goal, while
                // a Chat cannot be reconstructed without inventing intent.
                if turn.kind == "chat" {
                    return Err(EngineError::Corrupt(format!(
                        "legacy chat turn {} has no initiating payload",
                        turn.id
                    )));
                }
                resolved = Some(ResumeLineage {
                    kind: ResumeTurnKind::Goal,
                    objective: leveler_lifecycle::ObjectiveAnchor::from_session_goal(
                        fallback_objective,
                    ),
                    root_turn_id: TurnId::new(turn.id.clone()),
                    turn_ids: Vec::new(),
                    goal_id: None,
                    declaration: None,
                    latest_completed: turn.status == "completed",
                });
                break;
            };
            let payload_version = serde_json::from_str::<serde_json::Value>(payload)
                .ok()
                .and_then(|value| value.get("version")?.as_u64());
            if payload_version == Some(2) {
                let lineage = leveler_engine::decode_turn_continuation(payload)?;
                // A Chat turn that was bound by `/goal` names its OLD chat
                // lineage root, which predates the goal. The goal's root is
                // the binding turn itself (set below), so that older pointer
                // is not a goal-lineage claim to validate.
                let chat_became_goal = turn.kind == "chat" && lineage.goal_id.is_some();
                if let Some(root) = lineage.root_turn_id.as_ref().filter(|_| !chat_became_goal) {
                    let valid_root =
                        continuation_root_is_valid(&turns, turn, root, lineage.goal_id.as_ref());
                    if !valid_root {
                        return Err(EngineError::Corrupt(format!(
                            "turn {} has invalid continuation root {}",
                            turn.id,
                            root.as_str()
                        )));
                    }
                    let root_turn = turns
                        .iter()
                        .find(|candidate| candidate.id == root.as_str())
                        .expect("validated root exists");
                    if let Some(root_payload) = root_turn.payload.as_deref() {
                        let root_version = serde_json::from_str::<serde_json::Value>(root_payload)
                            .ok()
                            .and_then(|value| value.get("version")?.as_u64());
                        if root_version == Some(2) {
                            let root_lineage =
                                leveler_engine::decode_turn_continuation(root_payload)?;
                            // The goal id is the identity: a root that names
                            // this goal is its root, even when the root is an
                            // adopted continuation of an older chat window.
                            if root_lineage.goal_id != lineage.goal_id {
                                return Err(EngineError::Corrupt(format!(
                                    "turn {} names root {} from a different lineage",
                                    turn.id,
                                    root.as_str()
                                )));
                            }
                        }
                    }
                }
                resolved = Some(ResumeLineage {
                    kind: if turn.kind == "chat" {
                        ResumeTurnKind::Chat
                    } else {
                        ResumeTurnKind::Goal
                    },
                    objective: lineage.objective,
                    // A Chat turn bound by `/goal` is where THIS goal began:
                    // its chat lineage may be older, but the goal did not
                    // exist there. Using the binding window as the root keeps
                    // every later continuation inside the goal's own lineage.
                    root_turn_id: if turn.kind == "chat" && lineage.goal_id.is_some() {
                        TurnId::new(turn.id.clone())
                    } else {
                        lineage
                            .root_turn_id
                            .unwrap_or_else(|| TurnId::new(turn.id.clone()))
                    },
                    turn_ids: Vec::new(),
                    goal_id: lineage.goal_id,
                    declaration: None,
                    latest_completed: turn.status == "completed",
                });
                break;
            }
            // V1 compatibility: its initiating message is authoritative for
            // that fresh turn even though it did not yet persist an explicit
            // objective/root. Never fall back to the transcript's first user.
            if let Ok(message) = leveler_engine::decode_turn_initiating_message(payload) {
                if is_bare_legacy_continuation(&message) {
                    legacy_continuation_ids.push(TurnId::new(turn.id.clone()));
                    continue;
                }
                let text = message.text_content();
                let kind = if turn.kind == "chat" {
                    ResumeTurnKind::Chat
                } else {
                    ResumeTurnKind::Goal
                };
                let objective = match kind {
                    ResumeTurnKind::Chat => {
                        leveler_lifecycle::ObjectiveAnchor::from_user_message(text)
                    }
                    ResumeTurnKind::Goal => {
                        leveler_lifecycle::ObjectiveAnchor::from_session_goal(text)
                    }
                };
                resolved = Some(ResumeLineage {
                    kind,
                    objective,
                    root_turn_id: TurnId::new(turn.id.clone()),
                    turn_ids: Vec::new(),
                    goal_id: None,
                    declaration: None,
                    latest_completed: turn.status == "completed",
                });
                break;
            }
        }
        if let Some(mut lineage) = resolved {
            let root_turn_id = lineage.root_turn_id.clone();
            let goal_id = lineage.goal_id.clone();
            lineage.turn_ids = turns
                .iter()
                .filter_map(|turn| {
                    if turn.id == root_turn_id.as_str() {
                        return Some(TurnId::new(turn.id.clone()));
                    }
                    let payload = turn.payload.as_deref()?;
                    let decoded = leveler_engine::decode_turn_continuation(payload).ok()?;
                    (decoded.root_turn_id.as_ref() == Some(&root_turn_id)
                        && decoded.goal_id == goal_id)
                        .then(|| TurnId::new(turn.id.clone()))
                })
                .collect();
            legacy_continuation_ids.reverse();
            for turn_id in legacy_continuation_ids {
                if !lineage.turn_ids.contains(&turn_id) {
                    lineage.turn_ids.push(turn_id);
                }
            }
            if matches!(lineage.kind, ResumeTurnKind::Goal) && lineage.goal_id.is_none() {
                lineage.goal_id = self.unique_legacy_running_goal(session_id).await?;
            }
            return Ok(lineage);
        }
        // Pre-WAL sessions have no initiating payload at all. Bind their
        // explicit downgrade to a REAL persisted turn so a second resume can
        // validate the root and seed the first resume's events.
        let root = turns
            .iter()
            .rev()
            .find(|turn| turn.kind == "user" || turn.kind == "chat")
            .ok_or_else(|| EngineError::Corrupt("session has no resumable turn".into()))?;
        let goal_id = self.unique_legacy_running_goal(session_id).await?;
        Ok(ResumeLineage {
            kind: ResumeTurnKind::Goal,
            objective: leveler_lifecycle::ObjectiveAnchor::from_session_goal(fallback_objective),
            root_turn_id: TurnId::new(root.id.clone()),
            turn_ids: vec![TurnId::new(root.id.clone())],
            goal_id,
            declaration: None,
            latest_completed: root.status == "completed",
        })
    }

    /// The goal bound to the most recent user/chat turn, read from its WAL
    /// payload. Used when a turn aborts before it can report the binding, so
    /// terminal settlement still names the right goal.
    async fn latest_bound_goal(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<leveler_core::GoalId>, EngineError> {
        let turns = self
            .engine
            .stores
            .turns
            .list_for_session(session_id)
            .await?;
        for turn in turns.iter().rev() {
            if turn.kind != "chat" && turn.kind != "user" {
                continue;
            }
            let Some(payload) = turn.payload.as_deref() else {
                continue;
            };
            if let Ok(lineage) = leveler_engine::decode_turn_continuation(payload) {
                return Ok(lineage.goal_id);
            }
        }
        Ok(None)
    }

    async fn unique_legacy_running_goal(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<leveler_core::GoalId>, EngineError> {
        let task_id = self
            .engine
            .stores
            .tasks
            .task_for_session(session_id)
            .await?
            .ok_or_else(|| EngineError::Corrupt("legacy session has no task".into()))?;
        let mut running = self
            .engine
            .stores
            .goals
            .for_task(&task_id)
            .await?
            .into_iter()
            .filter(|goal| goal.state == leveler_storage::GoalState::Running);
        let Some(goal) = running.next() else {
            return Ok(None);
        };
        if running.next().is_some() {
            return Err(EngineError::Corrupt(
                "legacy goal continuation is ambiguous across multiple running goals".into(),
            ));
        }
        Ok(Some(goal.id))
    }

    async fn open_or_reuse_goal(
        &self,
        token: &leveler_core::OwnershipToken,
        objective: &str,
    ) -> Result<leveler_core::GoalId, EngineError> {
        if let Some(goal) = self
            .engine
            .stores
            .goals
            .for_task(&token.task_id)
            .await?
            .into_iter()
            .find(|goal| {
                goal.state == leveler_storage::GoalState::Running && goal.objective == objective
            })
        {
            return Ok(goal.id);
        }
        self.engine.open_goal(token, objective).await
    }

    /// The goal THIS turn drives.
    ///
    /// A session's durable pending declaration — written in the same
    /// transaction as the Goal axis — outranks the turn's own text. After a
    /// crash between `steer_into_goal` and the executor's next round it may be
    /// the only surviving copy of the user's objective, and a later `继续`
    /// must continue that objective rather than become one itself.
    ///
    /// A session that still owes a running goal continues THAT goal: opening a
    /// sibling whose objective is whatever sentence the user happened to type
    /// drops the original scope and leaves two goals for one task.
    async fn open_goal_for_turn(
        &self,
        token: &leveler_core::OwnershipToken,
        session_id: &SessionId,
        fallback_objective: &str,
    ) -> Result<(leveler_core::GoalId, bool), EngineError> {
        let pending = self.engine.stores.sessions.pending_goal(session_id).await?;
        if pending.is_none() {
            // The session's route already decided this message is goal work, so
            // it continues the work that route is about. One owed goal is
            // unambiguous. With two owed goals the one this session was last
            // DRIVING is still a durable fact (the turn's WAL binding), so that
            // is continued instead of opening a third identity.
            if let Some(goal) = self.single_running_goal(&token.task_id).await? {
                return Ok((goal, true));
            }
            if let Some(goal_id) = self.latest_bound_goal(session_id).await?
                && let Some(record) = self.engine.stores.goals.get(&goal_id).await?
                && record.task_id == token.task_id
                && record.state == leveler_storage::GoalState::Running
            {
                return Ok((record.id, true));
            }
        }
        let objective = match pending {
            Some(pending) => pending,
            None => {
                // Durable BEFORE the goal row. A Goal turn can start from a
                // plain message while the session already runs in Goal mode
                // (no `/goal` command re-declares it); without this, a crash
                // between opening the goal and starting its turn would leave
                // a Running goal that no later message can name.
                //
                // The write is conditional: a declaration that arrived while
                // this turn was staged is newer and wins.
                if self
                    .engine
                    .stores
                    .sessions
                    .set_pending_goal_if_absent(session_id, fallback_objective, leveler_core::now())
                    .await?
                {
                    fallback_objective.to_string()
                } else {
                    self.engine
                        .stores
                        .sessions
                        .pending_goal(session_id)
                        .await?
                        .unwrap_or_else(|| fallback_objective.to_string())
                }
            }
        };
        // The declaration is consumed by the turn start, atomically with its
        // WAL row — never here. A crash before that start must leave the text
        // recoverable.
        Ok((self.open_or_reuse_goal(token, &objective).await?, false))
    }

    /// The task's running goal when identity is unambiguous.
    ///
    /// More than one running goal means the task hosts two live objectives, so
    /// nothing here may pick one: the caller falls back to matching the
    /// objective text (recovery) instead of guessing.
    async fn single_running_goal(
        &self,
        task_id: &leveler_core::TaskId,
    ) -> Result<Option<leveler_core::GoalId>, EngineError> {
        let mut running = self
            .engine
            .stores
            .goals
            .for_task(task_id)
            .await?
            .into_iter()
            .filter(|goal| goal.state == leveler_storage::GoalState::Running);
        let Some(first) = running.next() else {
            return Ok(None);
        };
        Ok(running.next().is_none().then_some(first.id))
    }

    /// Re-attribute every Chat window that was bound to this goal.
    ///
    /// The binder moves the rows at bind time. A crash between binding the
    /// turn and moving the rows leaves the window's spend under its Chat
    /// scope, so resume repeats the move. A Chat window's scope is derivable
    /// from its own WAL payload: its turn id for a fresh window, its chat
    /// lineage root for a resumed one.
    async fn adopt_chat_window_budget(
        &self,
        session_id: &SessionId,
        goal_id: &leveler_core::GoalId,
    ) -> Result<(), EngineError> {
        for turn in self
            .engine
            .stores
            .turns
            .list_for_session(session_id)
            .await?
        {
            if turn.kind != "chat" {
                continue;
            }
            let Some(payload) = turn.payload.as_deref() else {
                continue;
            };
            let Ok(decoded) = leveler_engine::decode_turn_continuation(payload) else {
                continue;
            };
            if decoded.goal_id.as_ref() != Some(goal_id) {
                continue;
            }
            let scope = decoded
                .root_turn_id
                .unwrap_or_else(|| TurnId::new(turn.id.clone()));
            self.engine
                .stores
                .model_requests
                .reassign_budget_scope(session_id, scope.as_str(), goal_id.as_str())
                .await?;
        }
        Ok(())
    }

    /// The pending declaration this goal turn will consume.
    ///
    /// Only the exact objective of the goal being continued qualifies. A
    /// newer `/goal <other>` written while the previous turn ran is not this
    /// turn's declaration and must survive for the turn that should own it.
    async fn turn_goal_declaration(
        &self,
        session_id: &SessionId,
        goal_id: &leveler_core::GoalId,
    ) -> Result<Option<String>, EngineError> {
        let Some(pending) = self.engine.stores.sessions.pending_goal(session_id).await? else {
            return Ok(None);
        };
        let objective = self
            .engine
            .stores
            .goals
            .get(goal_id)
            .await?
            .map(|goal| goal.objective);
        Ok((objective.as_deref() == Some(pending.as_str())).then_some(pending))
    }

    /// Rebind a resume to the exact running goal persisted by its lineage.
    ///
    /// The logical task is the TASK, and the goal it owns is the one that was
    /// running when the turn was interrupted. Matching by objective text (or
    /// merely taking the newest running goal) is not identity and can attach a
    /// continuation to unrelated work.
    async fn resume_goal(
        &self,
        token: &leveler_core::OwnershipToken,
        goal_id: &leveler_core::GoalId,
    ) -> Result<leveler_core::GoalId, EngineError> {
        let goal = self
            .engine
            .stores
            .goals
            .get(goal_id)
            .await?
            .ok_or_else(|| EngineError::Corrupt(format!("lineage goal {goal_id} not found")))?;
        if goal.task_id != token.task_id {
            return Err(EngineError::Corrupt(format!(
                "lineage goal {goal_id} does not belong to task {}",
                token.task_id
            )));
        }
        if goal.state == leveler_storage::GoalState::Settled {
            self.engine.stores.goals.reopen(token, goal_id).await?;
        }
        Ok(goal.id)
    }

    async fn load_request_transcript(
        &self,
        session_id: &SessionId,
        strict: Option<&str>,
        checkpoint_scope: Option<&crate::coding::checkpoint::GoalCheckpointScope>,
    ) -> Result<leveler_engine::RawTranscript, EngineError> {
        // Serialized bytes include reasoning the provider projection can omit.
        // Only the projected request can prove history needs folding, so load
        // all rows before applying the model's resolved context policy.
        let _ = checkpoint_scope;
        match strict {
            Some(what) => {
                leveler_engine::RawTranscript::load_strict(
                    self.engine.stores.messages.as_ref(),
                    session_id,
                    what,
                )
                .await
            }
            None => {
                leveler_engine::RawTranscript::load_lossy(
                    self.engine.stores.messages.as_ref(),
                    session_id,
                )
                .await
            }
        }
    }

    async fn resolved_context_policy(
        &self,
    ) -> Result<crate::coding::policy::ResolvedExecutionPolicy, EngineError> {
        let profile = self
            .factory
            .runtime
            .profile(&self.factory.model)
            .await
            .map_err(|error| EngineError::Config(format!("cannot read model profile: {error}")))?;
        Ok(crate::coding::policy::resolve_execution_policy(
            &profile,
            crate::coding::policy::ExecutionRole::Main,
            &TurnProfile::Chat {
                continuation: crate::ContinuationPolicy::UntilTerminal,
                limits: crate::StepLimits::default(),
            },
            self.factory.overrides.as_ref(),
        ))
    }

    async fn context_threshold(&self) -> Result<u64, EngineError> {
        let policy = self.resolved_context_policy().await?.context_policy;
        Ok(if policy.folding_enabled() {
            u64::from(policy.pressure_threshold)
        } else {
            u64::MAX
        })
    }

    /// Build the bounded deterministic facts message from the current durable
    /// owners. `None` when there is nothing to retain.
    async fn compaction_facts_message(
        &self,
        session_id: &SessionId,
        goal_id: Option<&leveler_core::GoalId>,
        log: &EventLog<'_>,
        objective: Option<&str>,
    ) -> Result<Option<leveler_model::Message>, EngineError> {
        let stores = &self.engine.stores;
        let goal = if let Some(id) = goal_id {
            stores.goals.get(id).await?
        } else {
            let task = stores.tasks.task_for_session(session_id).await?;
            match task {
                Some(task) => {
                    let mut running = stores
                        .goals
                        .for_task(&task)
                        .await?
                        .into_iter()
                        .filter(|goal| goal.state == leveler_storage::GoalState::Running);
                    match (running.next(), running.next()) {
                        (Some(goal), None) => Some(goal),
                        _ => None,
                    }
                }
                None => None,
            }
        };
        let plan =
            crate::coding::turn::last_persisted_plan(stores.events.as_ref(), session_id).await?;
        let progress =
            crate::coding::turn::last_persisted_progress(stores.events.as_ref(), session_id)
                .await?;
        let verification = if let Some(goal) = goal.as_ref() {
            crate::coding::turn::load_goal_verification_runs(stores, session_id, &goal.id).await?
        } else {
            Vec::new()
        };
        let dangling: Vec<(String, String)> = log
            .dangling_tool_calls()
            .await?
            .into_iter()
            .take(5)
            .map(|call| (call.name, call.call_id))
            .collect();
        let background: Vec<(String, String, String)> =
            match self.factory.tool_context.session_scope.as_deref() {
                Some(scope) => self
                    .factory
                    .background_tasks
                    .active_snapshots_for_scope(scope)
                    .await
                    .into_iter()
                    .take(5)
                    .map(|snap| {
                        (
                            snap.id,
                            format!("{:?}", snap.status).to_lowercase(),
                            snap.lifetime.as_str().to_string(),
                        )
                    })
                    .collect(),
                None => Vec::new(),
            };
        let objective = goal
            .as_ref()
            .map(|goal| goal.objective.as_str())
            .or(objective);
        let effective = match goal.as_ref() {
            Some(goal) => stores.goals.requirements(&goal.id).await?,
            None => Vec::new(),
        };
        let input = CompactionFactsInput {
            objective,
            effective: &effective,
            amendments: goal
                .as_ref()
                .map(|goal| goal.amendments.as_slice())
                .unwrap_or(&[]),
            plan: plan.as_ref(),
            progress: progress.as_ref(),
            verification: &verification,
            dangling: &dangling,
            background: &background,
        };
        let Some(text) = render_compaction_facts(&input) else {
            return Ok(None);
        };
        // The facts are model-visible text assembled from durable rows; the
        // same Phase 2A boundary applies here as to any other projection.
        let (text, _) = leveler_core::sanitize_model_visible(&text);
        Ok(Some(leveler_model::Message::text(
            leveler_model::Role::System,
            text,
        )))
    }

    /// Insert the facts block after the summary/goal checkpoint (or after the
    /// leading system prompt) and drop any previous block, so a second fold
    /// replaces rather than accumulates.
    async fn with_compaction_facts(
        &self,
        mut messages: Vec<leveler_model::Message>,
        session_id: &SessionId,
        goal_id: Option<&leveler_core::GoalId>,
        log: &EventLog<'_>,
        objective: Option<&str>,
    ) -> Result<Vec<leveler_model::Message>, EngineError> {
        let Some(facts) = self
            .compaction_facts_message(session_id, goal_id, log, objective)
            .await?
        else {
            return Ok(messages);
        };
        // Eval-only: record the projection that is returned into the model
        // context. Unset in production; this is the same text, not a copy made
        // by a different path.
        if let Ok(path) = std::env::var("LEVELER_EVAL_FACTS_TRACE") {
            use std::io::Write;
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                let _ = writeln!(file, "{}", facts.text_content());
            }
        }
        messages.retain(|message| !message.text_content().contains(COMPACTION_FACTS_MARKER));
        let at = messages
            .iter()
            .position(|message| {
                let text = message.text_content();
                text.contains(leveler_model::COMPACTION_BREADCRUMB_MARKER)
                    || text.starts_with("[GOAL CHECKPOINT]")
            })
            .map(|index| index + 1)
            .or_else(|| {
                messages
                    .iter()
                    .position(|message| message.role == leveler_model::Role::System)
                    .map(|index| index + 1)
            })
            .unwrap_or(0);
        messages.insert(at, facts);
        Ok(messages)
    }

    #[allow(clippy::too_many_arguments)]
    async fn assembled_prior(
        &self,
        log: &EventLog<'_>,
        session_id: &SessionId,
        raw: leveler_engine::RawTranscript,
        objective: Option<&str>,
        continuing: bool,
        checkpoint_scope: Option<&crate::coding::checkpoint::GoalCheckpointScope>,
        workspace: Option<&dyn crate::coding::checkpoint::WorkspaceFacts>,
        summarizer: &dyn leveler_engine::ContextSummarizer,
        cancellation: &CancellationToken,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
    ) -> Result<Vec<leveler_model::Message>, EngineError> {
        if let Some(scope) = checkpoint_scope
            && let Some(prior) = self
                .checkpointed_prior(
                    log,
                    session_id,
                    &raw,
                    objective,
                    continuing,
                    workspace,
                    summarizer,
                    cancellation,
                    observer,
                    scope,
                )
                .await?
        {
            let prior =
                leveler_context::compact_messages(&prior, usize::MAX, u64::MAX, None, objective);
            return self
                .with_compaction_facts(prior, session_id, Some(&scope.goal_id), log, objective)
                .await;
        }
        let policy = self.resolved_context_policy().await?;
        // Build the executor once: its tool surface and its control context are
        // exactly what the request that follows this load will carry, so the
        // pressure figure here is the SAME projection the drive will take — the
        // state measured and the state sent cannot diverge.
        let executor = self
            .factory
            .build(
                TurnProfile::Chat {
                    continuation: ContinuationPolicy::UntilTerminal,
                    limits: StepLimits::default(),
                },
                None,
            )
            .await?;
        let tools = executor.request_tool_definitions();
        let request = objective.unwrap_or_default();
        let procedure_request = request.to_string();
        let control = executor.measurement_control_context(request, &procedure_request, continuing);
        let measure = |messages: &[Message]| {
            leveler_model::RequestProjection::project_with_control_context(
                messages,
                &tools,
                policy.reasoning_replay,
                policy.reasoning_retention,
                &control,
                executor.route_identity().as_ref(),
            )
            .estimated_tokens()
        };
        // The retained tail is sized from the same projected accounting the
        // threshold uses, so a pre-request fold also lands at low water instead
        // of back on the threshold.
        let (tail_budget, _non_foldable) = policy
            .context_policy
            .retention_budget_from_projection(&raw.messages, measure);
        let keep_recent_tokens = tail_budget.unwrap_or(1).max(1);
        let context = raw
            .assemble_measured(
                log,
                Some(summarizer),
                objective,
                if policy.context_policy.folding_enabled() {
                    u64::from(policy.context_policy.pressure_threshold)
                } else {
                    u64::MAX
                },
                policy.context_policy.hard_capacity(),
                policy.context_policy.retention.keep_recent_messages,
                keep_recent_tokens,
                &measure,
            )
            .await?;
        if context.compacted {
            log.append(None, context.snapshot_event(), observer).await?;
        }
        let compacted = context.compacted;
        let prior = leveler_context::compact_messages(
            &context.prior,
            usize::MAX,
            u64::MAX,
            None,
            objective,
        );
        if compacted
            || checkpoint_scope.is_some()
            || prior.iter().any(|message| {
                message
                    .text_content()
                    .contains(leveler_model::COMPACTION_BREADCRUMB_MARKER)
            })
        {
            let goal_id = checkpoint_scope.map(|scope| &scope.goal_id);
            return self
                .with_compaction_facts(prior, session_id, goal_id, log, objective)
                .await;
        }
        Ok(prior)
    }

    #[allow(clippy::too_many_arguments)]
    async fn checkpointed_prior(
        &self,
        log: &EventLog<'_>,
        session_id: &SessionId,
        raw: &leveler_engine::RawTranscript,
        objective: Option<&str>,
        continuing: bool,
        workspace: Option<&dyn crate::coding::checkpoint::WorkspaceFacts>,
        summarizer: &dyn leveler_engine::ContextSummarizer,
        _cancellation: &CancellationToken,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
        scope: &crate::coding::checkpoint::GoalCheckpointScope,
    ) -> Result<Option<Vec<leveler_model::Message>>, EngineError> {
        let policy = self.resolved_context_policy().await?;
        let threshold = self.context_threshold().await?;
        let executor = self
            .factory
            .build(
                TurnProfile::Chat {
                    continuation: ContinuationPolicy::UntilTerminal,
                    limits: StepLimits::default(),
                },
                None,
            )
            .await?;
        let tools = executor.request_tool_definitions();
        // The SAME projection `assembled_prior` measures: control context, tool
        // schemas and history. A checkpoint prior is only worth returning when
        // the request built from it is legal, and legality is a property of the
        // real request, not of the transcript alone.
        let request = objective.unwrap_or_default();
        let control = executor.measurement_control_context(request, request, continuing);
        let measure = |messages: &[Message]| {
            leveler_model::RequestProjection::project_with_control_context(
                messages,
                &tools,
                policy.reasoning_replay,
                policy.reasoning_retention,
                &control,
                executor.route_identity().as_ref(),
            )
            .estimated_tokens()
        };
        let Some(task) = self
            .engine
            .stores
            .tasks
            .task_for_session(session_id)
            .await?
        else {
            return Ok(None);
        };
        if self.engine.stores.goals.for_task(&task).await?.is_empty() {
            return Ok(None);
        }
        if let Some(prior) = crate::coding::checkpoint::resume_prior_from_checkpoint(
            &self.engine.stores,
            session_id,
            scope,
            raw,
        )
        .await?
            && measure(&prior) <= threshold
        {
            return Ok(Some(prior));
        }
        if measure(&raw.messages) <= threshold {
            return Ok(None);
        }
        // The verbatim tail is bounded by BOTH the round count and a token
        // budget sized from the same projected accounting the fold uses — and
        // the cut comes from the ONE span owner. A count-only tail can be many
        // times the intended budget when the newest rounds are large tool
        // results, so the checkpoint prior itself becomes an unsendable
        // request.
        let keep_recent_messages = policy.context_policy.retention.keep_recent_messages;
        let (tail_budget, _) = policy
            .context_policy
            .retention_budget_from_projection(&raw.messages, measure);
        let keep_recent_tokens = tail_budget.unwrap_or(1).max(1);
        let Some((_, tail_start)) = leveler_context::compaction_span(
            &raw.messages,
            keep_recent_messages,
            keep_recent_tokens,
        ) else {
            return Ok(None);
        };
        let Some(summary) = summarizer
            .summarize(&raw.messages, keep_recent_messages, keep_recent_tokens)
            .await?
        else {
            return Ok(None);
        };
        match crate::coding::checkpoint::create_goal_checkpoint(
            &self.engine,
            session_id,
            scope,
            leveler_lifecycle::CheckpointReason::ContextCompaction,
            workspace,
            crate::coding::checkpoint::SemanticRecap::briefing(Some(&summary)),
        )
        .await
        {
            Ok(Some(record)) => {
                log.append(
                    None,
                    crate::coding::checkpoint::checkpoint_created_event(&record),
                    observer,
                )
                .await?;
                let mut prior = Vec::with_capacity(1 + raw.messages.len() - tail_start);
                prior.push(leveler_model::Message::user(
                    record.payload.context_block(),
                    TranscriptOrigin::RuntimeNotice {
                        notice: RuntimeNoticeKind::GoalCheckpoint,
                    },
                ));
                prior.extend_from_slice(&raw.messages[tail_start..]);
                // Fail closed: the checkpoint prior must fit what the caller can
                // send. A prior that does not is not a context to hand to the
                // model; the caller falls back to the fold path, which re-measures
                // the request and refuses an impossible one explicitly.
                let projected = measure(&prior);
                if projected > threshold {
                    tracing::warn!(
                        operation = "goal_checkpoint",
                        projected_tokens = projected,
                        quality_threshold = threshold,
                        "goal checkpoint prior still exceeds the context budget; using the fold path"
                    );
                    return Ok(None);
                }
                Ok(Some(prior))
            }
            Ok(None) => Ok(None),
            Err(error) => {
                tracing::warn!(
                    %error,
                    "context-compaction checkpoint failed; using the pre-checkpoint fold"
                );
                Ok(None)
            }
        }
    }

    /// Load the session's prior Coding domain state and answer whether it is
    /// still open.
    ///
    /// The engine loads the same durable rows again when it actually seeds; the
    /// decision is the harness's, because forming it requires reading Coding
    /// semantics — which is exactly what F9.1 moved out of the engine.
    async fn prior_epoch_open(
        &self,
        session_id: &SessionId,
        continuing_goal: bool,
    ) -> Result<bool, EngineError> {
        let events = self.engine.stores.events.as_ref();
        // An answered Chat can leave its last progress phase active. The
        // durable task terminal closes that epoch for new work, including
        // sessions written before this check. An older terminal must not
        // close a newer window that crashed before it could publish its own.
        if !continuing_goal
            && let Some(terminal) = events
                .load_last_by_type(session_id, "task_finished", None)
                .await?
            && matches!(
                EngineEvent::from_payload(&terminal.payload)?,
                EngineEvent::TaskFinished {
                    outcome: TaskOutcome::Completed,
                    ..
                }
            )
            && events
                .load_last_by_type(session_id, "turn_started", None)
                .await?
                .is_none_or(|start| start.sequence < terminal.sequence)
        {
            return Ok(false);
        }
        let progress = crate::coding::turn::last_persisted_progress(events, session_id).await?;
        let plan = crate::coding::turn::last_persisted_plan(events, session_id).await?;
        Ok(prior_epoch_open(plan.as_ref(), progress.as_ref()))
    }

    /// Who speaks for a child this session lost. The engine settles the ghost
    /// either way; this is what lets the terminal also say what the child
    /// contributed (§F9.3).
    fn lost_child_voice(&self, session_id: &SessionId) -> Arc<dyn leveler_engine::LostChildVoice> {
        Arc::new(crate::coding::turn::CodingLostChildVoice {
            events: self.engine.stores.events.clone(),
            session_id: session_id.clone(),
        })
    }

    /// Create the session a Coding task runs in.
    ///
    /// The harness is what knows a Coding task has a repository, a permission
    /// mode and a sandbox flag; it hands the engine those already resolved to
    /// the durable values it persists.
    pub async fn create_task(&self, spec: &TaskSpec) -> Result<SessionId, EngineError> {
        self.engine
            .create_task(&leveler_engine::NewSession {
                workspace: spec
                    .coding
                    .repository
                    .as_ref()
                    .map(|root| root.display().to_string()),
                goal: spec.runtime.goal.clone(),
                model: Some(self.factory.model.to_string()),
                mode: mode_str(spec.coding.mode).to_string(),
                sandbox: spec.coding.sandbox,
                kind: spec.runtime.kind,
                axes: Some(leveler_engine::NewSessionAxes {
                    collaboration: "goal".to_string(),
                }),
                model_pinned: true,
            })
            .await
    }

    /// Attach mid-turn user input for this engine's runs.
    ///
    /// Set by the caller that knows which session is running, since the factory
    /// is built before that is decided.
    pub fn with_steering(mut self, source: Option<Arc<dyn crate::SteeringSource>>) -> Self {
        self.factory.steering = source;
        self
    }

    /// Attach the explicit-task-cancellation flag this turn runs under.
    ///
    /// The flag is created with the turn's admission slot and set by the
    /// runtime when the user cancels the logical task rather than merely
    /// interrupting the turn. It is read once, at terminal time.
    pub fn with_task_cancel(mut self, flag: Arc<AtomicBool>) -> Self {
        self.task_cancel = flag;
        self
    }

    /// Shared terminal handling for run/chat/resume: derive the lifecycle
    /// columns from the result and commit them with the TaskFinished event.
    async fn finish_from_result(
        &self,
        token: &leveler_core::OwnershipToken,
        session_id: &SessionId,
        result: &Result<TaskReport, EngineError>,
        goal: Option<&leveler_core::GoalId>,
        repo: Option<&std::path::Path>,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<(), EngineError> {
        // A stale runtime has no authority to write a terminal fact - not even
        // Failed. Abort silently here; the current owner decides the task's
        // future. (The fenced store would reject the write anyway; skipping
        // avoids a second, noisier failure.)
        if matches!(
            result,
            Err(EngineError::Ownership(
                leveler_storage::OwnershipError::Stale { .. }
            )) | Err(EngineError::OwnershipConflict { .. })
                | Err(EngineError::OwnedByLiveBoot { .. })
                | Err(EngineError::OwnershipUnknown { .. })
                | Err(EngineError::StaleOwnership(_))
        ) {
            return Ok(());
        }
        if matches!(result, Err(EngineError::UnclosedTerminalBoundary(_))) {
            // A task terminal is a closed evidence boundary. If a child that
            // can affect completion has no durable terminal, publishing even
            // a Failed task would put the parent behind an open activation.
            return Ok(());
        }
        let interrupted = matches!(result, Err(EngineError::Cancelled));
        // R007 F3: a WORK-WINDOW boundary is not a goal terminal. When the
        // round/step budget runs out the session stays resumable
        // (`AgentState::Execute`), and the goal's services must outlive the
        // window — R007 hit the ceiling twice and spent each next window
        // rebuilding the dev server this reap had just killed. A genuine goal
        // terminal still reaps, so R6-P4 is unaffected.
        let mut goal_continues = matches!(&result, Ok(report)
            if terminal_status_for(report).1 == AgentState::Execute);
        let log = EventLog::new_owned(
            self.engine.stores.events.as_ref(),
            session_id.clone(),
            token.clone(),
        );
        let resolution_phase =
            begin_finalization_phase(&log, FINALIZATION_RESOLVING_OUTCOME, observer).await;
        let mut goal_update = goal.map(|goal_id| leveler_storage::GoalTerminalUpdate {
            goal_id: goal_id.clone(),
            windows_delta: result.as_ref().map(|report| report.windows).unwrap_or(1),
            // Decided from the FINAL terminal below. The draft of an outcome is
            // not evidence: a cancel observed while finalizing, or a
            // continuation checkpoint that could not be cut, revises it.
            settle: false,
            // Decided with the terminal below, once the outcome is final.
            ends_session_goal: false,
        });
        // Whether this session routes ordinary messages to the Goal profile.
        // Read at terminal time: a `/goal` accepted mid-turn is the user's
        // current choice, and this is the commit that may end it.
        let goal_session = self
            .engine
            .stores
            .sessions
            .collaboration(session_id)
            .await?
            .as_deref()
            == Some(leveler_lifecycle::CollaborationMode::Goal.as_str());
        // A cooperative stop is `interrupted` (resumable) by default. When the
        // user explicitly cancelled the LOGICAL task, the same stop is
        // `cancelled`: terminal, so no later `继续` can reopen the work.
        let explicit_task_cancel = interrupted && self.task_cancel.load(Ordering::SeqCst);
        // `/goal cancel` ends the LOGICAL task, not merely the window that
        // happens to be running. A Chat window continuing owed work may not
        // have bound that goal when the cancel lands, so the terminal would
        // carry no goal and leave a `running` row behind a cancelled task no
        // continuation can re-enter. The task's running goal is the work the
        // user is ending: settle it in this same terminal transaction, exactly
        // as the idle cancel path does.
        if explicit_task_cancel && goal_update.is_none() {
            goal_update = self
                .engine
                .stores
                .goals
                .for_task(&token.task_id)
                .await?
                .into_iter()
                .find(|goal| goal.state == leveler_storage::GoalState::Running)
                .map(|goal| leveler_storage::GoalTerminalUpdate {
                    goal_id: goal.id,
                    // Cancelling is not a work window.
                    windows_delta: 0,
                    settle: true,
                    // A cancelled task can never continue, so it also ends the
                    // session's Goal route when that route still belongs to it.
                    ends_session_goal: true,
                });
        }
        // Is this terminal a DECISION about the work (the harness finished with
        // a verdict, or the user ended it), or a fault that decided nothing?
        // Only a decision can close a goal whose work was never delivered.
        let decided = match &result {
            Ok(_) => true,
            Err(EngineError::Cancelled) => explicit_task_cancel,
            Err(_) => false,
        };
        let mut terminal = match result {
            Ok(report) => {
                let (status, state) = terminal_status_for(report);
                leveler_engine::TaskTerminal {
                    outcome: report.outcome,
                    reason: task_terminal_reason(report),
                    failure: None,
                    stop: Some(task_terminal_stop(report)),
                    status,
                    state,
                    goal: goal_update,
                    warnings: report.completion_warnings.clone(),
                    verification: report.verification.clone(),
                }
            }
            Err(EngineError::Cancelled) if explicit_task_cancel => leveler_engine::TaskTerminal {
                outcome: TaskOutcome::Cancelled,
                reason: Some("user_cancelled_task".to_string()),
                failure: None,
                stop: None,
                status: SessionStatus::Cancelled,
                state: AgentState::Cancelled,
                goal: goal_update,
                warnings: Vec::new(),
                verification: None,
            },
            Err(EngineError::Cancelled) => leveler_engine::TaskTerminal {
                outcome: TaskOutcome::Interrupted,
                reason: None,
                failure: None,
                stop: None,
                status: SessionStatus::Interrupted,
                state: AgentState::Execute,
                goal: goal_update,
                warnings: Vec::new(),
                verification: None,
            },
            Err(error) => leveler_engine::TaskTerminal {
                outcome: TaskOutcome::Failed,
                reason: Some(error.to_string()),
                // The structured provider failure, when that is what ended the
                // turn. Presentation reads this instead of parsing `reason`.
                failure: match &error {
                    EngineError::Execution { model, .. } => model.clone(),
                    _ => None,
                },
                stop: None,
                status: SessionStatus::Failed,
                state: AgentState::Failed,
                goal: goal_update,
                warnings: Vec::new(),
                verification: None,
            },
        };
        // A continuing goal's milestone is part of the authoritative window
        // boundary. Persist it before TaskFinished releases admission, so its
        // cursor and workspace facts cannot absorb the next turn.
        let mut checkpoint_failure = None;
        if goal_continues && goal.is_some() {
            let checkpoint_phase =
                begin_finalization_phase(&log, FINALIZATION_CONTINUATION_CHECKPOINT, observer)
                    .await;
            let checkpoint = match goal {
                Some(goal_id) => match crate::coding::checkpoint::latest_goal_checkpoint_scope(
                    &self.engine.stores,
                    session_id,
                    goal_id,
                )
                .await
                {
                    Ok(Some(scope)) => {
                        crate::coding::checkpoint::create_goal_checkpoint(
                            &self.engine,
                            session_id,
                            &scope,
                            leveler_lifecycle::CheckpointReason::Milestone,
                            repo.map(GitWorkspace::new)
                                .as_ref()
                                .map(|w| w as &dyn crate::coding::checkpoint::WorkspaceFacts),
                            None,
                        )
                        .await
                    }
                    Ok(None) => Ok(None),
                    Err(error) => Err(error),
                },
                None => Ok(None),
            };
            match checkpoint {
                Ok(Some(record)) => {
                    let event = crate::coding::checkpoint::checkpoint_created_event(&record);
                    if let Err(error) = log.append(None, event, observer).await {
                        tracing::warn!(
                            %error,
                            "milestone checkpoint persisted but its announcement failed"
                        );
                    }
                }
                Ok(None) => {
                    checkpoint_failure = Some(EngineError::Config(
                        "continuing goal has no checkpointable running goal".to_string(),
                    ));
                }
                Err(error) => checkpoint_failure = Some(error),
            }
            if let Some(started) = checkpoint_phase {
                finish_finalization_phase(FINALIZATION_CONTINUATION_CHECKPOINT, started);
            }
            if let Some(error) = checkpoint_failure.as_ref() {
                // A checkpoint is a derived projection. Its failure does not
                // erase the committed transcript or settle the user's goal.
                // Pause explicitly; resume can rebuild from canonical history.
                terminal.outcome = TaskOutcome::Interrupted;
                terminal.reason = Some(format!("continuation checkpoint failed: {error}"));
                // A checkpoint failure is not a provider failure: do not carry
                // a stale provider error beside a different reason.
                terminal.failure = None;
                terminal.status = SessionStatus::Interrupted;
                terminal.state = AgentState::Execute;
                terminal.warnings.clear();
                goal_continues = true;
            }
        }
        if let Some(started) = resolution_phase {
            finish_finalization_phase(FINALIZATION_RESOLVING_OUTCOME, started);
        }
        // Detach the cleanup target set before publishing terminal. A new turn
        // may be admitted as soon as TaskFinished is projected; a later
        // session-wide lookup could otherwise kill processes owned by that new
        // turn.
        //
        // The target SET is detached from the pre-terminal scope (so a turn
        // admitted right after the terminal cannot be included), but whether
        // it is settled is decided below from the goal's own final state: a
        // goal that still owes work keeps its services, and a settled goal has
        // no window that could still use them.
        let mut cleanup_ticket = if explicit_task_cancel || !interrupted {
            match self.factory.tool_context.session_scope.as_deref() {
                Some(scope) => Some(
                    self.factory
                        .background_tasks
                        .detach_cleanup(scope, goal)
                        .await,
                ),
                None => None,
            }
        } else {
            None
        };
        let publishing_phase =
            begin_finalization_phase(&log, FINALIZATION_PUBLISHING_TERMINAL, observer).await;
        // No await may appear between this decision and entering finish_task:
        // cancellation owns the outcome throughout Finalizing, right up to
        // the canonical terminal commit's invocation.
        let mut cancelled_at_commit = cancellation.is_cancelled();
        // The generic cancellation guard rewrites a finalizing window to
        // resumable `interrupted`. An explicit task cancel is a stronger,
        // user-declared outcome and keeps its terminal.
        if cancelled_at_commit && !explicit_task_cancel {
            terminal.outcome = TaskOutcome::Interrupted;
            terminal.reason = None;
            terminal.stop = None;
            terminal.status = SessionStatus::Interrupted;
            terminal.state = AgentState::Execute;
            terminal.warnings.clear();
            // Interrupted work retains its background processes. The ticket is
            // only a detached target set and has performed no side effect yet.
            cleanup_ticket = None;
        }
        // Test-only exact crash window: a cancel has been observed and the
        // terminal is decided, but nothing is durable yet. The default build
        // compiles the barrier body away; the call itself is a no-op.
        if interrupted {
            crate::test_barrier::hit_before_cancel_terminal_persist();
        }
        // ONE rule decides whether the goal still owes work: the terminal's own
        // resumability. It is derived from the terminal actually being
        // committed, after every revision above, so no path can leave a goal
        // settled that a `继续` could still re-enter (or owed when nothing can
        // follow it up).
        settle_goal_from_terminal(&mut terminal, decided);
        end_session_goal_route(&mut terminal, goal_session);
        // The goal's own settle decision is the authority on whether any window
        // can still follow, so it is read from the FINAL terminal above and
        // used by the post-terminal cleanup gate below.
        // A Chat turn can end while its session still owns observable work.
        // With no goal row there is no goal that could still owe work, so the
        // turn's own terminal is the boundary a `goal`-lifetime task was
        // promised: settle it unless the window continues. The session's
        // collaboration axis is not that authority — a plain Chat session owns
        // no goal row at all, and reading the decision through the axis left
        // its services running past the turn.
        let mut reap_now = explicit_task_cancel
            || (!interrupted
                && terminal
                    .goal
                    .as_ref()
                    .map_or(!goal_continues, |goal| goal.settle));
        let mut settled = self
            .engine
            .finish_task(token, session_id, terminal.clone(), observer)
            .await;
        if matches!(settled, Err(EngineError::TaskCancellationRequested)) {
            // Storage arbitrated the race. Coding owns the revised goal and
            // cleanup disposition; Engine never derives these Harness facts.
            terminal.outcome = TaskOutcome::Cancelled;
            terminal.reason = Some("user_cancelled_task".to_string());
            terminal.failure = None;
            terminal.stop = None;
            terminal.status = SessionStatus::Cancelled;
            terminal.state = AgentState::Cancelled;
            terminal.warnings.clear();
            cancelled_at_commit = true;
            if cleanup_ticket.is_none() {
                cleanup_ticket = match self.factory.tool_context.session_scope.as_deref() {
                    Some(scope) => Some(
                        self.factory
                            .background_tasks
                            .detach_cleanup(scope, goal)
                            .await,
                    ),
                    None => None,
                };
            }
            // Storage arbitrated the race with the user's own cancel: that is
            // a decision, not a fault.
            settle_goal_from_terminal(&mut terminal, true);
            end_session_goal_route(&mut terminal, goal_session);
            reap_now = true;
            settled = self
                .engine
                .finish_task(token, session_id, terminal, observer)
                .await;
        }
        if settled.is_ok() {
            leveler_core::test_crash::point("after_task_terminal");
        }
        if let Some(started) = publishing_phase {
            tracing::debug!(
                session = %session_id,
                elapsed_ms = started.elapsed().as_millis(),
                "terminal publish finished"
            );
        }

        // Process cleanup is deliberately post-terminal. It cannot change the
        // TaskReport and the TaskFinished observer has already made the durable
        // outcome visible. Interrupted and continuing work windows retain their
        // processes exactly as before.
        //
        // The goal's final settle decision owns its services' lifetime. A
        // recoverable provider failure leaves the goal open even though no
        // TaskReport exists; a settled goal has no window that could use them.
        if settled.is_ok()
            && reap_now
            && let Some(ticket) = cleanup_ticket
            && !ticket.is_empty()
        {
            let cleanup_started = std::time::Instant::now();
            let reaped = ticket.settle().await;
            if reaped > 0 {
                tracing::info!(
                    session = %session_id,
                    "post-terminal cleanup reaped {reaped} session-owned background task(s)"
                );
            }
            tracing::debug!(
                session = %session_id,
                elapsed_ms = cleanup_started.elapsed().as_millis(),
                "post-terminal cleanup finished"
            );
        }
        settled?;
        if cancelled_at_commit {
            return Err(EngineError::Cancelled);
        }
        if let Some(error) = checkpoint_failure {
            return Err(error);
        }
        Ok(())
    }

    /// Run the task to a terminal outcome. Every turn, tool call, approval and
    /// verification result is persisted before observers see it.
    pub async fn run(
        &self,
        session_id: &SessionId,
        spec: &TaskSpec,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
        cancellation: CancellationToken,
    ) -> Result<TaskReport, EngineError> {
        self.run_with_content(
            session_id,
            spec,
            vec![ContentPart::Text {
                text: spec.runtime.goal.clone(),
            }],
            observer,
            cancellation,
        )
        .await
    }

    /// Run a goal whose initiating message includes attachments. Content changes
    /// the input representation, never the Goal profile or lifecycle owner.
    pub async fn run_with_content(
        &self,
        session_id: &SessionId,
        spec: &TaskSpec,
        content: Vec<ContentPart>,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
        cancellation: CancellationToken,
    ) -> Result<TaskReport, EngineError> {
        self.run_task(session_id, spec, content, observer, cancellation)
            .await
    }

    async fn run_task(
        &self,
        session_id: &SessionId,
        spec: &TaskSpec,
        content: Vec<ContentPart>,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
        cancellation: CancellationToken,
    ) -> Result<TaskReport, EngineError> {
        let token = self
            .engine
            .start_task(
                session_id,
                AgentState::Execute,
                &leveler_engine::TaskExecution {
                    mode: mode_str(spec.coding.mode).to_string(),
                    sandbox: spec.coding.sandbox,
                    kind: spec.runtime.kind,
                },
            )
            .await?;
        let (goal, continued_running) = match self
            .open_goal_for_turn(&token, session_id, &spec.runtime.goal)
            .await
        {
            Ok(goal) => goal,
            Err(error) => {
                let result = Err(error);
                self.finish_from_result(
                    &token,
                    session_id,
                    &result,
                    None,
                    spec.coding.repository.as_deref(),
                    observer,
                    &cancellation,
                )
                .await?;
                return result;
            }
        };
        let log = EventLog::new_owned(
            self.engine.stores.events.as_ref(),
            session_id.clone(),
            token.clone(),
        );
        let runner = TurnRunner {
            stores: &self.engine.stores,
            token: token.clone(),
            session_id: session_id.clone(),
            log: &log,
            approver: self.approver.clone(),
            clarifier: self.clarifier.clone(),
            lost_child_voice: Some(self.lost_child_voice(session_id)),
        };
        // The goal RECORD is the objective this turn serves. It equals the
        // turn's text on an ordinary `/goal <task>`; after recovering a lost
        // declaration it is the recovered text and the user's current message
        // is only the instruction.
        let goal_objective = self
            .engine
            .stores
            .goals
            .get(&goal)
            .await?
            .map(|record| record.objective)
            .unwrap_or_else(|| spec.runtime.goal.clone());
        let declaration = match self.turn_goal_declaration(session_id, &goal).await {
            Ok(declaration) => declaration,
            Err(error) => {
                let result = Err(error);
                self.finish_from_result(
                    &token,
                    session_id,
                    &result,
                    None,
                    spec.coding.repository.as_deref(),
                    observer,
                    &cancellation,
                )
                .await?;
                return result;
            }
        };
        log.append(
            None,
            EngineEvent::TaskStarted {
                goal: goal_objective.clone(),
                model: self.factory.model.to_string(),
                mode: mode_str(spec.coding.mode).to_string(),
                sandbox: spec.coding.sandbox,
                kind: spec.runtime.kind,
                task_id: Some(token.task_id.clone()),
            },
            observer,
        )
        .await?;

        // Orchestrate execution path removed; legacy kind falls through to direct.
        let terminal_cancellation = cancellation.clone();
        let result = match spec.runtime.kind {
            ExecutionKind::Direct => {
                self.run_direct(
                    &log,
                    &runner,
                    spec,
                    &goal,
                    continued_running,
                    content,
                    declaration,
                    observer,
                    cancellation,
                )
                .await
            }
            ExecutionKind::Parallel => Err(EngineError::Config(
                "the parallel strategy lands in B9".to_string(),
            )),
        };
        let result = if terminal_cancellation.is_cancelled() {
            Err(EngineError::Cancelled)
        } else {
            result
        };

        // Stamp the terminal outcome (interrupted on cancellation) and emit
        // TaskFinished before returning.
        self.finish_from_result(
            &token,
            session_id,
            &result,
            Some(&goal),
            spec.coding.repository.as_deref(),
            observer,
            &terminal_cancellation,
        )
        .await?;
        result
    }

    /// Run one conversational turn (multimodal content) in an existing
    /// session, carrying the prior transcript. Unlike resume, a finished
    /// session may keep chatting — the outcome column tracks the latest turn.
    pub async fn chat(
        &self,
        session_id: &SessionId,
        spec: &TaskSpec,
        content: Vec<leveler_model::ContentPart>,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
        cancellation: CancellationToken,
    ) -> Result<TaskReport, EngineError> {
        self.chat_with_origin(
            session_id,
            spec,
            content,
            TranscriptOrigin::UserInput,
            observer,
            cancellation,
        )
        .await
    }

    /// [`Self::chat`] with an explicit transcript origin.
    ///
    /// The origin is durable provenance: it says whether the row was submitted
    /// by the person or written by the harness. A caller that replays imported
    /// context must use this form so the model and every later reader see the
    /// same authority the writer intended.
    pub async fn chat_with_origin(
        &self,
        session_id: &SessionId,
        spec: &TaskSpec,
        content: Vec<leveler_model::ContentPart>,
        origin: TranscriptOrigin,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
        cancellation: CancellationToken,
    ) -> Result<TaskReport, EngineError> {
        // A Chat turn keeps the session's activated packs: the enable answered
        // "tools available on the following request", and dropping it here is
        // what made the next turn's catalog contradict the tool list the model
        // had just been given. Nothing goal-scoped is loaded: a Chat turn owns
        // no goal at start.
        // A chat turn tolerates the odd unreadable legacy row (it only loses
        // context), unlike resume which must reconstruct exactly. The load
        // happens after crash reconciliation so reconstructed results are in
        // the context of this very turn.
        let token = self
            .engine
            .start_task(
                session_id,
                AgentState::Execute,
                &leveler_engine::TaskExecution {
                    mode: mode_str(spec.coding.mode).to_string(),
                    sandbox: spec.coding.sandbox,
                    kind: spec.runtime.kind,
                },
            )
            .await?;
        let log = EventLog::new_owned(
            self.engine.stores.events.as_ref(),
            session_id.clone(),
            token.clone(),
        );
        // A finished tool whose transcript row was lost is reconstructed from
        // the durable event before the context is loaded; a transcript result
        // whose finished event was lost closes the event plane.
        self.reconcile_event_transcript(&token, session_id, &log, observer)
            .await?;
        // Reconcile the crash window before continuing — the interactive path
        // is how a crashed session normally gets reopened (TUI/Web), and a
        // dangling call means the workspace may already carry a side effect
        // the user has not seen. Nothing is replayed: every call the runtime
        // cannot close as a pure observation stops for explicit
        // acknowledgement.
        self.recover_crash_window(&log, observer, &cancellation)
            .await?;
        let raw = self.load_request_transcript(session_id, None, None).await?;
        // Long-goal P3: over the fold threshold the continuation runs from a
        // durable checkpoint (fresh existing one, or one cut right here)
        // instead of replayed old history. Under the threshold — or with no
        // goal — the pre-checkpoint path stands unchanged.
        let objective_hint = content
            .iter()
            .filter_map(|p| match p {
                leveler_model::ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .next()
            .map(str::to_string);
        // A Chat turn can add work to the one goal this task still owes. The
        // user's own words become scope only when the model declares an
        // amendment through `declare_intent`; binding and requirement changes
        // remain explicit model declarations, so this turn stays a Chat turn.
        let running_goal = self.single_running_goal(&token.task_id).await?;
        let runner = TurnRunner {
            stores: &self.engine.stores,
            token: token.clone(),
            session_id: session_id.clone(),
            log: &log,
            approver: self.approver.clone(),
            clarifier: self.clarifier.clone(),
            lost_child_voice: Some(self.lost_child_voice(session_id)),
        };
        let prior_epoch_open = self
            .prior_epoch_open(session_id, running_goal.is_some())
            .await?;
        let initiating_message =
            Message::from_parts(Role::User, content.clone(), Some(origin.clone()));
        let terminal_cancellation = cancellation.clone();
        // The pre-request context fold appends its `ContextSnapshot` (or
        // checkpoint) fact to the log from inside the turn, where the engine's
        // pump owns the caller's observer. Capture those facts and forward them
        // to the caller after the turn: they are already durable, and the
        // caller must still learn that its history was folded.
        let pre_turn_events: Arc<std::sync::Mutex<Vec<EngineEvent>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = Arc::clone(&pre_turn_events);
        let concluded = async {
            let recorded = runner
                .run_turn(
                    TurnKind::Chat,
                    leveler_engine::TurnStart::Anchored {
                        message: initiating_message,
                        objective: leveler_lifecycle::ObjectiveAnchor::from_user_message(
                            content_objective_text(&content),
                        ),
                        goal_id: None,
                        // A Chat turn consumes no declaration at start; one
                        // that arrives mid-turn is bound by the executor.
                        declaration: None,
                    },
                    observer,
                    cancellation.clone(),
                    |ports| async {
                        let mut prior = self
                            .assembled_prior(
                                &log,
                                session_id,
                                raw,
                                objective_hint.as_deref(),
                                false,
                                None,
                                spec.coding
                                    .repository
                                    .as_deref()
                                    .map(GitWorkspace::new)
                                    .as_ref()
                                    .map(|workspace| {
                                        workspace as &dyn crate::coding::checkpoint::WorkspaceFacts
                                    }),
                                &self.context_summarizer(
                                    session_id,
                                    &ports.budget_scope,
                                    spec.runtime.limits,
                                    &cancellation,
                                ),
                                &cancellation,
                                &mut |event| captured.lock().unwrap().push(event),
                            )
                            .await
                            .map_err(crate::coding::turn::seed_failure)?;
                        // The interrupted call's result was never persisted;
                        // tell the model what the user acknowledged before it
                        // re-drives the request blind.
                        if let Some(note) = self
                            .crash_recovery_notice(&log)
                            .await
                            .map_err(crate::coding::turn::seed_failure)?
                        {
                            prior.push(note);
                        }
                        drive_turn(
                            &self.factory,
                            chat_profile(spec),
                            TurnInput::Content {
                                prior,
                                content,
                                origin,
                            },
                            prior_epoch_open,
                            session_id.clone(),
                            self.engine.stores.events.clone(),
                            Some(crate::coding::checkpoint::CodingCheckpointContext::new(
                                self.engine.clone(),
                                spec.coding.repository.as_deref().map(|root| {
                                    Arc::new(GitWorkspace::new(root))
                                        as Arc<dyn crate::coding::checkpoint::WorkspaceFacts>
                                }),
                            )),
                            ports,
                            cancellation.clone(),
                        )
                        .await
                    },
                )
                .await?;
            for event in std::mem::take(&mut *pre_turn_events.lock().unwrap()) {
                observer(event);
            }
            // A Chat turn upgraded by `/goal` names the goal it bound; the
            // terminal must settle that record, not write a goal-less end.
            // Work the conclusion launches on the harness's own account (the
            // required reviewer) is billed to that same goal.
            let bound_goal = recorded.outcome.bound_goal.clone();
            let review_budget_scope = bound_goal
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| recorded.turn_id.to_string());
            let concluded = self
                .conclude_direct(
                    &log,
                    &runner,
                    spec,
                    &review_budget_scope,
                    recorded.outcome,
                    self.factory.independent_review,
                    observer,
                    cancellation,
                )
                .await;
            Ok::<_, EngineError>((concluded, bound_goal))
        }
        .await;
        let (result, bound_goal) = match concluded {
            Ok(pair) => pair,
            Err(error) => {
                // The turn aborted before it could report its facts. The goal
                // it bound is still in the turn WAL, and a user cancellation
                // must settle that goal rather than leave it falsely owed.
                let goal = match self.latest_bound_goal(session_id).await {
                    Ok(goal) => goal,
                    Err(recovery) => {
                        tracing::warn!(
                            session = %session_id,
                            error = %recovery,
                            "could not recover the window's bound goal after an aborted turn"
                        );
                        None
                    }
                };
                (Err(error), goal)
            }
        };
        let result = if terminal_cancellation.is_cancelled() {
            Err(EngineError::Cancelled)
        } else {
            result
        };
        self.finish_from_result(
            &token,
            session_id,
            &result,
            bound_goal.as_ref(),
            spec.coding.repository.as_deref(),
            observer,
            &terminal_cancellation,
        )
        .await?;
        result
    }

    /// Resume an interrupted direct task from its persisted transcript. The
    /// caller builds `spec` FROM the persisted execution config (see
    /// `SessionRepository::execution`); this refuses a parallel parent session,
    /// a kind mismatch, and a session that already ended successfully.
    pub async fn resume(
        &self,
        session_id: &SessionId,
        spec: &TaskSpec,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
        cancellation: CancellationToken,
    ) -> Result<TaskReport, EngineError> {
        self.resume_inner(session_id, spec, None, observer, cancellation)
            .await
    }

    /// Continue the session's interrupted logical task with the user's
    /// continuation message (a bare `继续` or an amendment on top of it).
    ///
    /// Same resume path as [`Self::resume`]: the logical task, goal, plan,
    /// ledger and progress are the ones the session already owns. The
    /// instruction is recorded verbatim as this turn's user message and
    /// appended after the seeded transcript, so the original objective remains
    /// the anchor. Whether the words amend the goal's scope is declared by the
    /// model through `declare_scope`, never derived from their phrasing.
    pub async fn resume_with_instruction(
        &self,
        session_id: &SessionId,
        spec: &TaskSpec,
        instruction: Message,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
        cancellation: CancellationToken,
    ) -> Result<TaskReport, EngineError> {
        self.resume_inner(session_id, spec, Some(instruction), observer, cancellation)
            .await
    }

    /// Whether the session's latest durable terminal is a declaration that the
    /// goal is complete, rather than a turn that merely ended with an answer.
    ///
    /// The `sessions.outcome` column collapses both into `completed`, so it
    /// cannot answer this on its own. `StopReason::Answered` is the model
    /// stopping with text and no goal terminal — the shape a prematurely
    /// closed long task lands in — and it must stay resumable. A declared
    /// `completed` stop, or a legacy terminal with no stop at all, is a real
    /// declaration and stays terminal.
    async fn session_terminal_declares_completion(
        &self,
        session_id: &SessionId,
    ) -> Result<bool, EngineError> {
        let Some(row) = self
            .engine
            .stores
            .events
            .load_last_by_type(session_id, "task_finished", None)
            .await?
        else {
            return Ok(true);
        };
        let Ok(event) = leveler_engine::EngineEvent::from_payload(&row.payload) else {
            return Ok(true);
        };
        let leveler_engine::EngineEvent::TaskFinished { stop, .. } = event else {
            return Ok(true);
        };
        Ok(stop != Some(crate::StopReason::Answered))
    }

    /// The delivery a crash left between the transcript and the task terminal.
    ///
    /// [`Self::finish_from_result`] commits the task terminal last, so the
    /// drive's answer can be durable (a persisted [`EngineEvent::DeliveryCommitted`]
    /// and, depending on where the process died, its `TurnFinished`) while the
    /// task terminal is not. Replaying that turn would ask the model for a
    /// second answer; the recorded delivery is already the answer the user is
    /// owed.
    ///
    /// The cut is identified by durable facts only: the latest delivery has no
    /// task terminal after it, its stop is a terminal delivery, and that turn
    /// is still the newest user/chat turn of this very goal. The delivery
    /// event carries the turn facts, so recovery works whether the crash hit
    /// the turn terminal, a finalization phase, or only the task terminal: a
    /// finished turn closed cleanly, and a turn with no terminal must still be
    /// `running`. Anything else falls through to the ordinary resume.
    async fn uncommitted_delivery(
        &self,
        session_id: &SessionId,
        expected_goal: Option<&leveler_core::GoalId>,
        allow_committed: bool,
    ) -> Result<Option<UncommittedDelivery>, EngineError> {
        let rows = self.engine.stores.events.load(session_id).await?;
        let Some(at) = rows
            .iter()
            .rposition(|row| row.event_type == "delivery_committed")
        else {
            return Ok(None);
        };
        let delivery = &rows[at];
        // A task terminal after the delivery means the delivery was already
        // committed. Recovery normally has nothing left to do, but a client
        // that missed the final frame may still ask: `allow_committed` reads
        // the same delivery back for replay without appending anything.
        if !allow_committed
            && rows[at + 1..]
                .iter()
                .any(|row| row.event_type == "task_finished")
        {
            return Ok(None);
        }
        let EngineEvent::DeliveryCommitted {
            text,
            stop,
            model_steps,
            modified_files,
            transcript_ordinal,
        } = EngineEvent::from_payload(&delivery.payload)?
        else {
            return Ok(None);
        };
        // Goal deliveries end Completed or Blocked; an ordinary Chat answer is
        // Answered. Anything else is not a formal delivery and must not be
        // recovered as one (DEL-07).
        let stop_is_delivery = match expected_goal {
            Some(_) => matches!(stop, StopReason::Completed | StopReason::Blocked),
            None => matches!(stop, StopReason::Answered),
        };
        if text.trim().is_empty() || !stop_is_delivery {
            return Ok(None);
        }
        let Some(turn_id) = delivery.turn_id.as_deref() else {
            return Ok(None);
        };
        // The delivery is this goal's, and its turn is the newest user/chat
        // turn. A later turn means the session moved on; this is history.
        let turns = self
            .engine
            .stores
            .turns
            .list_for_session(session_id)
            .await?;
        let Some(newest) = turns
            .iter()
            .rev()
            .find(|turn| turn.kind == "user" || turn.kind == "chat")
        else {
            return Ok(None);
        };
        if newest.id != turn_id {
            return Ok(None);
        }
        let Some(payload) = newest.payload.as_deref() else {
            return Ok(None);
        };
        let Ok(lineage) = leveler_engine::decode_turn_continuation(payload) else {
            return Ok(None);
        };
        if lineage.goal_id.as_ref() != expected_goal {
            return Ok(None);
        }
        // Two crash cuts share this record: after the turn terminal (only the
        // task commit died) and inside it (the turn's own commit died). The
        // delivery event is written by the turn's event pump, so it is durable
        // in both; the turn row tells them apart.
        let turn_terminal = rows
            .iter()
            .filter(|row| {
                row.turn_id.as_deref() == Some(turn_id) && row.event_type == "turn_finished"
            })
            .map(|row| EngineEvent::from_payload(&row.payload))
            .next()
            .transpose()?;
        let turn_open = match (turn_terminal, newest.status.as_str()) {
            (None, "running") => true,
            (
                Some(EngineEvent::TurnFinished {
                    outcome: leveler_lifecycle::TurnOutcome::Completed,
                    ..
                }),
                "completed",
            ) => {
                // Finalization runs after the turn: review, outcome
                // resolution, then the terminal publish. A crash anywhere in
                // that window leaves the same durable delivery and no task
                // terminal, so recovery owns every remaining phase — including
                // a review the crash interrupted.
                false
            }
            _ => return Ok(None),
        };
        // The closure review's durable breadcrumbs decide whether recovery
        // still owes it. `launching` without a settle means the crash hit
        // inside the review; a settled action means it must not run again.
        let mut review = DeliveryReviewState::Absent;
        let mut last_reviewer: Option<usize> = None;
        for row in &rows[at + 1..] {
            match EngineEvent::from_payload(&row.payload) {
                Ok(EngineEvent::SubAgentFinished {
                    nickname,
                    contribution,
                    ..
                }) if nickname == "reviewer" => {
                    last_reviewer = contribution.map(|value| value.findings_total as usize);
                }
                Ok(EngineEvent::ReviewStage { action, .. }) => match action.as_str() {
                    "launching" => review = DeliveryReviewState::Unsettled,
                    "not_required" => {
                        review = DeliveryReviewState::Settled(ClosureReview::NotRequired);
                    }
                    "finished_ok" => {
                        review = DeliveryReviewState::Settled(ClosureReview::Completed {
                            findings: last_reviewer.unwrap_or(0),
                        });
                    }
                    "finished_incomplete" => {
                        review = DeliveryReviewState::Settled(ClosureReview::Incomplete {
                            findings: last_reviewer.unwrap_or(0),
                        });
                    }
                    "launch_failed" => {
                        review = DeliveryReviewState::Settled(ClosureReview::LaunchFailed);
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        Ok(Some(UncommittedDelivery {
            turn_id: TurnId::new(turn_id),
            text,
            stop,
            model_steps,
            modified_files,
            transcript_ordinal,
            turn_open,
            review,
        }))
    }

    /// The delivery of the newest Chat turn when both terminals are already
    /// durable. A resume with no new instruction must not re-drive the model
    /// for an answer the harness already accepted; it replays that answer so
    /// a client that missed the final frame can still receive it.
    async fn committed_chat_delivery(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<UncommittedDelivery>, EngineError> {
        let Some(delivery) = self.uncommitted_delivery(session_id, None, true).await? else {
            return Ok(None);
        };
        if delivery.turn_open {
            return Ok(None);
        }
        let rows = self.engine.stores.events.load(session_id).await?;
        let Some(at) = rows
            .iter()
            .rposition(|row| row.event_type == "delivery_committed")
        else {
            return Ok(None);
        };
        if !rows[at + 1..]
            .iter()
            .any(|row| row.event_type == "task_finished")
        {
            return Ok(None);
        }
        Ok(Some(delivery))
    }

    /// Close the delivery turn a crash left open, before its task terminal.
    ///
    /// Mirrors the turn-end sequence in [`TurnRunner::run_turn`] exactly:
    /// children this turn started and never finished are settled first, then
    /// the `TurnFinished` row commits, and only then is the observer told.
    /// The delivery event recorded every fact that row carries, so no model
    /// call and no replayed tool work are involved.
    async fn close_recovered_turn(
        &self,
        token: &leveler_core::OwnershipToken,
        session_id: &SessionId,
        delivery: &UncommittedDelivery,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
    ) -> Result<(), EngineError> {
        let log = EventLog::new_owned(
            self.engine.stores.events.as_ref(),
            session_id.clone(),
            token.clone(),
        );
        let runner = TurnRunner {
            stores: &self.engine.stores,
            token: token.clone(),
            session_id: session_id.clone(),
            log: &log,
            approver: self.approver.clone(),
            clarifier: self.clarifier.clone(),
            lost_child_voice: Some(self.lost_child_voice(session_id)),
        };
        runner
            .reconcile_terminal_children(leveler_lifecycle::ChildStop::Lost, observer)
            .await?;
        // The crashed window already persisted its model calls. A recovered
        // terminal carries the same booked projection a normal one does; a
        // failed read leaves it absent rather than fabricating a zero.
        let usage = self
            .engine
            .stores
            .model_requests
            .load_for_turn(session_id, &delivery.turn_id)
            .await
            .ok()
            .map(|rows| leveler_engine::TurnUsage::from_records(&rows));
        let event = EngineEvent::TurnFinished {
            turn_id: delivery.turn_id.clone(),
            outcome: leveler_lifecycle::TurnOutcome::Completed,
            stop_reason: format!("{:?}", delivery.stop),
            stop: Some(delivery.stop),
            model_steps: delivery.model_steps,
            modified_files: delivery.modified_files.clone(),
            usage,
        };
        let (event_type, payload) = event.to_row().map_err(|error| {
            EngineError::UnclosedTerminalBoundary(format!(
                "could not serialize the recovered turn terminal: {error}"
            ))
        })?;
        self.engine
            .stores
            .terminal
            .finish_turn_owned(
                token,
                session_id,
                &delivery.turn_id,
                &event_type,
                &payload,
                leveler_lifecycle::TurnOutcome::Completed,
                leveler_core::now(),
            )
            .await
            .map_err(|error| {
                EngineError::UnclosedTerminalBoundary(format!(
                    "could not commit the recovered turn terminal: {error}"
                ))
            })?;
        observer(event);
        Ok(())
    }

    /// Settle the closure review a crash interrupted, without re-driving the
    /// parent model.
    ///
    /// The delivery event already carries the answer and the modified files,
    /// so the reviewer can be launched from durable facts alone. A review that
    /// already settled is returned as recorded. A `launching` with no outcome
    /// is a live obligation from the process that died: it is finished even if
    /// the current configuration no longer asks for a review.
    async fn resume_closure_review(
        &self,
        token: &leveler_core::OwnershipToken,
        session_id: &SessionId,
        spec: &TaskSpec,
        budget_scope: &str,
        delivery: &UncommittedDelivery,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<ClosureReview, EngineError> {
        if let DeliveryReviewState::Settled(review) = delivery.review {
            return Ok(review);
        }
        let policy = match delivery.review {
            DeliveryReviewState::Unsettled => IndependentReviewPolicy::Required,
            _ => self.factory.independent_review,
        };
        let log = EventLog::new_owned(
            self.engine.stores.events.as_ref(),
            session_id.clone(),
            token.clone(),
        );
        let runner = TurnRunner {
            stores: &self.engine.stores,
            token: token.clone(),
            session_id: session_id.clone(),
            log: &log,
            approver: self.approver.clone(),
            clarifier: self.clarifier.clone(),
            lost_child_voice: Some(self.lost_child_voice(session_id)),
        };
        // The reviewer the crash interrupted has no terminal. Settle that
        // ghost before launching its replacement, so the task terminal is a
        // closed evidence boundary.
        runner
            .reconcile_terminal_children(leveler_lifecycle::ChildStop::Lost, observer)
            .await?;
        let review_phase = begin_finalization_phase(&log, FINALIZATION_REVIEW, observer).await;
        // The parent's wall clock is gone with the old process; the reviewer
        // starts from zero rather than charging the resume for time it did not
        // observe.
        let review = self
            .closure_review_stage(
                &log,
                &runner,
                spec,
                budget_scope,
                &delivery.modified_files,
                0,
                policy,
                observer,
                cancellation,
            )
            .await;
        if let Some(started) = review_phase {
            finish_finalization_phase(FINALIZATION_REVIEW, started);
        }
        review
    }

    async fn resume_inner(
        &self,
        session_id: &SessionId,
        spec: &TaskSpec,
        instruction: Option<Message>,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
        cancellation: CancellationToken,
    ) -> Result<TaskReport, EngineError> {
        let (_, _, kind, outcome) = self
            .engine
            .stores
            .sessions
            .execution(session_id)
            .await?
            .ok_or_else(|| EngineError::Config(format!("no session {session_id}")))?;
        // A parallel multi-agent parent has no direct Coding transcript or
        // continuation strategy. Refuse it by its kind rather than relying on
        // its transcript happening to be empty.
        if kind == ExecutionKind::Parallel.as_str() {
            return Err(EngineError::Config(format!(
                "session {session_id} is a parallel parent session and is not resumable"
            )));
        }
        if kind != spec.runtime.kind.as_str() {
            return Err(EngineError::Config(format!(
                "session {session_id} is `{kind}`, not `{}`",
                spec.runtime.kind.as_str()
            )));
        }
        if outcome == Some(TaskOutcome::Completed)
            && self
                .session_terminal_declares_completion(session_id)
                .await?
        {
            return Err(EngineError::Config(format!(
                "session {session_id} already completed ({}); start a new task instead",
                outcome.map(|o| o.as_str()).unwrap_or_default()
            )));
        }
        let mut lineage = self.resume_lineage(session_id, &spec.runtime.goal).await?;
        // A Chat turn can legitimately belong to a goal: `/goal <task>` while
        // the turn runs binds the goal to that turn. That identity — not the
        // text of `继续` — decides that this resume is a Goal resume. Without
        // it (or a declaration still waiting to be bound) the turn keeps its
        // recorded kind, so an ordinary chat continuation stays chat.
        let lineage_was_chat = matches!(lineage.kind, ResumeTurnKind::Chat);
        let pending = self.engine.stores.sessions.pending_goal(session_id).await?;
        let goal_axis = self
            .engine
            .stores
            .sessions
            .collaboration(session_id)
            .await?
            .as_deref()
            == Some(leveler_lifecycle::CollaborationMode::Goal.as_str());
        // A bound goal id is sufficient on its own: a Chat turn can commit an
        // explicitly delegated task (`commit_task`) without changing the
        // session's axis, and the goal it bound still owes the work. The axis
        // only decides the fallback for a declaration that has not been bound
        // yet.
        if lineage_was_chat && (lineage.goal_id.is_some() || (goal_axis && pending.is_some())) {
            lineage.kind = ResumeTurnKind::Goal;
        }
        // Any user message asks the running model to read what the person said;
        // whether the words continue the task, amend its scope or ask a
        // question is the model's declaration, never a comparison against a
        // fixed phrase. Only a resume that carries no message may replay a
        // durable delivery instead of driving the model.
        // A fully committed Chat delivery is replayed, not regenerated. This
        // is the D6 cut: the task terminal is durable but the client may not
        // have received the final frame. Taking a new ownership or starting a
        // turn here would both re-drive the model and make the answer a
        // transaction side effect.
        if instruction.is_none()
            && lineage.goal_id.is_none()
            && matches!(lineage.kind, ResumeTurnKind::Chat)
            && let Some(delivery) = self.committed_chat_delivery(session_id).await?
        {
            observer(EngineEvent::DeliveryCommitted {
                text: delivery.text.clone(),
                stop: delivery.stop,
                model_steps: delivery.model_steps,
                modified_files: delivery.modified_files.clone(),
                transcript_ordinal: delivery.transcript_ordinal,
            });
            observer(EngineEvent::RunFinished {
                text: delivery.text.clone(),
            });
            return Ok(TaskReport::new(
                TaskOutcome::Completed,
                delivery.text,
                delivery.modified_files,
                delivery.stop,
                delivery.model_steps,
            ));
        }
        let token = self
            .engine
            .mark_running(session_id, AgentState::Execute)
            .await?;
        // Work the task still owes is what a continuation continues, once the
        // turn in front of it has finished. A blocked or failed goal window
        // hands the conversation back; the question they ask ends as a
        // completed Chat turn that carries no goal of its own, and `继续`
        // re-enters the one goal that owes the work. A Chat turn that itself
        // failed or was interrupted IS the work being resumed: `继续` stays on
        // that chat objective instead of pinning the older goal. More than one
        // running goal is ambiguous, so nothing is guessed.
        if lineage_was_chat
            && lineage.latest_completed
            && lineage.goal_id.is_none()
            && pending.is_none()
            && let Some(owed) = self.single_running_goal(&token.task_id).await?
        {
            lineage.goal_id = Some(owed);
            lineage.kind = ResumeTurnKind::Goal;
        }
        if matches!(lineage.kind, ResumeTurnKind::Goal) && lineage.goal_id.is_none() {
            // Identity first: a single running goal IS the work this turn was
            // continuing (the mid-turn bind). The durable declaration outranks
            // that guess when one is pending, because nothing has bound it yet.
            let bound = if lineage_was_chat && pending.is_none() {
                match self.unique_legacy_running_goal(session_id).await? {
                    Some(goal_id) => Ok(goal_id),
                    None => self
                        .open_goal_for_turn(&token, session_id, lineage.objective.text())
                        .await
                        .map(|(goal, _)| goal),
                }
            } else {
                self.open_goal_for_turn(&token, session_id, lineage.objective.text())
                    .await
                    .map(|(goal, _)| goal)
            };
            lineage.goal_id = match bound {
                Ok(goal_id) => Some(goal_id),
                Err(error) => {
                    let result = Err(error);
                    self.finish_from_result(
                        &token,
                        session_id,
                        &result,
                        None,
                        spec.coding.repository.as_deref(),
                        observer,
                        &cancellation,
                    )
                    .await?;
                    return result;
                }
            };
        }
        // A Chat lineage's anchor is the chat message, not a goal. When that
        // turn turns out to own a goal, the goal's own objective is the honest
        // anchor (and after a crash it is the recovered text).
        if lineage_was_chat
            && let Some(goal_id) = lineage.goal_id.as_ref()
            && let Some(record) = self.engine.stores.goals.get(goal_id).await?
        {
            lineage.objective =
                leveler_lifecycle::ObjectiveAnchor::from_session_goal(record.objective);
        }
        // A crash between the goal bind and the request-row reassignment
        // leaves the upgraded window's spend under the Chat lineage root.
        // Adopt it BEFORE any budget is reconstructed for this goal: the
        // resume must count the whole window, not only the calls that
        // happened to be written after the bind. Idempotent.
        if let Some(goal_id) = lineage.goal_id.as_ref() {
            self.adopt_chat_window_budget(session_id, goal_id).await?;
        }
        // A crash between the drive's published delivery and the task terminal
        // leaves the answer durable and the task unfinished. Re-driving the
        // model here would make it answer a second time; the recorded delivery
        // is already the answer the user is owed. A resume that carries a
        // message asks for new work, so only a resume with no message recovers
        // the recorded answer.
        if pending.is_none()
            && instruction.is_none()
            && let Some(goal_id) = lineage.goal_id.clone()
            && let Some(delivery) = self
                .uncommitted_delivery(session_id, Some(&goal_id), false)
                .await?
        {
            tracing::info!(
                %session_id,
                goal = %goal_id.as_str(),
                "recovering the delivery the crashed terminal commit never published"
            );
            // Replay the already durable delivery to this continuation's
            // observer. This is presentation recovery, not a second append:
            // headless clients emit the accepted answer, and interactive
            // clients use the fact to name the answer already on screen.
            observer(EngineEvent::DeliveryCommitted {
                text: delivery.text.clone(),
                stop: delivery.stop,
                model_steps: delivery.model_steps,
                modified_files: delivery.modified_files.clone(),
                transcript_ordinal: delivery.transcript_ordinal,
            });
            observer(EngineEvent::RunFinished {
                text: delivery.text.clone(),
            });
            if delivery.turn_open {
                self.close_recovered_turn(&token, session_id, &delivery, observer)
                    .await?;
            }
            // The finalization phases after the turn are mechanical and were
            // interrupted mid-way: a required review may still be owed, and
            // the outcome/terminal commits never happened.
            let review = self
                .resume_closure_review(
                    &token,
                    session_id,
                    spec,
                    goal_id.as_str(),
                    &delivery,
                    observer,
                    &cancellation,
                )
                .await?;
            let blocked = matches!(delivery.stop, StopReason::Blocked);
            let mut report = TaskReport::new(
                if blocked {
                    TaskOutcome::Blocked
                } else {
                    TaskOutcome::Completed
                },
                delivery.text,
                delivery.modified_files,
                delivery.stop,
                delivery.model_steps,
            );
            apply_review_warnings(&mut report, review);
            let result = Ok(report);
            self.finish_from_result(
                &token,
                session_id,
                &result,
                Some(&goal_id),
                spec.coding.repository.as_deref(),
                observer,
                &cancellation,
            )
            .await?;
            return result;
        }
        // The same cut on an ordinary Chat turn. A Chat answer has no goal to
        // bind the recovery to, so the turn identity and the durable delivery
        // are the whole evidence. Replay that answer and commit the missing
        // terminals; never ask the model for a second one.
        if pending.is_none()
            && instruction.is_none()
            && lineage.goal_id.is_none()
            && matches!(lineage.kind, ResumeTurnKind::Chat)
            && let Some(delivery) = self.uncommitted_delivery(session_id, None, false).await?
        {
            tracing::info!(
                %session_id,
                turn = %delivery.turn_id.as_str(),
                "recovering the Chat delivery the crashed terminal commit never published"
            );
            observer(EngineEvent::DeliveryCommitted {
                text: delivery.text.clone(),
                stop: delivery.stop,
                model_steps: delivery.model_steps,
                modified_files: delivery.modified_files.clone(),
                transcript_ordinal: delivery.transcript_ordinal,
            });
            observer(EngineEvent::RunFinished {
                text: delivery.text.clone(),
            });
            if delivery.turn_open {
                self.close_recovered_turn(&token, session_id, &delivery, observer)
                    .await?;
            }
            let report = TaskReport::new(
                TaskOutcome::Completed,
                delivery.text,
                delivery.modified_files,
                delivery.stop,
                delivery.model_steps,
            );
            let result = Ok(report);
            self.finish_from_result(
                &token,
                session_id,
                &result,
                None,
                spec.coding.repository.as_deref(),
                observer,
                &cancellation,
            )
            .await?;
            return result;
        }
        // The declaration this resume continues, if one is still pending. It
        // is consumed atomically with the resumed turn's WAL row below.
        if matches!(lineage.kind, ResumeTurnKind::Goal)
            && let Some(goal_id) = lineage.goal_id.as_ref()
        {
            lineage.declaration = match self.turn_goal_declaration(session_id, goal_id).await {
                Ok(declaration) => declaration,
                Err(error) => {
                    let result = Err(error);
                    self.finish_from_result(
                        &token,
                        session_id,
                        &result,
                        lineage.goal_id.as_ref(),
                        spec.coding.repository.as_deref(),
                        observer,
                        &cancellation,
                    )
                    .await?;
                    return result;
                }
            };
        }
        if let Some(message) = instruction.as_ref() {
            let image_count = message
                .content
                .iter()
                .filter(|part| matches!(part, leveler_model::ContentPart::Image { .. }))
                .count();
            let mut amendment = message.text_content();
            if image_count > 0 {
                if !amendment.is_empty() {
                    amendment.push('\n');
                }
                amendment.push_str(&format!("[{image_count} image attachment(s)]"));
            }
            // The user's own continuation text is scope authority. Persist it
            // with the goal BEFORE the turn runs: a compaction or crash must
            // not resurrect a requirement the user has already waived, nor
            // lose one they added.
            if !amendment.is_empty()
                && let Some(goal_id) = lineage.goal_id.as_ref()
            {
                self.engine
                    .stores
                    .goals
                    .append_amendment(&token, goal_id, &amendment)
                    .await?;
            }
            lineage.objective.amend(amendment);
        }
        let checkpoint_scope = match (&lineage.kind, lineage.goal_id.as_ref()) {
            (ResumeTurnKind::Goal, Some(goal_id)) => {
                Some(crate::coding::checkpoint::GoalCheckpointScope::new(
                    goal_id.clone(),
                    lineage.root_turn_id.clone(),
                    lineage.turn_ids.clone(),
                ))
            }
            _ => None,
        };
        let log = EventLog::new_owned(
            self.engine.stores.events.as_ref(),
            session_id.clone(),
            token.clone(),
        );
        // Recover a crash after the turn WAL was inserted but before its
        // initiating message reached the transcript. This must happen before
        // the transcript is loaded for the resumed model request.
        for event in leveler_engine::reap_running_turns_owned(
            self.engine.stores.turns.as_ref(),
            self.engine.stores.messages.as_ref(),
            self.engine.stores.terminal.as_ref(),
            &token,
            Some(session_id),
        )
        .await?
        {
            observer(event);
        }
        // Event/transcript consistency before the model context is loaded:
        // a tool result whose `ToolCallFinished` is durable must not be lost
        // from the resumed conversation, and a transcript result must not look
        // dangling. This appends only reconstructed facts (never runs a tool).
        self.reconcile_event_transcript(&token, session_id, &log, observer)
            .await?;
        let raw = self
            .load_request_transcript(session_id, Some("transcript"), checkpoint_scope.as_ref())
            .await?;
        if raw.is_empty() {
            let result = Err(EngineError::Config(format!(
                "session {session_id} has no transcript to resume; \
                 for interactive chat reopen with: leveler tui --session {session_id}"
            )));
            self.finish_from_result(
                &token,
                session_id,
                &result,
                None,
                spec.coding.repository.as_deref(),
                observer,
                &cancellation,
            )
            .await?;
            return result;
        }
        // Resume rebinds the goal by IDENTITY: the task's running goal is the
        // logical task being continued, regardless of how the continuation
        // message is phrased. `spec.runtime.goal` is only the fallback for a
        // session that lost its goal row.
        let goal = if matches!(lineage.kind, ResumeTurnKind::Goal) {
            let lineage_goal = lineage.goal_id.as_ref().ok_or_else(|| {
                EngineError::Corrupt("goal continuation has no durable goal id".into())
            })?;
            match self.resume_goal(&token, lineage_goal).await {
                Ok(goal) => Some(goal),
                Err(error) => {
                    let result = Err(error);
                    self.finish_from_result(
                        &token,
                        session_id,
                        &result,
                        None,
                        spec.coding.repository.as_deref(),
                        observer,
                        &cancellation,
                    )
                    .await?;
                    return result;
                }
            }
        } else {
            None
        };
        // Long-goal P3: a valid durable checkpoint replaces the replayed old
        // context — resume receives the checkpoint block plus exactly the
        // transcript after its watermark. No usable checkpoint (none written,
        // corrupt, future version, stale watermark) falls back to the
        // pre-checkpoint full-history path below.
        // Same rules as chat: a checkpoint's block when one is fresh, else the
        // snapshot merged with the post-snapshot rows, folded if still oversized.
        let prior_result = async {
        if let Some(capabilities) = &self.factory.capabilities
            && let Some(goal) = &goal
        {
            capabilities
                .bind_goal(
                    self.engine.stores.goals.clone(),
                    token.clone(),
                    goal.clone(),
                )
                .await
                .map_err(EngineError::Config)?;
        }
            let scope = lineage.goal_id.as_ref().map(|id| id.as_str()).unwrap_or(lineage.root_turn_id.as_str());
            let scoped = crate::coding::turn::persisted_budget_progress(self.engine.stores.events.as_ref(), session_id, scope).await?;
            // F-12: every legacy unscoped spend row matters, not only the last
            // progress row. A scoped row appended after legacy spend used to
            // hide it and let a capped resume rebuild the cap from zero.
            let legacy_unscoped =
                crate::coding::turn::has_unscoped_model_spend(self.engine.stores.events.as_ref(), session_id).await?;
            if scoped.is_none() && legacy_unscoped
                && (spec.runtime.limits.max_model_tokens.is_some() || spec.runtime.limits.max_cost_usd_micros.is_some()) {
                return Err(EngineError::Config("cannot reconstruct capped legacy task budget: request scope attribution is unavailable".into()));
            }
            self
            .assembled_prior(
                &log,
                session_id,
                raw,
                Some(lineage.objective.text()),
                true,
                checkpoint_scope.as_ref(),
                spec.coding.repository.as_deref().map(GitWorkspace::new).as_ref().map(|workspace| workspace as &dyn crate::coding::checkpoint::WorkspaceFacts),
                &self.context_summarizer(session_id, lineage.goal_id.as_ref().map(|id| id.as_str()).unwrap_or(lineage.root_turn_id.as_str()), spec.runtime.limits, &cancellation),
                &cancellation,
                observer,
            )
            .await
        }.await;
        let prior = match prior_result {
            Ok(prior) => prior,
            Err(error) => {
                let result = Err(error);
                self.finish_from_result(
                    &token,
                    session_id,
                    &result,
                    lineage.goal_id.as_ref(),
                    spec.coding.repository.as_deref(),
                    observer,
                    &cancellation,
                )
                .await?;
                return result;
            }
        };
        // A resumed Goal window (and any resume whose transcript already
        // carries a compaction/checkpoint boundary) gets the deterministic
        // facts projection. It is rebuilt from durable owners here, so a
        // compacted summary never becomes the only carrier of the objective,
        // plan, verification or unknown tool facts.
        let objective_text = lineage.objective.text().to_string();
        let prior = self
            .with_compaction_facts(
                prior,
                session_id,
                lineage.goal_id.as_ref(),
                &log,
                Some(objective_text.as_str()),
            )
            .await?;
        let runner = TurnRunner {
            stores: &self.engine.stores,
            token: token.clone(),
            session_id: session_id.clone(),
            log: &log,
            approver: self.approver.clone(),
            clarifier: self.clarifier.clone(),
            lost_child_voice: Some(self.lost_child_voice(session_id)),
        };

        // Reconcile the crash window before continuing: a tool that started but
        // never finished (the process died mid-execution) is closed as an
        // interrupted observation or stops the turn for explicit
        // acknowledgement. Nothing is replayed.
        self.recover_crash_window(&log, observer, &cancellation)
            .await?;

        let mut prior = prior;
        // An acknowledged call's result was never persisted. The model
        // re-drives from the last clean turn boundary, so without this notice
        // it would repeat the request blind — possibly repeating the side
        // effect the crash already performed.
        if let Some(note) = self.crash_recovery_notice(&log).await? {
            prior.push(note);
        }

        let terminal_cancellation = cancellation.clone();
        let result = self
            .resume_direct(
                &log,
                &runner,
                spec,
                prior,
                instruction,
                lineage,
                observer,
                cancellation,
            )
            .await;
        let result = if terminal_cancellation.is_cancelled() {
            Err(EngineError::Cancelled)
        } else {
            result
        };
        self.finish_from_result(
            &token,
            session_id,
            &result,
            goal.as_ref(),
            spec.coding.repository.as_deref(),
            observer,
            &terminal_cancellation,
        )
        .await?;
        result
    }

    /// The explicit reconciliation flow behind `RecoveryConfirmationRequired`:
    /// after the user has inspected the workspace, close every dangling tool
    /// call with an explicit user-acknowledged marker so resume can proceed.
    /// The marker is an errored result — never a fake success — and nothing is
    /// replayed; the model re-drives from the last clean turn boundary.
    /// Returns how many calls were closed.
    /// Acknowledging is a canonical recovery write, so it is ownership-
    /// fenced: this acquires (or same-runtime reacquires) the task first — a
    /// foreign-owned task is an explicit conflict, never auto-stolen.
    pub async fn acknowledge_crash_window(
        &self,
        session_id: &SessionId,
    ) -> Result<usize, EngineError> {
        let token = self.engine.acquire_ownership(session_id).await?;
        let closed = leveler_engine::acknowledge_crash_window(
            self.engine.stores.events.as_ref(),
            &token,
            session_id,
        )
        .await;
        // Acknowledging starts no execution: whatever runs next acquires anew.
        self.engine.release_ownership(&token).await?;
        closed
    }

    /// The recovery context the model gets on the first turn after the user
    /// acknowledged a crash window: which calls were interrupted, that their
    /// outcome is unknown, and that they were not replayed. `None` when no
    /// acknowledged window is still awaiting its first post-recovery turn.
    async fn crash_recovery_notice(
        &self,
        log: &EventLog<'_>,
    ) -> Result<Option<Message>, EngineError> {
        let calls = log.acknowledged_crash_calls().await?;
        if calls.is_empty() {
            return Ok(None);
        }
        let mut text = String::from(
            "## Recovered after an interruption\n\
             The runtime stopped while these tool calls were running. Their results are not in \
             the conversation above:\n",
        );
        for call in &calls {
            let arguments = leveler_core::truncate_head_bytes(&call.arguments, 200, "…");
            let target = if arguments.is_empty() {
                format!("(call {})", call.call_id)
            } else {
                format!("{arguments} (call {})", call.call_id)
            };
            text.push_str(&format!(
                "- {} {target} — outcome unknown: the call may already have taken effect and \
                 was not replayed.\n",
                call.name
            ));
        }
        text.push_str("Check the actual state before repeating a side-effecting action.\n");
        Ok(Some(Message::user(
            text,
            TranscriptOrigin::RuntimeNotice {
                notice: RuntimeNoticeKind::CrashRecovery,
            },
        )))
    }

    /// Reconcile the crash window before resume or chat.
    ///
    /// Nothing is replayed. The process died while these calls were running,
    /// so their real outcome is unknown; running one again would record a
    /// fresh result as if it were the interrupted call's answer, and could
    /// repeat a side effect. A pure runtime observation whose answer the crash
    /// destroyed (`recovery_is_skippable`) is closed as explicitly interrupted;
    /// every other call stops the turn until the user acknowledges the crash
    /// window. The reconciling `ToolCallFinished` goes to the event log only —
    /// the model re-drives from the last clean turn boundary (tool-call results
    /// are not injected into the transcript; see the M5 crash-window notes).
    async fn recover_crash_window(
        &self,
        log: &EventLog<'_>,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
        _cancellation: &CancellationToken,
    ) -> Result<(), EngineError> {
        for call in log.dangling_tool_calls().await? {
            let turn_id = call.turn_id.as_ref().map(|t| TurnId::new(t.clone()));
            let turn_ref = turn_id.as_ref();

            // Seeing ApprovalRequested without a persisted ApprovalResolved does
            // NOT prove dispatch never ran: the approval recorder queues the
            // resolution for persistence, then the executor may start the tool
            // before the event-log pump flushes it. A crash in that window looks
            // pending even though the side effect may have happened. Stop before
            // any model re-drive; the explicit reconciliation flow resolves the
            // dangling call after the user inspects the workspace.
            if call.pending_approval {
                return Err(EngineError::RecoveryConfirmationRequired {
                    call_id: call.call_id,
                    tool: call.name,
                });
            }

            // An observation the runtime can simply read again is not an
            // unresolved side effect: the crash destroyed its answer, not the
            // state it would have returned. Say exactly that and let the model
            // re-drive. Halting here made a whole session unusable after an
            // interrupted `wait_task`.
            if self.factory.registry.recovery_is_skippable(&call.name) {
                self.engine
                    .record_recovery_skip(
                        log,
                        &call,
                        turn_ref,
                        "interrupted observation; not replayed",
                        observer,
                    )
                    .await?;
                continue;
            }

            return Err(EngineError::RecoveryConfirmationRequired {
                call_id: call.call_id,
                tool: call.name,
            });
        }
        Ok(())
    }

    /// Reconcile the event plane with the transcript plane before a resume
    /// loads the model context.
    ///
    /// The fatal F-05 window is `ToolCallFinished` durable, transcript batch
    /// not: the tool may already have had a side effect, but the model has
    /// seen no result, so a resume that only checks "dangling" calls misses it
    /// and can repeat the effect. This compares the turn's durable call
    /// identities with the tool results actually written for that turn:
    ///
    /// - a finished call with no transcript result gets a recovered
    ///   assistant/tool exchange built from the durable event, with an
    ///   explicit note that the effect may have happened and the output may be
    ///   truncated;
    /// - a started call whose result IS in the transcript but whose
    ///   `ToolCallFinished` never committed gets that event appended, so the
    ///   dangling check does not demand confirmation for a known result.
    ///
    /// Call ids can repeat across rounds (providers do not promise uniqueness),
    /// so calls are paired FIFO and compared by occurrence count, not by set
    /// membership. Nothing here executes a tool or asks the model anything.
    async fn reconcile_event_transcript(
        &self,
        token: &leveler_core::OwnershipToken,
        session_id: &SessionId,
        log: &EventLog<'_>,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
    ) -> Result<(), EngineError> {
        struct Started {
            call_id: String,
            name: String,
            arguments: serde_json::Value,
            model_step: u32,
            sequence: i64,
        }
        struct Finished {
            call_id: String,
            name: String,
            is_error: bool,
            preview: String,
        }
        let turns = self
            .engine
            .stores
            .turns
            .list_for_session(session_id)
            .await?;
        let Some(turn) = turns
            .iter()
            .rev()
            .find(|turn| turn.status == "running" || turn.status == "interrupted")
        else {
            return Ok(());
        };
        let turn_id = TurnId::new(turn.id.clone());
        let rows = self.engine.stores.events.load(session_id).await?;
        let mut started: Vec<Started> = Vec::new();
        let mut finished: Vec<Finished> = Vec::new();
        for row in rows
            .iter()
            .filter(|row| row.turn_id.as_deref() == Some(turn.id.as_str()))
        {
            match EngineEvent::from_payload(&row.payload) {
                // Delegated calls have their own transcript path; the parent
                // turn's messages are the subject here.
                Ok(EngineEvent::ToolCallStarted {
                    call_id,
                    name,
                    arguments,
                    model_step,
                    agent_id: None,
                    ..
                }) => started.push(Started {
                    call_id,
                    name,
                    arguments: serde_json::from_str(&arguments).unwrap_or(serde_json::Value::Null),
                    model_step: model_step.unwrap_or(0),
                    sequence: row.sequence,
                }),
                Ok(EngineEvent::ToolCallFinished {
                    call_id,
                    name,
                    is_error,
                    preview,
                    agent_id: None,
                    ..
                }) => finished.push(Finished {
                    call_id,
                    name,
                    is_error,
                    preview,
                }),
                _ => {}
            }
        }
        let payloads = self
            .engine
            .stores
            .messages
            .load_for_turn(session_id, &turn_id)
            .await?;
        let mut actual: Vec<(String, String, bool)> = Vec::new();
        for payload in &payloads {
            let Ok(message) = serde_json::from_str::<Message>(payload) else {
                continue;
            };
            for part in message.content {
                if let ContentPart::ToolResult { result } = part {
                    actual.push((
                        result.call_id.as_str().to_string(),
                        result.content,
                        result.is_error,
                    ));
                }
            }
        }
        // FIFO pairing: the oldest unmatched started call owns each finished
        // result. Multiple rounds may reuse one provider id; order is stable.
        let mut used = vec![false; started.len()];
        let mut paired: Vec<(usize, &Finished)> = Vec::new();
        for call in &finished {
            if let Some(index) = started.iter().enumerate().position(|(index, start)| {
                !used[index] && start.call_id == call.call_id && start.name == call.name
            }) {
                used[index] = true;
                paired.push((index, call));
            }
        }
        let mut actual_counts: HashMap<&str, usize> = HashMap::new();
        for (call_id, _, _) in &actual {
            *actual_counts.entry(call_id.as_str()).or_default() += 1;
        }
        let mut expected_counts: HashMap<&str, usize> = HashMap::new();
        let mut missing: Vec<(usize, &Finished)> = Vec::new();
        for (index, call) in &paired {
            let seen = expected_counts.entry(call.call_id.as_str()).or_default();
            *seen += 1;
            if actual_counts
                .get(call.call_id.as_str())
                .copied()
                .unwrap_or(0)
                < *seen
            {
                missing.push((*index, call));
            }
        }
        // Reconstruct each missing call in its own round, in call order. Only
        // the missing calls are written, so an already-persisted round is
        // never duplicated.
        missing.sort_by_key(|(index, _)| (started[*index].model_step, started[*index].sequence));
        let mut groups: Vec<Vec<(usize, &Finished)>> = Vec::new();
        for item in missing {
            match groups.last_mut() {
                Some(group) if started[group[0].0].model_step == started[item.0].model_step => {
                    group.push(item);
                }
                _ => groups.push(vec![item]),
            }
        }
        for group in groups {
            let assistant = Message {
                role: Role::Assistant,
                content: group
                    .iter()
                    .map(|(index, _)| ContentPart::ToolCall {
                        call: ToolCall {
                            id: ToolCallId::new(started[*index].call_id.clone()),
                            name: started[*index].name.clone(),
                            arguments: started[*index].arguments.clone(),
                        },
                    })
                    .collect(),
                origin: None,
                route: None,
            };
            let tool = Message {
                role: Role::Tool,
                content: group
                    .iter()
                    .map(|(index, call)| {
                        let has_output = !call.preview.trim().is_empty();
                        let content = if has_output {
                            format!(
                                "[recovery] this tool finished but its transcript result was lost \
                                 in an unclean exit. The effect may already have happened; do not \
                                 run it again only to recover the output. Recorded output \
                                 (possibly truncated):\n{}",
                                call.preview
                            )
                        } else {
                            "[recovery] this tool finished but its transcript result was lost in \
                             an unclean exit and no output was recorded. The effect may already \
                             have happened; its real outcome is unknown. Check the workspace \
                             before repeating a side-effecting call."
                                .to_string()
                        };
                        ContentPart::ToolResult {
                            result: ToolResultContent {
                                call_id: ToolCallId::new(started[*index].call_id.clone()),
                                content,
                                // An empty recording is an unknown result, never a success.
                                is_error: call.is_error || !has_output,
                            },
                        }
                    })
                    .collect(),
                origin: None,
                route: None,
            };
            let payloads = [
                serde_json::to_string(&assistant).map_err(|error| {
                    EngineError::Corrupt(format!("could not encode a recovered tool call: {error}"))
                })?,
                serde_json::to_string(&tool).map_err(|error| {
                    EngineError::Corrupt(format!(
                        "could not encode a recovered tool result: {error}"
                    ))
                })?,
            ];
            self.engine
                .stores
                .messages
                .append_in_turn_owned(token, session_id, &turn_id, &payloads, leveler_core::now())
                .await
                .map_err(|error| {
                    EngineError::Corrupt(format!(
                        "could not persist the recovered tool exchange: {error}"
                    ))
                })?;
        }
        // Reverse relation: the transcript result is durable but the finished
        // event never committed. Recording that event from the transcript is a
        // mechanical fact, not a replay.
        for (index, start) in started.iter().enumerate() {
            if used[index] {
                continue;
            }
            if finished.iter().any(|call| call.call_id == start.call_id) {
                // An ambiguous duplicate id: leave it to the dangling check.
                continue;
            }
            let Some((_, content, is_error)) = actual
                .iter()
                .find(|(call_id, _, _)| call_id == &start.call_id)
            else {
                continue;
            };
            let event = EngineEvent::ToolCallFinished {
                call_id: start.call_id.clone(),
                name: start.name.clone(),
                is_error: *is_error,
                preview: content.chars().take(2_000).collect(),
                agent_id: None,
                applied_diff: None,
                exit_code: None,
                stop: None,
            };
            log.append(Some(&turn_id), event, observer).await?;
        }
        Ok(())
    }

    /// Continue the direct strategy from a prior transcript — the EXPLICIT
    /// continuation (`resume`, a caller asking for another turn): one resume
    /// turn, then the same conclusion as a fresh run.
    async fn resume_direct(
        &self,
        log: &EventLog<'_>,
        runner: &TurnRunner<'_>,
        spec: &TaskSpec,
        prior: Vec<leveler_model::Message>,
        instruction: Option<leveler_model::Message>,
        lineage: ResumeLineage,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
        cancellation: CancellationToken,
    ) -> Result<TaskReport, EngineError> {
        // A continuation message is this turn's persisted user input; without
        // one (headless resume) the turn carries no new initiating message and
        // the engine records it as a pure resume.
        let kind = match lineage.kind {
            ResumeTurnKind::Chat => TurnKind::Chat,
            ResumeTurnKind::Goal => TurnKind::User,
        };
        let budget_scope = lineage
            .goal_id
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| lineage.root_turn_id.to_string());
        let start = leveler_engine::TurnStart::Continue {
            message: instruction.clone(),
            objective: lineage.objective.clone(),
            root_turn_id: lineage.root_turn_id.clone(),
            goal_id: lineage.goal_id.clone(),
            declaration: lineage.declaration.clone(),
        };
        let profile = match lineage.kind {
            ResumeTurnKind::Chat => chat_profile(spec),
            ResumeTurnKind::Goal => goal_profile(spec),
        };
        // The goal's commands from earlier windows are part of the task's
        // verification record; a resumed window must not forget them.
        let verification_runs = match lineage.goal_id.as_ref() {
            Some(goal_id) => {
                crate::coding::turn::load_goal_verification_runs(
                    &self.engine.stores,
                    &runner.session_id,
                    goal_id,
                )
                .await?
            }
            None => Vec::new(),
        };
        let recorded = runner
            .run_turn(kind, start, observer, cancellation.clone(), |ports| {
                drive_turn(
                    &self.factory,
                    profile,
                    TurnInput::Resume {
                        prior,
                        instruction,
                        objective: lineage.objective,
                        lineage_turn_ids: lineage.turn_ids,
                        root_turn_id: lineage.root_turn_id,
                        goal_id: lineage.goal_id,
                        verification_runs,
                    },
                    true,
                    runner.session_id.clone(),
                    self.engine.stores.events.clone(),
                    Some(crate::coding::checkpoint::CodingCheckpointContext::new(
                        self.engine.clone(),
                        spec.coding.repository.as_deref().map(|root| {
                            Arc::new(GitWorkspace::new(root))
                                as Arc<dyn crate::coding::checkpoint::WorkspaceFacts>
                        }),
                    )),
                    ports,
                    cancellation.clone(),
                )
            })
            .await?;
        self.conclude_direct(
            log,
            runner,
            spec,
            &budget_scope,
            recorded.outcome,
            self.factory.independent_review,
            observer,
            cancellation,
        )
        .await
    }

    /// The direct strategy: one goal turn.
    #[allow(clippy::too_many_arguments)]
    async fn run_direct(
        &self,
        log: &EventLog<'_>,
        runner: &TurnRunner<'_>,
        spec: &TaskSpec,
        goal_id: &leveler_core::GoalId,
        continuing_goal: bool,
        content: Vec<ContentPart>,
        declaration: Option<String>,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
        cancellation: CancellationToken,
    ) -> Result<TaskReport, EngineError> {
        self.run_coding_turn(
            log,
            runner,
            spec,
            goal_id,
            Message::from_parts(Role::User, content, Some(TranscriptOrigin::UserInput)),
            self.factory.independent_review,
            continuing_goal,
            declaration,
            observer,
            cancellation,
        )
        .await
    }

    /// One Coding turn on `task`.
    ///
    /// `task` is the goal's initiating message: the user's words, plus any
    /// attachments. Same executor, same plan, same tools, same completion
    /// semantics for every turn.
    #[allow(clippy::too_many_arguments)]
    async fn run_coding_turn(
        &self,
        log: &EventLog<'_>,
        runner: &TurnRunner<'_>,
        spec: &TaskSpec,
        goal_id: &leveler_core::GoalId,
        task: Message,
        review_policy: IndependentReviewPolicy,
        continuing_goal: bool,
        goal_declaration: Option<String>,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
        cancellation: CancellationToken,
    ) -> Result<TaskReport, EngineError> {
        if let Some(capabilities) = &self.factory.capabilities {
            capabilities
                .bind_goal(
                    self.engine.stores.goals.clone(),
                    runner.token.clone(),
                    goal_id.clone(),
                )
                .await
                .map_err(EngineError::Config)?;
        }
        // A Goal turn reopens a crashed session exactly like a chat/resume turn:
        // the durable log can hold a started tool call whose result never
        // reached the transcript. Rebuild the transcript-plane result from the
        // event plane, then stop on a side-effecting dangling call instead of
        // replaying it. Both are the canonical readers the other interactive
        // paths use; a turn with nothing to reconcile is a no-op.
        self.reconcile_event_transcript(&runner.token, &runner.session_id, log, observer)
            .await?;
        self.recover_crash_window(log, observer, &cancellation)
            .await?;
        // Multi-turn Goal: inject bounded session history so follow-ups can
        // resolve deictic references ("刚才那个超时"). The objective is the
        // goal RECORD's: a recovered declaration and the turn's instruction
        // text can differ, and the record is what the user declared.
        let objective_text = match self.engine.stores.goals.get(goal_id).await? {
            Some(record) => record.objective,
            None => content_objective_text(&task.content),
        };
        let mut prior = self
            .bounded_session_history(
                log,
                &runner.session_id,
                goal_id,
                &objective_text,
                spec.coding.repository.as_deref(),
                spec.runtime.limits,
                &cancellation,
                observer,
            )
            .await?;
        // The user's message and the goal's objective can differ when a lost
        // declaration is recovered: the turn is `继续`, the goal is the
        // objective the crash swallowed. The model must be told that
        // objective, so it gets the same host-pinned row a fold would use.
        if objective_text.trim() != content_objective_text(&task.content).trim() {
            prior.insert(0, leveler_context::objective_pin_message(&objective_text));
        }
        let prior_epoch_open = self
            .prior_epoch_open(&runner.session_id, continuing_goal)
            .await?;
        // A goal that already ran windows keeps their mechanical verification
        // facts. A brand-new goal has none, and the loader returns empty.
        let verification_runs = crate::coding::turn::load_goal_verification_runs(
            &self.engine.stores,
            &runner.session_id,
            goal_id,
        )
        .await?;
        let recorded = runner
            .run_turn(
                TurnKind::User,
                leveler_engine::TurnStart::Anchored {
                    message: task.clone(),
                    objective: leveler_lifecycle::ObjectiveAnchor::from_session_goal(
                        objective_text.clone(),
                    ),
                    goal_id: Some(goal_id.clone()),
                    declaration: goal_declaration,
                },
                observer,
                cancellation.clone(),
                |ports| {
                    drive_turn(
                        &self.factory,
                        goal_profile(spec),
                        TurnInput::Goal {
                            goal: objective_text,
                            origin: task
                                .origin
                                .clone()
                                .expect("coding turn message records its origin"),
                            content: task.content,
                            prior,
                            goal_id: goal_id.clone(),
                            verification_runs,
                            continuing: continuing_goal,
                        },
                        prior_epoch_open,
                        runner.session_id.clone(),
                        self.engine.stores.events.clone(),
                        Some(crate::coding::checkpoint::CodingCheckpointContext::new(
                            self.engine.clone(),
                            spec.coding.repository.as_deref().map(|root| {
                                Arc::new(GitWorkspace::new(root))
                                    as Arc<dyn crate::coding::checkpoint::WorkspaceFacts>
                            }),
                        )),
                        ports,
                        cancellation.clone(),
                    )
                },
            )
            .await?;
        // One turn: where the model stops, or a hard limit stops it, is where
        // the run ends. Epoch spend lives on ProgressLedger inside the drive
        // (seeded across explicit resumes); nothing re-accumulates it here.
        self.conclude_direct(
            log,
            runner,
            spec,
            goal_id.as_str(),
            recorded.outcome,
            review_policy,
            observer,
            cancellation,
        )
        .await
    }

    /// Load session messages (prefer snapshot), bound length for Goal injection.
    #[allow(clippy::too_many_arguments)]
    async fn bounded_session_history(
        &self,
        log: &EventLog<'_>,
        session_id: &SessionId,
        goal_id: &leveler_core::GoalId,
        goal: &str,
        repo: Option<&std::path::Path>,
        limits: StepLimits,
        cancellation: &CancellationToken,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
    ) -> Result<Vec<leveler_model::Message>, EngineError> {
        let scope = crate::coding::checkpoint::latest_goal_checkpoint_scope(
            &self.engine.stores,
            session_id,
            goal_id,
        )
        .await?;
        let raw = self
            .load_request_transcript(session_id, Some("transcript"), scope.as_ref())
            .await?;
        if raw.is_empty() {
            return Ok(Vec::new());
        }
        // Long-goal P3: the interactive multi-turn path is where a long
        // session's history actually grows — over the fold threshold it
        // continues from a durable checkpoint (fresh or cut here) instead of
        // a blunt last-N tail of replayed history.
        if let Some(scope) = scope.as_ref()
            && let Some(prior) = self
                .checkpointed_prior(
                    log,
                    session_id,
                    &raw,
                    Some(goal),
                    true,
                    repo.map(GitWorkspace::new)
                        .as_ref()
                        .map(|w| w as &dyn crate::coding::checkpoint::WorkspaceFacts),
                    &self.context_summarizer(session_id, goal_id.as_str(), limits, cancellation),
                    cancellation,
                    observer,
                    scope,
                )
                .await?
        {
            return Ok(prior);
        }
        self.assembled_prior(
            log,
            session_id,
            raw,
            Some(goal),
            true,
            None,
            None,
            &self.context_summarizer(session_id, goal_id.as_str(), limits, cancellation),
            cancellation,
            observer,
        )
        .await
    }

    /// Settle the configured required review before the terminal boundary.
    /// Findings remain model-authored advisory information, but whether the
    /// review completed and whether it reported findings are explicit
    /// completion warnings rather than hidden post-terminal work.
    async fn closure_review_stage(
        &self,
        log: &EventLog<'_>,
        runner: &TurnRunner<'_>,
        spec: &TaskSpec,
        budget_scope: &str,
        modified_files: &[String],
        execution_duration_ms: u64,
        // The policy in force for THIS conclusion.
        policy: IndependentReviewPolicy,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<ClosureReview, EngineError> {
        let stage = |required: bool, action: &str, detail: String| EngineEvent::ReviewStage {
            required,
            action: action.to_string(),
            detail,
        };
        if modified_files.is_empty() {
            log.append(
                None,
                stage(false, "not_required", "no product mutation".to_string()),
                observer,
            )
            .await?;
            return Ok(ClosureReview::NotRequired);
        }
        let reason = match policy {
            IndependentReviewPolicy::Off => {
                log.append(
                    None,
                    stage(false, "not_required", "independent_review off".to_string()),
                    observer,
                )
                .await?;
                return Ok(ClosureReview::NotRequired);
            }
            IndependentReviewPolicy::Required => format!(
                "independent_review required, {} modified file(s)",
                modified_files.len()
            ),
        };
        // Persist the attempt BEFORE it runs, so even a crash mid-launch
        // leaves a breadcrumb instead of silence.
        log.append(None, stage(true, "launching", reason.clone()), observer)
            .await?;
        let diff = match spec.coding.repository.as_deref() {
            Some(root) => review_diff(root, modified_files).await,
            None => None,
        };
        match run_harness_child(
            runner,
            &self.factory,
            goal_profile(spec),
            budget_scope,
            "reviewer",
            // Unchanged: the closure reviewer has always announced itself with
            // its brief, and nothing in this change is about that path.
            review_brief(&spec.runtime.goal, modified_files, diff.as_deref()),
            review_brief(&spec.runtime.goal, modified_files, diff.as_deref()),
            modified_files.to_vec(),
            std::time::Duration::from_millis(execution_duration_ms),
            observer,
            cancellation.clone(),
        )
        .await
        {
            Ok(review) if review.completed => {
                log.append(None, stage(true, "finished_ok", reason), observer)
                    .await?;
                Ok(ClosureReview::Completed {
                    findings: review.findings,
                })
            }
            Ok(review) => {
                log.append(None, stage(true, "finished_incomplete", reason), observer)
                    .await?;
                Ok(ClosureReview::Incomplete {
                    findings: review.findings,
                })
            }
            Err(ReviewRunError::Launch(error)) => {
                tracing::warn!(%error, "required review could not be launched");
                log.append(
                    None,
                    stage(true, "launch_failed", format!("{reason}: {error}")),
                    observer,
                )
                .await?;
                Ok(ClosureReview::LaunchFailed)
            }
            Err(ReviewRunError::Settlement(error)) => Err(error),
            Err(ReviewRunError::Unclosed(detail)) => {
                Err(EngineError::UnclosedTerminalBoundary(detail))
            }
        }
    }

    async fn conclude_direct(
        &self,
        log: &EventLog<'_>,
        runner: &TurnRunner<'_>,
        spec: &TaskSpec,
        budget_scope: &str,
        outcome: crate::AgentOutcome,
        review_policy: IndependentReviewPolicy,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
        cancellation: CancellationToken,
    ) -> Result<TaskReport, EngineError> {
        // Goal continuation and bounded budget extension already happened in
        // `supervise` — one loop, one decision point (convergence plan phase 4).
        //
        // The model owns task execution. The runtime records its outcome but
        // does not append a host-owned build/test gate after the model has
        // finished. Tests remain ordinary tools that run only when requested
        // by the user or chosen by the model during the task.
        let mut task_report =
            if let Some(terminal) = direct_non_success_outcome(outcome.stop_reason) {
                report_from_agent_outcome(outcome, terminal)
            } else {
                report_from_agent_outcome(outcome, TaskOutcome::Completed)
            };

        // The user's pause authorization outlives this window. The window's own
        // terminal says whether it stopped AT the boundary; the goal's durable
        // user instructions say whether a pause is still in force. Recording it
        // here is what keeps a later status question from silently releasing a
        // boundary the person set in an earlier turn.
        self.sync_goal_user_pause(&runner.token, budget_scope, &task_report)
            .await?;
        // A configured required reviewer contributes durable completion
        // evidence (findings and a child terminal). Settle it before
        // TaskFinished so that terminal is a closed evidence boundary, not a
        // point after which the old task keeps writing into a possibly newer
        // turn. The Finalizing projection names this wait explicitly.
        let review_phase = begin_finalization_phase(log, FINALIZATION_REVIEW, observer).await;
        let review = self
            .closure_review_stage(
                log,
                runner,
                spec,
                budget_scope,
                &task_report.modified_files,
                task_report.execution_duration_ms,
                review_policy,
                observer,
                &cancellation,
            )
            .await?;
        if let Some(started) = review_phase {
            finish_finalization_phase(FINALIZATION_REVIEW, started);
        }
        apply_review_warnings(&mut task_report, review);
        Ok(task_report)
    }

    /// Persist the user's pause authorization for one goal window.
    ///
    /// `budget_scope` is the goal id for a Goal turn (and for a Chat turn that
    /// committed a task); a plain Chat turn passes its turn id and has no goal
    /// row, so this is a no-op there.
    ///
    /// Only SET here: the window reached the structured boundary the user
    /// declared. Release is owned by the validated intent declaration
    /// (`Goaler::record_intent`), and settlement clears it atomically with the
    /// goal row. No reader of the user's words runs in this path — an absent
    /// pause wording is never read as a release, and a status question cannot
    /// silently lift a boundary the person set.
    async fn sync_goal_user_pause(
        &self,
        token: &leveler_core::OwnershipToken,
        budget_scope: &str,
        report: &TaskReport,
    ) -> Result<(), EngineError> {
        let goal_id = leveler_core::GoalId::new(budget_scope.to_string());
        let Some(record) = self.engine.stores.goals.get(&goal_id).await? else {
            return Ok(());
        };
        let paused = report
            .stop_detail
            .as_deref()
            .is_some_and(crate::executor::closeout::is_user_pause_detail);
        if paused && record.user_paused_at.is_none() {
            self.engine
                .stores
                .goals
                .set_user_pause(token, &goal_id, Some(leveler_core::now()))
                .await?;
        }
        Ok(())
    }

    /// Produce a tracked handoff briefing when the projected request exceeds
    /// its resolved context threshold. Rejected summaries preserve history;
    /// request or accounting failures propagate to the durable turn.
    pub fn context_summarizer<'a>(
        &'a self,
        session_id: &'a SessionId,
        budget_scope: &str,
        limits: StepLimits,
        cancellation: &'a CancellationToken,
    ) -> ModelSummarizer<'a> {
        ModelSummarizer {
            runtime: self.factory.runtime.as_ref(),
            model: &self.factory.model,
            cancellation,
            engine: &self.engine,
            session_id,
            limits,
            budget_scope: budget_scope.to_string(),
            overrides: self.factory.overrides.as_ref(),
        }
    }
}

/// Run one independent reviewer child over work this session already did,
/// and report whether the review actually completed.
///
/// R007b N7 (mechanism half): the harness decides the review is warranted
/// and launches it here, through the same child primitive the `spawn_agent`
/// tool uses — same registry, limits, cancellation and ownership fence. It
/// is not a turn: no turn row, no plan/ledger seeding, no goal state. The
/// started/finished pair is persisted because "was this reviewed?" has to be
/// answerable from durable history rather than from a task card.
#[allow(clippy::too_many_arguments)]
async fn run_harness_child(
    runner: &TurnRunner<'_>,
    factory: &ExecutorFactory,
    profile: TurnProfile,
    budget_scope: &str,
    // What this child is FOR, in the user's vocabulary: `reviewer` names the
    // closure review. It names the child in events and in the parent's rollup.
    //
    // It is NOT a capability: every child launched here runs under the
    // Reviewer contract (read-only, harness-only, bounded rounds), which is
    // why the profile trace below is still looked up by that role. A stage
    // name that could select a capability would be a way to widen one.
    nickname: &'static str,
    // What this child is doing, in one line, for the durable start event and
    // therefore for every client that shows a running child. The BRIEF is what
    // the model reads; this is what a person reads. They are different texts
    // because they answer different questions, and using the brief for both
    // put a page of instructions on screen as status.
    headline: String,
    brief: String,
    files: Vec<String>,
    // The task's wall time already spent: this child is its tail.
    parent_elapsed: std::time::Duration,
    observer: &mut (dyn FnMut(EngineEvent) + Send),
    cancellation: CancellationToken,
) -> Result<HarnessChildOutcome, ReviewRunError> {
    let profile_for_budget = profile.clone();
    let mut executor = factory
        .build(profile, None)
        .await
        .map_err(ReviewRunError::Launch)?
        .with_approver(runner.approver.clone())
        .with_execution_fence(runner.ownership_fence())
        .with_budget_scope(budget_scope.to_string())
        .with_model_request_store(
            runner.stores.model_requests.clone(),
            runner.session_id.clone(),
        );
    let prior_progress = crate::coding::turn::persisted_budget_progress(
        runner.stores.events.as_ref(),
        &runner.session_id,
        budget_scope,
    )
    .await
    .map_err(ReviewRunError::Launch)?
    .unwrap_or_default();
    let prior_progress = crate::coding::turn::reconcile_model_spend(
        prior_progress,
        runner.stores.model_requests.as_ref(),
        &runner.session_id,
        budget_scope,
    )
    .await
    .map_err(ReviewRunError::Launch)?;
    executor = executor.with_residual_budget(&prior_progress, parent_elapsed);
    if prior_progress.has_unpriced_model_attempt
        && crate::coding::factory::profile_step_limits(&profile_for_budget)
            .max_cost_usd_micros
            .is_some()
    {
        return Err(ReviewRunError::Launch(EngineError::Config(
            "cannot run reviewer under cost cap after an unpriced attempt".into(),
        )));
    }
    let id = format!("{nickname}-{}", leveler_core::RequestId::generate());
    let (profile_id, profile_role, read_only) = crate::child_profile_trace("reviewer");
    let (profile_id_trace, profile_role_trace, read_only_trace) =
        (profile_id.clone(), profile_role.clone(), read_only);
    runner
        .log
        .append(
            None,
            EngineEvent::SubAgentStarted {
                id: id.clone(),
                nickname: nickname.to_string(),
                // The execution contract it actually runs under.
                role: "reviewer".to_string(),
                task: headline,
                profile_id: Some(profile_id),
                profile_role: Some(profile_role),
                read_only,
                spec: None,
            },
            observer,
        )
        .await
        .map_err(ReviewRunError::Launch)?;
    let settlement = async {
        // Child requests are already durable before progress arrives. Fold
        // those same facts live; no second insertion or request buffer.
        let mut progress = prior_progress.clone();
        let child_started = std::time::Instant::now();
        let result = {
            let mut forward = |event: crate::AgentEvent| {
                if let crate::AgentEvent::SubAgentModelRequest { record } = &event {
                    progress.has_unpriced_model_attempt |= record.cost_usd_micros.is_none();
                    progress.absorb_request_spend(
                        record.usage.total(),
                        record.estimated_tokens.unwrap_or(0),
                        record.cost_usd_micros.unwrap_or(0),
                    );
                }
                observer(EngineEvent::from(event))
            };
            executor
                .run_reviewer_child(
                    id.clone(),
                    brief,
                    files,
                    std::time::Duration::ZERO,
                    &mut forward,
                    cancellation,
                )
                .await
        };
        // The review's spend is the session's spend: the reviewer's own rounds
        // and commands fold in from its ledger, and its TOKENS AND COST fold in
        // from the very records being written down here — the same authority
        // the bill reconciles against, never a second summary of it.
        progress.absorb_child_work(&result.progress);
        progress.cumulative_duration_ms = progress
            .cumulative_duration_ms
            .saturating_add(child_started.elapsed().as_millis().min(u64::MAX as u128) as u64);
        runner
            .log
            .append(
                None,
                EngineEvent::ProgressUpdated { ledger: progress },
                observer,
            )
            .await?;
        // Unified findings: adopt first so the finish summary can name the
        // parent-side ids the TUI projects as a finding count.
        let mut summary = result.result.for_parent(nickname);
        // Held across the finish event: the projection below is computed from
        // the same ledger the findings were adopted into. Scoping it to the
        // adoption branch is what left `contribution: None` on every reviewer
        // that ran — the data was there, one block too deep.
        let mut adopted_ledger: Option<leveler_lifecycle::EvidenceLedger> = None;
        if !result.findings.is_empty() {
            let mut ledger = crate::coding::turn::last_persisted_ledger(
                runner.stores.events.as_ref(),
                &runner.session_id,
            )
            .await?
            .unwrap_or_default();
            let adopted: Vec<String> = result
                .findings
                .iter()
                .map(|finding| ledger.adopt_finding(&id, nickname, finding))
                .collect();
            if let Some(pos) = summary.find('\n') {
                summary.insert_str(
                    pos + 1,
                    &format!("Structured findings adopted: {}.\n", adopted.join(", ")),
                );
            } else {
                summary.push_str(&format!(
                    "\nStructured findings adopted: {}.",
                    adopted.join(", ")
                ));
            }
            runner
                .log
                .append(
                    None,
                    EngineEvent::EvidenceLedgerUpdated {
                        ledger: ledger.clone(),
                    },
                    observer,
                )
                .await?;
            adopted_ledger = Some(ledger);
        }
        runner
            .log
            .append(
                None,
                EngineEvent::SubAgentFinished {
                    id: id.clone(),
                    nickname: nickname.to_string(),
                    ok: result.ok,
                    summary: leveler_core::truncate_head_bytes(summary.trim(), 4000, "…"),
                    // A reviewer that ran always reports a projection. Zero
                    // findings is a measured zero — a real fact about this
                    // review — and only an absent projection means "not
                    // measured". MA-VALUE-REVIEWER-PILOT could not tell those
                    // apart and reported five zero-finding reviewers that had
                    // every one of them reported.
                    contribution: Some(
                        leveler_lifecycle::ChildResultProjection::from_findings(
                            &id,
                            nickname,
                            adopted_ledger
                                .as_ref()
                                .map(|l| l.findings.as_slice())
                                .unwrap_or(&[]),
                        )
                        .with_profile(
                            profile_id_trace,
                            profile_role_trace,
                            read_only_trace,
                        ),
                    ),
                    outcome: Some(result.result.status),
                    stop: Some(result.stop),
                    limit: result.limit,
                },
                observer,
            )
            .await?;
        Ok::<HarnessChildOutcome, EngineError>(HarnessChildOutcome {
            completed: result.ok,
            findings: result.findings.len(),
            report: result.result.findings.clone(),
        })
    }
    .await;
    match settlement {
        Ok(outcome) => Ok(outcome),
        Err(error) => match runner
            .reconcile_terminal_children(leveler_lifecycle::ChildStop::Failed, observer)
            .await
        {
            Ok(()) => Err(ReviewRunError::Settlement(error)),
            Err(settlement_error) => Err(ReviewRunError::Unclosed(format!(
                "required review persistence failed ({error}); child settlement also failed: \
                 {settlement_error}"
            ))),
        },
    }
}

#[derive(Debug, thiserror::Error)]
enum ReviewRunError {
    #[error("{0}")]
    Launch(EngineError),
    #[error("{0}")]
    Settlement(EngineError),
    #[error("{0}")]
    Unclosed(String),
}
/// Whether this session has an independent review on record.
///
/// R007b N7: the reviewer designation only means something if the runtime can
/// answer "was this reviewed?" from durable history rather than from a task
/// card. The role lives on `SubAgentStarted` and the terminal on
/// `SubAgentFinished`, so a review counts only when the same agent id appears
/// in both — a reviewer that started and died without finishing has not
/// reviewed anything (N1's shape, deliberately not credited).
#[cfg(test)]
pub(crate) async fn session_had_review(
    events: &dyn EventStore,
    session_id: &SessionId,
) -> Result<bool, EngineError> {
    let mut reviewers: std::collections::HashSet<String> = std::collections::HashSet::new();
    let rows = events
        .load_by_types(session_id, &["sub_agent_started", "sub_agent_finished"])
        .await?;
    for row in rows {
        match row.event_type.as_str() {
            "sub_agent_started" => {
                if let Ok(EngineEvent::SubAgentStarted { id, role, .. }) =
                    EngineEvent::from_payload(&row.payload)
                    && role.eq_ignore_ascii_case("reviewer")
                {
                    reviewers.insert(id);
                }
            }
            "sub_agent_finished" => {
                if let Ok(EngineEvent::SubAgentFinished { id, ok, .. }) =
                    EngineEvent::from_payload(&row.payload)
                    && ok
                    && reviewers.contains(&id)
                {
                    return Ok(true);
                }
            }
            _ => {}
        }
    }
    Ok(false)
}

/// How the closure-boundary review ended. Recorded through `ReviewStage`
/// events; never a verdict on the task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClosureReview {
    NotRequired,
    Completed { findings: usize },
    Incomplete { findings: usize },
    LaunchFailed,
}

/// Surface the review outcome as completion warnings. The review is advisory
/// evidence, but whether it ran and what it reported belongs on the terminal.
fn apply_review_warnings(report: &mut TaskReport, review: ClosureReview) {
    if report.outcome != TaskOutcome::Completed {
        return;
    }
    match review {
        ClosureReview::Completed { findings: 0 } | ClosureReview::NotRequired => {}
        ClosureReview::Completed { findings } => report.completion_warnings.push(format!(
            "required independent review reported {findings} finding(s)"
        )),
        ClosureReview::Incomplete { findings: 0 } | ClosureReview::LaunchFailed => {
            report
                .completion_warnings
                .push("required independent review did not complete".into());
        }
        ClosureReview::Incomplete { findings } => report.completion_warnings.push(format!(
            "required independent review stopped early after reporting {findings} finding(s)"
        )),
    }
}

/// What a harness-launched read-only child came back with.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HarnessChildOutcome {
    /// Whether it reached a clean terminal. A child that ran out of rounds is
    /// `false` even when it wrote something useful.
    completed: bool,
    findings: usize,
    /// The child's own report: findings, and the text the caller reads.
    report: String,
}

/// Cap on the unified diff embedded in a reviewer brief. Beyond it the diff is
/// truncated with an explicit marker — a truncated diff plus an instruction
/// beats a bare file list, which sent every reviewer into whole-repo
/// reconstruction (7/7 recent launches died at their round budget with zero
/// findings, last words "scan the whole tree…").
const REVIEW_DIFF_MAX_BYTES: usize = 60 * 1024;

/// Best-effort unified diff of `files` in `repo` for the reviewer brief:
/// `git diff` for tracked changes plus an explicit list of untracked (new)
/// files. `None` when git is unavailable or shows nothing (fall back to the
/// file-list brief).
async fn review_diff(repo: &std::path::Path, files: &[String]) -> Option<String> {
    let repo = repo.to_path_buf();
    let files = files.to_vec();
    tokio::task::spawn_blocking(move || {
        let run = |args: &[&str]| -> Option<String> {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .arg("--")
                .args(&files)
                .output()
                .ok()?;
            out.status
                .success()
                .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
        };
        let diff = run(&["diff"]).unwrap_or_default();
        let untracked: Vec<String> = run(&["status", "--porcelain"])
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.strip_prefix("?? ").map(|p| p.trim().to_string()))
            .collect();
        if diff.trim().is_empty() && untracked.is_empty() {
            return None;
        }
        let mut out = String::new();
        if !diff.trim().is_empty() {
            if diff.len() > REVIEW_DIFF_MAX_BYTES {
                let mut cut = REVIEW_DIFF_MAX_BYTES;
                while !diff.is_char_boundary(cut) {
                    cut -= 1;
                }
                out.push_str(&diff[..cut]);
                out.push_str("\n… [diff truncated — run `git diff -- <file>` for the rest]\n");
            } else {
                out.push_str(&diff);
            }
        }
        if !untracked.is_empty() {
            out.push_str("\nNEW (untracked) files, each entirely part of the change:\n");
            for path in untracked {
                out.push_str(&format!("- {path}\n"));
            }
        }
        Some(out)
    })
    .await
    .ok()
    .flatten()
}

/// The brief handed to a harness-launched reviewer.
///
/// It names the task and the change (the diff when git can produce one, otherwise
/// the file list). How far to read, and whether to run anything, is the reviewer's
/// decision. The seat is already read-only.
fn review_brief(goal: &str, files: &[String], diff: Option<&str>) -> String {
    // A wide diff is exactly the case that triggers review; listing hundreds of
    // paths would spend the reviewer's context before it reads anything.
    const MAX_LISTED: usize = 40;
    let listed = files
        .iter()
        .take(MAX_LISTED)
        .map(|path| format!("- {path}"))
        .collect::<Vec<_>>()
        .join("\n");
    let elided = files.len().saturating_sub(MAX_LISTED);
    let more = if elided > 0 {
        format!("\n- …and {elided} more file(s)")
    } else {
        String::new()
    };
    let change = match diff {
        Some(diff) => format!(
            "The change, as a unified diff of the changed file(s):\n\
             ```diff\n{diff}\n```"
        ),
        None => format!("Files changed:\n{listed}{more}"),
    };
    format!(
        "Independently review the change that was just made for this task.\n\n\
         Task: {goal}\n\n\
         {change}\n"
    )
}

/// Whether the runtime's own continuation path can resume this terminal.
///
/// This is the ONE rule behind two surfaces: what the client offers as `继续`
/// and what the resume path will accept. They must agree — a session that still
/// advertises a continuation while `继续` is refused (or the reverse) is a lie
/// in one of them.
///
/// Resumability is a different fact from the session's Goal route: a terminal
/// can be resumable while its goal no longer claims the conversation, and the
/// resume path reopens that goal by id when the user does continue.
///
/// It is a classification of the terminal fact, never of the user's words.
pub fn task_outcome_is_resumable(
    outcome: TaskOutcome,
    stop: Option<crate::StopReason>,
    failure: Option<&leveler_model::ModelError>,
) -> bool {
    use crate::StopReason;
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
        // A blocked goal still owes its work, so `继续` retries that same goal.
        // It no longer holds the session's route: the window ended, and the
        // next ordinary message is a conversation.
        TaskOutcome::Blocked => true,
        TaskOutcome::Cancelled => false,
    }
}

/// End the session's Goal route at a goal window's terminal.
///
/// The route answers exactly one question: is this session's next ordinary
/// message a piece of goal work? Two kinds of terminal end it, because the
/// window no longer speaks for the conversation:
///
/// - delivered or cancelled: there is nothing left to continue;
/// - blocked or failed: the model handed control back, and the user's next
///   message is them answering — a question, not a new work order.
///
/// Leaving the route behind after one of those is what minted a NEW goal whose
/// objective was the sentence the user typed (`为什么没有权限？` — the reported
/// accident).
///
/// A PAUSED window is different: an interruption or a round boundary left the
/// work mid-flight with nothing for the user to answer, so the session keeps its
/// route and the supplement they type next continues the same goal (one of the
/// reasons a goal survives its work window at all).
///
/// Either way the goal keeps its identity, objective and recovery path: a
/// continuation re-enters this exact goal by id (see `resume_goal`), and work
/// declared from a Chat turn joins the task's one owed goal instead of opening a
/// duplicate (see `TurnGoalBinder::bind`).
fn end_session_goal_route(terminal: &mut leveler_engine::TaskTerminal, goal_session: bool) {
    let Some(goal) = terminal.goal.as_mut() else {
        return;
    };
    let paused = matches!(
        terminal.outcome,
        TaskOutcome::Interrupted | TaskOutcome::BudgetLimited
    ) || terminal
        .reason
        .as_deref()
        .is_some_and(crate::executor::closeout::is_user_pause_detail);
    goal.ends_session_goal = goal_session && !paused;
}

/// Map non-success agent stops for Direct conclude (shipped path used by
/// `conclude_direct`).
pub(crate) fn direct_non_success_outcome(stop: crate::StopReason) -> Option<TaskOutcome> {
    use crate::StopReason as S;
    match stop {
        // A declared end is complete; the runtime does not add another gate.
        S::Completed | S::Answered => None,
        // The round ceiling is a resource boundary, not a model failure: a
        // ceiling stop is BudgetLimited — incomplete and resumable — the same
        // class as an exhausted budget.
        S::BudgetExhausted | S::TurnLimitReached => Some(TaskOutcome::BudgetLimited),
        // The model said the goal cannot be reached as stated.
        S::Blocked => Some(TaskOutcome::Blocked),
        // A stalled work window leaves its goal owed and explicitly resumable.
        S::Stalled => Some(TaskOutcome::Interrupted),
        S::Incomplete => Some(TaskOutcome::Failed),
    }
}
#[cfg(test)]
mod review_brief_tests {
    use super::*;

    /// Accident regression (reviewer stability): a brief that only lists file
    /// paths makes the reviewer re-derive the change by reading whole files —
    /// 7/7 recent production launches died at their round budget with zero
    /// findings. With a diff available, the brief must scope the review to it
    /// and demand a bounded conclusion.
    #[test]
    fn a_brief_with_a_diff_scopes_review_to_the_diff_and_demands_a_verdict() {
        let files = vec!["src/a.rs".to_string()];
        let brief = review_brief(
            "add --json",
            &files,
            Some("--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1 @@\n-old\n+new"),
        );
        assert!(brief.contains("```diff"), "{brief}");
        assert!(brief.contains("-old\n+new"), "{brief}");
        assert!(brief.contains("Task: add --json"), "{brief}");
        for banned in [
            "do not survey",
            "Do not re-run",
            "the moment you confirm",
            "conclude immediately",
        ] {
            assert!(!brief.contains(banned), "{banned} in {brief}");
        }
    }

    #[test]
    fn a_brief_without_a_diff_still_directs_diff_first_and_bounded_conclusion() {
        let files = vec!["src/a.rs".to_string(), "src/b.rs".to_string()];
        let brief = review_brief("add --json", &files, None);
        assert!(brief.contains("- src/a.rs"), "{brief}");
        assert!(brief.contains("- src/b.rs"), "{brief}");
        assert!(!brief.contains("git diff -- <file>"), "{brief}");
        assert!(!brief.contains("Do not survey"), "{brief}");
    }

    #[tokio::test]
    async fn review_diff_reports_tracked_hunks_and_untracked_files() {
        let dir = std::env::temp_dir().join(format!(
            "leveler-review-diff-{}",
            std::process::id() as u64 * 37 + 5
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(&dir)
                    .args(args)
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(dir.join("a.txt"), "old\n").unwrap();
        git(&["add", "a.txt"]);
        git(&["commit", "-qm", "init"]);
        std::fs::write(dir.join("a.txt"), "new\n").unwrap();
        std::fs::write(dir.join("fresh.txt"), "brand new\n").unwrap();

        let diff = review_diff(&dir, &["a.txt".to_string(), "fresh.txt".to_string()])
            .await
            .expect("a real change must produce a diff");
        assert!(diff.contains("-old"), "{diff}");
        assert!(diff.contains("+new"), "{diff}");
        assert!(diff.contains("NEW (untracked)"), "{diff}");
        assert!(diff.contains("fresh.txt"), "{diff}");

        // No git repo → honest None (brief falls back to the file list form).
        let bare = std::env::temp_dir().join(format!(
            "leveler-review-diff-bare-{}",
            std::process::id() as u64 * 37 + 6
        ));
        let _ = std::fs::remove_dir_all(&bare);
        std::fs::create_dir_all(&bare).unwrap();
        std::fs::write(bare.join("a.txt"), "x\n").unwrap();
        assert!(review_diff(&bare, &["a.txt".to_string()]).await.is_none());
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&bare).ok();
    }

    #[test]
    fn an_oversized_diff_is_truncated_with_an_explicit_marker() {
        // The truncation lives in review_diff's blocking closure; assert the
        // brief side stays well-formed with a marker-bearing diff.
        let big = "x".repeat(10);
        let brief = review_brief(
            "goal",
            &["a".to_string()],
            Some(&format!(
                "{big}\n… [diff truncated — run `git diff -- <file>` for the rest]"
            )),
        );
        assert!(brief.contains("diff truncated"), "{brief}");
    }
}

#[cfg(test)]
mod terminal_mapping_tests {
    use super::*;

    /// The session status is read off how the run ended.
    #[test]
    fn terminal_status_follows_the_stop_reason_not_the_checks() {
        let report = TaskReport::new(
            TaskOutcome::Completed,
            String::new(),
            vec!["a.rs".into()],
            crate::StopReason::Incomplete,
            1,
        );
        assert_eq!(terminal_status_for(&report).0, SessionStatus::Incomplete);

        let clean = TaskReport::new(
            TaskOutcome::Completed,
            String::new(),
            vec!["a.rs".into()],
            crate::StopReason::Completed,
            1,
        );
        assert_eq!(
            terminal_status_for(&clean),
            (SessionStatus::Completed, AgentState::Complete),
            "a completed task remains completed"
        );

        let blocked = TaskReport::new(
            TaskOutcome::Blocked,
            String::new(),
            vec![],
            crate::StopReason::Blocked,
            1,
        );
        assert_eq!(terminal_status_for(&blocked).0, SessionStatus::Blocked);
    }

    #[test]
    fn non_success_stops_map_to_their_outcome() {
        // conclude_direct uses this mapping: an Incomplete stop surfaces as
        // TaskOutcome::Failed, a model-declared block as Blocked.
        assert_eq!(
            direct_non_success_outcome(crate::StopReason::Incomplete),
            Some(TaskOutcome::Failed)
        );
        assert_eq!(
            direct_non_success_outcome(crate::StopReason::Blocked),
            Some(TaskOutcome::Blocked)
        );
        assert_eq!(
            direct_non_success_outcome(crate::StopReason::Stalled),
            Some(TaskOutcome::Interrupted)
        );
        assert_eq!(
            direct_non_success_outcome(crate::StopReason::BudgetExhausted),
            Some(TaskOutcome::BudgetLimited)
        );
        assert_eq!(
            direct_non_success_outcome(crate::StopReason::TurnLimitReached),
            Some(TaskOutcome::BudgetLimited),
            "after multi-window continuation, a ceiling stop is a resource boundary \
             (incomplete + resumable), not a model failure"
        );
        assert_eq!(
            direct_non_success_outcome(crate::StopReason::Answered),
            None,
            "Answered is already a successful terminal"
        );
        assert_eq!(
            direct_non_success_outcome(crate::StopReason::Completed),
            None
        );
    }
}

#[cfg(test)]
mod session_review_tests {
    use leveler_core::SessionId;
    use leveler_engine::{EngineEvent, EventLog};
    use leveler_storage::MemoryEventStore;

    fn started(id: &str, role: &str) -> EngineEvent {
        EngineEvent::SubAgentStarted {
            id: id.into(),
            nickname: "Newton".into(),
            role: role.into(),
            task: "review the work".into(),
            profile_id: None,
            profile_role: None,
            read_only: true,
            spec: None,
        }
    }

    fn finished(id: &str) -> EngineEvent {
        EngineEvent::SubAgentFinished {
            id: id.into(),
            nickname: "Newton".into(),
            ok: true,
            summary: "done".into(),
            contribution: None,
            outcome: None,
            stop: None,
            limit: None,
        }
    }

    /// A reviewer that started and died without finishing has reviewed
    /// nothing. Crediting it would let a lost child stand in for a review
    /// that never produced a verdict.
    #[tokio::test]
    async fn a_reviewer_that_never_finished_has_not_reviewed() {
        let store = MemoryEventStore::new();
        let session = SessionId::generate();
        let log = EventLog::new(&store, session.clone());
        let mut sink = |_: EngineEvent| {};
        log.append(None, started("r1", "reviewer"), &mut sink)
            .await
            .unwrap();
        assert!(!super::session_had_review(&store, &session).await.unwrap());
    }

    /// The started/finished pair on the same agent id is what counts.
    #[tokio::test]
    async fn a_reviewer_that_finished_counts_as_a_review() {
        let store = MemoryEventStore::new();
        let session = SessionId::generate();
        let log = EventLog::new(&store, session.clone());
        let mut sink = |_: EngineEvent| {};
        log.append(None, started("r1", "reviewer"), &mut sink)
            .await
            .unwrap();
        log.append(None, finished("r1"), &mut sink).await.unwrap();
        assert!(super::session_had_review(&store, &session).await.unwrap());
    }

    /// Only a reviewer counts: an explorer that started and finished is not
    /// an independent review of the work.
    #[tokio::test]
    async fn an_explorer_is_not_a_review() {
        let store = MemoryEventStore::new();
        let session = SessionId::generate();
        let log = EventLog::new(&store, session.clone());
        let mut sink = |_: EngineEvent| {};
        log.append(None, started("e1", "explorer"), &mut sink)
            .await
            .unwrap();
        log.append(None, finished("e1"), &mut sink).await.unwrap();
        assert!(!super::session_had_review(&store, &session).await.unwrap());
    }
}

#[cfg(test)]
mod seed_tests {
    use super::*;
    use leveler_lifecycle::{PlanOrigin, PlanStep};

    fn plan_with(status: &str) -> PlanState {
        PlanState {
            steps: vec![PlanStep {
                step: "step".into(),
                status: status.into(),
                id: Some("1".into()),
                origin: PlanOrigin::ModelExplicit,
            }],
        }
    }

    #[test]
    fn a_fully_completed_plan_is_a_closed_epoch() {
        assert!(!prior_epoch_open(Some(&plan_with("completed")), None));
    }

    #[test]
    fn an_incomplete_plan_is_an_open_epoch() {
        assert!(prior_epoch_open(Some(&plan_with("pending")), None));
    }

    #[test]
    fn closing_or_terminal_progress_is_a_closed_epoch() {
        let mut closing = ProgressLedger::default();
        closing.enter_closing();
        assert!(!prior_epoch_open(None, Some(&closing)));
        let mut terminal = ProgressLedger::default();
        terminal.enter_closed();
        assert!(!prior_epoch_open(None, Some(&terminal)));
    }

    #[test]
    fn active_progress_is_an_open_epoch() {
        assert!(prior_epoch_open(None, Some(&ProgressLedger::default())));
    }

    #[test]
    fn absent_prior_state_is_an_open_epoch() {
        assert!(prior_epoch_open(None, None));
    }

    #[test]
    fn coding_mutation_preserves_the_answered_declaration() {
        let mut report = TaskReport::new(
            TaskOutcome::Completed,
            "done".to_string(),
            vec!["src/lib.rs".to_string()],
            StopReason::Answered,
            1,
        );
        assert_eq!(task_terminal_stop(&report), StopReason::Answered);

        report.modified_files.clear();
        assert_eq!(task_terminal_stop(&report), StopReason::Answered);
    }
}

#[cfg(test)]
mod continuation_lineage_tests {
    use super::*;

    fn turn(id: &str, session: &str, ordinal: i64, kind: &str) -> leveler_storage::TurnRecord {
        leveler_storage::TurnRecord {
            id: id.into(),
            session_id: session.into(),
            ordinal,
            kind: kind.into(),
            payload: None,
            status: "failed".into(),
            created_at: leveler_core::now().to_rfc3339(),
            finished_at: None,
            owner_boot_id: None,
        }
    }

    #[test]
    fn continuation_root_must_be_same_session_earlier_and_same_kind() {
        let root = turn("root", "session-a", 1, "chat");
        let continuation = turn("next", "session-a", 2, "chat");
        assert!(continuation_root_is_valid(
            std::slice::from_ref(&root),
            &continuation,
            &TurnId::new("root"),
            None
        ));

        let cross_session = turn("root", "session-b", 1, "chat");
        assert!(!continuation_root_is_valid(
            &[cross_session],
            &continuation,
            &TurnId::new("root"),
            None
        ));
        let wrong_kind = turn("root", "session-a", 1, "user");
        assert!(!continuation_root_is_valid(
            &[wrong_kind],
            &continuation,
            &TurnId::new("root"),
            None
        ));
        let future = turn("root", "session-a", 3, "chat");
        assert!(!continuation_root_is_valid(
            &[future],
            &continuation,
            &TurnId::new("root"),
            None
        ));
        let self_root = turn("next", "session-a", 2, "chat");
        assert!(!continuation_root_is_valid(
            &[self_root],
            &continuation,
            &TurnId::new("next"),
            None
        ));

        // A Chat turn upgraded by `/goal` is a valid Goal root when both
        // payloads name the same goal.
        let goal_id = leveler_core::GoalId::new("goal-1");
        let payload = serde_json::json!({
            "version": 2,
            "initiating_message": Message::text(Role::User, "start"),
            "objective": leveler_lifecycle::ObjectiveAnchor::from_session_goal("task"),
            "goal_id": goal_id,
        })
        .to_string();
        let mut chat_root = turn("root", "session-a", 1, "chat");
        chat_root.payload = Some(payload.clone());
        let mut goal_continuation = turn("next", "session-a", 2, "user");
        goal_continuation.payload = Some(payload);
        assert!(continuation_root_is_valid(
            std::slice::from_ref(&chat_root),
            &goal_continuation,
            &TurnId::new("root"),
            Some(&goal_id)
        ));
        let other = leveler_core::GoalId::new("goal-2");
        assert!(!continuation_root_is_valid(
            &[chat_root],
            &goal_continuation,
            &TurnId::new("root"),
            Some(&other)
        ));
    }

    #[test]
    fn only_bare_legacy_continue_commands_backtrack() {
        assert!(is_bare_legacy_continuation(&Message::text(
            Role::User,
            "继续"
        )));
        assert!(is_bare_legacy_continuation(&Message::text(
            Role::User,
            "continue"
        )));
        assert!(!is_bare_legacy_continuation(&Message::text(
            Role::User,
            "继续开发新功能"
        )));
        assert!(!is_bare_legacy_continuation(&Message::text(
            Role::User,
            "Continue implementing feature X"
        )));
    }
}

/// The continuation rule every Goal-route decision reads. A wrong answer here
/// either strands owed work (a route released while `继续` still resumes it) or
/// keeps a Goal session alive after its goal is over (every later message opens
/// a new Goal). Both are product failures, so the table is locked.
#[cfg(test)]
mod continuation_rule_tests {
    use super::*;
    use leveler_lifecycle::StopReason;

    fn recoverable() -> leveler_model::ModelError {
        leveler_model::ModelError::new(leveler_model::ModelErrorKind::Transport, "connection reset")
    }

    fn hard() -> leveler_model::ModelError {
        leveler_model::ModelError::new(leveler_model::ModelErrorKind::InvalidRequest, "bad request")
    }

    #[test]
    fn a_declared_completion_is_final_and_an_answer_is_not() {
        assert!(!task_outcome_is_resumable(
            TaskOutcome::Completed,
            Some(StopReason::Completed),
            None
        ));
        // A legacy terminal with no stop is a declaration too.
        assert!(!task_outcome_is_resumable(
            TaskOutcome::Completed,
            None,
            None
        ));
        // A quiet answer proved nothing about the work.
        assert!(task_outcome_is_resumable(
            TaskOutcome::Completed,
            Some(StopReason::Answered),
            None
        ));
    }

    #[test]
    fn owed_and_interrupted_work_stays_resumable() {
        for outcome in [
            TaskOutcome::Interrupted,
            TaskOutcome::BudgetLimited,
            TaskOutcome::Blocked,
        ] {
            assert!(
                task_outcome_is_resumable(outcome, None, None),
                "{outcome:?} still owes its work"
            );
        }
    }

    #[test]
    fn failures_are_resumable_only_when_the_fault_is_recoverable() {
        assert!(task_outcome_is_resumable(
            TaskOutcome::Failed,
            None,
            Some(&recoverable())
        ));
        assert!(!task_outcome_is_resumable(
            TaskOutcome::Failed,
            None,
            Some(&hard())
        ));
        assert!(
            !task_outcome_is_resumable(TaskOutcome::Failed, None, None),
            "an untyped failure is not evidence of a recoverable fault"
        );
    }

    #[test]
    fn an_explicit_cancellation_is_never_resumable() {
        assert!(!task_outcome_is_resumable(
            TaskOutcome::Cancelled,
            None,
            None
        ));
    }
}

#[cfg(test)]
mod compaction_facts_tests {
    use super::{
        COMPACTION_FACTS_MARKER, CompactionFactsInput, MAX_COMPACTION_FACTS_CHARS,
        render_compaction_facts,
    };

    fn requirement(id: &str, text: &str) -> leveler_lifecycle::RequirementItem {
        leveler_lifecycle::RequirementItem {
            id: id.into(),
            text: text.into(),
            kind: leveler_lifecycle::RequirementKind::Named,
            target: None,
            source_quote: text.into(),
            check: None,
            state: leveler_lifecycle::RequirementState::Active,
            superseded_by: None,
        }
    }

    fn render(
        objective: Option<&str>,
        effective: &[leveler_lifecycle::RequirementItem],
        amendments: &[String],
        progress: Option<&leveler_lifecycle::ProgressLedger>,
    ) -> Option<String> {
        render_compaction_facts(&CompactionFactsInput {
            objective,
            effective,
            amendments,
            plan: None,
            progress,
            verification: &[],
            dangling: &[],
            background: &[],
        })
    }

    #[test]
    fn the_facts_block_names_the_objective_requirements_and_ordered_instructions() {
        let recorded = vec![requirement("R1", "tests pass")];
        let amendments = vec![
            "use an env var".to_string(),
            "do not edit config".to_string(),
        ];
        let text = render(Some("ship the feature"), &recorded, &amendments, None).unwrap();
        assert!(text.starts_with(COMPACTION_FACTS_MARKER), "{text}");
        assert!(text.contains("ship the feature"));
        assert!(text.contains("tests pass"));
        assert!(text.contains("the Gate uses exactly this set"), "{text}");
        let first = text.find("use an env var").unwrap();
        let later = text.find("do not edit config").unwrap();
        assert!(first < later, "oldest-to-newest order is preserved: {text}");
        assert!(text.contains("declared separately"));
    }

    /// The facts block mirrors the gate: a waived requirement is recorded
    /// history, not owed work, so it must not reappear as an obligation.
    #[test]
    fn a_waived_requirement_is_not_rendered_as_owed() {
        let mut waived = requirement("R1", "old docs");
        waived.state = leveler_lifecycle::RequirementState::Waived;
        let active = requirement("R2", "new docs");
        let text = render(Some("task"), &[waived, active], &[], None).unwrap();
        assert!(text.contains("R2"), "{text}");
        assert!(!text.contains("old docs"), "{text}");
    }

    #[test]
    fn no_durable_facts_means_no_block() {
        assert!(render(None, &[], &[], None).is_none());
    }

    #[test]
    fn a_large_fact_set_is_bounded_and_says_so() {
        let amendments: Vec<String> = (0..80)
            .map(|index| format!("instruction {index} {}", "x".repeat(400)))
            .collect();
        let text = render(Some("objective"), &[], &amendments, None).unwrap();
        assert!(
            text.len() <= MAX_COMPACTION_FACTS_CHARS + 64,
            "facts block must stay bounded: {}",
            text.len()
        );
        // The per-section caps usually keep the block under the hard bound;
        // when it does exceed it, the renderer truncates explicitly.
        if text.len() > MAX_COMPACTION_FACTS_CHARS {
            assert!(text.contains("facts truncated to the context budget"));
        }
    }

    #[test]
    fn progress_facts_are_included_without_raw_tool_output() {
        let progress = leveler_lifecycle::ProgressLedger {
            cumulative_commands: 7,
            cumulative_modified_files: 2,
            cumulative_modified_paths: vec!["src/a.rs".into(), "src/b.rs".into()],
            ..Default::default()
        };
        let text = render(Some("task"), &[], &[], Some(&progress)).unwrap();
        assert!(text.contains("commands=7"), "{text}");
        assert!(text.contains("src/a.rs, src/b.rs"), "{text}");
    }
}

#[cfg(test)]
mod effective_acceptance_facts_tests {
    use super::{COMPACTION_FACTS_MARKER, CompactionFactsInput, render_compaction_facts};

    fn req(id: usize, text: &str) -> leveler_lifecycle::RequirementItem {
        leveler_lifecycle::RequirementItem {
            id: format!("R{id}"),
            text: text.into(),
            kind: leveler_lifecycle::RequirementKind::Named,
            target: None,
            source_quote: text.into(),
            check: None,
            state: leveler_lifecycle::RequirementState::Active,
            superseded_by: None,
        }
    }

    #[test]
    fn more_than_eight_recorded_requirements_are_not_claimed_as_complete() {
        let effective: Vec<leveler_lifecycle::RequirementItem> = (0..10)
            .map(|i| req(i, &format!("deliverable-{i}")))
            .collect();
        let text = render_compaction_facts(&CompactionFactsInput {
            objective: Some("task"),
            effective: &effective,
            amendments: &[],
            plan: None,
            progress: None,
            verification: &[],
            dangling: &[],
            background: &[],
        })
        .unwrap();
        assert!(text.starts_with(COMPACTION_FACTS_MARKER));
        assert!(text.contains("deliverable-7"), "{text}");
        assert!(
            !text.contains("deliverable-8"),
            "only eight are expanded: {text}"
        );
        assert!(text.contains("…and 2 more recorded requirements"), "{text}");
    }
}
