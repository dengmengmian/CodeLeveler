//! The task engine: session lifecycle + strategy dispatch (plan B3).
//!
//! `create_task` persists WHAT will run (goal/mode/sandbox/kind) so resume
//! never guesses; `run` executes the session's strategy over fully-persisted
//! turns and stamps the terminal [`TaskOutcome`] on the session row.

use std::path::PathBuf;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use leveler_agent::{Clarifier, ContinuationPolicy, StepLimits, StopReason};
use leveler_core::{SessionId, TaskId, TurnId};
use leveler_execution::{Approver, PermissionProfile, RiskLevel};
use leveler_lifecycle::{AgentState, SessionStatus};
use leveler_storage::{EngineStores, EventStore, SessionRecord};
use leveler_verifier::{
    CompletionVerdict, ExpectedEvidence, Verdict, VerificationPlan, VerificationReport, Verifier,
    finalize_task_outcome,
};

use crate::factory::{ExecutorFactory, TurnProfile};
use crate::log::{DanglingCall, EventLog, SnapshotView};
use crate::turn::{TurnInput, TurnRunner};
use crate::{EngineError, EngineEvent, ExecutionKind, TaskOutcome, TurnKind};

/// How many verification-repair turns a direct task may spend.
const DIRECT_REPAIR_ATTEMPTS: u32 = 1;

/// Whether a dangling call may be re-run automatically after a crash.
///
/// This asks the TOOL, never the risk label. `RiskLevel::Safe` answers "does
/// this need approval" and it admits side effects — `create_checkpoint` resets
/// the rollback baseline, `wait_task` can restore a workspace snapshot, and
/// both are Safe. Deriving replay-safety from risk (as this once did) would
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
    }
}

/// The round policy for the NEXT supervised window. A pinned (bounded) budget
/// is a task TOTAL: a follow-up window may only consume rounds the epoch has
/// not already spent — `None` means the total is exhausted and no window may
/// open, whatever the policy decided (settlement × continuation seam: the
/// integration window spends the remainder, it never manufactures budget).
pub(crate) fn continuation_for_next_window(
    policy: ContinuationPolicy,
    epoch_rounds_spent: u32,
) -> Option<ContinuationPolicy> {
    match policy.round_limit() {
        Some(total) if epoch_rounds_spent >= total => None,
        Some(total) => Some(ContinuationPolicy::bounded(total - epoch_rounds_spent)),
        None => Some(policy),
    }
}

/// Fold a continuation turn's outcome into the running aggregate — the ONE
/// merge that both goal continuation and budget extension use. Rounds, token
/// spend, and modified files accumulate; the latest turn's text, stop reason,
/// stop detail, budget exhaustion, and progress ledger replace the previous
/// ones (epoch spend inside `progress` is already absolute after seeding).
fn merge_continued_outcome(
    outcome: &mut leveler_agent::AgentOutcome,
    continued: leveler_agent::AgentOutcome,
) {
    outcome.rounds = outcome.rounds.saturating_add(continued.rounds);
    outcome.final_text = continued.final_text;
    outcome.stop_reason = continued.stop_reason;
    outcome.stop_detail = continued.stop_detail;
    outcome.budget_exhaustion = continued.budget_exhaustion;
    outcome.progress = continued.progress;
    outcome.metrics.model_tokens = outcome
        .metrics
        .model_tokens
        .saturating_add(continued.metrics.model_tokens);
    outcome.metrics.extra_model_calls = outcome
        .metrics
        .extra_model_calls
        .saturating_add(continued.metrics.extra_model_calls);
    for path in continued.modified_files {
        if !outcome.modified_files.contains(&path) {
            outcome.modified_files.push(path);
        }
    }
}

/// The user-facing terminal lifecycle columns for a finished task. This is
/// the product interpretation of the report (a passing gate upgrades a mere
/// "answered" to completed; a failed verification reads as incomplete), moved
/// here from the app layer so the engine is the single lifecycle writer.
/// The goal-invocation no-progress window counter update. A window that made
/// material progress (grew the modified-file set) resets it to 0; one that did
/// not increments it. `after_turn` stops opening windows once it reaches
/// `MAX_NO_PROGRESS_WINDOWS`, so a stuck goal converges in a couple of windows
/// rather than burning the absolute `MAX_SUPERVISED_TURNS` ceiling. Extracted so
/// the hard-bound termination is unit-testable without a live model.
/// Window-progress marks from the persisted evidence ledger: (total mutation
/// operations, does a green verification cover the latest mutations). Absent
/// or unreadable ledger degrades to zeros — the file-set signal still works,
/// so a load failure can only under-credit, never spin a goal forever.
async fn evidence_progress_marks(
    events: &dyn leveler_storage::EventStore,
    session_id: &SessionId,
) -> (u64, bool) {
    match crate::turn::last_persisted_ledger(events, session_id).await {
        Ok(Some(ledger)) => (
            ledger.total_mutation_ops,
            ledger.has_fresh_successful_verify(),
        ),
        _ => (0, false),
    }
}

pub(crate) fn advance_no_progress_windows(current: u32, made_progress: bool) -> u32 {
    if made_progress {
        0
    } else {
        current.saturating_add(1)
    }
}

pub(crate) fn terminal_status_for(report: &TaskReport) -> (SessionStatus, AgentState) {
    use StopReason as S;
    let verification_failed = report
        .verification
        .as_ref()
        .is_some_and(|verification| verification.verdict() == Verdict::Failed);
    let did_work = report.stop_reason == S::Completed || !report.modified_files.is_empty();
    let effective = if verification_failed {
        S::Incomplete
    } else {
        match report.outcome {
            // A guard-forced Incomplete stop keeps its honest terminal status:
            // gate-green describes the tree, not task completion (R004 F4).
            TaskOutcome::Verified if did_work && report.stop_reason != S::Incomplete => {
                S::Completed
            }
            TaskOutcome::CompletedUnverified if did_work && report.stop_reason != S::Incomplete => {
                S::CompletedUnverified
            }
            _ => report.stop_reason,
        }
    };
    match effective {
        S::Completed | S::Answered | S::CloseoutForced | S::CompletedUnverified => {
            (SessionStatus::Completed, AgentState::Complete)
        }
        S::Incomplete | S::BudgetExhausted | S::TurnLimitReached | S::Stalled => {
            (SessionStatus::Incomplete, AgentState::Execute)
        }
        // A harness-policy dead end needs attention, not silent retry: same
        // resumable-with-attention class as a model-declared block (R006 R6-P1).
        S::Blocked | S::PolicyBlocked => (SessionStatus::Blocked, AgentState::Execute),
    }
}

fn chat_profile(spec: &TaskSpec) -> TurnProfile {
    TurnProfile::Chat {
        continuation: spec.runtime.continuation,
        limits: spec.runtime.limits,
    }
}

/// Bound prior messages for a model request.
///
/// **Under threshold:** always use full `raw` from MessageRepository — a
/// ContextSnapshot is never a permanent replacement for later turns.
/// **Over threshold:** merge snapshot (compact base) with the raw tail that
/// arrived after the snapshot was taken, then fold if still oversized. A
/// snapshot with a `through_ordinal` watermark appends exactly `raw[n..]`;
/// only watermark-less legacy snapshots use suffix-overlap inference.
///
/// Returns `(messages_for_model, wrote_compact)` — `wrote_compact` means the
/// caller should persist a new ContextSnapshot.
pub fn budget_prior_messages(
    raw: Vec<leveler_model::Message>,
    snapshot: Option<SnapshotView>,
    summary: Option<&str>,
    active_objective: Option<&str>,
    threshold: u64,
) -> (Vec<leveler_model::Message>, bool) {
    let raw_tokens = leveler_agent::estimate_tokens(&raw);
    if raw_tokens <= threshold {
        return (raw, false);
    }

    let base = match snapshot {
        Some(view) if !view.messages.is_empty() => match view.through_ordinal {
            Some(n) if (n as usize) <= raw.len() => {
                // Exact watermark: everything after the first `n` transcript
                // messages post-dates the snapshot. No inference, so rounds
                // that repeat earlier text verbatim are never mistaken for
                // the snapshot's own tail and dropped.
                let mut out = view.messages;
                out.extend_from_slice(&raw[n as usize..]);
                out
            }
            Some(n) => {
                // A watermark beyond the live transcript means the transcript
                // was truncated after the snapshot (context ops normally
                // rewrite the snapshot too). Never guess a slice: fall back
                // to the legacy overlap merge and say so.
                tracing::warn!(
                    through_ordinal = n,
                    raw_len = raw.len(),
                    "context snapshot watermark exceeds transcript; using overlap merge"
                );
                merge_snapshot_with_raw_tail(view.messages, &raw)
            }
            None => merge_snapshot_with_raw_tail(view.messages, &raw),
        },
        _ => raw,
    };
    let tokens = leveler_agent::estimate_tokens(&base);
    if tokens <= threshold {
        // Snapshot+tail already fits: persist so next request starts shorter.
        return (base, true);
    }
    // HCH-FIX-2: bound the retained tail by TOKENS as well as by count —
    // half the fold threshold, mirroring the agent loop's `budget / 2`
    // (drive loop passes `current_budget / 2`). With `0` here, a single
    // huge tool result inside the last 12 messages rode through a
    // 24k-threshold fold intact, retaining 5-19x the threshold.
    let folded = leveler_agent::compact_messages(
        &base,
        leveler_agent::COMPACT_KEEP_RECENT,
        threshold / 2,
        summary,
        active_objective,
    );
    let changed = leveler_agent::estimate_tokens(&folded) < tokens || folded.len() < base.len();
    (folded, changed || tokens > threshold)
}

/// Append raw messages that post-date the snapshot. Snapshot is often a
/// compacted view (summary + recent window), so we locate the longest suffix of
/// `snap` that appears as a contiguous slice of `raw` and keep everything after.
fn merge_snapshot_with_raw_tail(
    snap: Vec<leveler_model::Message>,
    raw: &[leveler_model::Message],
) -> Vec<leveler_model::Message> {
    if raw.is_empty() {
        return snap;
    }
    let snap_len = snap.len();
    let max_k = snap_len.min(raw.len());
    for k in (1..=max_k).rev() {
        let suffix = &snap[snap_len - k..];
        // Search from the end so we match the most recent occurrence.
        for i in (0..=raw.len() - k).rev() {
            if messages_slice_eq(suffix, &raw[i..i + k]) {
                let mut out = snap;
                out.extend_from_slice(&raw[i + k..]);
                return out;
            }
        }
    }
    // No overlap (pure summary snapshot): keep snap + trailing raw window.
    let keep = leveler_agent::COMPACT_KEEP_RECENT.min(raw.len());
    let mut out = snap;
    out.extend_from_slice(&raw[raw.len() - keep..]);
    out
}

fn messages_slice_eq(a: &[leveler_model::Message], b: &[leveler_model::Message]) -> bool {
    a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x == y)
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

