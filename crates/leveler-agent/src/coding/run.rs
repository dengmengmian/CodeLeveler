//! The Coding task strategy: what a coding agent does with an engine turn.
//!
//! This is the harness half of what used to be one fused `TaskEngine`. The
//! engine below owns session/turn/task lifecycle, persistence and resume; the
//! decisions here — what a Coding task IS, when it is concluded, what gets
//! verified, whether a reviewer runs — belong to the domain and live here.

use std::path::PathBuf;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use leveler_core::{SessionId, TurnId};
use leveler_engine::{
    DanglingCall, EngineError, EngineEvent, EventLog, ExecutionKind, TaskEngine, TaskOutcome,
    TurnKind, TurnRunner,
};
use leveler_execution::{Approver, Clarifier, PermissionProfile, RiskLevel};
use leveler_lifecycle::{
    AgentState, PlanState, ProgressLedger, SessionStatus, StopReason, VerificationStatus,
};
use leveler_model::{Message, Role};
#[cfg(test)]
use leveler_storage::EventStore;
use leveler_verifier::{Verdict, VerificationPlan, VerificationReport, Verifier};

use crate::coding::factory::{ExecutorFactory, TurnProfile};
use crate::coding::turn::{TurnInput, drive_turn};
use crate::coding::workspace::GitWorkspace;
use crate::{ContinuationPolicy, StepLimits};

const FINALIZATION_VERIFICATION: &str = "verification";
const FINALIZATION_REVIEW: &str = "review";
const FINALIZATION_RESOLVING_OUTCOME: &str = "resolving_outcome";
const FINALIZATION_CONTINUATION_CHECKPOINT: &str = "continuation_checkpoint";
const FINALIZATION_PUBLISHING_TERMINAL: &str = "publishing_terminal";

fn engine_verification_observation(
    observation: &leveler_verifier::CheckObservation,
) -> leveler_engine::VerificationObservation {
    match observation {
        leveler_verifier::CheckObservation::Passed => {
            leveler_engine::VerificationObservation::Passed
        }
        leveler_verifier::CheckObservation::Failed => {
            leveler_engine::VerificationObservation::Failed
        }
        leveler_verifier::CheckObservation::NotRun(reason) => {
            leveler_engine::VerificationObservation::NotRun {
                reason: reason.as_str().to_string(),
            }
        }
    }
}

fn engine_verification_disposition(
    disposition: &leveler_verifier::GateDisposition,
) -> leveler_engine::VerificationDisposition {
    use leveler_verifier::{BaselineSource, GateDisposition, GateSkipReason};
    match disposition {
        GateDisposition::Required => leveler_engine::VerificationDisposition::Required,
        GateDisposition::Skipped(reason) => {
            let (reason, revision, source, failed_tests) = match reason {
                GateSkipReason::ConfirmedBaselineFailure {
                    revision,
                    provenance,
                } => (
                    "confirmed_baseline_failure".to_string(),
                    Some(revision.clone()),
                    Some(match provenance.source {
                        BaselineSource::DetachedWorktreeRerun => {
                            "detached_worktree_rerun".to_string()
                        }
                    }),
                    provenance.failed_tests.iter().cloned().collect(),
                ),
                GateSkipReason::NotApplicable => {
                    ("not_applicable".to_string(), None, None, Vec::new())
                }
                GateSkipReason::Superseded => ("superseded".to_string(), None, None, Vec::new()),
            };
            leveler_engine::VerificationDisposition::Skipped {
                reason,
                revision,
                source,
                failed_tests,
            }
        }
    }
}

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
/// Whether a dangling call may be re-run automatically after a crash.
///
/// This asks the TOOL, never the risk label. `RiskLevel::Safe` answers "does
/// this need approval" and it admits side effects — `create_checkpoint` resets
/// the rollback baseline, `wait_task` consumes a background task's one-time
/// settlement report, and both are Safe. Deriving replay-safety from risk (as this once did) would
/// silently undo the user's work during recovery.
///
/// A tool this build does not know, or one that never declared itself
/// replay-safe, is NOT replayed: recovery stops for human reconciliation.
/// `risk` is still consulted first as a coarse veto so a legacy event with no
/// persisted risk can never be replayed either.
fn is_auto_replayable(
    registry: &leveler_tools::ToolRegistry,
    name: &str,
    risk: Option<RiskLevel>,
) -> bool {
    matches!(risk, Some(RiskLevel::Safe)) && registry.replay_is_side_effect_free(name)
}

