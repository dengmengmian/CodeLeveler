//! The task engine: session lifecycle + strategy dispatch (plan B3).
//!
//! `create_task` persists WHAT will run (goal/mode/sandbox/kind) so resume
//! never guesses; `run` executes the session's strategy over fully-persisted
//! turns and stamps the terminal [`TaskOutcome`] on the session row.

use std::path::PathBuf;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use leveler_agent::{
    Clarifier, ContinuationPolicy, MAX_BUDGET_EXTENSIONS, StepLimits, StopReason,
    budget_extension_allowed, grant_budget_extension, stop_detail_indicates_no_progress,
};
use leveler_core::{SessionId, TurnId};
use leveler_execution::{Approver, PermissionProfile, RiskLevel};
use leveler_storage::{Database, SessionRecord, SessionRepository, TerminalRepository};
use leveler_verifier::{
    CompletionVerdict, ExpectedEvidence, Verdict, VerificationPlan, VerificationReport, Verifier,
    finalize_task_outcome,
};

use crate::factory::{ExecutorFactory, TurnProfile};
use crate::log::{DanglingCall, EventLog};
use crate::turn::{TurnInput, TurnRunner};
use crate::{EngineError, EngineEvent, ExecutionKind, TaskOutcome, TurnKind};

/// How many verification-repair turns a direct task may spend.
const DIRECT_REPAIR_ATTEMPTS: u32 = 1;

/// A read-only tool (`RiskLevel::Safe`) is idempotent, so re-running it after a
/// crash has no external effect; anything that mutates the workspace or runs a
/// process is not and must not auto-replay.
fn is_auto_replayable(risk: RiskLevel) -> bool {
    risk == RiskLevel::Safe
}

/// Bound a replayed tool's output for the event-log preview (the full result is
/// not needed here — the model re-drives from the clean turn boundary).
fn recovery_preview(text: &str) -> String {
    text.chars().take(200).collect()
}

/// Everything needed to create a task.
#[derive(Clone)]
pub struct TaskSpec {
    pub repository: PathBuf,
    pub goal: String,
    pub mode: PermissionProfile,
    pub sandbox: bool,
    pub kind: ExecutionKind,
    /// Top-level continuation is independent from model capability. Interactive
    /// tasks use `UntilTerminal`; evals may supply a fixed case budget.
    pub continuation: ContinuationPolicy,
    /// Optional top-level token/cost/duration limits. Defaults are unlimited.
    /// Evaluation may additionally supply an explicit case-wide round budget.
    pub limits: StepLimits,
    /// The post-edit verification plan (empty = nothing to verify → the task
    /// can at best finish `CompletedUnverified`).
    pub verification: VerificationPlan,
    /// The repo's `HEAD` at task start, used as the baseline for delta
    /// attribution of gate failures. Callers leave this `None`; the engine
    /// stamps it (from `git rev-parse HEAD`) before the first turn edits.
    pub base_commit: Option<String>,
}

fn goal_profile(spec: &TaskSpec) -> TurnProfile {
    TurnProfile::Goal {
        continuation: spec.continuation,
        limits: spec.limits,
    }
}

fn chat_profile(spec: &TaskSpec) -> TurnProfile {
    TurnProfile::Chat {
        continuation: spec.continuation,
        limits: spec.limits,
    }
}