/// The engine's terminal report for a task.
#[derive(Debug)]
pub struct TaskReport {
    pub outcome: TaskOutcome,
    pub final_text: String,
    pub modified_files: Vec<String>,
    pub verification: Option<VerificationReport>,
    /// The executor's stop reason (legacy status mapping needs its nuance).
    pub stop_reason: StopReason,
    /// The executor's concrete reason for a non-success stop, when available.
    pub stop_detail: Option<String>,
    pub rounds: u32,
    /// Legacy review findings (unused; kept for report shape stability).
    pub review: Option<Vec<String>>,
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
            final_text,
            modified_files,
            verification: None,
            stop_reason,
            stop_detail: None,
            rounds,
            review: None,
        }
    }

    fn with_stop_detail(mut self, stop_detail: Option<String>) -> Self {
        self.stop_detail = stop_detail;
        self
    }
}

fn report_from_agent_outcome(
    outcome: leveler_agent::AgentOutcome,
    task_outcome: TaskOutcome,
) -> TaskReport {
    TaskReport::new(
        task_outcome,
        outcome.final_text,
        outcome.modified_files,
        outcome.stop_reason,
        outcome.rounds,
    )
    .with_stop_detail(outcome.stop_detail)
}

pub fn mode_str(mode: PermissionProfile) -> &'static str {
    mode.as_str()
}

/// The persistent task engine.
///
/// Persistence enters exclusively through [`EngineStores`] — narrow
/// capability ports the composition root wires to its adapter (SQLite
/// locally). The engine never names a concrete database.
pub struct TaskEngine {
    pub stores: EngineStores,
    /// This runtime's durable identity (from the composition root). Task
    /// ownership is acquired for it at every execution entry.
    pub runtime_id: leveler_core::RuntimeId,
    pub factory: ExecutorFactory,
    pub approver: Arc<dyn Approver>,
    pub clarifier: Arc<dyn Clarifier>,
    /// Decides whether a finished turn gets a successor. The engine owns the
    /// mechanism and the hard bounds; this owns the judgement. `None` uses
    /// [`DefaultSupervisorPolicy`] (historical behavior).
    pub supervisor: Option<Arc<dyn crate::SupervisorPolicy>>,
}

/// Absolute ceiling on supervisor-initiated turns for one task. Policies are
/// replaceable; this bound is not — it guarantees the supervision loop
/// terminates even if a policy keeps asking for another turn.
const MAX_SUPERVISED_TURNS: u32 = 32;

impl TaskEngine {
    /// Attach mid-turn user input for this engine's runs.
    ///
    /// Set by the caller that knows which session is running, since the factory
    /// is built before that is decided.
    pub fn with_steering(mut self, source: Option<Arc<dyn leveler_agent::SteeringSource>>) -> Self {
        self.factory.steering = source;
        self
    }

    /// Install a supervision policy (see [`crate::SupervisorPolicy`]).
    pub fn with_supervisor(mut self, policy: Arc<dyn crate::SupervisorPolicy>) -> Self {
        self.supervisor = Some(policy);
        self
    }

    fn supervisor_policy(&self) -> Arc<dyn crate::SupervisorPolicy> {
        self.supervisor
            .clone()
            .unwrap_or_else(|| Arc::new(crate::DefaultSupervisorPolicy::default()))
    }

    /// Commit the canonical terminal event and every session lifecycle column
    /// (outcome + status + state) atomically, then forward the event. The
    /// engine is the ONE writer of the session lifecycle — no app layer stamps
    /// a second copy — and an observer can never see an uncommitted fact.
    async fn finish_task(
        &self,
        token: &leveler_core::OwnershipToken,
        session_id: &SessionId,
        outcome: TaskOutcome,
        reason: Option<String>,
        stop: Option<StopReason>,
        status: SessionStatus,
        state: AgentState,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
    ) -> Result<(), EngineError> {
        let event = EngineEvent::TaskFinished {
            outcome,
            reason,
            stop,
        };
        let (event_type, payload) = event.to_row()?;
        self.stores
            .terminal
            .finish_task_owned(
                token,
                session_id,
                &event_type,
                &payload,
                outcome,
                status,
                state,
                leveler_core::now(),
            )
            .await?;
        observer(event);
        Ok(())
    }