/// The Coding-domain half of a task: where the work happens and how its
/// completion is proven. Verification and baseline attribution live here —
/// they are the Coding completion gate's inputs, not runtime lifecycle.
#[derive(Clone)]
pub struct CodingTaskSpec {
    pub repository: PathBuf,
    pub mode: PermissionProfile,
    pub sandbox: bool,
    /// The post-edit verification plan (empty = nothing to verify → the task
    /// can at best finish `CompletedUnverified`).
    pub verification: VerificationPlan,
    /// The repo's `HEAD` at task start, used as the baseline for delta
    /// attribution of gate failures. Callers leave this `None`; the engine
    /// stamps it (from `git rev-parse HEAD`) before the first turn edits.
    pub base_commit: Option<String>,
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
/// Read off how the run ENDED, never off the verification verdict: a task the
/// model declared complete is a completed session whether or not the
/// project's checks passed — the checks are reported beside it as
/// [`VerificationStatus`], not folded into the status.
pub(crate) fn terminal_status_for(report: &TaskReport) -> (SessionStatus, AgentState) {
    use StopReason as S;
    match report.stop_reason {
        S::Completed | S::Answered | S::CompletedUnverified | S::CompletedChecksFailed => {
            (SessionStatus::Completed, AgentState::Complete)
        }
        S::Incomplete | S::BudgetExhausted | S::TurnLimitReached | S::Stalled => {
            (SessionStatus::Incomplete, AgentState::Execute)
        }
        S::Blocked => (SessionStatus::Blocked, AgentState::Execute),
    }
}

fn goal_owes_no_more_work(result: &Result<TaskReport, EngineError>) -> bool {
    match result {
        Ok(report) => matches!(
            report.outcome,
            TaskOutcome::Completed | TaskOutcome::Blocked | TaskOutcome::Failed
        ),
        Err(_) => false,
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
    /// What the project's own checks said over the final tree. Orthogonal to
    /// `outcome`.
    pub verification_status: VerificationStatus,
    pub final_text: String,
    pub modified_files: Vec<String>,
    pub verification: Option<VerificationReport>,
    /// The executor's stop reason (legacy status mapping needs its nuance).
    pub stop_reason: StopReason,
    /// The executor's concrete reason for a non-success stop, when available.
    pub stop_detail: Option<String>,
    pub rounds: u32,
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
}

impl TaskReport {
    /// A report with the always-present fields set and the orchestration-only
    /// extras (`verification`/`review`) defaulted to `None`.
    /// Sites that produce those set them via `TaskReport { field: Some(..),
    /// ..TaskReport::new(..) }`, so a new optional field defaults in one place.
    pub(crate) fn new(
        outcome: TaskOutcome,
        final_text: String,
        modified_files: Vec<String>,
        stop_reason: StopReason,
        rounds: u32,
    ) -> Self {
        Self {
            outcome,
            verification_status: VerificationStatus::NotRun,
            final_text,
            modified_files,
            verification: None,
            stop_reason,
            stop_detail: None,
            rounds,
            execution_duration_ms: 0,
            windows: 1,
            review: None,
            completion_warnings: Vec::new(),
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
        outcome.rounds,
    )
    .with_stop_detail(outcome.stop_detail);
    report.execution_duration_ms = execution_duration_ms;
    report
}

fn task_terminal_reason(report: &TaskReport) -> Option<String> {
    if report.outcome != TaskOutcome::Completed {
        return report.stop_detail.clone();
    }
    if task_terminal_stop(report) != StopReason::Completed {
        return report.stop_detail.clone();
    }
    match report.verification_status {
        VerificationStatus::Passed => None,
        VerificationStatus::Failed => Some(match report.verification.as_ref() {
            Some(verification) if !verification.scope_ok => format!(
                "modified files outside allowed scope: {}",
                verification.scope_violations.join(", ")
            ),
            Some(verification) => {
                let failed = verification
                    .failed_gates()
                    .into_iter()
                    .map(failed_gate_label)
                    .collect::<Vec<_>>();
                if failed.is_empty() {
                    "verification did not pass".to_string()
                } else {
                    format!("failed gate(s): {}", failed.join(", "))
                }
            }
            None => "verification did not pass".to_string(),
        }),
        VerificationStatus::NotRun | VerificationStatus::Unavailable => {
            if report.modified_files.is_empty() {
                return Some("no_code_changes".to_string());
            }
            Some(match report.verification.as_ref() {
                Some(verification) if !verification.has_gating_checks() => {
                    "no_automatic_verification".to_string()
                }
                Some(verification) => match verification.verdict() {
                    Verdict::Unverified(reason) => reason,
                    _ => "no_automatic_verification".to_string(),
                },
                None => "no_automatic_verification".to_string(),
            })
        }
    }
}

fn task_terminal_stop(report: &TaskReport) -> StopReason {
    if report.outcome == TaskOutcome::Completed
        && report.stop_reason == StopReason::Answered
        && !report.modified_files.is_empty()
    {
        // `Answered` is a truthful executor stop for pure Q&A. Once the Coding
        // Harness has observed a product mutation, the durable task terminal
        // must enter the completion+verification path; otherwise replay would
        // discard failed/unavailable verification and review warnings.
        StopReason::Completed
    } else {
        report.stop_reason
    }
}

fn failed_gate_label(check: &leveler_verifier::CheckOutcome) -> String {
    if check.failed_tests.is_empty() {
        return check.name.clone();
    }
    let shown = check
        .failed_tests
        .iter()
        .take(2)
        .map(String::as_str)
        .collect::<Vec<_>>();
    let remaining = check.failed_tests.len() - shown.len();
    if remaining == 0 {
        format!("{} ({})", check.name, shown.join(", "))
    } else {
        format!("{} ({}, +{} more)", check.name, shown.join(", "), remaining)
    }
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
}

#[async_trait::async_trait]
impl leveler_engine::ContextSummarizer for ModelSummarizer<'_> {
    async fn summarize(&self, messages: &[leveler_model::Message]) -> Option<String> {
        crate::summarize_with_model(
            self.runtime,
            self.model,
            None,
            messages,
            crate::COMPACT_KEEP_RECENT,
            0,
            self.cancellation,
        )
        .await
        .map(|summary| summary.text)
    }
}
/// Keep only the last `max` messages for Goal history injection (bounded).
pub(crate) fn bound_goal_history(
    messages: Vec<leveler_model::Message>,
    max: usize,
) -> Vec<leveler_model::Message> {
    if messages.len() <= max {
        return messages;
    }
    messages[messages.len() - max..].to_vec()
}
impl CodingRuntime {
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

    async fn load_request_transcript(
        &self,
        session_id: &SessionId,
        strict: Option<&str>,
    ) -> Result<leveler_engine::RawTranscript, EngineError> {
        let checkpoint_ordinal = crate::coding::checkpoint::checkpoint_transcript_ordinal(
            &self.engine.stores,
            session_id,
        )
        .await?;
        self.engine
            .load_request_transcript(session_id, checkpoint_ordinal, strict)
            .await
    }

    async fn assembled_prior(
        &self,
        log: &EventLog<'_>,
        session_id: &SessionId,
        raw: leveler_engine::RawTranscript,
        objective: Option<&str>,
        workspace: Option<&dyn crate::coding::checkpoint::WorkspaceFacts>,
        summarizer: &dyn leveler_engine::ContextSummarizer,
        cancellation: &CancellationToken,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
    ) -> Result<Vec<leveler_model::Message>, EngineError> {
        if let Some(prior) = self
            .checkpointed_prior(
                log,
                session_id,
                &raw,
                workspace,
                summarizer,
                cancellation,
                observer,
            )
            .await?
        {
            return Ok(prior);
        }
        let context = raw
            .assemble(
                log,
                Some(summarizer),
                objective,
                leveler_context::PRE_REQUEST_COMPACT_THRESHOLD,
            )
            .await?;
        if context.compacted {
            log.append(None, context.snapshot_event(), observer).await?;
        }
        Ok(context.prior)
    }

    async fn checkpointed_prior(
        &self,
        log: &EventLog<'_>,
        session_id: &SessionId,
        raw: &leveler_engine::RawTranscript,
        workspace: Option<&dyn crate::coding::checkpoint::WorkspaceFacts>,
        summarizer: &dyn leveler_engine::ContextSummarizer,
        _cancellation: &CancellationToken,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
    ) -> Result<Option<Vec<leveler_model::Message>>, EngineError> {
        let threshold = leveler_context::PRE_REQUEST_COMPACT_THRESHOLD;
        if leveler_context::estimate_tokens(&raw.messages) <= threshold {
            return Ok(None);
        }
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
            raw,
        )
        .await?
            && leveler_context::estimate_tokens(&prior) <= threshold
        {
            return Ok(Some(prior));
        }
        let summary = summarizer.summarize(&raw.messages).await;
        match crate::coding::checkpoint::create_goal_checkpoint(
            &self.engine,
            session_id,
            leveler_lifecycle::CheckpointReason::ContextCompaction,
            workspace,
            crate::coding::checkpoint::SemanticRecap::briefing(summary.as_deref()),
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
                let tail_start = raw
                    .messages
                    .len()
                    .saturating_sub(leveler_context::COMPACT_KEEP_RECENT);
                let mut prior = Vec::with_capacity(1 + raw.messages.len() - tail_start);
                prior.push(leveler_model::Message {
                    role: leveler_model::Role::User,
                    content: vec![leveler_model::ContentPart::Text {
                        text: record.payload.context_block(),
                    }],
                });
                prior.extend_from_slice(&raw.messages[tail_start..]);
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
    async fn prior_epoch_open(&self, session_id: &SessionId) -> Result<bool, EngineError> {
        let events = self.engine.stores.events.as_ref();
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
                workspace: spec.coding.repository.display().to_string(),
                goal: spec.runtime.goal.clone(),
                model: self.factory.model.to_string(),
                mode: mode_str(spec.coding.mode).to_string(),
                sandbox: spec.coding.sandbox,
                kind: spec.runtime.kind,
                axes: Some(leveler_engine::NewSessionAxes {
                    collaboration: "goal".to_string(),
                    work_profile: "balanced".to_string(),
                }),
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
        let goal_update = goal.map(|goal_id| leveler_storage::GoalTerminalUpdate {
            goal_id: goal_id.clone(),
            windows_delta: result.as_ref().map(|report| report.windows).unwrap_or(1),
            settle: goal_owes_no_more_work(result),
        });
        let mut terminal = match result {
            Ok(report) => {
                let (status, state) = terminal_status_for(report);
                leveler_engine::TaskTerminal {
                    outcome: report.outcome,
                    verification: report.verification_status,
                    reason: task_terminal_reason(report),
                    stop: Some(task_terminal_stop(report)),
                    status,
                    state,
                    goal: goal_update,
                    warnings: report.completion_warnings.clone(),
                }
            }
            Err(EngineError::Cancelled) => leveler_engine::TaskTerminal {
                outcome: TaskOutcome::Interrupted,
                verification: VerificationStatus::NotRun,
                reason: None,
                stop: None,
                status: SessionStatus::Interrupted,
                state: AgentState::Execute,
                goal: goal_update,
                warnings: Vec::new(),
            },
            Err(error) => leveler_engine::TaskTerminal {
                outcome: TaskOutcome::Failed,
                verification: VerificationStatus::NotRun,
                reason: Some(error.to_string()),
                stop: None,
                status: SessionStatus::Failed,
                state: AgentState::Failed,
                goal: goal_update,
                warnings: Vec::new(),
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
            let checkpoint = crate::coding::checkpoint::create_goal_checkpoint(
                &self.engine,
                session_id,
                leveler_lifecycle::CheckpointReason::Milestone,
                repo.map(GitWorkspace::new)
                    .as_ref()
                    .map(|w| w as &dyn crate::coding::checkpoint::WorkspaceFacts),
                None,
            )
            .await;
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
                // A continuation without its checkpoint is not safely
                // resumable. Make that the authoritative terminal truth
                // rather than logging a best-effort failure and claiming the
                // work window closed correctly.
                terminal.outcome = TaskOutcome::Failed;
                terminal.verification = VerificationStatus::NotRun;
                terminal.reason = Some(format!("continuation checkpoint failed: {error}"));
                terminal.stop = None;
                terminal.status = SessionStatus::Failed;
                terminal.state = AgentState::Failed;
                if let Some(goal) = terminal.goal.as_mut() {
                    goal.settle = true;
                }
                terminal.warnings.clear();
                goal_continues = false;
            }
        }
        if let Some(started) = resolution_phase {
            finish_finalization_phase(FINALIZATION_RESOLVING_OUTCOME, started);
        }
        // Detach the cleanup target set before publishing terminal. A new turn
        // may be admitted as soon as TaskFinished is projected; a later
        // session-wide lookup could otherwise kill processes owned by that new
        // turn.
        let mut cleanup_ticket = if !interrupted && !goal_continues {
            match self.factory.tool_context.session_scope.as_deref() {
                Some(scope) => Some(self.factory.background_tasks.detach_cleanup(scope).await),
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
        let cancelled_at_commit = cancellation.is_cancelled();
        if cancelled_at_commit {
            terminal.outcome = TaskOutcome::Interrupted;
            terminal.verification = VerificationStatus::NotRun;
            terminal.reason = None;
            terminal.stop = None;
            terminal.status = SessionStatus::Interrupted;
            terminal.state = AgentState::Execute;
            if let Some(goal) = terminal.goal.as_mut() {
                goal.settle = false;
            }
            terminal.warnings.clear();
            // Interrupted work retains its background processes. The ticket is
            // only a detached target set and has performed no side effect yet.
            cleanup_ticket = None;
        }
        let settled = self
            .engine
            .finish_task(token, session_id, terminal, observer)
            .await;
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
        if settled.is_ok()
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
        let goal = match self.open_or_reuse_goal(&token, &spec.runtime.goal).await {
            Ok(goal) => goal,
            Err(error) => {
                let result = Err(error);
                self.finish_from_result(
                    &token,
                    session_id,
                    &result,
                    None,
                    Some(&spec.coding.repository),
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
        log.append(
            None,
            EngineEvent::TaskStarted {
                goal: spec.runtime.goal.clone(),
                model: self.factory.model.to_string(),
                mode: mode_str(spec.coding.mode).to_string(),
                sandbox: spec.coding.sandbox,
                kind: spec.runtime.kind,
                task_id: Some(token.task_id.clone()),
            },
            observer,
        )
        .await?;

        // Stamp the pre-change baseline anchor onto the spec, captured before
        // any turn edits so the post-edit gate can tell this change's failures
        // from ones the repo already carried (see `baseline`). Carried on the
        // spec so every path that reaches `verify` — including resume — sees it
        // without threading. None (left as-is) outside a git work tree.
        let owned_spec;
        let spec = if spec.coding.base_commit.is_none() {
            if let Some(head) = crate::coding::baseline::capture_head(&spec.coding.repository).await
            {
                owned_spec = TaskSpec {
                    coding: CodingTaskSpec {
                        base_commit: Some(head),
                        ..spec.coding.clone()
                    },
                    runtime: spec.runtime.clone(),
                };
                &owned_spec
            } else {
                spec
            }
        } else {
            spec
        };

        // Orchestrate execution path removed; legacy kind falls through to direct.
        let terminal_cancellation = cancellation.clone();
        let result = match spec.runtime.kind {
            ExecutionKind::Direct => {
                self.run_direct(&log, &runner, spec, observer, cancellation)
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
            Some(&spec.coding.repository),
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
        // Anchor the baseline for THIS turn before it edits anything, exactly as
        // `run` does. Without it `reconcile_with_baseline` has nothing to compare
        // against, so failures the repository already carried are charged to this
        // turn: measured on a repo with one pre-existing red test, a 3-round edit
        // became a 45-round run in which the repair turn started rewriting
        // unrelated files trying to make someone else's failure go away.
        // A dirty worktree cannot use HEAD as a truthful before-change
        // snapshot, so `capture_head` deliberately returns None there and the
        // turn remains unattributed rather than inventing baseline authority.
        let owned_spec;
        let spec = if spec.coding.base_commit.is_none() {
            match crate::coding::baseline::capture_head(&spec.coding.repository).await {
                Some(head) => {
                    owned_spec = TaskSpec {
                        coding: CodingTaskSpec {
                            base_commit: Some(head),
                            ..spec.coding.clone()
                        },
                        runtime: spec.runtime.clone(),
                    };
                    &owned_spec
                }
                None => spec,
            }
        } else {
            spec
        };
        // A chat turn tolerates the odd unreadable legacy row (it only loses
        // context), unlike resume which must reconstruct exactly.
        let raw = self.load_request_transcript(session_id, None).await?;
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
        // Reconcile the crash window before continuing — the interactive path
        // is how a crashed session normally gets reopened (TUI/Web), and a
        // dangling mutating call means the workspace may already carry a side
        // effect the user has not seen. Same classification as resume: safe
        // reads replay, everything else stops for explicit acknowledgement.
        self.recover_crash_window(&log, observer, &cancellation)
            .await?;
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
            .next();
        let prior = self
            .assembled_prior(
                &log,
                session_id,
                raw,
                objective_hint,
                Some(&GitWorkspace::new(&spec.coding.repository)),
                &self.context_summarizer(&cancellation),
                &cancellation,
                observer,
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
        let prior_epoch_open = self.prior_epoch_open(session_id).await?;
        let initiating_message = Message {
            role: Role::User,
            content: content.clone(),
        };
        let terminal_cancellation = cancellation.clone();
        let result = async {
            let recorded = runner
                .run_turn(
                    TurnKind::Chat,
                    leveler_engine::TurnStart::Fresh(initiating_message),
                    observer,
                    cancellation.clone(),
                    |ports| {
                        drive_turn(
                            &self.factory,
                            chat_profile(spec),
                            TurnInput::Content { prior, content },
                            prior_epoch_open,
                            session_id.clone(),
                            self.engine.stores.events.clone(),
                            Some(crate::coding::checkpoint::CodingCheckpointContext::new(
                                self.engine.clone(),
                                Some(Arc::new(GitWorkspace::new(&spec.coding.repository))),
                            )),
                            ports,
                            cancellation.clone(),
                        )
                    },
                )
                .await?;
            self.conclude_direct(
                &log,
                &runner,
                spec,
                recorded.outcome,
                observer,
                cancellation,
            )
            .await
        }
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
            None,
            Some(&spec.coding.repository),
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
        if outcome == Some(TaskOutcome::Completed) {
            return Err(EngineError::Config(format!(
                "session {session_id} already completed ({}); start a new task instead",
                outcome.map(|o| o.as_str()).unwrap_or_default()
            )));
        }
        let raw = self
            .load_request_transcript(session_id, Some("transcript"))
            .await?;
        if raw.is_empty() {
            return Err(EngineError::Config(format!(
                "session {session_id} has no transcript to resume; \
                 for interactive chat reopen with: leveler tui --session {session_id}"
            )));
        }
        let token = self
            .engine
            .mark_running(session_id, AgentState::Execute)
            .await?;
        let goal = match self.open_or_reuse_goal(&token, &spec.runtime.goal).await {
            Ok(goal) => goal,
            Err(error) => {
                let result = Err(error);
                self.finish_from_result(
                    &token,
                    session_id,
                    &result,
                    None,
                    Some(&spec.coding.repository),
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
        // Long-goal P3: a valid durable checkpoint replaces the replayed old
        // context — resume receives the checkpoint block plus exactly the
        // transcript after its watermark. No usable checkpoint (none written,
        // corrupt, future version, stale watermark) falls back to the
        // pre-checkpoint full-history path below.
        // Same rules as chat: a checkpoint's block when one is fresh, else the
        // snapshot merged with the post-snapshot rows, folded if still oversized.
        let prior = self
            .assembled_prior(
                &log,
                session_id,
                raw,
                Some(spec.runtime.goal.as_str()),
                Some(&GitWorkspace::new(&spec.coding.repository)),
                &self.context_summarizer(&cancellation),
                &cancellation,
                observer,
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
        // never finished (the process died mid-execution) is replayed if
        // idempotent, or surfaced for approval if it has a side effect (M5).
        self.recover_crash_window(&log, observer, &cancellation)
            .await?;

        let terminal_cancellation = cancellation.clone();
        let result = self
            .resume_direct(&log, &runner, spec, prior, observer, cancellation)
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
            Some(&goal),
            Some(&spec.coding.repository),
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

    /// Reconcile the crash window on resume: for every tool call that started
    /// but never finished, replay it if idempotent, surface it for approval if
    /// it has a side effect, or skip it if it never actually ran. The
    /// reconciling `ToolCallFinished` goes to the event log only — the model
    /// re-drives from the last clean turn boundary (tool-call results are not
    /// injected into the transcript; see the M5 crash-window notes).
    async fn recover_crash_window(
        &self,
        log: &EventLog<'_>,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<(), EngineError> {
        for call in log.dangling_tool_calls().await? {
            let turn_id = call.turn_id.as_ref().map(|t| TurnId::new(t.clone()));
            let turn_ref = turn_id.as_ref();

            // Seeing ApprovalRequested without a persisted ApprovalResolved does
            // NOT prove dispatch never ran: the approval recorder queues the
            // resolution for persistence, then the executor may start the tool
            // before the event-log pump flushes it. A crash in that window looks
            // pending even though the side effect may have happened. Stop before
            // replay or model re-drive; a future explicit reconciliation flow can
            // resolve the dangling call after the user inspects the workspace.
            if call.pending_approval {
                return Err(EngineError::RecoveryConfirmationRequired {
                    call_id: call.call_id,
                    tool: call.name,
                });
            }

            if !is_auto_replayable(&self.factory.registry, &call.name, call.risk) {
                // Risk classification must precede argument parsing. Corrupt
                // arguments do not make a mutating/unknown call safe: its side
                // effect may already have happened before the crash.
                return Err(EngineError::RecoveryConfirmationRequired {
                    call_id: call.call_id,
                    tool: call.name,
                });
            }

            let args = match serde_json::from_str::<serde_json::Value>(&call.arguments) {
                Ok(value) => value,
                Err(_) => {
                    self.engine
                        .record_recovery_skip(
                            log,
                            &call,
                            turn_ref,
                            "corrupt arguments for safe tool; not replayed",
                            observer,
                        )
                        .await?;
                    continue;
                }
            };

            self.replay_dangling(log, &call, args, turn_ref, observer, cancellation)
                .await?;
        }
        Ok(())
    }

    /// Re-run a dangling tool and record its outcome as a `ToolCallFinished`. A
    /// replay failure is recorded as an errored result — it never fails resume.
    async fn replay_dangling(
        &self,
        log: &EventLog<'_>,
        call: &DanglingCall,
        args: serde_json::Value,
        turn_ref: Option<&TurnId>,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<(), EngineError> {
        // Execution during recovery goes through the host's reconciliation
        // entry, so the engine has exactly one auditable place that runs a
        // tool (enforced by the ToolHost boundary tripwire).
        // The replay gate above already established this tool declares itself
        // replay-safe; if the host still refuses to reconstruct the call, that
        // is a disagreement between two checks and must stop, not proceed.
        let Some((is_error, preview)) = crate::coding::recovery::replay_tool(
            &self.factory.registry,
            self.factory.tool_context.clone(),
            &call.name,
            args,
            cancellation,
        )
        .await
        else {
            return Err(EngineError::RecoveryConfirmationRequired {
                call_id: call.call_id.clone(),
                tool: call.name.clone(),
            });
        };
        log.append(
            turn_ref,
            EngineEvent::ToolCallFinished {
                exit_code: None,
                stop: None,
                call_id: call.call_id.clone(),
                name: call.name.clone(),
                is_error,
                preview,
                agent_id: call.agent_id.clone(),
                applied_diff: None,
            },
            observer,
        )
        .await
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
        observer: &mut (dyn FnMut(EngineEvent) + Send),
        cancellation: CancellationToken,
    ) -> Result<TaskReport, EngineError> {
        let recorded = runner
            .run_turn(
                TurnKind::User,
                leveler_engine::TurnStart::Resume,
                observer,
                cancellation.clone(),
                |ports| {
                    drive_turn(
                        &self.factory,
                        goal_profile(spec),
                        TurnInput::Resume(prior),
                        true,
                        runner.session_id.clone(),
                        self.engine.stores.events.clone(),
                        Some(crate::coding::checkpoint::CodingCheckpointContext::new(
                            self.engine.clone(),
                            Some(Arc::new(GitWorkspace::new(&spec.coding.repository))),
                        )),
                        ports,
                        cancellation.clone(),
                    )
                },
            )
            .await?;
        self.conclude_direct(log, runner, spec, recorded.outcome, observer, cancellation)
            .await
    }

    /// The direct strategy: one goal turn, then mechanical verification.
    async fn run_direct(
        &self,
        log: &EventLog<'_>,
        runner: &TurnRunner<'_>,
        spec: &TaskSpec,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
        cancellation: CancellationToken,
    ) -> Result<TaskReport, EngineError> {
        // Multi-turn Goal: inject bounded session history so follow-ups can
        // resolve deictic references ("刚才那个超时").
        let prior = self
            .bounded_session_history(
                log,
                &runner.session_id,
                &spec.runtime.goal,
                Some(&spec.coding.repository),
                &cancellation,
                observer,
            )
            .await?;
        let prior_epoch_open = self.prior_epoch_open(&runner.session_id).await?;
        let recorded = runner
            .run_turn(
                TurnKind::User,
                leveler_engine::TurnStart::Fresh(Message::text(
                    Role::User,
                    spec.runtime.goal.clone(),
                )),
                observer,
                cancellation.clone(),
                |ports| {
                    drive_turn(
                        &self.factory,
                        goal_profile(spec),
                        TurnInput::Goal {
                            goal: spec.runtime.goal.clone(),
                            prior,
                        },
                        prior_epoch_open,
                        runner.session_id.clone(),
                        self.engine.stores.events.clone(),
                        Some(crate::coding::checkpoint::CodingCheckpointContext::new(
                            self.engine.clone(),
                            Some(Arc::new(GitWorkspace::new(&spec.coding.repository))),
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
        self.conclude_direct(log, runner, spec, recorded.outcome, observer, cancellation)
            .await
    }

    /// Load session messages (prefer snapshot), bound length for Goal injection.
    #[allow(clippy::too_many_arguments)]
    async fn bounded_session_history(
        &self,
        log: &EventLog<'_>,
        session_id: &SessionId,
        goal: &str,
        repo: Option<&std::path::Path>,
        cancellation: &CancellationToken,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
    ) -> Result<Vec<leveler_model::Message>, EngineError> {
        const GOAL_HISTORY_MAX: usize = 24;
        let raw = self.load_request_transcript(session_id, None).await?;
        if raw.is_empty() {
            return Ok(Vec::new());
        }
        // Long-goal P3: the interactive multi-turn path is where a long
        // session's history actually grows — over the fold threshold it
        // continues from a durable checkpoint (fresh or cut here) instead of
        // a blunt last-N tail of replayed history.
        if let Some(prior) = self
            .checkpointed_prior(
                log,
                session_id,
                &raw,
                repo.map(GitWorkspace::new)
                    .as_ref()
                    .map(|w| w as &dyn crate::coding::checkpoint::WorkspaceFacts),
                &self.context_summarizer(cancellation),
                cancellation,
                observer,
            )
            .await?
        {
            return Ok(prior);
        }
        let context = raw
            .assemble(
                log,
                None,
                Some(goal),
                u64::from(crate::coding::policy::CHAT_CONTEXT_BUDGET),
            )
            .await?;
        Ok(bound_goal_history(context.prior, GOAL_HISTORY_MAX))
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
        modified_files: &[String],
        execution_duration_ms: u64,
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
        use crate::coding::policy::IndependentReviewPolicy;
        let reason = match self.factory.independent_review {
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
        let diff = review_diff(&spec.coding.repository, modified_files).await;
        match run_review(
            runner,
            &self.factory,
            goal_profile(spec),
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
        outcome: crate::AgentOutcome,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
        cancellation: CancellationToken,
    ) -> Result<TaskReport, EngineError> {
        // Goal continuation and bounded budget extension already happened in
        // `supervise` — one loop, one decision point (convergence plan phase 4).
        //
        // What happens here is mechanical bookkeeping, not judgement: the
        // run's stop reason names the outcome, the project's own checks run
        // once over the final tree and are reported beside it, and an
        // explicitly configured reviewer is launched. Nothing here repairs
        // on the model's behalf, re-reads the goal, or downgrades a completed
        // run because a heuristic disagreed with the model.
        let mut task_report =
            if let Some(terminal) = direct_non_success_outcome(outcome.stop_reason) {
                report_from_agent_outcome(outcome, terminal)
            } else if outcome.modified_files.is_empty() || !spec.coding.verification.has_gates() {
                // No mutation, or no checks configured: nothing to run, and the
                // report says so instead of pretending a verdict.
                report_from_agent_outcome(outcome, TaskOutcome::Completed)
            } else {
                let report = self
                    .verify(
                        log,
                        spec,
                        &[],
                        &outcome.modified_files,
                        observer,
                        &cancellation,
                    )
                    .await?;
                let verification_status = verification_status_of(&report);
                let mut base = report_from_agent_outcome(outcome, TaskOutcome::Completed);
                let baseline_failures = report.confirmed_baseline_failures();
                if !baseline_failures.is_empty() {
                    base.completion_warnings.push(format!(
                        "mechanically confirmed baseline failure: {}",
                        baseline_failures
                            .iter()
                            .map(|check| check.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                TaskReport {
                    verification: Some(report),
                    verification_status,
                    ..base
                }
            };

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
                &task_report.modified_files,
                task_report.execution_duration_ms,
                observer,
                &cancellation,
            )
            .await?;
        if let Some(started) = review_phase {
            finish_finalization_phase(FINALIZATION_REVIEW, started);
        }
        if task_report.outcome == TaskOutcome::Completed {
            match review {
                ClosureReview::Completed { findings: 0 } | ClosureReview::NotRequired => {}
                ClosureReview::Completed { findings } => task_report.completion_warnings.push(
                    format!("required independent review reported {findings} finding(s)"),
                ),
                ClosureReview::Incomplete { findings: 0 } | ClosureReview::LaunchFailed => {
                    task_report
                        .completion_warnings
                        .push("required independent review did not complete".into());
                }
                ClosureReview::Incomplete { findings } => task_report.completion_warnings.push(
                    format!(
                        "required independent review stopped early after reporting {findings} finding(s)"
                    ),
                ),
            }
        }
        Ok(task_report)
    }

    async fn verify(
        &self,
        log: &EventLog<'_>,
        spec: &TaskSpec,
        allowed_paths: &[String],
        modified_files: &[String],
        observer: &mut (dyn FnMut(EngineEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<VerificationReport, EngineError> {
        let verification_phase =
            begin_finalization_phase(log, FINALIZATION_VERIFICATION, observer).await;
        log.append(None, EngineEvent::VerificationStarted, observer)
            .await?;
        let verifier = Verifier::with_environment(
            &spec.coding.repository,
            self.factory.tool_context.execution.environment.clone(),
        );
        let mut plan = gate_plan(spec);
        // Blast-radius scoping: a change that touches no compiled input (docs,
        // scripts, lock files) must not run — and be blamed for — the whole
        // workspace's pre-existing red. Downgrades those gates to non-gating.
        plan.scope_gates_to_changes(modified_files);
        let mut report = verifier
            .verify(
                &plan,
                allowed_paths,
                modified_files,
                cancellation,
                &mut |_| {},
            )
            .await;

        // Attribute pre-existing/flaky failures to the baseline so only THIS
        // change's failures gate completion. No-op when the gate is green or no
        // baseline is available (`base_commit` captured at task start).
        if let Some(base_commit) = spec.coding.base_commit.as_deref() {
            crate::coding::baseline::reconcile_with_baseline(
                &mut report,
                &spec.coding.repository,
                base_commit,
                &plan,
                modified_files,
                self.factory.tool_context.execution.environment.clone(),
                cancellation,
            )
            .await;
        }
        for check in &report.checks {
            log.append(
                None,
                EngineEvent::VerificationCheck {
                    name: check.name.clone(),
                    // The durable vocabulary, from the owner of the type.
                    // `format!("{:?}").to_lowercase()` wrote `toolmissing`,
                    // which this event's own contract spells `tool_missing`.
                    status: check.legacy_status().as_str().to_string(),
                    observation: Some(engine_verification_observation(&check.observation)),
                    disposition: Some(engine_verification_disposition(&check.disposition)),
                    execution: check.execution.as_ref().map(|execution| {
                        leveler_engine::VerificationExecution {
                            program: execution.program.clone(),
                            args: execution.args.clone(),
                            exit_code: execution.exit_code,
                            timed_out: execution.timed_out,
                        }
                    }),
                    evidence: matches!(
                        &check.observation,
                        leveler_verifier::CheckObservation::Failed
                            | leveler_verifier::CheckObservation::NotRun(_)
                    )
                    .then(|| check.evidence.clone()),
                },
                observer,
            )
            .await?;
        }
        log.append(
            None,
            EngineEvent::VerificationFinished {
                // The completion gate, which is deliberately true for a run
                // that owed no check.
                passed: report.passed(),
                // The fact, mapped by the one mapper every consumer shares.
                verification: Some(verification_status_of(&report)),
            },
            observer,
        )
        .await?;
        if let Some(started) = verification_phase {
            finish_finalization_phase(FINALIZATION_VERIFICATION, started);
        }
        Ok(report)
    }
    /// Best-effort model handoff briefing for a pre-request fold: only called
    /// when the raw history exceeds the compact threshold, and any failure
    /// degrades to the bare-breadcrumb fold (never blocks the turn).
    /// The handoff-briefing producer [`leveler_engine::RawTranscript::assemble`] calls
    /// only when the merged context is still over the fold threshold.
    pub fn context_summarizer<'a>(
        &'a self,
        cancellation: &'a CancellationToken,
    ) -> ModelSummarizer<'a> {
        ModelSummarizer {
            runtime: self.factory.runtime.as_ref(),
            model: &self.factory.model,
            cancellation,
        }
    }

    pub async fn summarize_if_over(
        &self,
        raw: &[leveler_model::Message],
        cancellation: &CancellationToken,
    ) -> Option<String> {
        if leveler_context::estimate_tokens(raw) <= leveler_context::PRE_REQUEST_COMPACT_THRESHOLD {
            return None;
        }
        crate::summarize_with_model(
            self.factory.runtime.as_ref(),
            &self.factory.model,
            None,
            raw,
            leveler_context::COMPACT_KEEP_RECENT,
            0,
            cancellation,
        )
        .await
        .map(|summary| summary.text)
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
async fn run_review(
    runner: &TurnRunner<'_>,
    factory: &ExecutorFactory,
    profile: TurnProfile,
    brief: String,
    files: Vec<String>,
    // The task's wall time already spent: the review is its tail.
    parent_elapsed: std::time::Duration,
    observer: &mut (dyn FnMut(EngineEvent) + Send),
    cancellation: CancellationToken,
) -> Result<ReviewRunOutcome, ReviewRunError> {
    let executor = factory
        .build(profile, None)
        .await
        .map_err(ReviewRunError::Launch)?
        .with_execution_fence(runner.ownership_fence());
    let id = format!("reviewer-{}", leveler_core::RequestId::generate());
    let (profile_id, profile_role, read_only) = crate::child_profile_trace("reviewer");
    let (profile_id_trace, profile_role_trace, read_only_trace) =
        (profile_id.clone(), profile_role.clone(), read_only);
    runner
        .log
        .append(
            None,
            EngineEvent::SubAgentStarted {
                id: id.clone(),
                nickname: "reviewer".to_string(),
                role: "reviewer".to_string(),
                task: brief.clone(),
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
        // The reviewer's model calls arrive as progress events; the engine's
        // own sink is the only thing that can make them rows. Collect here,
        // write below — the child drains its channel after it finishes.
        let mut child_records: Vec<crate::ModelRequestRecord> = Vec::new();
        let result = {
            let mut forward = |event: crate::AgentEvent| {
                if let crate::AgentEvent::SubAgentModelRequest { record } = &event {
                    child_records.push((**record).clone());
                }
                observer(EngineEvent::from(event))
            };
            executor
                .run_reviewer_child(
                    id.clone(),
                    brief,
                    files,
                    parent_elapsed,
                    &mut forward,
                    cancellation,
                )
                .await
        };
        // The review's spend is the session's spend: the reviewer's own rounds
        // and commands fold in from its ledger, and its TOKENS AND COST fold in
        // from the very records being written down here — the same authority
        // the bill reconciles against, never a second summary of it.
        let mut progress = crate::coding::turn::last_persisted_progress(
            runner.stores.events.as_ref(),
            &runner.session_id,
        )
        .await?
        .unwrap_or_default();
        for record in &child_records {
            runner
                .stores
                .model_requests
                .insert(&leveler_engine::storage_model_request(
                    record,
                    &runner.session_id,
                ))
                .await?;
            progress.absorb_request_spend(
                record.usage.total(),
                0,
                record.cost_usd_micros.unwrap_or(0),
            );
        }
        progress.absorb_child_work(&result.progress);
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
        let mut summary = result.result.for_parent("reviewer");
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
                .map(|finding| ledger.adopt_finding(&id, "reviewer", finding))
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
                    nickname: "reviewer".to_string(),
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
                            "reviewer",
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
        Ok::<ReviewRunOutcome, EngineError>(ReviewRunOutcome {
            completed: result.ok,
            findings: result.findings.len(),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReviewRunOutcome {
    completed: bool,
    findings: usize,
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
            out.status.success().then(|| {
                String::from_utf8_lossy(&out.stdout).into_owned()
            })
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
            out.push_str("\nNEW (untracked) files — each is entirely part of the change; read it directly:\n");
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
/// It names the task, the changed files, and — when derivable — the actual
/// unified diff, with a bounded conclusion contract. The reviewer is read-only
/// and must reach its own conclusions from the code, not from the implementing
/// agent's account of what it did; but it must judge THE CHANGE, not re-derive
/// it: briefs that only listed file paths sent every recent reviewer into
/// whole-repository exploration and round-budget death with zero findings.
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
             ```diff\n{diff}\n```\n\n\
             Judge THIS diff — do not survey the rest of the repository. Read a \
             changed file or its direct callers only where the diff's context is \
             not enough to judge correctness."
        ),
        None => format!(
            "Files changed:\n{listed}{more}\n\n\
             Start from the change itself: if git is available, run \
             `git diff -- <file>` on the changed files first; only read beyond \
             the change where its context is not enough to judge correctness. \
             Do not survey the rest of the repository."
        ),
    };
    format!(
        "Independently review the change that was just made for this task.\n\n\
         Task: {goal}\n\n\
         {change}\n\n\
         Report each concrete defect the moment you confirm it with one \
         report_finding call — correctness, security, concurrency and error \
         paths first — naming the file and the specific problem. Your round \
         budget is small and fixed: when every hunk is judged, conclude \
         immediately with a short final verdict — the defects found, or an \
         explicit \"no blocking defects\". Do not re-run builds or tests; do \
         not invent findings."
    )
}

/// The mechanical verdict of the project's own checks, as a status.
///
/// THE single `VerificationReport` → `VerificationStatus` mapping. Every
/// surface reads this one: the `verification_finished` event, the task report,
/// and through it `task_finished.verification`. Two mappings is how an event
/// log ends up saying `passed` while the terminal row says `unavailable`.
///
/// The rule is the status enum's own definition, not a new one: a gate that
/// failed is `Failed`; nothing configured to run is `NotRun` ("no checks are
/// configured"); checks that were configured but could not speak — tool
/// missing, environment mismatch, skipped — are `Unavailable`.
fn verification_status_of(report: &VerificationReport) -> VerificationStatus {
    match report.verdict() {
        Verdict::Verified => VerificationStatus::Passed,
        Verdict::Failed => VerificationStatus::Failed,
        Verdict::Unverified(_) if !report.has_gating_checks() => VerificationStatus::NotRun,
        Verdict::Unverified(_) => VerificationStatus::Unavailable,
    }
}
/// The plan the post-edit gate actually runs.
///
/// A spec's plan is discovered when the turn is created. That is too early for a
/// turn that BUILDS a project: a repo with no manifest yields an empty plan, so
/// the agent could `go mod init`, write a full test suite, and still finish
/// `CompletedUnverified` because the gate had been told there was nothing to
/// run. When the spec carries no plan, re-read the repository at gate time —
/// by then the project it created is on disk. An explicit plan is always
/// honored as given.
fn gate_plan(spec: &TaskSpec) -> VerificationPlan {
    if spec.coding.verification.commands.is_empty() {
        leveler_verifier::discover::plan_for_repo(&spec.coding.repository)
    } else {
        spec.coding.verification.clone()
    }
}

/// Map non-success agent stops for Direct conclude (shipped path used by
/// `conclude_direct`). `None` means continue into verification.
pub(crate) fn direct_non_success_outcome(stop: crate::StopReason) -> Option<TaskOutcome> {
    use crate::StopReason as S;
    match stop {
        // A declared end: the project's checks run and are reported beside it.
        S::Completed | S::Answered | S::CompletedUnverified | S::CompletedChecksFailed => None,
        // The round ceiling is a resource boundary, not a model failure: a
        // ceiling stop is BudgetLimited — incomplete and resumable — the same
        // class as an exhausted budget.
        S::BudgetExhausted | S::TurnLimitReached => Some(TaskOutcome::BudgetLimited),
        // The model said the goal cannot be reached as stated.
        S::Blocked => Some(TaskOutcome::Blocked),
        // Every action refused for several rounds, or a goal that went quiet
        // through every nudge: never success.
        S::Incomplete | S::Stalled => Some(TaskOutcome::Failed),
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
        assert!(brief.contains("Judge THIS diff"), "{brief}");
        assert!(
            brief.contains("do not survey the rest of the repository"),
            "{brief}"
        );
        assert!(brief.contains("no blocking defects"), "{brief}");
        assert!(brief.contains("report_finding"), "{brief}");
    }

    #[test]
    fn a_brief_without_a_diff_still_directs_diff_first_and_bounded_conclusion() {
        let files = vec!["src/a.rs".to_string(), "src/b.rs".to_string()];
        let brief = review_brief("add --json", &files, None);
        assert!(brief.contains("- src/a.rs"), "{brief}");
        assert!(brief.contains("git diff -- <file>"), "{brief}");
        assert!(brief.contains("Do not survey the rest"), "{brief}");
        assert!(brief.contains("no blocking defects"), "{brief}");
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
mod verification_status_tests {
    use super::*;
    use leveler_verifier::{
        BaselineProvenance, BaselineSource, CheckExecution, CheckKind, CheckObservation,
        CheckOutcome, CheckStatus, GateDisposition, GateSkipReason, NotRunReason,
    };

    fn report(status: CheckStatus) -> VerificationReport {
        let observation = match status {
            CheckStatus::Passed => CheckObservation::Passed,
            CheckStatus::Failed => CheckObservation::Failed,
            CheckStatus::Skipped => CheckObservation::NotRun(NotRunReason::VerificationIncomplete),
            CheckStatus::ToolMissing => CheckObservation::NotRun(NotRunReason::ToolMissing),
            CheckStatus::EnvironmentUnavailable => {
                CheckObservation::NotRun(NotRunReason::EnvironmentUnavailable)
            }
        };
        VerificationReport {
            checks: vec![CheckOutcome {
                name: "test".into(),
                kind: CheckKind::Test,
                gating: true,
                observation,
                disposition: GateDisposition::Required,
                execution: matches!(status, CheckStatus::Passed | CheckStatus::Failed).then_some(
                    CheckExecution {
                        program: "cargo".into(),
                        args: vec!["test".into()],
                        exit_code: Some(if status == CheckStatus::Failed { 1 } else { 0 }),
                        timed_out: false,
                    },
                ),
                evidence: String::new(),
                failure: None,
                failed_tests: std::collections::BTreeSet::new(),
            }],
            scope_ok: true,
            scope_violations: vec![],
        }
    }

    #[test]
    fn verification_status_mirrors_the_report_verdict() {
        assert_eq!(
            verification_status_of(&report(CheckStatus::Passed)),
            VerificationStatus::Passed
        );
        assert_eq!(
            verification_status_of(&report(CheckStatus::Failed)),
            VerificationStatus::Failed
        );
        // Configured but unable to speak: the checks exist, so this is
        // `Unavailable` and not "nothing to verify".
        assert_eq!(
            verification_status_of(&report(CheckStatus::ToolMissing)),
            VerificationStatus::Unavailable
        );
        assert_eq!(
            verification_status_of(&report(CheckStatus::EnvironmentUnavailable)),
            VerificationStatus::Unavailable
        );
    }

    /// A failure the baseline already had does not gate, and this mapper must
    /// not decide that for itself by reading the check rows: the report
    /// decides (`verdict()` reports it as unverified, with the reason naming
    /// the pre-existing failure), and the status follows the report exactly as
    /// it did before the split.
    #[test]
    fn a_baseline_attributed_failure_keeps_the_canonical_status() {
        let mut pre_existing = report(CheckStatus::Failed);
        pre_existing.checks[0].disposition =
            GateDisposition::Skipped(GateSkipReason::ConfirmedBaselineFailure {
                revision: "base".into(),
                provenance: BaselineProvenance {
                    source: BaselineSource::DetachedWorktreeRerun,
                    failed_tests: std::collections::BTreeSet::new(),
                },
            });
        assert!(matches!(pre_existing.verdict(), Verdict::Unverified(_)));
        assert_eq!(
            verification_status_of(&pre_existing),
            VerificationStatus::Unavailable
        );

        // The same rows WITHOUT the attribution are a failure. The rows are
        // identical in both cases, so nothing here can be reading them.
        assert_eq!(
            verification_status_of(&report(CheckStatus::Failed)),
            VerificationStatus::Failed
        );
    }

    /// The one case where the gate and the truth must not be read the same
    /// way: a run with nothing configured to run. Its gate is open, and it
    /// proved nothing.
    #[test]
    fn a_report_with_no_gates_is_not_run_rather_than_unavailable() {
        let no_gates = VerificationReport {
            checks: vec![],
            scope_ok: true,
            scope_violations: vec![],
        };
        assert!(
            no_gates.passed(),
            "the gate is open for a run that owes nothing"
        );
        assert_eq!(
            verification_status_of(&no_gates),
            VerificationStatus::NotRun
        );

        // A non-gating check is still nothing to verify against.
        let mut advisory = report(CheckStatus::Passed);
        advisory.checks[0].gating = false;
        assert!(advisory.passed());
        assert_eq!(
            verification_status_of(&advisory),
            VerificationStatus::NotRun
        );
    }
}

#[cfg(test)]
mod terminal_mapping_tests {
    use super::*;

    /// The session status is read off how the run ended, never off the
    /// verification verdict: a guard-forced Incomplete stop stays Incomplete
    /// however green the tree, and a completed run with failed checks is a
    /// completed session that reports `VerificationStatus::Failed` beside it.
    #[test]
    fn terminal_status_follows_the_stop_reason_not_the_checks() {
        let mut report = TaskReport::new(
            TaskOutcome::Completed,
            String::new(),
            vec!["a.rs".into()],
            crate::StopReason::Incomplete,
            1,
        );
        report.verification_status = VerificationStatus::Passed;
        assert_eq!(terminal_status_for(&report).0, SessionStatus::Incomplete);

        let mut clean = TaskReport::new(
            TaskOutcome::Completed,
            String::new(),
            vec!["a.rs".into()],
            crate::StopReason::Completed,
            1,
        );
        clean.verification_status = VerificationStatus::Failed;
        assert_eq!(
            terminal_status_for(&clean),
            (SessionStatus::Completed, AgentState::Complete),
            "failed checks are reported, not laundered into an incomplete session"
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
            Some(TaskOutcome::Failed)
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
            "Answered continues into verify path"
        );
        assert_eq!(
            direct_non_success_outcome(crate::StopReason::Completed),
            None
        );
    }
}

#[cfg(test)]
mod gate_plan_tests {
    use super::*;
    use leveler_verifier::VerificationCommand;

    fn spec(repository: std::path::PathBuf, verification: VerificationPlan) -> TaskSpec {
        TaskSpec {
            runtime: RuntimeTaskSpec {
                goal: "build it".to_string(),
                kind: ExecutionKind::Direct,
                continuation: ContinuationPolicy::UntilTerminal,
                limits: StepLimits::default(),
            },
            coding: CodingTaskSpec {
                repository,
                mode: leveler_execution::PermissionProfile::Assisted,
                sandbox: false,
                verification,
                base_commit: None,
            },
        }
    }

    #[test]
    fn a_project_created_during_the_turn_is_still_verified() {
        // The turn began in an empty repo (no manifest → empty plan) and ended
        // having created a Go module. The gate must see the module.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("go.mod"),
            "module example.com/x\n\ngo 1.21\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("main.go"),
            "package main\n\nfunc main() {}\n",
        )
        .unwrap();

        let plan = gate_plan(&spec(dir.path().to_path_buf(), VerificationPlan::default()));

        assert!(
            plan.commands.iter().any(|c| c.program == "go"),
            "an empty spec plan must be re-discovered against the repo as it is at \
             gate time, got: {:?}",
            plan.commands.iter().map(|c| &c.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn an_explicit_plan_is_honored_as_given() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("go.mod"), "module example.com/x\n").unwrap();
        let declared = VerificationPlan {
            commands: vec![VerificationCommand {
                name: "custom".to_string(),
                program: "make".to_string(),
                args: vec!["check".to_string()],
                kind: leveler_verifier::CheckKind::Test,
                gating: true,
                timeout_seconds: 600,
                scope_policy: Default::default(),
            }],
        };

        let plan = gate_plan(&spec(dir.path().to_path_buf(), declared.clone()));

        assert_eq!(plan, declared, "a declared plan must not be second-guessed");
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
mod goal_history_tests {
    use super::bound_goal_history;
    use leveler_model::{ContentPart, Message, Role};

    fn long_prior(n: usize) -> Vec<Message> {
        (0..n)
            .map(|i| Message {
                role: if i % 2 == 0 {
                    Role::User
                } else {
                    Role::Assistant
                },
                content: vec![ContentPart::Text {
                    text: format!("message {i}"),
                }],
            })
            .collect()
    }

    #[test]
    fn bound_goal_history_keeps_tail() {
        let raw = long_prior(10);
        let bound = bound_goal_history(raw.clone(), 4);
        assert_eq!(bound.len(), 4);
        assert_eq!(
            bound.last().unwrap().text_content(),
            raw.last().unwrap().text_content()
        );
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
    fn coding_mutation_normalizes_answered_into_completion_projection() {
        let mut report = TaskReport::new(
            TaskOutcome::Completed,
            "done".to_string(),
            vec!["src/lib.rs".to_string()],
            StopReason::Answered,
            1,
        );
        report.verification_status = VerificationStatus::Unavailable;

        assert_eq!(task_terminal_stop(&report), StopReason::Completed);

        report.modified_files.clear();
        assert_eq!(task_terminal_stop(&report), StopReason::Answered);
    }
}