/// Bound prior messages for a model request.
///
/// **Under threshold:** always use full `raw` from MessageRepository — a
/// ContextSnapshot is never a permanent replacement for later turns.
/// **Over threshold:** merge snapshot (compact base) with the raw tail that
/// arrived after the snapshot was taken, then fold if still oversized.
///
/// Returns `(messages_for_model, wrote_compact)` — `wrote_compact` means the
/// caller should persist a new ContextSnapshot.
pub fn budget_prior_messages(
    raw: Vec<leveler_model::Message>,
    snapshot: Option<Vec<leveler_model::Message>>,
    summary: Option<&str>,
    active_objective: Option<&str>,
    threshold: u64,
) -> (Vec<leveler_model::Message>, bool) {
    let raw_tokens = leveler_agent::estimate_tokens(&raw);
    if raw_tokens <= threshold {
        return (raw, false);
    }

    let base = match snapshot {
        Some(snap) if !snap.is_empty() => merge_snapshot_with_raw_tail(snap, &raw),
        _ => raw,
    };
    let tokens = leveler_agent::estimate_tokens(&base);
    if tokens <= threshold {
        // Snapshot+tail already fits: persist so next request starts shorter.
        return (base, true);
    }
    let folded = leveler_agent::compact_messages(
        &base,
        leveler_agent::COMPACT_KEEP_RECENT,
        0,
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
    /// Command-backed acceptance-criteria evidence (unused after orchestrate removal).
    pub acceptance: Option<leveler_verifier::AcceptanceLedger>,
}

impl TaskReport {
    /// A report with the always-present fields set and the orchestration-only
    /// extras (`verification`/`review`/`acceptance`) defaulted to `None`.
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
            acceptance: None,
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
pub struct TaskEngine {
    pub db: Database,
    pub factory: ExecutorFactory,
    pub approver: Arc<dyn Approver>,
    pub clarifier: Arc<dyn Clarifier>,
}

impl TaskEngine {
    /// Attach mid-turn user input for this engine's runs.
    ///
    /// Set by the caller that knows which session is running, since the factory
    /// is built before that is decided.
    pub fn with_steering(
        mut self,
        source: Option<Arc<dyn leveler_agent::SteeringSource>>,
    ) -> Self {
        self.factory.steering = source;
        self
    }

    /// Commit the canonical terminal event and query projection atomically,
    /// then forward the event. An observer can never see an uncommitted fact.
    async fn finish_task(
        &self,
        session_id: &SessionId,
        outcome: TaskOutcome,
        reason: Option<String>,
        observer: &mut dyn FnMut(EngineEvent),
    ) -> Result<(), EngineError> {
        let event = EngineEvent::TaskFinished { outcome, reason };
        let (event_type, payload) = event.to_row()?;
        TerminalRepository::new(&self.db)
            .finish_task(
                session_id,
                &event_type,
                &payload,
                outcome,
                leveler_core::now(),
            )
            .await?;
        observer(event);
        Ok(())
    }

    /// Create and persist the session row, including its execution config.
    pub async fn create_task(&self, spec: &TaskSpec) -> Result<SessionId, EngineError> {
        let record = SessionRecord::new(
            spec.repository.display().to_string(),
            spec.goal.clone(),
            self.factory.model.to_string(),
            leveler_core::now(),
        );
        let repo = SessionRepository::new(&self.db);
        repo.create(&record).await?;
        let id = SessionId::new(record.id);
        repo.set_execution(
            &id,
            mode_str(spec.mode),
            spec.sandbox,
            spec.kind.as_str(),
            leveler_core::now(),
        )
        .await?;
        Ok(id)
    }

    /// Run the task to a terminal outcome. Every turn, tool call, approval and
    /// verification result is persisted before observers see it.
    pub async fn run(
        &self,
        session_id: &SessionId,
        spec: &TaskSpec,
        observer: &mut dyn FnMut(EngineEvent),
        cancellation: CancellationToken,
    ) -> Result<TaskReport, EngineError> {
        let log = EventLog::new(&self.db, session_id.clone());
        let runner = TurnRunner {
            db: &self.db,
            session_id: session_id.clone(),
            log: &log,
            factory: &self.factory,
            approver: self.approver.clone(),
            clarifier: self.clarifier.clone(),
        };
        log.append(
            None,
            EngineEvent::TaskStarted {
                goal: spec.goal.clone(),
                model: self.factory.model.to_string(),
                mode: mode_str(spec.mode).to_string(),
                sandbox: spec.sandbox,
                kind: spec.kind,
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
        let spec = if spec.base_commit.is_none() {
            if let Some(head) = crate::baseline::capture_head(&spec.repository).await {
                owned_spec = TaskSpec {
                    base_commit: Some(head),
                    ..spec.clone()
                };
                &owned_spec
            } else {
                spec
            }
        } else {
            spec
        };

        // Orchestrate execution path removed; legacy kind falls through to direct.
        let result = match spec.kind {
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
        match &result {
            Ok(report) => {
                self.finish_task(
                    session_id,
                    report.outcome,
                    (report.outcome != TaskOutcome::Verified).then(|| report.final_text.clone()),
                    observer,
                )
                .await?;
            }
            Err(EngineError::Agent(leveler_agent::AgentError::Cancelled)) => {
                self.finish_task(session_id, TaskOutcome::Interrupted, None, observer)
                    .await?;
            }
            Err(error) => {
                self.finish_task(
                    session_id,
                    TaskOutcome::Failed,
                    Some(error.to_string()),
                    observer,
                )
                .await?;
            }
        }
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
        observer: &mut dyn FnMut(EngineEvent),
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
        let spec = if spec.base_commit.is_none() {
            match crate::baseline::capture_head(&spec.repository).await {
                Some(head) => {
                    owned_spec = TaskSpec {
                        base_commit: Some(head),
                        ..spec.clone()
                    };
                    &owned_spec
                }
                None => spec,
            }
        } else {
            spec
        };
        let payloads = leveler_storage::MessageRepository::new(&self.db)
            .load(session_id)
            .await?;
        // A chat turn tolerates the odd unreadable legacy row (it only loses
        // context), unlike resume which must reconstruct exactly.
        let raw_prior: Vec<leveler_model::Message> = payloads
            .iter()
            .filter_map(|p| serde_json::from_str(p).ok())
            .collect();

        let log = EventLog::new(&self.db, session_id.clone());
        let snapshot = log.latest_context_snapshot(None).await?;
        let summary = self.summarize_if_over(&raw_prior, &cancellation).await;
        let objective_hint = content
            .iter()
            .filter_map(|p| match p {
                leveler_model::ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .next();
        let (prior, compacted) = budget_prior_messages(
            raw_prior,
            snapshot,
            summary.as_deref(),
            objective_hint,
            leveler_agent::PRE_REQUEST_COMPACT_THRESHOLD,
        );
        if compacted {
            log.append(
                None,
                EngineEvent::ContextSnapshot {
                    messages: prior.clone(),
                },
                observer,
            )
            .await?;
        }
        let runner = TurnRunner {
            db: &self.db,
            session_id: session_id.clone(),
            log: &log,
            factory: &self.factory,
            approver: self.approver.clone(),
            clarifier: self.clarifier.clone(),
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
        match &result {
            Ok(report) => {
                self.finish_task(
                    session_id,
                    report.outcome,
                    (report.outcome != TaskOutcome::Verified).then(|| report.final_text.clone()),
                    observer,
                )
                .await?;
            }
            Err(EngineError::Agent(leveler_agent::AgentError::Cancelled)) => {
                self.finish_task(session_id, TaskOutcome::Interrupted, None, observer)
                    .await?;
            }
            Err(error) => {
                self.finish_task(
                    session_id,
                    TaskOutcome::Failed,
                    Some(error.to_string()),
                    observer,
                )
                .await?;
            }
        }
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
        observer: &mut dyn FnMut(EngineEvent),
        cancellation: CancellationToken,
    ) -> Result<TaskReport, EngineError> {
        let repo = SessionRepository::new(&self.db);
        let (_, _, kind, outcome) = repo
            .execution(session_id)
            .await?
            .ok_or_else(|| EngineError::Config(format!("no session {session_id}")))?;
        if kind != spec.kind.as_str() {
            return Err(EngineError::Config(format!(
                "session {session_id} is `{kind}`, not `{}`",
                spec.kind.as_str()
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
        let payloads = leveler_storage::MessageRepository::new(&self.db)
            .load(session_id)
            .await?;
        if payloads.is_empty() {
            return Err(EngineError::Config(format!(
                "session {session_id} has no transcript to resume; \
                 for interactive chat reopen with: leveler tui --session {session_id}"
            )));
        }
        let raw_prior: Vec<leveler_model::Message> = payloads
            .iter()
            .map(|p| serde_json::from_str(p))
            .collect::<Result<_, _>>()
            .map_err(|e| EngineError::Corrupt(format!("unreplayable transcript: {e}")))?;

        let log = EventLog::new(&self.db, session_id.clone());
        let snapshot = log.latest_context_snapshot(None).await?;
        let summary = self.summarize_if_over(&raw_prior, &cancellation).await;
        // Same merge rules as chat: never drop post-snapshot transcript rows.
        let (prior, compacted) = budget_prior_messages(
            raw_prior,
            snapshot,
            summary.as_deref(),
            Some(spec.goal.as_str()),
            leveler_agent::PRE_REQUEST_COMPACT_THRESHOLD,
        );
        if compacted {
            log.append(
                None,
                EngineEvent::ContextSnapshot {
                    messages: prior.clone(),
                },
                observer,
            )
            .await?;
        }
        let runner = TurnRunner {
            db: &self.db,
            session_id: session_id.clone(),
            log: &log,
            factory: &self.factory,
            approver: self.approver.clone(),
            clarifier: self.clarifier.clone(),
        };

        // Reconcile the crash window before continuing: a tool that started but
        // never finished (the process died mid-execution) is replayed if
        // idempotent, or surfaced for approval if it has a side effect (M5).
        self.recover_crash_window(&log, observer, &cancellation)
            .await?;

        let result = self
            .resume_direct(&log, &runner, spec, prior, observer, cancellation)
            .await;
        match &result {
            Ok(report) => {
                self.finish_task(
                    session_id,
                    report.outcome,
                    (report.outcome != TaskOutcome::Verified).then(|| report.final_text.clone()),
                    observer,
                )
                .await?;
            }
            Err(EngineError::Agent(leveler_agent::AgentError::Cancelled)) => {
                self.finish_task(session_id, TaskOutcome::Interrupted, None, observer)
                    .await?;
            }
            Err(error) => {
                self.finish_task(
                    session_id,
                    TaskOutcome::Failed,
                    Some(error.to_string()),
                    observer,
                )
                .await?;
            }
        }
        result
    }

    /// Best-effort model handoff briefing for a pre-request fold: only called
    /// when the raw history exceeds the compact threshold, and any failure
    /// degrades to the bare-breadcrumb fold (never blocks the turn).
    async fn summarize_if_over(
        &self,
        raw: &[leveler_model::Message],
        cancellation: &CancellationToken,
    ) -> Option<String> {
        if leveler_agent::estimate_tokens(raw) <= leveler_agent::PRE_REQUEST_COMPACT_THRESHOLD {
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
    pub async fn acknowledge_crash_window(
        &self,
        session_id: &SessionId,
    ) -> Result<usize, EngineError> {
        acknowledge_crash_window(&self.db, session_id).await
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
        observer: &mut dyn FnMut(EngineEvent),
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

            if !matches!(call.risk, Some(r) if is_auto_replayable(r)) {
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
        observer: &mut dyn FnMut(EngineEvent),
        cancellation: &CancellationToken,
    ) -> Result<(), EngineError> {
        let (is_error, preview) = match self
            .factory
            .registry
            .execute(
                &call.name,
                args,
                self.factory.tool_context.clone(),
                cancellation.child_token(),
            )
            .await
        {
            Ok(output) => (output.is_error, recovery_preview(&output.content)),
            Err(error) => (true, recovery_preview(&error.to_string())),
        };
        log.append(
            turn_ref,
            EngineEvent::ToolCallFinished {
                call_id: call.call_id.clone(),
                name: call.name.clone(),
                is_error,
                preview,
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
        observer: &mut dyn FnMut(EngineEvent),
    ) -> Result<(), EngineError> {
        log.append(
            turn_ref,
            EngineEvent::ToolCallFinished {
                call_id: call.call_id.clone(),
                name: call.name.clone(),
                is_error: true,
                preview: reason.to_string(),
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
        observer: &mut dyn FnMut(EngineEvent),
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
            .continue_active_goal(
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
        observer: &mut dyn FnMut(EngineEvent),
        cancellation: CancellationToken,
    ) -> Result<TaskReport, EngineError> {
        // Multi-turn Goal: inject bounded session history so follow-ups can
        // resolve deictic references ("刚才那个超时").
        let prior = self
            .bounded_session_history(log, &runner.session_id, &spec.goal)
            .await?;
        let recorded = runner
            .run_turn(
                TurnKind::User,
                goal_profile(spec),
                TurnInput::Goal {
                    goal: spec.goal.clone(),
                    prior,
                },
                observer,
                cancellation.clone(),
            )
            .await?;
        // Epoch spend lives on ProgressLedger inside the drive (seeded across
        // continue/resume). Do not re-accumulate here — that would double-count.
        let outcome = self
            .continue_active_goal(
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
    async fn bounded_session_history(
        &self,
        log: &EventLog<'_>,
        session_id: &SessionId,
        goal: &str,
    ) -> Result<Vec<leveler_model::Message>, EngineError> {
        const GOAL_HISTORY_MAX: usize = 24;
        let payloads = leveler_storage::MessageRepository::new(&self.db)
            .load(session_id)
            .await?;
        let raw: Vec<leveler_model::Message> = payloads
            .iter()
            .filter_map(|p| serde_json::from_str(p).ok())
            .collect();
        if raw.is_empty() {
            return Ok(Vec::new());
        }
        let snapshot = log.latest_context_snapshot(None).await?;
        let (budgeted, _) = budget_prior_messages(
            raw,
            snapshot,
            None,
            Some(goal),
            leveler_agent::PRE_REQUEST_COMPACT_THRESHOLD,
        );
        Ok(bound_goal_history(budgeted, GOAL_HISTORY_MAX))
    }

    /// Goal continuity: a quiet turn does not end an unbounded
    /// goal. Start another persisted turn from the latest model-visible context
    /// until the model explicitly completes/blocks, the user cancels, or an
    /// explicit resource limit stops the executor.
    /// Pure gate used by continue_active_goal (testable without DB).
    pub(crate) fn stalled_goal_may_continue(
        stop_reason: leveler_agent::StopReason,
        progress: &leveler_lifecycle::ProgressLedger,
        caps: leveler_lifecycle::ProgressCaps,
    ) -> bool {
        stop_reason == leveler_agent::StopReason::Stalled && progress.allows_engine_continue(caps)
    }

    async fn continue_active_goal(
        &self,
        log: &EventLog<'_>,
        runner: &TurnRunner<'_>,
        spec: &TaskSpec,
        mut outcome: leveler_agent::AgentOutcome,
        observer: &mut dyn FnMut(EngineEvent),
        cancellation: CancellationToken,
    ) -> Result<leveler_agent::AgentOutcome, EngineError> {
        if spec.continuation.round_limit().is_some() {
            return Ok(outcome);
        }

        let progress_caps = leveler_lifecycle::ProgressCaps::default();
        while outcome.stop_reason == StopReason::Stalled {
            // No infinite continue when the prior drive already showed zero
            // progress / closeout thrash — stops fake “always running” loops.
            if !Self::stalled_goal_may_continue(
                outcome.stop_reason,
                &outcome.progress,
                progress_caps,
            ) {
                outcome.stop_detail = Some(
                    outcome
                        .stop_detail
                        .clone()
                        .unwrap_or_else(|| "continue suppressed: no-progress cap".into()),
                );
                break;
            }
            // Announce BEFORE the transcript load / compaction / model call.
            // Everything below is invisible work that happens after the user
            // already read a final answer; without this the status line shows
            // a bare "waiting for model" for the whole continuation.
            observer(EngineEvent::AdvisoryStarted {
                kind: leveler_agent::AdvisoryKind::GoalContinuation
                    .as_key()
                    .to_string(),
            });
            let payloads = leveler_storage::MessageRepository::new(&self.db)
                .load(&runner.session_id)
                .await?;
            let raw_prior = payloads
                .iter()
                .map(|payload| serde_json::from_str(payload))
                .collect::<Result<Vec<leveler_model::Message>, _>>()
                .map_err(|error| {
                    EngineError::Corrupt(format!(
                        "unreplayable goal transcript during continuation: {error}"
                    ))
                })?;
            let snapshot = log.latest_context_snapshot(None).await?;
            let summary = self.summarize_if_over(&raw_prior, &cancellation).await;
            let (prior, compacted) = budget_prior_messages(
                raw_prior,
                snapshot,
                summary.as_deref(),
                Some(spec.goal.as_str()),
                leveler_agent::PRE_REQUEST_COMPACT_THRESHOLD,
            );
            if compacted {
                log.append(
                    None,
                    EngineEvent::ContextSnapshot {
                        messages: prior.clone(),
                    },
                    observer,
                )
                .await?;
            }
            // Full objective restatement — not a vague “Continue…” only. When
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
                spec.goal
            );
            let continued = runner
                .run_turn(
                    TurnKind::User,
                    goal_profile(spec),
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
            let cont_rounds = continued.outcome.rounds;
            outcome.rounds = outcome.rounds.saturating_add(cont_rounds);
            outcome.final_text = continued.outcome.final_text;
            outcome.stop_reason = continued.outcome.stop_reason;
            outcome.stop_detail = continued.outcome.stop_detail;
            // Carry progress ledger across continues. Epoch spend
            // (cumulative_rounds / cumulative_model_tokens) is already absolute
            // inside the drive after seeding the prior ledger from the event log.
            outcome.progress = continued.outcome.progress;
            outcome.metrics.model_tokens = outcome
                .metrics
                .model_tokens
                .saturating_add(continued.outcome.metrics.model_tokens);
            outcome.metrics.extra_model_calls = outcome
                .metrics
                .extra_model_calls
                .saturating_add(continued.outcome.metrics.extra_model_calls);
            for path in continued.outcome.modified_files {
                if !outcome.modified_files.contains(&path) {
                    outcome.modified_files.push(path);
                }
            }
        }
        Ok(outcome)
    }

    /// When a drive stops on resource budget with real progress, grant a small
    /// extra allowance and resume the transcript (at most
    /// [`MAX_BUDGET_EXTENSIONS`] times). Absolute round ceilings and
    /// no-progress stops never extend.
    async fn extend_budget_exhausted(
        &self,
        _log: &EventLog<'_>,
        runner: &TurnRunner<'_>,
        spec: &TaskSpec,
        mut outcome: leveler_agent::AgentOutcome,
        observer: &mut dyn FnMut(EngineEvent),
        cancellation: CancellationToken,
    ) -> Result<leveler_agent::AgentOutcome, EngineError> {
        let mut extensions = 0u32;
        let mut limits = spec.limits;
        while budget_extension_allowed(
            outcome.stop_reason,
            extensions,
            !outcome.modified_files.is_empty(),
            stop_detail_indicates_no_progress(outcome.stop_detail.as_deref()),
        ) {
            let Some(exhaustion) = outcome.budget_exhaustion.clone() else {
                break;
            };
            limits = grant_budget_extension(limits, &exhaustion);
            extensions = extensions.saturating_add(1);
            observer(EngineEvent::AdvisoryStarted {
                kind: format!(
                    "budget_extension:{}/{}:{}",
                    extensions,
                    MAX_BUDGET_EXTENSIONS,
                    exhaustion.dimension.as_str()
                ),
            });
            let payloads = leveler_storage::MessageRepository::new(&self.db)
                .load(&runner.session_id)
                .await?;
            let prior = payloads
                .iter()
                .map(|payload| serde_json::from_str(payload))
                .collect::<Result<Vec<leveler_model::Message>, _>>()
                .map_err(|error| {
                    EngineError::Corrupt(format!(
                        "unreplayable transcript during budget extension: {error}"
                    ))
                })?;
            if prior.is_empty() {
                break;
            }
            let continued = runner
                .run_turn(
                    TurnKind::User,
                    TurnProfile::Goal {
                        continuation: spec.continuation,
                        limits,
                    },
                    TurnInput::Resume(prior),
                    observer,
                    cancellation.clone(),
                )
                .await?;
            outcome.rounds = outcome.rounds.saturating_add(continued.outcome.rounds);
            outcome.final_text = continued.outcome.final_text;
            outcome.stop_reason = continued.outcome.stop_reason;
            outcome.stop_detail = continued.outcome.stop_detail;
            outcome.budget_exhaustion = continued.outcome.budget_exhaustion;
            outcome.progress = continued.outcome.progress;
            outcome.metrics.model_tokens = outcome
                .metrics
                .model_tokens
                .saturating_add(continued.outcome.metrics.model_tokens);
            outcome.metrics.extra_model_calls = outcome
                .metrics
                .extra_model_calls
                .saturating_add(continued.outcome.metrics.extra_model_calls);
            for path in continued.outcome.modified_files {
                if !outcome.modified_files.contains(&path) {
                    outcome.modified_files.push(path);
                }
            }
        }
        Ok(outcome)
    }

    /// Shared tail of fresh and resumed direct runs: map the stop reason,
    /// then verify + bounded repair.
    async fn conclude_direct(
        &self,
        log: &EventLog<'_>,
        runner: &TurnRunner<'_>,
        spec: &TaskSpec,
        mut outcome: leveler_agent::AgentOutcome,
        observer: &mut dyn FnMut(EngineEvent),
        cancellation: CancellationToken,
    ) -> Result<TaskReport, EngineError> {
        // Bounded L2 resource extension: BudgetExhausted + real progress may
        // get up to MAX_BUDGET_EXTENSIONS extra grants. TurnLimitReached and
        // no-progress stops never extend. Eval cases with a fixed round budget
        // skip auto-extend (reproducibility).
        if spec.continuation.round_limit().is_none() {
            outcome = self
                .extend_budget_exhausted(log, runner, spec, outcome, observer, cancellation.clone())
                .await?;
        }

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
                return Ok(report_from_agent_outcome(outcome, terminal));
            }
        }

        // K19 early short-circuit: no mutation or no gates → never claim Verified
        // (pure Q&A over a green repo must stay CompletedUnverified).
        if outcome.modified_files.is_empty() || !spec.verification.has_gates() {
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
                        goal: repair_goal(&spec.goal, &report),
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
                &spec.goal,
                matches!(
                    self.factory.work_profile,
                    leveler_agent::WorkProfile::Delivery
                ),
            ),
            has_mutation: !outcome.modified_files.is_empty(),
        };
        let task_outcome = map_completion_verdict(finalize_task_outcome(&report, expected));
        let base = report_from_agent_outcome(outcome, task_outcome);
        Ok(TaskReport {
            verification: Some(report),
            acceptance: None,
            ..base
        })
    }

    async fn verify(
        &self,
        log: &EventLog<'_>,
        spec: &TaskSpec,
        allowed_paths: &[String],
        modified_files: &[String],
        observer: &mut dyn FnMut(EngineEvent),
        cancellation: &CancellationToken,
    ) -> Result<VerificationReport, EngineError> {
        log.append(None, EngineEvent::VerificationStarted, observer)
            .await?;
        let verifier = Verifier::with_environment(
            &spec.repository,
            self.factory.tool_context.environment.clone(),
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
        if let Some(base_commit) = spec.base_commit.as_deref() {
            crate::baseline::reconcile_with_baseline(
                &mut report,
                &spec.repository,
                base_commit,
                &plan,
                modified_files,
                self.factory.tool_context.environment.clone(),
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
    if spec.verification.commands.is_empty() {
        leveler_verifier::discover::plan_for_repo(&spec.repository)
    } else {
        spec.verification.clone()
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
            Some(TaskOutcome::Failed),
            "an absolute-ceiling loop break is a failed turn, not a resumable budget"
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
    db: &Database,
    session_id: &SessionId,
) -> Result<usize, EngineError> {
    let log = EventLog::new(db, session_id.clone());
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
        S::BudgetExhausted => Some(TaskOutcome::BudgetLimited),
        // Incomplete thrash, stalled quiet, blocked, etc. — never success. A
        // turn broken by the absolute round ceiling belongs here, NOT with
        // BudgetLimited: it is a failed (likely looping) turn, so it must
        // terminate the auto-orchestrated path instead of auto-resuming — which
        // would re-enter the same loop the ceiling just broke.
        S::Incomplete | S::Blocked | S::Stalled | S::CompletedUnverified | S::TurnLimitReached => {
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
            repository,
            goal: "build it".to_string(),
            mode: leveler_execution::PermissionProfile::Assisted,
            sandbox: false,
            kind: ExecutionKind::Direct,
            continuation: ContinuationPolicy::UntilTerminal,
            limits: StepLimits::default(),
            verification,
            base_commit: None,
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
        let (out, compacted) = budget_prior_messages(raw.clone(), Some(snap), None, None, 100_000);
        assert!(!compacted);
        assert_eq!(out.len(), raw.len());
        assert!(
            out.iter()
                .any(|m| m.text_content().contains("second turn after snapshot")),
            "under-threshold prior must include post-snapshot raw: {out:?}"
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
        let (out, compacted) = budget_prior_messages(raw, Some(snap), None, Some("fix login"), 200);
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