    /// Shared terminal handling for run/chat/resume: derive the lifecycle
    /// columns from the result and commit them with the TaskFinished event.
    async fn finish_from_result(
        &self,
        token: &leveler_core::OwnershipToken,
        session_id: &SessionId,
        result: &Result<TaskReport, EngineError>,
        repo: Option<&std::path::Path>,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
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
                | Err(EngineError::Agent(
                    leveler_agent::AgentError::StaleOwnership(_)
                ))
        ) {
            return Ok(());
        }
        // Settle this session's background tasks at the terminal fact — the
        // ONE choke point every path shares (run/chat/resume/continuation,
        // completed/failed/budget_limited). R006 R6-P4: the reap used to hang
        // off one spawn function in the app layer, so a goal continued via
        // ordinary messages (chat-routed) leaked its dev servers. Interrupted
        // (user cancel) deliberately keeps them inspectable. Daemon-owned
        // services (browser/MCP/LSP) have no session scope and are untouched.
        let interrupted = matches!(
            result,
            Err(EngineError::Agent(leveler_agent::AgentError::Cancelled))
        );
        // R007 F3: a WORK-WINDOW boundary is not a goal terminal. When the
        // round/step budget runs out the session stays resumable
        // (`AgentState::Execute`), and the goal's services must outlive the
        // window — R007 hit the ceiling twice and spent each next window
        // rebuilding the dev server this reap had just killed. A genuine goal
        // terminal still reaps, so R6-P4 is unaffected.
        let goal_continues = matches!(&result, Ok(report)
            if terminal_status_for(report).1 == AgentState::Execute);
        if !interrupted
            && !goal_continues
            && let (Some(scope), Some(registry)) = (
                self.factory.tool_context.session_scope.as_deref(),
                self.factory.tool_context.services.background_tasks.as_ref(),
            )
        {
            let reaped = registry.kill_scope(scope).await;
            if reaped > 0 {
                tracing::info!(
                    session = scope,
                    "terminal settlement reaped {reaped} session-owned background task(s)"
                );
            }
        }
        let settled = match result {
            Ok(report) => {
                let (status, state) = terminal_status_for(report);
                self.finish_task(
                    token,
                    session_id,
                    report.outcome,
                    (report.outcome != TaskOutcome::Verified).then(|| report.final_text.clone()),
                    Some(report.stop_reason),
                    status,
                    state,
                    observer,
                )
                .await
            }
            Err(EngineError::Agent(leveler_agent::AgentError::Cancelled)) => {
                self.finish_task(
                    token,
                    session_id,
                    TaskOutcome::Interrupted,
                    None,
                    None,
                    SessionStatus::Interrupted,
                    AgentState::Execute,
                    observer,
                )
                .await
            }
            Err(error) => {
                self.finish_task(
                    token,
                    session_id,
                    TaskOutcome::Failed,
                    Some(error.to_string()),
                    None,
                    SessionStatus::Failed,
                    AgentState::Failed,
                    observer,
                )
                .await
            }
        };
        // Long-goal P3 milestone: a work-window boundary where the goal still
        // continues is the deterministic phase signal — cut a durable
        // checkpoint AFTER the terminal fact committed (the cursor then
        // includes it). Best-effort: a failed checkpoint never un-settles a
        // committed terminal.
        if settled.is_ok() && goal_continues {
            match crate::checkpoint::create_goal_checkpoint(
                &self.stores,
                session_id,
                leveler_lifecycle::CheckpointReason::Milestone,
                repo,
                None,
            )
            .await
            {
                Ok(Some(record)) => {
                    let log = EventLog::new_owned(
                        self.stores.events.as_ref(),
                        session_id.clone(),
                        token.clone(),
                    );
                    let event = crate::checkpoint::checkpoint_created_event(&record);
                    if let Err(error) = log.append(None, event, observer).await {
                        tracing::warn!(
                            %error,
                            "milestone checkpoint persisted but its announcement failed"
                        );
                    }
                }
                Ok(None) => {}
                Err(error) => tracing::warn!(
                    %error,
                    "milestone goal checkpoint failed; the terminal fact stands"
                ),
            }
        }
        settled
    }

    /// Mark the session running before the first turn. The engine owns this
    /// transition too — clients observe lifecycle, they never write it.
    ///
    /// Also the ONE seam where the durable task identity is guaranteed: every
    /// execution entry (run/chat/resume) passes here, so a session created by
    /// any path — including one that predates the tasks table — has its task
    /// row before the first turn. Returns that task id.
    /// Acquire (or same-runtime reacquire) ownership of the session's task.
    /// A task owned by a DIFFERENT runtime is a hard conflict - never
    /// auto-stolen. The epoch always advances, fencing prior incarnations.
    async fn acquire_ownership(
        &self,
        session_id: &SessionId,
    ) -> Result<leveler_core::OwnershipToken, EngineError> {
        let task_id = self
            .stores
            .tasks
            .ensure_for_session(session_id, leveler_core::now())
            .await?;
        // Acquire (or same-runtime reacquire) ownership BEFORE any
        // authoritative write. A task owned by another runtime is a hard
        // conflict — never auto-stolen; the CAS itself refuses concurrent
        // racers. The epoch always advances, so tokens from this runtime's
        // previous incarnation become stale here.
        let current = self
            .stores
            .ownership
            .current(&task_id)
            .await?
            .ok_or_else(|| EngineError::Config(format!("no task row for session {session_id}")))?;
        if let Some(owner) = &current.runtime
            && owner != &self.runtime_id
        {
            return Err(EngineError::OwnershipConflict {
                task_id,
                owner: owner.clone(),
                epoch: current.epoch,
                this_runtime: self.runtime_id.clone(),
            });
        }
        Ok(self
            .stores
            .ownership
            .acquire(&task_id, &self.runtime_id, current.epoch)
            .await?)
    }

    /// Mark the session running before the first turn (fenced), acquiring
    /// ownership first — the ONE seam every execution entry passes through.
    async fn mark_running(
        &self,
        session_id: &SessionId,
    ) -> Result<leveler_core::OwnershipToken, EngineError> {
        let token = self.acquire_ownership(session_id).await?;
        self.stores
            .sessions
            .update_status_owned(
                &token,
                session_id,
                SessionStatus::Running,
                AgentState::Execute,
                leveler_core::now(),
            )
            .await?;
        Ok(token)
    }

    /// The durable task owning `session_id`, if the association exists yet.
    /// (It is created at latest when the session first runs.)
    pub async fn task_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<TaskId>, EngineError> {
        Ok(self.stores.tasks.task_for_session(session_id).await?)
    }

    /// Create and persist the session row, including its execution config,
    /// and the durable task row associated with it.
    pub async fn create_task(&self, spec: &TaskSpec) -> Result<SessionId, EngineError> {
        let record = SessionRecord::new(
            spec.coding.repository.display().to_string(),
            spec.runtime.goal.clone(),
            self.factory.model.to_string(),
            leveler_core::now(),
        );
        self.stores.sessions.create(&record).await?;
        let id = SessionId::new(record.id);
        self.stores
            .sessions
            .set_execution(
                &id,
                mode_str(spec.coding.mode),
                spec.coding.sandbox,
                spec.runtime.kind.as_str(),
                leveler_core::now(),
            )
            .await?;
        self.stores
            .tasks
            .ensure_for_session(&id, leveler_core::now())
            .await?;
        Ok(id)
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
        let token = self.mark_running(session_id).await?;
        let log = EventLog::new_owned(
            self.stores.events.as_ref(),
            session_id.clone(),
            token.clone(),
        );
        let expanded_seed = log.max_expanded_context_budget().await?.unwrap_or(0);
        let runner = TurnRunner {
            stores: &self.stores,
            token: token.clone(),
            session_id: session_id.clone(),
            log: &log,
            factory: &self.factory,
            approver: self.approver.clone(),
            clarifier: self.clarifier.clone(),
            expanded_context_budget: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(
                expanded_seed,
            )),
            repo: Some(spec.coding.repository.clone()),
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
            if let Some(head) = crate::baseline::capture_head(&spec.coding.repository).await {
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
        let result = match spec.runtime.kind {
            ExecutionKind::Direct => {
                self.run_direct(&log, &runner, spec, observer, cancellation)
                    .await
            }
            ExecutionKind::Parallel => Err(EngineError::Config(
                "the parallel strategy lands in B9".to_string(),
            )),
        };

        // Stamp the terminal outcome (interrupted on cancellation) and emit
        // TaskFinished before returning.
        self.finish_from_result(
            &token,
            session_id,
            &result,
            Some(&spec.coding.repository),
            observer,
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
        // Interactive chat is the path where a dirty, already-red worktree is the
        // normal case, so it needs this more than `run` does.
        let owned_spec;
        let spec = if spec.coding.base_commit.is_none() {
            match crate::baseline::capture_head(&spec.coding.repository).await {
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
        let raw =
            crate::RawTranscript::load_lossy(self.stores.messages.as_ref(), session_id).await?;
        let token = self.mark_running(session_id).await?;
        let log = EventLog::new_owned(
            self.stores.events.as_ref(),
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
        let checkpoint_prior = self
            .checkpointed_prior(
                &log,
                session_id,
                &raw.messages,
                Some(&spec.coding.repository),
                &cancellation,
                observer,
            )
            .await?;
        let prior = if let Some(prior) = checkpoint_prior {
            prior
        } else {
            let summary = self.summarize_if_over(&raw.messages, &cancellation).await;
            let objective_hint = content
                .iter()
                .filter_map(|p| match p {
                    leveler_model::ContentPart::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .next();
            let context = raw
                .assemble(
                    &log,
                    summary.as_deref(),
                    objective_hint,
                    u64::from(crate::ContextPolicy::chat_default().initial_budget),
                )
                .await?;
            if context.compacted {
                log.append(None, context.snapshot_event(), observer).await?;
            }
            context.prior
        };
        let expanded_seed = log.max_expanded_context_budget().await?.unwrap_or(0);
        let runner = TurnRunner {
            stores: &self.stores,
            token: token.clone(),
            session_id: session_id.clone(),
            log: &log,
            factory: &self.factory,
            approver: self.approver.clone(),
            clarifier: self.clarifier.clone(),
            expanded_context_budget: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(
                expanded_seed,
            )),
            repo: Some(spec.coding.repository.clone()),
        };
        let result = async {
            let recorded = runner
                .run_turn(
                    TurnKind::Chat,
                    chat_profile(spec),
                    TurnInput::Content { prior, content },
                    observer,
                    cancellation.clone(),
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
        self.finish_from_result(
            &token,
            session_id,
            &result,
            Some(&spec.coding.repository),
            observer,
        )
        .await?;
        result
    }

    /// Resume an interrupted direct task from its persisted transcript. The
    /// caller builds `spec` FROM the persisted execution config (see
    /// `SessionRepository::execution`); the engine refuses a kind mismatch and
    /// a session that already ended successfully.
    pub async fn resume(
        &self,
        session_id: &SessionId,
        spec: &TaskSpec,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
        cancellation: CancellationToken,
    ) -> Result<TaskReport, EngineError> {
        let (_, _, kind, outcome) = self
            .stores
            .sessions
            .execution(session_id)
            .await?
            .ok_or_else(|| EngineError::Config(format!("no session {session_id}")))?;
        if kind != spec.runtime.kind.as_str() {
            return Err(EngineError::Config(format!(
                "session {session_id} is `{kind}`, not `{}`",
                spec.runtime.kind.as_str()
            )));
        }
        if matches!(
            outcome,
            Some(TaskOutcome::Verified) | Some(TaskOutcome::CompletedUnverified)
        ) {
            return Err(EngineError::Config(format!(
                "session {session_id} already completed ({}); start a new task instead",
                outcome.map(|o| o.as_str()).unwrap_or_default()
            )));
        }
        let raw = crate::RawTranscript::load_strict(
            self.stores.messages.as_ref(),
            session_id,
            "transcript",
        )
        .await?;
        if raw.is_empty() {
            return Err(EngineError::Config(format!(
                "session {session_id} has no transcript to resume; \
                 for interactive chat reopen with: leveler tui --session {session_id}"
            )));
        }
        let token = self.mark_running(session_id).await?;
        let log = EventLog::new_owned(
            self.stores.events.as_ref(),
            session_id.clone(),
            token.clone(),
        );
        // Long-goal P3: a valid durable checkpoint replaces the replayed old
        // context — resume receives the checkpoint block plus exactly the
        // transcript after its watermark. No usable checkpoint (none written,
        // corrupt, future version, stale watermark) falls back to the
        // pre-checkpoint full-history path below.
        let checkpoint_prior = crate::checkpoint::resume_prior_from_checkpoint(
            &self.stores,
            session_id,
            &raw.messages,
        )
        .await?;
        let prior = if let Some(prior) = checkpoint_prior {
            prior
        } else {
            let summary = self.summarize_if_over(&raw.messages, &cancellation).await;
            // Same merge rules as chat: never drop post-snapshot transcript rows.
            let context = raw
                .assemble(
                    &log,
                    summary.as_deref(),
                    Some(spec.runtime.goal.as_str()),
                    u64::from(crate::ContextPolicy::chat_default().initial_budget),
                )
                .await?;
            if context.compacted {
                log.append(None, context.snapshot_event(), observer).await?;
            }
            context.prior
        };
        let expanded_seed = log.max_expanded_context_budget().await?.unwrap_or(0);
        let runner = TurnRunner {
            stores: &self.stores,
            token: token.clone(),
            session_id: session_id.clone(),
            log: &log,
            factory: &self.factory,
            approver: self.approver.clone(),
            clarifier: self.clarifier.clone(),
            expanded_context_budget: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(
                expanded_seed,
            )),
            repo: Some(spec.coding.repository.clone()),
        };

        // Reconcile the crash window before continuing: a tool that started but
        // never finished (the process died mid-execution) is replayed if
        // idempotent, or surfaced for approval if it has a side effect (M5).
        self.recover_crash_window(&log, observer, &cancellation)
            .await?;

        let result = self
            .resume_direct(&log, &runner, spec, prior, observer, cancellation)
            .await;
        self.finish_from_result(
            &token,
            session_id,
            &result,
            Some(&spec.coding.repository),
            observer,
        )
        .await?;
        result
    }

    /// Long-goal P3: the checkpoint-backed pre-request fold.
    ///
    /// Over the fold threshold, prefer a FRESH durable checkpoint (one whose
    /// delta still fits the threshold); when the newest checkpoint is stale
    /// or absent, cut a `ContextCompaction` checkpoint at the current
    /// committed boundary and continue from its block plus a bounded recent
    /// tail. `None` = keep the pre-checkpoint path: transcript under the
    /// threshold, no goal in scope, or checkpoint creation failed — every
    /// fold leaves the durable transcript untouched, so the fallback
    /// degrades only to exactly the pre-P3 context, never to lost history.
    async fn checkpointed_prior(
        &self,
        log: &EventLog<'_>,
        session_id: &SessionId,
        raw: &[leveler_model::Message],
        repo: Option<&std::path::Path>,
        cancellation: &CancellationToken,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
    ) -> Result<Option<Vec<leveler_model::Message>>, EngineError> {
        let threshold = u64::from(crate::ContextPolicy::chat_default().initial_budget);
        if leveler_agent::estimate_tokens(raw) <= threshold {
            return Ok(None);
        }
        // Cheap goal probe BEFORE any model call: a session with no goal
        // keeps the pre-checkpoint path bit-for-bit (including exactly one
        // summarization call, which mocks and cost accounting rely on).
        let Some(task) = self.stores.tasks.task_for_session(session_id).await? else {
            return Ok(None);
        };
        if self.stores.goals.for_task(&task).await?.is_empty() {
            return Ok(None);
        }
        if let Some(prior) =
            crate::checkpoint::resume_prior_from_checkpoint(&self.stores, session_id, raw).await?
            && leveler_agent::estimate_tokens(&prior) <= threshold
        {
            return Ok(Some(prior));
        }
        let summary = self.summarize_if_over(raw, cancellation).await;
        match crate::checkpoint::create_goal_checkpoint(
            &self.stores,
            session_id,
            leveler_lifecycle::CheckpointReason::ContextCompaction,
            repo,
            crate::checkpoint::SemanticRecap::briefing(summary.as_deref()),
        )
        .await
        {
            Ok(Some(record)) => {
                let event = crate::checkpoint::checkpoint_created_event(&record);
                log.append(None, event, observer).await?;
                // The checkpoint block, plus a bounded raw tail for local
                // continuity — the same recency window the pre-P3 fold kept.
                let tail_start = raw.len().saturating_sub(leveler_agent::COMPACT_KEEP_RECENT);
                let mut prior = Vec::with_capacity(1 + raw.len() - tail_start);
                prior.push(leveler_model::Message {
                    role: leveler_model::Role::User,
                    content: vec![leveler_model::ContentPart::Text {
                        text: record.payload.context_block(),
                    }],
                });
                prior.extend_from_slice(&raw[tail_start..]);
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

    /// Best-effort model handoff briefing for a pre-request fold: only called
    /// when the raw history exceeds the compact threshold, and any failure
    /// degrades to the bare-breadcrumb fold (never blocks the turn).
    async fn summarize_if_over(
        &self,
        raw: &[leveler_model::Message],
        cancellation: &CancellationToken,
    ) -> Option<String> {
        if leveler_agent::estimate_tokens(raw)
            <= u64::from(crate::ContextPolicy::chat_default().initial_budget)
        {
            return None;
        }
        leveler_agent::summarize_with_model(
            self.factory.runtime.as_ref(),
            &self.factory.model,
            None,
            raw,
            leveler_agent::COMPACT_KEEP_RECENT,
            0,
            cancellation,
        )
        .await
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
        let token = self.acquire_ownership(session_id).await?;
        acknowledge_crash_window(self.stores.events.as_ref(), &token, session_id).await
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
                    self.record_recovery_skip(
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
        let Some((is_error, preview)) = crate::recovery::replay_tool(
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
                call_id: call.call_id.clone(),
                name: call.name.clone(),
                is_error,
                preview,
                agent_id: call.agent_id.clone(),
            },
            observer,
        )
        .await
    }

    async fn record_recovery_skip(
        &self,
        log: &EventLog<'_>,
        call: &DanglingCall,
        turn_ref: Option<&TurnId>,
        reason: &str,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
    ) -> Result<(), EngineError> {
        log.append(
            turn_ref,
            EngineEvent::ToolCallFinished {
                call_id: call.call_id.clone(),
                name: call.name.clone(),
                is_error: true,
                preview: reason.to_string(),
                agent_id: call.agent_id.clone(),
            },
            observer,
        )
        .await
    }
    /// Continue the direct strategy from a prior transcript: one resume turn,
    /// then the same verify + bounded repair as a fresh run.
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
                goal_profile(spec),
                TurnInput::Resume(prior),
                observer,
                cancellation.clone(),
            )
            .await?;
        let outcome = self
            .supervise(
                log,
                runner,
                spec,
                recorded.outcome,
                observer,
                cancellation.clone(),
            )
            .await?;
        self.conclude_direct(log, runner, spec, outcome, observer, cancellation)
            .await
    }

    /// The direct strategy: one goal turn, then verify + bounded repair.
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
        let recorded = runner
            .run_turn(
                TurnKind::User,
                goal_profile(spec),
                TurnInput::Goal {
                    goal: spec.runtime.goal.clone(),
                    prior,
                },
                observer,
                cancellation.clone(),
            )
            .await?;
        // Epoch spend lives on ProgressLedger inside the drive (seeded across
        // continue/resume). Do not re-accumulate here — that would double-count.
        let outcome = self
            .supervise(
                log,
                runner,
                spec,
                recorded.outcome,
                observer,
                cancellation.clone(),
            )
            .await?;
        self.conclude_direct(log, runner, spec, outcome, observer, cancellation)
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
        let raw =
            crate::RawTranscript::load_lossy(self.stores.messages.as_ref(), session_id).await?;
        if raw.is_empty() {
            return Ok(Vec::new());
        }
        // Long-goal P3: the interactive multi-turn path is where a long
        // session's history actually grows — over the fold threshold it
        // continues from a durable checkpoint (fresh or cut here) instead of
        // a blunt last-N tail of replayed history.
        if let Some(prior) = self
            .checkpointed_prior(log, session_id, &raw.messages, repo, cancellation, observer)
            .await?
        {
            return Ok(prior);
        }
        let context = raw
            .assemble(
                log,
                None,
                Some(goal),
                u64::from(crate::ContextPolicy::chat_default().initial_budget),
            )
            .await?;
        Ok(bound_goal_history(context.prior, GOAL_HISTORY_MAX))
    }

    /// Goal continuity: a quiet turn does not end an unbounded
    /// goal. Start another persisted turn from the latest model-visible context
    /// until the model explicitly completes/blocks, the user cancels, or an
    /// explicit resource limit stops the executor.
    /// The stalled-goal rule, kept as a named delegate onto the default
    /// policy so the rule has one implementation and its tests keep pointing
    /// at the behavior users actually get.
    #[cfg(test)]
    pub(crate) fn stalled_goal_may_continue(
        stop_reason: leveler_agent::StopReason,
        progress: &leveler_lifecycle::ProgressLedger,
        caps: leveler_lifecycle::ProgressCaps,
    ) -> bool {
        stop_reason == leveler_agent::StopReason::Stalled && progress.allows_engine_continue(caps)
    }

    /// The ONE supervision loop: after each turn, ask the policy whether the
    /// task deserves another one and run the mechanism it names. The engine
    /// keeps every bound — a pinned round budget, the extension cap, the
    /// absolute [`MAX_SUPERVISED_TURNS`] ceiling, and cancellation — so a
    /// policy can shorten a run but never make one unbounded.
    async fn supervise(
        &self,
        log: &EventLog<'_>,
        runner: &TurnRunner<'_>,
        spec: &TaskSpec,
        mut outcome: leveler_agent::AgentOutcome,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
        cancellation: CancellationToken,
    ) -> Result<leveler_agent::AgentOutcome, EngineError> {
        let policy = self.supervisor_policy();
        let mut extensions = 0u32;
        let mut limits = spec.runtime.limits;
        // Goal-invocation-scoped, in-memory (not persisted): how many windows in
        // a row have produced no effective work. Bounds multi-window
        // continuation so a stuck goal converges without a durable window ledger.
        let mut windows_without_progress = 0u32;
        let mut progress_mark = outcome.modified_files.len();
        // R011-F1: the file-set mark alone starved refinement windows (fixing
        // files already written read as no progress and killed the goal). The
        // persisted evidence ledger carries two more window-grained signals:
        // total mutation OPERATIONS (re-edits included) and whether a green
        // verification now covers the latest mutations. Loaded per window —
        // windows are rare, so the full-log scan is affordable here.
        let (mut ops_mark, mut fresh_verify_mark) =
            evidence_progress_marks(runner.stores.events.as_ref(), &runner.session_id).await;

        for _ in 0..MAX_SUPERVISED_TURNS {
            if cancellation.is_cancelled() {
                break;
            }
            let decision = policy.after_turn(&crate::TurnEnded {
                stop_reason: outcome.stop_reason,
                stop_detail: outcome.stop_detail.as_deref(),
                progress: &outcome.progress,
                budget_exhaustion: outcome.budget_exhaustion.as_ref(),
                modified_files: &outcome.modified_files,
                extensions_granted: extensions,
                round_budget: spec.runtime.continuation,
                windows_without_progress,
            });

            let continued = match decision {
                crate::Continuation::Stop => {
                    // Name why a quiet goal was not nudged again, so the stop
                    // reads as a decision instead of an unexplained end.
                    if outcome.stop_reason == StopReason::Stalled && outcome.stop_detail.is_none() {
                        outcome.stop_detail =
                            Some("continue suppressed: no-progress cap".to_string());
                    }
                    break;
                }
                crate::Continuation::DriveGoalAgain => {
                    // Engine bound, not policy: a pinned round budget is a task
                    // TOTAL. The next window may only consume what the epoch
                    // has not already spent; at zero remainder no window opens,
                    // whatever the policy asked for.
                    let Some(continuation) = continuation_for_next_window(
                        spec.runtime.continuation,
                        outcome.progress.cumulative_rounds,
                    ) else {
                        break;
                    };
                    self.drive_goal_again(
                        log,
                        runner,
                        spec,
                        continuation,
                        &outcome,
                        observer,
                        &cancellation,
                    )
                    .await?
                }
                crate::Continuation::ExtendBudget(exhaustion) => {
                    extensions = extensions.saturating_add(1);
                    limits = crate::continuation::extended_limits(limits, &exhaustion);
                    observer(EngineEvent::AdvisoryStarted {
                        kind: format!(
                            "budget_extension:{}/{}:{}",
                            extensions,
                            crate::MAX_EXTENSIONS,
                            exhaustion.dimension.as_str()
                        ),
                    });
                    match self
                        .resume_with_limits(runner, spec, limits, observer, &cancellation)
                        .await?
                    {
                        Some(continued) => continued,
                        // Nothing to resume from: stop rather than spin.
                        None => break,
                    }
                }
            };
            merge_continued_outcome(&mut outcome, continued);
            // Effective work this window, from three independent signals:
            //   1. the modified-file set grew (first-touch writes),
            //   2. mutation OPERATIONS advanced — refinement of files already
            //      written counts (R011-F1),
            //   3. a green verification newly covers the latest mutations
            //      (a window spent turning red checks green is progress).
            // Repeated reads/searches and rerunning an already-green check move
            // none of these, so genuine spinning still hits the cap.
            let (ops_now, fresh_now) =
                evidence_progress_marks(runner.stores.events.as_ref(), &runner.session_id).await;
            let made_progress = outcome.modified_files.len() > progress_mark
                || ops_now > ops_mark
                || (fresh_now && !fresh_verify_mark);
            progress_mark = outcome.modified_files.len();
            ops_mark = ops_mark.max(ops_now);
            fresh_verify_mark = fresh_now;
            windows_without_progress =
                advance_no_progress_windows(windows_without_progress, made_progress);
        }
        Ok(outcome)
    }

    /// Mechanism for [`crate::Continuation::DriveGoalAgain`]: restate the
    /// objective over the latest model-visible context and run one more turn.
    /// `continuation` is the window's round policy — for pinned budgets the
    /// supervisor passes the REMAINING total, never the original.
    #[allow(clippy::too_many_arguments)]
    async fn drive_goal_again(
        &self,
        log: &EventLog<'_>,
        runner: &TurnRunner<'_>,
        spec: &TaskSpec,
        continuation: ContinuationPolicy,
        outcome: &leveler_agent::AgentOutcome,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<leveler_agent::AgentOutcome, EngineError> {
        // Announce BEFORE the transcript load / compaction / model call.
        // Everything below is invisible work that happens after the user
        // already read a final answer; without this the status line shows
        // a bare "waiting for model" for the whole continuation.
        observer(EngineEvent::AdvisoryStarted {
            kind: leveler_agent::AdvisoryKind::GoalContinuation
                .as_key()
                .to_string(),
        });
        let raw = crate::RawTranscript::load_strict(
            self.stores.messages.as_ref(),
            &runner.session_id,
            "goal transcript during continuation",
        )
        .await?;
        // Long-goal P3: a window boundary over the fold threshold continues
        // from a durable checkpoint (fresh or cut here) instead of replaying
        // the earlier windows' history.
        let checkpoint_prior = self
            .checkpointed_prior(
                log,
                &runner.session_id,
                &raw.messages,
                Some(&spec.coding.repository),
                cancellation,
                observer,
            )
            .await?;
        let prior = if let Some(prior) = checkpoint_prior {
            prior
        } else {
            let summary = self.summarize_if_over(&raw.messages, cancellation).await;
            let context = raw
                .assemble(
                    log,
                    summary.as_deref(),
                    Some(spec.runtime.goal.as_str()),
                    u64::from(crate::ContextPolicy::chat_default().initial_budget),
                )
                .await?;
            if context.compacted {
                log.append(None, context.snapshot_event(), observer).await?;
            }
            context.prior
        };
        // Full objective restatement — not a vague "Continue…" only. When
        // the previous drive recorded WHY its closeout stalled, name that
        // gap so the continuation addresses it instead of repeating the
        // same summary into the same wall.
        let closeout_note = outcome
            .stop_detail
            .as_deref()
            .and_then(leveler_agent::closeout::reason_from_stalled_detail)
            .map(|reason| {
                format!(
                    "\n\nThe previous turn's closeout stalled on: {}. Close that specific \
                     gap first instead of re-stating what was already done.",
                    reason.as_key()
                )
            })
            .unwrap_or_default();
        let continue_text = format!(
            "Continue working toward the active goal. The previous turn ended without \
             proving completion.{closeout_note}\n\n\
             <objective>\n{}\n</objective>\n\n\
             Inspect the current workspace, make concrete progress, and call update_goal \
             only when the full objective is complete or genuinely blocked. Do not \
             re-audit already finished plan steps with git status thrash.",
            spec.runtime.goal
        );
        let recorded = runner
            .run_turn(
                TurnKind::User,
                TurnProfile::Goal {
                    continuation,
                    limits: spec.runtime.limits,
                },
                TurnInput::Content {
                    prior,
                    content: vec![leveler_model::ContentPart::Text {
                        text: continue_text,
                    }],
                },
                observer,
                cancellation.clone(),
            )
            .await?;
        Ok(recorded.outcome)
    }

    /// Mechanism for [`crate::Continuation::ExtendBudget`]: resume the
    /// transcript with the granted allowance. `None` when there is no
    /// transcript to resume from.
    async fn resume_with_limits(
        &self,
        runner: &TurnRunner<'_>,
        spec: &TaskSpec,
        limits: StepLimits,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<Option<leveler_agent::AgentOutcome>, EngineError> {
        let raw = crate::RawTranscript::load_strict(
            self.stores.messages.as_ref(),
            &runner.session_id,
            "transcript during budget extension",
        )
        .await?;
        if raw.is_empty() {
            return Ok(None);
        }
        let recorded = runner
            .run_turn(
                TurnKind::User,
                TurnProfile::Goal {
                    continuation: spec.runtime.continuation,
                    limits,
                },
                TurnInput::Resume(raw.messages),
                observer,
                cancellation.clone(),
            )
            .await?;
        Ok(Some(recorded.outcome))
    }

    /// Shared tail of fresh and resumed direct runs: map the stop reason,
    /// then verify + bounded repair.
    /// Evaluate and (when required) run the independent review at the CLOSURE
    /// boundary — once per task, for every conclude_direct exit that follows a
    /// real product mutation. R011-F2: binding this to the Verified label meant
    /// a failed wide-diff goal — where a reviewer pays most — could never get
    /// one. R013-F1: a launch failure was swallowed into an unexplained
    /// downgrade; every branch here persists a `review_stage` event first.
    ///
    /// The review result never upgrades or downgrades a non-Verified outcome;
    /// only the Verified label depends on it (a required review that did not
    /// complete keeps refusing Verified, exactly as before).
    async fn closure_review_stage(
        &self,
        log: &EventLog<'_>,
        runner: &TurnRunner<'_>,
        spec: &TaskSpec,
        outcome: &leveler_agent::AgentOutcome,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<ClosureReview, EngineError> {
        let stage = |required: bool, action: &str, detail: String| EngineEvent::ReviewStage {
            required,
            action: action.to_string(),
            detail,
        };
        if outcome.modified_files.is_empty() {
            log.append(
                None,
                stage(false, "not_required", "no product mutation".to_string()),
                observer,
            )
            .await?;
            return Ok(ClosureReview::NotRequired);
        }
        use crate::policy_resolver::IndependentReviewPolicy;
        match self.factory.independent_review {
            IndependentReviewPolicy::Off => {
                log.append(
                    None,
                    stage(false, "not_required", "independent_review off".to_string()),
                    observer,
                )
                .await?;
                return Ok(ClosureReview::NotRequired);
            }
            IndependentReviewPolicy::Always | IndependentReviewPolicy::Auto => {}
        }
        let trigger = crate::policy_resolver::ReviewTrigger::from_modified_paths(
            &outcome.modified_files,
            matches!(
                self.factory.independent_review,
                IndependentReviewPolicy::Always
            ),
        );
        let reason = review_reason(&trigger);
        if !trigger.review_required() {
            log.append(None, stage(false, "not_required", reason), observer)
                .await?;
            return Ok(ClosureReview::NotRequired);
        }
        if crate::turn::session_had_review(runner.stores.events.as_ref(), &runner.session_id)
            .await
            .unwrap_or(false)
        {
            log.append(None, stage(true, "already_reviewed", reason), observer)
                .await?;
            return Ok(ClosureReview::AlreadyReviewed);
        }
        // Persist the attempt BEFORE it runs, so even a crash mid-launch
        // leaves a breadcrumb instead of silence.
        log.append(None, stage(true, "launching", reason.clone()), observer)
            .await?;
        let diff = review_diff(&spec.coding.repository, &outcome.modified_files).await;
        match runner
            .run_review(
                goal_profile(spec),
                review_brief(&spec.runtime.goal, &outcome.modified_files, diff.as_deref()),
                outcome.modified_files.clone(),
                observer,
                cancellation.clone(),
            )
            .await
        {
            Ok(true) => {
                log.append(None, stage(true, "finished_ok", reason), observer)
                    .await?;
                Ok(ClosureReview::Completed)
            }
            Ok(false) => {
                log.append(None, stage(true, "finished_incomplete", reason), observer)
                    .await?;
                Ok(ClosureReview::Incomplete)
            }
            Err(error) => {
                tracing::warn!(%error, "required review could not be launched");
                log.append(
                    None,
                    stage(true, "launch_failed", format!("{reason}: {error}")),
                    observer,
                )
                .await?;
                Ok(ClosureReview::LaunchFailed)
            }
        }
    }

    async fn conclude_direct(
        &self,
        log: &EventLog<'_>,
        runner: &TurnRunner<'_>,
        spec: &TaskSpec,
        mut outcome: leveler_agent::AgentOutcome,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
        cancellation: CancellationToken,
    ) -> Result<TaskReport, EngineError> {
        // Goal continuation and bounded budget extension already happened in
        // `supervise` — one loop, one decision point (convergence plan phase 4).

        // Completed and Answered both count as clean finishes — and both must
        // verify if they touched files. Most other stop reasons are terminal
        // failure (Stalled/Blocked/BudgetExhausted never read as success).
        // Incomplete with mutations is the exception: thrash guards may stop a
        // turn whose workspace is already gate-green — still enter verify so
        // expect-green work is not reported as Failed (orchestrate node_status
        // uses the same rule).
        if let Some(terminal) = direct_non_success_outcome(outcome.stop_reason) {
            let incomplete_with_work =
                outcome.stop_reason == StopReason::Incomplete && !outcome.modified_files.is_empty();
            if !incomplete_with_work {
                // R011-F2: a failed high-risk change needs the required review
                // MORE, not less. The result is recorded for the user; the
                // failed outcome itself never changes.
                let _ = self
                    .closure_review_stage(log, runner, spec, &outcome, observer, &cancellation)
                    .await?;
                return Ok(report_from_agent_outcome(outcome, terminal));
            }
        }

        // K19 early short-circuit: no mutation or no gates → never claim Verified
        // (pure Q&A over a green repo must stay CompletedUnverified).
        if outcome.modified_files.is_empty() || !spec.coding.verification.has_gates() {
            let _ = self
                .closure_review_stage(log, runner, spec, &outcome, observer, &cancellation)
                .await?;
            return Ok(report_from_agent_outcome(
                outcome,
                TaskOutcome::CompletedUnverified,
            ));
        }

        let mut report = self
            .verify(
                log,
                spec,
                &[],
                &outcome.modified_files,
                observer,
                &cancellation,
            )
            .await?;
        let mut attempts = 0;
        while report.verdict() == Verdict::Failed
            && attempts < DIRECT_REPAIR_ATTEMPTS
            && verification_is_repairable(&report)
            && !cancellation.is_cancelled()
        {
            attempts += 1;
            log.append(
                None,
                EngineEvent::RepairStarted { attempt: attempts },
                observer,
            )
            .await?;
            let repair = runner
                .run_turn(
                    TurnKind::Repair { attempt: attempts },
                    goal_profile(spec),
                    TurnInput::Goal {
                        goal: repair_goal(&spec.runtime.goal, &report),
                        prior: Vec::new(),
                    },
                    observer,
                    cancellation.clone(),
                )
                .await?;
            outcome.rounds += repair.outcome.rounds;
            outcome.final_text = repair.outcome.final_text;
            for path in repair.outcome.modified_files {
                if !outcome.modified_files.contains(&path) {
                    outcome.modified_files.push(path);
                }
            }
            report = self
                .verify(
                    log,
                    spec,
                    &[],
                    &outcome.modified_files,
                    observer,
                    &cancellation,
                )
                .await?;
        }

        // Shared closed-loop exit with Orchestrate (design §1.3–§1.4 / PR-7).
        // needs_mutation is heuristic/delivery only — never derived from
        // modified_files (self-referential). has_mutation is separate.
        //
        // The project's own gating checks already ran against the edited tree
        // and that is the verdict. Direct used to spend one more full model call
        // here asking the model to restate its goal as acceptance criteria and
        // then evaluate them — criteria that could only ever downgrade a green
        // gate, and that measurably did so for reasons that had nothing to do
        // with the code (see `leveler_verifier::outcome` docs). The call is gone.
        let expected = ExpectedEvidence {
            needs_mutation: direct_needs_mutation(
                &spec.runtime.goal,
                matches!(
                    self.factory.work_profile,
                    leveler_agent::WorkProfile::Delivery
                ),
            ),
            has_mutation: !outcome.modified_files.is_empty(),
        };
        let mut task_outcome = map_completion_verdict(finalize_task_outcome(&report, expected));
        // Terminal audit applies the same open-todo rule readiness::check
        // enforces on the model's own completion claim: green gates over a plan
        // with open steps is healthy-tree evidence, not task completion. An
        // honest model that DECLINES to claim completion must not receive a
        // better verdict than one that claims it (R004 F4).
        if task_outcome == TaskOutcome::Verified
            && let Ok(Some(plan)) =
                crate::turn::last_persisted_plan(runner.stores.events.as_ref(), &runner.session_id)
                    .await
            && plan.has_incomplete_model_todos()
        {
            task_outcome = TaskOutcome::CompletedUnverified;
        }
        // R007b N2: the same rule applied to verification EVIDENCE. A check
        // that passed before this task changed anything says the tree already
        // satisfied it — on a goal that was expected to change code, that is
        // not proof the work was done. R007b's agent watched a reproduction go
        // green on an untouched tree, concluded the defect did not exist, and
        // drifted to an unrelated fix; the runtime must not call that verified.
        if task_outcome == TaskOutcome::Verified
            && expected.has_mutation
            && let Ok(Some(ledger)) = crate::turn::last_persisted_ledger(
                runner.stores.events.as_ref(),
                &runner.session_id,
            )
            .await
            && ledger.only_baseline_green_evidence()
        {
            task_outcome = TaskOutcome::CompletedUnverified;
        }
        // TERMINAL TRUTH: the Completion Contract, asked at the boundary rather
        // than only at the door the executor happens to use. `update_goal`
        // consults it, but a run can reach this point without ever calling
        // update_goal — a forced closeout, for instance — and a green
        // workspace over an unwritten test used to be mapped straight to
        // verified. Closeout is a lifecycle condition, not proof of
        // completion; tests going green is evidence, not proof that every
        // requirement was met. Same debt, whichever door the run came through.
        if task_outcome == TaskOutcome::Verified {
            match crate::turn::last_persisted_ledger(
                runner.stores.events.as_ref(),
                &runner.session_id,
            )
            .await
            {
                // Fail closed: an unreadable ledger cannot prove there is no
                // outstanding obligation.
                Err(e) => {
                    task_outcome = TaskOutcome::CompletedUnverified;
                    log.append(
                        None,
                        EngineEvent::ReviewStage {
                            required: true,
                            action: "completion_contract_open".to_string(),
                            detail: format!("completion contract unreadable: {e}"),
                        },
                        observer,
                    )
                    .await?;
                }
                Ok(Some(ledger)) => {
                    if let Some(debt) = ledger.completion_debt() {
                        task_outcome = TaskOutcome::CompletedUnverified;
                        log.append(
                            None,
                            EngineEvent::ReviewStage {
                                required: true,
                                action: "completion_contract_open".to_string(),
                                detail: debt,
                            },
                            observer,
                        )
                        .await?;
                    }
                }
                Ok(None) => {}
            }
        }
        // R007b N7 / R013-F1: the closure-boundary review, staged with durable
        // eligibility/launch/terminal events so an absent reviewer is always
        // explainable. A required review that did not complete keeps refusing
        // Verified, exactly as before — but never silently.
        let review = self
            .closure_review_stage(log, runner, spec, &outcome, observer, &cancellation)
            .await?;
        if task_outcome == TaskOutcome::Verified && !review.satisfies_required() {
            task_outcome = TaskOutcome::CompletedUnverified;
        }
        // Blocking-finding closure truth: an open blocking finding (raised by
        // the reviewer, not yet rejected or verified) refuses Verified. The
        // check runs AFTER the review stage so a finding adopted moments ago
        // is seen; Addressed findings are host-promoted first when fresh
        // post-mutation verification exists, so a fixed-and-proven finding
        // never blocks. The refusal is staged durably — never silent.
        if task_outcome == TaskOutcome::Verified {
            match crate::turn::last_persisted_ledger(
                runner.stores.events.as_ref(),
                &runner.session_id,
            )
            .await
            {
                Err(e) => {
                    // Fail closed: if we cannot read the ledger we cannot
                    // prove there is no open blocking finding.
                    task_outcome = TaskOutcome::CompletedUnverified;
                    log.append(
                        None,
                        EngineEvent::ReviewStage {
                            required: true,
                            action: "blocking_finding_open".to_string(),
                            detail: format!("findings ledger unreadable: {e}"),
                        },
                        observer,
                    )
                    .await?;
                }
                Ok(None) => {}
                Ok(Some(ledger)) if ledger.findings.is_empty() => {}
                Ok(Some(mut ledger)) => {
                    if ledger.promote_addressed_findings(ledger.has_fresh_successful_verify()) > 0 {
                        log.append(
                            None,
                            EngineEvent::EvidenceLedgerUpdated {
                                ledger: ledger.clone(),
                            },
                            observer,
                        )
                        .await?;
                    }
                    let open: Vec<String> = ledger
                        .open_blocking_findings()
                        .iter()
                        .map(|f| format!("{} ({}: {})", f.id, f.state.label(), f.summary))
                        .collect();
                    if !open.is_empty() {
                        task_outcome = TaskOutcome::CompletedUnverified;
                        log.append(
                            None,
                            EngineEvent::ReviewStage {
                                required: true,
                                action: "blocking_finding_open".to_string(),
                                detail: open.join("; "),
                            },
                            observer,
                        )
                        .await?;
                    }
                }
            }
        }
        let base = report_from_agent_outcome(outcome, task_outcome);
        Ok(TaskReport {
            verification: Some(report),
            ..base
        })
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
            crate::baseline::reconcile_with_baseline(
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
                    status: format!("{:?}", check.status).to_lowercase(),
                    evidence: matches!(
                        check.status,
                        leveler_verifier::CheckStatus::Failed
                            | leveler_verifier::CheckStatus::ToolMissing
                            | leveler_verifier::CheckStatus::EnvironmentUnavailable
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
                passed: report.passed(),
            },
            observer,
        )
        .await?;
        Ok(report)
    }
}

/// Whether a failed report is worth a repair turn: scope violations are not
/// repairable, and neither is a failure classified as non-retryable
/// (environment problems).
fn verification_is_repairable(report: &VerificationReport) -> bool {
    report.scope_ok
        && report
            .failed_gates()
            .into_iter()
            .any(|check| check.failure.as_ref().map(|f| f.retryable).unwrap_or(true))
}

/// Compose the repair goal from the failed report (engine-local equivalent of
/// the app layer's compose_repair_goal).
/// How the closure-boundary review ended, for the Verified label decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClosureReview {
    NotRequired,
    AlreadyReviewed,
    Completed,
    Incomplete,
    LaunchFailed,
}

impl ClosureReview {
    /// Whether this result satisfies a required-review obligation.
    fn satisfies_required(self) -> bool {
        matches!(
            self,
            ClosureReview::NotRequired | ClosureReview::AlreadyReviewed | ClosureReview::Completed
        )
    }
}

/// One sentence naming why policy required (or did not require) a review.
fn review_reason(trigger: &crate::policy_resolver::ReviewTrigger) -> String {
    let mut parts = Vec::new();
    if trigger.security_relevant {
        parts.push("security-sensitive path".to_string());
    }
    if trigger.concurrency_relevant {
        parts.push("concurrency-sensitive path".to_string());
    }
    parts.push(format!("{} modified file(s)", trigger.modified_files));
    parts.join(", ")
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

fn repair_goal(goal: &str, report: &VerificationReport) -> String {
    let mut failures = String::new();
    for check in report.failed_gates() {
        failures.push_str(&format!(
            "\n- `{}` failed:\n{}\n",
            check.name, check.evidence
        ));
    }
    format!(
        "Verification failed after working on: {goal}\n\nFailing checks:{failures}\n\
         Repair only the failing change, keep the scope narrow, then re-run the \
         failing checks to prove they pass."
    )
}

/// Map verifier [`CompletionVerdict`] onto lifecycle [`TaskOutcome`].
fn map_completion_verdict(v: CompletionVerdict) -> TaskOutcome {
    match v {
        CompletionVerdict::Verified => TaskOutcome::Verified,
        CompletionVerdict::CompletedUnverified => TaskOutcome::CompletedUnverified,
        CompletionVerdict::Failed => TaskOutcome::Failed,
    }
}
/// Direct ExpectedMutation decision (design §1.3 / K19).
///
/// `needs_mutation = task_looks_like_implementation(goal) || delivery_gate`.
/// Must **never** use `modified_files` / `has_mutation` (self-referential).
/// K19 early-exit in `conclude_direct` additionally forbids Verified when
/// there is no mutation at all (even if `needs_mutation` is false).
fn direct_needs_mutation(goal: &str, delivery_gate: bool) -> bool {
    delivery_gate || leveler_lifecycle::task_looks_like_implementation(goal)
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
mod needs_mutation_tests {
    use super::*;

    #[test]
    fn map_completion_verdict_covers_all_variants() {
        assert_eq!(
            map_completion_verdict(CompletionVerdict::Verified),
            TaskOutcome::Verified
        );
        assert_eq!(
            map_completion_verdict(CompletionVerdict::CompletedUnverified),
            TaskOutcome::CompletedUnverified
        );
        assert_eq!(
            map_completion_verdict(CompletionVerdict::Failed),
            TaskOutcome::Failed
        );
    }

    #[test]
    fn direct_needs_mutation_is_heuristic_or_delivery_not_files() {
        // Pure Q&A: no impl verbs → needs_mutation false (regardless of files).
        assert!(!direct_needs_mutation("explain how auth works", false));
        // Delivery forces needs_mutation even on a Q&A-shaped goal.
        assert!(direct_needs_mutation("explain how auth works", true));
        // Implementation-class goals require mutation.
        assert!(direct_needs_mutation("add a function", false));
        assert!(direct_needs_mutation("fix the login bug", false));
    }
}

#[cfg(test)]
mod continue_cap_tests {
    use super::*;
    use leveler_lifecycle::{ProgressCaps, ProgressLedger};

    #[test]
    fn stalled_with_no_progress_cap_must_not_auto_continue() {
        let caps = ProgressCaps::default();
        let mut progress = ProgressLedger::default();
        progress.note_no_progress_round(1);
        progress.note_no_progress_round(2);
        assert!(
            !TaskEngine::stalled_goal_may_continue(
                leveler_agent::StopReason::Stalled,
                &progress,
                caps,
            ),
            "engine must not open another turn after no-progress cap"
        );
    }

    // M-2 — the multi-window loop is hard-bounded. These drive the SAME policy
    // (`after_turn`) and the SAME counter rule (`advance_no_progress_windows`)
    // that `supervise()` uses, in the same order, so the termination guarantee is
    // exercised deterministically without a live model.
    fn goal_ended(
        progress: &ProgressLedger,
        windows_without_progress: u32,
    ) -> crate::TurnEnded<'_> {
        crate::TurnEnded {
            stop_reason: leveler_agent::StopReason::TurnLimitReached,
            stop_detail: None,
            progress,
            budget_exhaustion: None,
            modified_files: &[],
            extensions_granted: 0,
            round_budget: leveler_agent::ContinuationPolicy::UntilTerminal,
            windows_without_progress,
        }
    }

    #[test]
    fn a_stuck_goal_stops_within_the_no_progress_cap_not_the_hard_bound() {
        use crate::SupervisorPolicy;
        use crate::continuation::MAX_NO_PROGRESS_WINDOWS;
        let policy = crate::DefaultSupervisorPolicy::default();
        let progress = ProgressLedger {
            phase: leveler_lifecycle::TurnPhase::Active,
            closing: false,
            ..Default::default()
        };
        let mut wwp = 0u32;
        let mut windows_opened = 0u32;
        let mut stopped = false;
        // Every window hits the ceiling and grows no files (no material progress).
        for _ in 0..MAX_SUPERVISED_TURNS {
            match policy.after_turn(&goal_ended(&progress, wwp)) {
                crate::Continuation::Stop => {
                    stopped = true;
                    break;
                }
                _ => {
                    windows_opened += 1;
                    wwp = advance_no_progress_windows(wwp, /*made_progress*/ false);
                }
            }
        }
        assert!(
            stopped,
            "a stuck goal must stop, never reach the {MAX_SUPERVISED_TURNS}-window hard bound"
        );
        assert!(
            windows_opened <= MAX_NO_PROGRESS_WINDOWS,
            "opened {windows_opened} windows; must converge within the no-progress cap {MAX_NO_PROGRESS_WINDOWS} (< the {MAX_SUPERVISED_TURNS}-window hard bound)"
        );
    }

    #[test]
    fn even_a_progressing_never_completing_goal_is_capped_by_the_hard_bound() {
        use crate::SupervisorPolicy;
        let policy = crate::DefaultSupervisorPolicy::default();
        let progress = ProgressLedger {
            phase: leveler_lifecycle::TurnPhase::Active,
            closing: false,
            ..Default::default()
        };
        let mut wwp = 0u32;
        let mut windows_opened = 0u32;
        // Every window makes progress (counter resets), so the no-progress cap
        // never fires — only the absolute MAX_SUPERVISED_TURNS bounds the loop.
        for _ in 0..MAX_SUPERVISED_TURNS {
            match policy.after_turn(&goal_ended(&progress, wwp)) {
                crate::Continuation::Stop => break,
                _ => {
                    windows_opened += 1;
                    wwp = advance_no_progress_windows(wwp, /*made_progress*/ true);
                }
            }
        }
        assert_eq!(
            windows_opened, MAX_SUPERVISED_TURNS,
            "a progressing goal opens windows up to — and never beyond — the hard bound"
        );
    }

    #[test]
    fn the_next_window_gets_the_remaining_pinned_budget_only() {
        use leveler_agent::ContinuationPolicy;
        // ORC-B1 shape: 280 total, first window spent 100 → the integration
        // window may consume at most the remaining 180.
        assert_eq!(
            continuation_for_next_window(ContinuationPolicy::bounded(280), 100),
            Some(ContinuationPolicy::bounded(180))
        );
        // Exhausted total: no window opens, whatever the policy decided.
        assert_eq!(
            continuation_for_next_window(ContinuationPolicy::bounded(280), 280),
            None
        );
        assert_eq!(
            continuation_for_next_window(ContinuationPolicy::bounded(280), 300),
            None
        );
        // Unpinned goals keep their policy untouched.
        assert_eq!(
            continuation_for_next_window(ContinuationPolicy::UntilTerminal, 100),
            Some(ContinuationPolicy::UntilTerminal)
        );
    }

    #[test]
    fn settlement_debt_windows_consume_the_pinned_total_and_terminate() {
        // Deterministic simulation of the supervise loop for the accident
        // shape: SAME policy decision (`after_turn`), SAME counter rule
        // (`advance_no_progress_windows`), SAME engine clamp
        // (`continuation_for_next_window`) — a goal whose every window ceilings
        // with unconsumed settlement debt spends the 280 total exactly, never
        // more, across clamped windows (100-round local ceiling per window).
        use crate::SupervisorPolicy;
        use leveler_agent::ContinuationPolicy;
        const TOTAL: u32 = 280;
        const LOCAL_WINDOW: u32 = 100;
        let policy = crate::DefaultSupervisorPolicy::default();
        let mut spent = LOCAL_WINDOW; // window 1 hit the local ceiling
        let mut wwp = 0u32;
        let mut windows_opened = 0u32;
        for _ in 0..MAX_SUPERVISED_TURNS {
            let progress = ProgressLedger {
                phase: leveler_lifecycle::TurnPhase::Active,
                closing: false,
                cumulative_rounds: spent,
                unconsumed_child_settlements: 1,
                ..Default::default()
            };
            let mut ended = goal_ended(&progress, wwp);
            ended.round_budget = ContinuationPolicy::bounded(TOTAL);
            match policy.after_turn(&ended) {
                crate::Continuation::Stop => break,
                crate::Continuation::DriveGoalAgain => {
                    let Some(next) =
                        continuation_for_next_window(ContinuationPolicy::bounded(TOTAL), spent)
                    else {
                        break;
                    };
                    let window = next
                        .round_limit()
                        .expect("pinned budgets clamp to a bounded window")
                        .min(LOCAL_WINDOW);
                    windows_opened += 1;
                    spent += window;
                    // Integration windows move the workspace in this shape.
                    wwp = advance_no_progress_windows(wwp, /*made_progress*/ true);
                }
                other => panic!("unexpected continuation {other:?}"),
            }
        }
        assert!(
            spent <= TOTAL,
            "debt windows must never spend past the pinned total (spent {spent})"
        );
        assert_eq!(
            spent, TOTAL,
            "with debt standing the goal consumes the full remaining budget"
        );
        assert_eq!(
            windows_opened, 2,
            "280 total / 100 local = two follow-up windows"
        );
    }

    #[test]
    fn stalled_with_fresh_progress_may_continue() {
        let caps = ProgressCaps::default();
        let mut progress = ProgressLedger::default();
        progress.note_progress(1);
        assert!(TaskEngine::stalled_goal_may_continue(
            leveler_agent::StopReason::Stalled,
            &progress,
            caps,
        ));
    }

    #[test]
    fn non_stalled_never_continues() {
        let caps = ProgressCaps::default();
        let progress = ProgressLedger::default();
        assert!(!TaskEngine::stalled_goal_may_continue(
            leveler_agent::StopReason::Answered,
            &progress,
            caps,
        ));
        assert!(!TaskEngine::stalled_goal_may_continue(
            leveler_agent::StopReason::Incomplete,
            &progress,
            caps,
        ));
    }

    #[test]
    fn budget_extension_policy_grants_refuses_and_caps() {
        use leveler_agent::{
            BudgetDimension, BudgetExhaustion, MAX_BUDGET_EXTENSIONS, StepLimits,
            budget_extension_allowed, grant_budget_extension, stop_detail_indicates_no_progress,
        };

        // Grant path: BudgetExhausted + mutation + room under MAX.
        assert!(budget_extension_allowed(
            leveler_agent::StopReason::BudgetExhausted,
            0,
            true,
            false
        ));
        // Refuse: no real progress.
        assert!(!budget_extension_allowed(
            leveler_agent::StopReason::BudgetExhausted,
            0,
            false,
            false
        ));
        // Refuse: stagnation / no-progress detail.
        assert!(stop_detail_indicates_no_progress(Some(
            "no-progress streak; all-refused rounds short-circuited"
        )));
        assert!(!budget_extension_allowed(
            leveler_agent::StopReason::BudgetExhausted,
            0,
            true,
            true
        ));
        // Refuse: absolute round ceiling.
        assert!(!budget_extension_allowed(
            leveler_agent::StopReason::TurnLimitReached,
            0,
            true,
            false
        ));
        // Cap exhaustion.
        assert!(!budget_extension_allowed(
            leveler_agent::StopReason::BudgetExhausted,
            MAX_BUDGET_EXTENSIONS,
            true,
            false
        ));
        // Grant raises the fired dimension above spent.
        let limits = StepLimits {
            max_model_tokens: Some(100),
            ..StepLimits::default()
        };
        let next = grant_budget_extension(
            limits,
            &BudgetExhaustion::new(BudgetDimension::ModelTokens, 100, 100),
        );
        assert_eq!(next.max_model_tokens, Some(150));
    }

    /// R004 F4: `terminal_status_for` must not launder a guard-forced
    /// Incomplete stop into a Completed session because the gates were green.
    #[test]
    fn incomplete_stop_keeps_session_incomplete_even_when_verified() {
        let report = TaskReport {
            outcome: TaskOutcome::Verified,
            final_text: String::new(),
            modified_files: vec!["a.rs".into()],
            verification: None,
            stop_reason: leveler_agent::StopReason::Incomplete,
            stop_detail: None,
            rounds: 1,
            review: None,
        };
        let (status, _) = terminal_status_for(&report);
        assert_eq!(status, SessionStatus::Incomplete);

        let clean = TaskReport {
            stop_reason: leveler_agent::StopReason::Completed,
            outcome: TaskOutcome::Verified,
            final_text: String::new(),
            modified_files: vec!["a.rs".into()],
            verification: None,
            stop_detail: None,
            rounds: 1,
            review: None,
        };
        let (status, _) = terminal_status_for(&clean);
        assert_eq!(status, SessionStatus::Completed);
    }

    #[test]
    fn thrash_incomplete_maps_to_failed_not_completed() {
        // conclude_direct uses this mapping: Incomplete thrash must surface as
        // TaskOutcome::Failed, never success/CompletedUnverified.
        assert_eq!(
            direct_non_success_outcome(leveler_agent::StopReason::Incomplete),
            Some(TaskOutcome::Failed)
        );
        assert_eq!(
            direct_non_success_outcome(leveler_agent::StopReason::Stalled),
            Some(TaskOutcome::Failed)
        );
        assert_eq!(
            direct_non_success_outcome(leveler_agent::StopReason::BudgetExhausted),
            Some(TaskOutcome::BudgetLimited)
        );
        assert_eq!(
            direct_non_success_outcome(leveler_agent::StopReason::TurnLimitReached),
            Some(TaskOutcome::BudgetLimited),
            "after multi-window continuation, a ceiling stop is a resource boundary \
             (incomplete + resumable), not a model failure"
        );
        assert_eq!(
            direct_non_success_outcome(leveler_agent::StopReason::Answered),
            None,
            "Answered continues into verify path"
        );
        assert_eq!(
            direct_non_success_outcome(leveler_agent::StopReason::Completed),
            None
        );
    }
}

/// Close every dangling tool call of a session with an explicit
/// user-acknowledged marker (an errored `ToolCallFinished`, never a fake
/// success), so a resume blocked by `RecoveryConfirmationRequired` can
/// proceed. Nothing is replayed. Returns how many calls were closed.
pub async fn acknowledge_crash_window(
    events: &dyn EventStore,
    token: &leveler_core::OwnershipToken,
    session_id: &SessionId,
) -> Result<usize, EngineError> {
    // The reconciling markers are canonical recovery facts: fenced, so a
    // stale or non-owner runtime cannot rewrite crash-window history.
    let log = EventLog::new_owned(events, session_id.clone(), token.clone());
    let dangling = log.dangling_tool_calls().await?;
    let closed = dangling.len();
    for call in dangling {
        let turn_id = call.turn_id.as_ref().map(|t| TurnId::new(t.clone()));
        log.append(
            turn_id.as_ref(),
            EngineEvent::ToolCallFinished {
                call_id: call.call_id.clone(),
                name: call.name.clone(),
                is_error: true,
                preview: "user-acknowledged crash recovery: the interrupted call's outcome \
                          is unknown; the workspace was verified manually and the call was \
                          not replayed"
                    .to_string(),
                agent_id: call.agent_id.clone(),
            },
            &mut |_| {},
        )
        .await?;
    }
    Ok(closed)
}

/// Map non-success agent stops for Direct conclude (shipped path used by
/// `conclude_direct`). `None` means continue into verification.
pub(crate) fn direct_non_success_outcome(stop: leveler_agent::StopReason) -> Option<TaskOutcome> {
    use leveler_agent::StopReason as S;
    match stop {
        // Plan done (incl. a forced closeout stop): let verification decide.
        S::Completed | S::Answered | S::CloseoutForced => None,
        // The round ceiling is a resource boundary, not a model failure. After a
        // goal has exhausted its bounded work windows (the supervisor already
        // decided to stop opening more), a ceiling stop is BudgetLimited —
        // incomplete and resumable — the same class as an exhausted budget.
        S::BudgetExhausted | S::TurnLimitReached => Some(TaskOutcome::BudgetLimited),
        // Incomplete thrash, stalled quiet, blocked (model- or policy-side),
        // etc. — never success.
        S::Incomplete | S::Blocked | S::PolicyBlocked | S::Stalled | S::CompletedUnverified => {
            Some(TaskOutcome::Failed)
        }
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
mod multi_turn_session_tests {
    use super::*;
    use leveler_model::{Message, Role};

    fn msg(role: Role, text: &str) -> Message {
        Message::text(role, text)
    }

    fn long_prior(n: usize) -> Vec<Message> {
        let mut v = vec![
            msg(Role::System, "you are leveler"),
            msg(Role::User, "first task: fix login"),
        ];
        for i in 0..n {
            v.push(msg(Role::Assistant, &format!("working step {i} with lots of detail about the codebase path src/auth/login.rs and error handling")));
            v.push(msg(
                Role::User,
                &format!("continue step {i} please keep going on the login timeout issue"),
            ));
        }
        v
    }

    #[test]
    fn budget_prior_under_threshold_prefers_raw_over_stale_snapshot() {
        // Snapshot must never permanently replace later MessageRepository rows.
        let raw = vec![
            msg(Role::User, "first turn"),
            msg(Role::Assistant, "first answer"),
            msg(Role::User, "second turn after snapshot"),
            msg(Role::Assistant, "second answer"),
        ];
        let snap = vec![msg(Role::User, "stale snapshot only")];
        let (out, compacted) = budget_prior_messages(
            raw.clone(),
            Some(SnapshotView {
                messages: snap,
                through_ordinal: None,
            }),
            None,
            None,
            100_000,
        );
        assert!(!compacted);
        assert_eq!(out.len(), raw.len());
        assert!(
            out.iter()
                .any(|m| m.text_content().contains("second turn after snapshot")),
            "under-threshold prior must include post-snapshot raw: {out:?}"
        );
    }

    /// HCH-OPT-5 characterization (reproduction only, no fix in this train):
    /// a legacy snapshot with `through_ordinal: None` whose LAST message is
    /// an in-memory-only injection (scoped rules / nudge — never persisted
    /// to the transcript) defeats the suffix-overlap heuristic, and the
    /// fallback appends the last 12 raw messages on top of a snapshot that
    /// already contains that same recent window.
    #[test]
    fn characterize_overlap_fallback_duplicates_the_recent_window() {
        let raw = long_prior(30);
        // The executor's in-loop snapshot: summary + the recent window it
        // already carries + a memory-only tail (never in the transcript).
        let mut snap = vec![msg(Role::User, "[compact summary of earlier work]")];
        snap.extend_from_slice(&raw[raw.len() - 12..]);
        snap.push(msg(
            Role::System,
            "Project rules:\n- memory-only injection, never persisted",
        ));
        // The duplication is visible on the "merged base already fits"
        // branch (over-threshold raw, under-threshold merge — the common
        // real shape: raw is long, the snapshot is a folded view). When the
        // merge itself is over threshold the subsequent fold swallows the
        // duplicated window into the summarized middle — efficiency waste,
        // not resent duplication.
        let mut expected_base = snap.clone();
        expected_base.extend_from_slice(&raw[raw.len() - 12..]);
        let threshold = leveler_agent::estimate_tokens(&expected_base) + 10;
        assert!(leveler_agent::estimate_tokens(&raw) > threshold);

        let (out, _) = budget_prior_messages(
            raw.clone(),
            Some(SnapshotView {
                messages: snap,
                through_ordinal: None,
            }),
            None,
            None,
            threshold,
        );

        // Quantify the duplication: every text in the last-12 raw window that
        // appears more than once in the merged output is a duplicate.
        let texts: Vec<String> = out.iter().map(|m| m.text_content()).collect();
        let duplicated = raw[raw.len() - 12..]
            .iter()
            .filter(|m| {
                let t = m.text_content();
                texts.iter().filter(|x| **x == t).count() > 1
            })
            .count();
        let inflation = leveler_agent::estimate_tokens(&raw[raw.len() - 12..]);
        let total = leveler_agent::estimate_tokens(&out);
        println!(
            "OPT5: duplicated_messages={duplicated} duplicate_window_tokens={inflation} \
             merged_total_tokens={total}"
        );
        assert!(
            duplicated > 0,
            "characterization: the fallback is expected to duplicate the window \
             (if this starts passing with 0, the heuristic changed — re-audit OPT-5)"
        );
    }

    /// HCH-FIX-2: the engine fold must bound the retained recent tail by
    /// TOKENS, not only by message count. A single huge tool result inside
    /// the last 12 messages used to ride through a 24k-threshold fold intact
    /// (keep_recent_tokens = 0), leaving ~5-19x the threshold behind.
    #[test]
    fn engine_fold_bounds_the_retained_tail_by_tokens() {
        let threshold: u64 = 24_000;
        let mut raw = long_prior(20);
        // A huge tool-ish payload well inside the last 12 messages:
        // ~300 KiB ASCII ≈ 75k estimated tokens on its own.
        raw.push(msg(Role::Assistant, &"x".repeat(300 * 1024)));
        for i in 0..3 {
            raw.push(msg(Role::Assistant, &format!("tail {i}")));
        }
        let before = leveler_agent::estimate_tokens(&raw);
        assert!(
            before > threshold,
            "precondition: over threshold ({before})"
        );

        let (folded, changed) = budget_prior_messages(
            raw,
            None,
            Some("summary of earlier work"),
            Some("obj"),
            threshold,
        );

        assert!(changed, "an over-threshold prior must fold");
        let after = leveler_agent::estimate_tokens(&folded);
        assert!(
            after <= threshold,
            "a {threshold}-token fold must not retain {after} tokens"
        );
    }

    #[test]
    fn budget_prior_merges_snapshot_tail_when_over_threshold() {
        // Oversized raw with a compact snap that ends with a shared suffix;
        // messages after that suffix must appear in the merged prior.
        let mut raw = long_prior(40);
        let shared = msg(Role::Assistant, "shared recent window tail");
        let after = msg(Role::User, "POST_SNAPSHOT_MARKER unique follow-up");
        raw.push(shared.clone());
        raw.push(after.clone());
        let snap = vec![
            msg(Role::User, "[compact summary of early work]"),
            shared.clone(),
        ];
        let tokens = leveler_agent::estimate_tokens(&raw);
        assert!(tokens > 200, "need over-threshold raw: {tokens}");
        let (out, compacted) = budget_prior_messages(
            raw,
            Some(SnapshotView {
                messages: snap,
                through_ordinal: None,
            }),
            None,
            Some("fix login"),
            200,
        );
        assert!(compacted);
        let joined: String = out
            .iter()
            .map(|m| m.text_content())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("POST_SNAPSHOT_MARKER")
                || joined.contains("shared recent window")
                || joined.contains("login"),
            "over-threshold merge/compact must not drop the active topic: {joined}"
        );
    }

    #[test]
    fn watermark_merge_survives_duplicate_rounds() {
        // Two textually IDENTICAL user/assistant rounds; the snapshot was
        // taken after the first (message watermark = 2). Suffix-overlap
        // inference matches the snapshot tail against the MOST RECENT
        // occurrence in raw and silently drops one whole round; the explicit
        // watermark appends exactly raw[2..] and keeps both.
        let pad = "padding so the token estimate clears the tiny threshold xxxxxxxxxxxxxxxx";
        let round = [
            msg(Role::User, &format!("run the tests {pad}")),
            msg(Role::Assistant, &format!("all green {pad}")),
        ];
        // A realistically LONG raw transcript whose snapshot is a small
        // folded view: the first identical round sits before the watermark,
        // the second after it. Suffix-overlap inference would match the
        // snapshot tail against the MOST RECENT occurrence and drop a round;
        // the explicit watermark appends exactly raw[wm..].
        let mut raw = long_prior(30);
        raw.extend(round.to_vec());
        let watermark = raw.len() as u64;
        raw.extend(round.to_vec());
        raw.push(msg(
            Role::User,
            &format!("what changed between runs? {pad}"),
        ));
        let snap = vec![
            msg(Role::User, "[compact summary of earlier work]"),
            round[0].clone(),
            round[1].clone(),
        ];
        let threshold = leveler_agent::estimate_tokens(&snap)
            + leveler_agent::estimate_tokens(&raw[watermark as usize..])
            + 10;
        assert!(
            leveler_agent::estimate_tokens(&raw) > threshold,
            "raw must exceed the threshold"
        );

        let (out, _) = budget_prior_messages(
            raw,
            Some(SnapshotView {
                messages: snap,
                through_ordinal: Some(watermark),
            }),
            None,
            None,
            threshold,
        );
        assert_eq!(
            out.len(),
            6,
            "snapshot(3) + raw[wm..](3): the duplicate round after the \
             watermark must survive the merge: {out:?}"
        );
    }

    #[test]
    fn budget_prior_folds_with_the_model_summary_when_given() {
        // The engine pre-request path passes a model handoff briefing; the
        // fold must carry it instead of a bare no-summary breadcrumb.
        let raw = long_prior(40);
        let (out, compacted) = budget_prior_messages(
            raw,
            None,
            Some("HANDOFF_SUMMARY_TEXT for the elided rounds"),
            Some("fix login"),
            200,
        );
        assert!(compacted);
        let joined: String = out
            .iter()
            .map(|m| m.text_content())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("HANDOFF_SUMMARY_TEXT"),
            "the provided summary must survive into the folded transcript: {joined}"
        );
    }

    #[test]
    fn budget_prior_compacts_oversized_history() {
        let raw = long_prior(40);
        let tokens = leveler_agent::estimate_tokens(&raw);
        assert!(
            tokens > 100,
            "synthetic history should be non-trivial: {tokens}"
        );
        let (out, compacted) =
            budget_prior_messages(raw.clone(), None, None, Some("fix login"), 200);
        assert!(compacted, "must take compact path when over threshold");
        assert!(
            leveler_agent::estimate_tokens(&out) < tokens || out.len() < raw.len(),
            "compacted transcript should shrink"
        );
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

    #[test]
    fn cumulative_rounds_do_not_reset_on_continue_merge() {
        // Mirrors continue_active_goal: epoch totals grow, not reset.
        let mut progress = leveler_lifecycle::ProgressLedger::default();
        progress.accumulate_drive(5, 1000);
        progress.accumulate_drive(3, 500);
        assert_eq!(progress.cumulative_rounds, 8);
        assert_eq!(progress.cumulative_model_tokens, 1500);
        // A fresh Content turn with terminal progress must not seed (epoch gate).
        progress.enter_terminal();
        assert!(progress.is_terminal_for_inheritance());
        assert!(!crate::turn::should_seed_task_state(None, Some(&progress)));
    }
}
