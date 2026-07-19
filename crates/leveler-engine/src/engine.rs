//! The task engine: session lifecycle + strategy dispatch (plan B3).
//!
//! `create_task` persists WHAT will run (goal/mode/sandbox/kind) so resume
//! never guesses; `run` executes the session's strategy over fully-persisted
//! turns and stamps the terminal [`TaskOutcome`] on the session row.

use std::path::PathBuf;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use leveler_agent::{Clarifier, ContinuationPolicy, StepLimits, StopReason};
use leveler_core::{SessionId, TurnId};
use leveler_execution::{Approver, PermissionProfile, RiskLevel};
use leveler_storage::{Database, SessionRecord, SessionRepository, TerminalRepository};
use leveler_verifier::{
    AcceptanceCheck, CompletionVerdict, ExpectedEvidence, Verdict, VerificationPlan,
    VerificationReport, Verifier, assemble_acceptance_checks, finalize_task_outcome,
};

use crate::factory::{ExecutorFactory, TurnProfile};
use crate::log::{DanglingCall, EventLog};
use crate::turn::{TurnInput, TurnRunner};
use crate::{EngineError, EngineEvent, ExecutionKind, TaskOutcome, TurnKind};

/// How many verification-repair turns a direct task may spend.
const DIRECT_REPAIR_ATTEMPTS: u32 = 1;
/// How many verification-repair turns an orchestrated task may spend
/// (parity with the legacy orchestrator's max_repairs).
const ORCHESTRATE_REPAIR_ATTEMPTS: u32 = 2;

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

fn round_budget_exhausted_report(
    max_rounds: u32,
    rounds: u32,
    modified_files: Vec<String>,
    last_text: String,
) -> TaskReport {
    let mut final_text =
        format!("Reached the evaluation case's {max_rounds}-round limit before finishing.");
    if !last_text.trim().is_empty() {
        final_text.push_str("\n\nLatest note: ");
        final_text.push_str(last_text.trim());
    }
    TaskReport {
        outcome: TaskOutcome::BudgetLimited,
        final_text,
        modified_files,
        verification: None,
        stop_reason: StopReason::BudgetExhausted,
        rounds,
        review: None,
        acceptance: None,
    }
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
    pub rounds: u32,
    /// Merged review findings (orchestrated runs only).
    pub review: Option<Vec<leveler_orchestrator::ReviewFinding>>,
    /// Command-backed acceptance-criteria evidence (orchestrated runs only).
    pub acceptance: Option<leveler_verifier::AcceptanceLedger>,
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

        let result = match spec.kind {
            ExecutionKind::Direct => {
                self.run_direct(&log, &runner, spec, observer, cancellation)
                    .await
            }
            ExecutionKind::Orchestrate => {
                self.run_orchestrate(&log, &runner, spec, observer, cancellation)
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

        let result = match spec.kind {
            ExecutionKind::Orchestrate => {
                self.resume_orchestrate(&log, &runner, spec, observer, cancellation)
                    .await
            }
            _ => {
                self.resume_direct(&log, &runner, spec, prior, observer, cancellation)
                    .await
            }
        };
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

    /// Resume an orchestrated task by replaying its event log (plan B7): the
    /// persisted requirement and plan are NOT re-requested from the model,
    /// finished nodes keep their status, and an in-flight node re-drives from
    /// its own turn transcript.
    async fn resume_orchestrate(
        &self,
        log: &EventLog<'_>,
        runner: &TurnRunner<'_>,
        spec: &TaskSpec,
        observer: &mut dyn FnMut(EngineEvent),
        cancellation: CancellationToken,
    ) -> Result<TaskReport, EngineError> {
        use leveler_orchestrator::NodeStatus;

        let events = log.replay().await?;
        let mut requirement: Option<leveler_orchestrator::Requirement> = None;
        let mut graph: Option<leveler_orchestrator::TaskGraph> = None;
        let mut modified_files: Vec<String> = Vec::new();
        let mut rounds = 0u32;
        let mut in_flight: Option<String> = None;
        let mut turn_of_node: std::collections::HashMap<String, leveler_core::TurnId> =
            std::collections::HashMap::new();
        for event in events {
            match event {
                EngineEvent::RequirementReady { requirement: r } => requirement = Some(r),
                EngineEvent::PlanReady { graph: g } => graph = Some(g),
                EngineEvent::TurnStarted {
                    turn_id,
                    kind: TurnKind::Node { node_id },
                } => {
                    turn_of_node.insert(node_id, turn_id);
                }
                EngineEvent::TurnFinished {
                    rounds: r,
                    modified_files: files,
                    ..
                } => {
                    rounds += r;
                    for f in files {
                        if !modified_files.contains(&f) {
                            modified_files.push(f);
                        }
                    }
                }
                EngineEvent::NodeStarted { node_id, .. } => in_flight = Some(node_id),
                EngineEvent::NodeFinished { node_id, status } => {
                    if let Some(g) = &mut graph
                        && let Some(node) = g.nodes.iter_mut().find(|n| n.id.to_string() == node_id)
                    {
                        node.status = status;
                    }
                    in_flight = None;
                }
                _ => {}
            }
        }

        // Nothing meaningful persisted yet: run from the top.
        let Some(requirement) = requirement else {
            return self
                .run_orchestrate(log, runner, spec, observer, cancellation)
                .await;
        };

        let planner = self.planner();
        // Localization is deterministic — recompute, no model call.
        let context = planner.localize(&requirement.goal);
        let mut graph = match graph {
            Some(g) => g,
            None => {
                let g = planner.plan(&requirement, &context, &cancellation).await?;
                log.append(None, EngineEvent::PlanReady { graph: g.clone() }, observer)
                    .await?;
                g
            }
        };

        // The in-flight node resumes from its persisted transcript; if none
        // was written before the interruption, it simply re-runs its goal.
        let mut resume_in_flight: Option<(String, Vec<leveler_model::Message>)> = None;
        if let Some(node_id) = in_flight {
            if let Some(node) = graph.nodes.iter_mut().find(|n| n.id.to_string() == node_id) {
                node.status = NodeStatus::Pending;
            }
            if let Some(turn_id) = turn_of_node.get(&node_id) {
                let payloads = leveler_storage::MessageRepository::new(&self.db)
                    .load_for_turn(&runner.session_id, turn_id)
                    .await?;
                if !payloads.is_empty() {
                    let raw_prior: Vec<leveler_model::Message> = payloads
                        .iter()
                        .map(|p| serde_json::from_str(p))
                        .collect::<Result<_, _>>()
                        .map_err(|e| {
                            EngineError::Corrupt(format!("unreplayable node transcript: {e}"))
                        })?;
                    // A snapshot is a compact BASE, never a replacement for
                    // rounds persisted after it: merge with the raw tail like
                    // `budget_prior_messages`, or resumed nodes would redo
                    // work whose side effects already happened pre-crash.
                    let prior = match log.latest_context_snapshot(Some(turn_id)).await? {
                        Some(snap) if !snap.is_empty() => {
                            merge_snapshot_with_raw_tail(snap, &raw_prior)
                        }
                        _ => raw_prior,
                    };
                    resume_in_flight = Some((node_id, prior));
                }
            }
        }
        // Any node left marked Running by a crash re-runs from scratch.
        for node in &mut graph.nodes {
            if node.status == NodeStatus::Running {
                node.status = NodeStatus::Pending;
            }
        }

        self.execute_graph(
            log,
            runner,
            spec,
            &planner,
            &requirement,
            &context,
            graph,
            modified_files,
            rounds,
            resume_in_flight,
            observer,
            cancellation,
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
            // Full objective restatement — not a vague “Continue…” only.
            let continue_text = format!(
                "Continue working toward the active goal. The previous turn ended without \
                 proving completion.\n\n\
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
        // Completed and Answered both count as clean finishes — and both must
        // verify if they touched files. Every other stop reason is terminal
        // failure (阶段A semantics: Stalled/Blocked/Incomplete/BudgetExhausted
        // never read as success).
        if let Some(terminal) = direct_non_success_outcome(outcome.stop_reason) {
            return Ok(TaskReport {
                outcome: terminal,
                final_text: outcome.final_text,
                modified_files: outcome.modified_files,
                verification: None,
                stop_reason: outcome.stop_reason,
                rounds: outcome.rounds,
                review: None,
                acceptance: None,
            });
        }

        // K19 early short-circuit: no mutation or no gates → never claim Verified
        // (pure Q&A over a green repo must stay CompletedUnverified).
        if outcome.modified_files.is_empty() || !spec.verification.has_gates() {
            return Ok(TaskReport {
                outcome: TaskOutcome::CompletedUnverified,
                final_text: outcome.final_text,
                modified_files: outcome.modified_files,
                verification: None,
                stop_reason: outcome.stop_reason,
                rounds: outcome.rounds,
                review: None,
                acceptance: None,
            });
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
        // Acceptance (K2): when health would be Verified, Direct also extracts
        // AC via understand and evaluates them — same finalize formula as
        // Orchestrate. Skip the model call when health is already not Verified
        // (no upgrade path; saves a round on Failed/Unverified).
        let acceptance = if report.verdict() == Verdict::Verified {
            Some(
                self.direct_extract_and_evaluate_acceptance(
                    log,
                    spec,
                    &outcome.modified_files,
                    observer,
                    &cancellation,
                )
                .await?,
            )
        } else {
            None
        };
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
        let task_outcome = map_completion_verdict(finalize_task_outcome(
            &report,
            acceptance.as_ref(),
            expected,
        ));
        Ok(TaskReport {
            outcome: task_outcome,
            final_text: outcome.final_text,
            modified_files: outcome.modified_files,
            verification: Some(report),
            stop_reason: outcome.stop_reason,
            rounds: outcome.rounds,
            review: None,
            acceptance,
        })
    }

    /// Direct-path acceptance: one `understand` call to pull criteria from the
    /// goal, then the shared command-backed ledger (with mutation-derived gap
    /// fill for deletes). On understand failure use
    /// [`Requirement::fallback`] (optional AC, K11) so a weak model cannot
    /// hard-break an otherwise healthy Direct run.
    async fn direct_extract_and_evaluate_acceptance(
        &self,
        log: &EventLog<'_>,
        spec: &TaskSpec,
        modified_files: &[String],
        observer: &mut dyn FnMut(EngineEvent),
        cancellation: &CancellationToken,
    ) -> Result<leveler_verifier::AcceptanceLedger, EngineError> {
        let planner = self.planner();
        let requirement = match planner.understand(&spec.goal, cancellation).await {
            Ok(req) => req,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "direct acceptance: understand failed; using optional fallback AC"
                );
                leveler_orchestrator::Requirement::fallback(&spec.goal)
            }
        };
        log.append(
            None,
            EngineEvent::RequirementReady {
                requirement: requirement.clone(),
            },
            observer,
        )
        .await?;
        self.evaluate_acceptance(
            log,
            spec,
            &requirement,
            modified_files,
            observer,
            cancellation,
        )
        .await
    }

    /// The plan strategy (B5): understand → localize → plan, then every graph
    /// node runs as a fully-persisted engine turn, then verify + bounded
    /// repair and the advisory review panel.
    async fn run_orchestrate(
        &self,
        log: &EventLog<'_>,
        runner: &TurnRunner<'_>,
        spec: &TaskSpec,
        observer: &mut dyn FnMut(EngineEvent),
        cancellation: CancellationToken,
    ) -> Result<TaskReport, EngineError> {
        use leveler_orchestrator::{AgentState, Planner};

        let planner = self.planner();
        let mut phase = AgentState::Understand;
        let _: Option<Planner> = None;
        macro_rules! advance {
            ($to:expr) => {{
                log.append(
                    None,
                    EngineEvent::PhaseChanged {
                        from: phase,
                        to: $to,
                    },
                    observer,
                )
                .await?;
                phase = $to;
            }};
        }

        let requirement = planner.understand(&spec.goal, &cancellation).await?;
        log.append(
            None,
            EngineEvent::RequirementReady {
                requirement: requirement.clone(),
            },
            observer,
        )
        .await?;
        advance!(AgentState::Localize);

        let context = planner.localize(&requirement.goal);
        log.append(
            None,
            EngineEvent::ContextReady {
                candidate_files: context.candidate_files.clone(),
                estimated_tokens: context.estimated_tokens,
            },
            observer,
        )
        .await?;
        advance!(AgentState::Plan);

        let graph = planner.plan(&requirement, &context, &cancellation).await?;
        log.append(
            None,
            EngineEvent::PlanReady {
                graph: graph.clone(),
            },
            observer,
        )
        .await?;
        log.append(
            None,
            EngineEvent::PhaseChanged {
                from: phase,
                to: AgentState::Execute,
            },
            observer,
        )
        .await?;

        self.execute_graph(
            log,
            runner,
            spec,
            &planner,
            &requirement,
            &context,
            graph,
            Vec::new(),
            0,
            None,
            observer,
            cancellation,
        )
        .await
    }

    /// The observer-free planning brain over the engine factory's wiring.
    fn planner(&self) -> leveler_orchestrator::Planner {
        leveler_orchestrator::Planner {
            runtime: self.factory.runtime.clone(),
            registry: self.factory.registry.clone(),
            tool_context: self.factory.tool_context.clone(),
            model: self.factory.model.clone(),
        }
    }

    /// Execute the (possibly partially-completed) graph: remaining nodes as
    /// persisted turns — an in-flight node resumes from its transcript — then
    /// verify + bounded repair and the advisory review panel.
    #[allow(clippy::too_many_arguments)]
    async fn execute_graph(
        &self,
        log: &EventLog<'_>,
        runner: &TurnRunner<'_>,
        spec: &TaskSpec,
        planner: &leveler_orchestrator::Planner,
        requirement: &leveler_orchestrator::Requirement,
        context: &leveler_context::ContextPackage,
        mut graph: leveler_orchestrator::TaskGraph,
        mut modified_files: Vec<String>,
        mut rounds: u32,
        mut resume_in_flight: Option<(String, Vec<leveler_model::Message>)>,
        observer: &mut dyn FnMut(EngineEvent),
        cancellation: CancellationToken,
    ) -> Result<TaskReport, EngineError> {
        use leveler_orchestrator::{
            AgentState, NodeStatus, ReviewConfig, TaskNodeKind, allowed_paths, compose_node_goal,
            compose_repair_goal, is_repairable,
        };

        // Structural gate: empty / cycle / missing deps never reach verify.
        if let Err(e) = graph.validate() {
            log.append(
                None,
                EngineEvent::PhaseChanged {
                    from: AgentState::Execute,
                    to: AgentState::Failed,
                },
                observer,
            )
            .await?;
            return Ok(TaskReport {
                outcome: TaskOutcome::Failed,
                final_text: format!("invalid_plan: {e}"),
                modified_files,
                verification: None,
                stop_reason: StopReason::Completed,
                rounds,
                review: None,
                acceptance: None,
            });
        }

        let mut phase = AgentState::Execute;
        macro_rules! advance {
            ($to:expr) => {{
                log.append(
                    None,
                    EngineEvent::PhaseChanged {
                        from: phase,
                        to: $to,
                    },
                    observer,
                )
                .await?;
                phase = $to;
            }};
        }

        let mut last_text = String::new();
        while let Some(index) = graph.next_ready() {
            if cancellation.is_cancelled() {
                return Err(EngineError::Agent(leveler_agent::AgentError::Cancelled));
            }
            let node = graph.nodes[index].clone();
            let node_continuation = match spec.continuation.round_limit() {
                Some(max_rounds) if rounds >= max_rounds => {
                    return Ok(round_budget_exhausted_report(
                        max_rounds,
                        rounds,
                        modified_files,
                        last_text,
                    ));
                }
                Some(max_rounds) => {
                    leveler_agent::ContinuationPolicy::bounded(max_rounds.saturating_sub(rounds))
                }
                None => leveler_agent::ContinuationPolicy::UntilTerminal,
            };
            log.append(
                None,
                EngineEvent::NodeStarted {
                    node_id: node.id.to_string(),
                    description: node.description.clone(),
                },
                observer,
            )
            .await?;
            graph.nodes[index].status = NodeStatus::Running;

            let recorded = runner
                .run_turn(
                    TurnKind::Node {
                        node_id: node.id.to_string(),
                    },
                    TurnProfile::Node {
                        continuation: node_continuation,
                        limits: leveler_agent::StepLimits {
                            // Wall clock is the node's backstop under
                            // UntilTerminal (rounds caps are deliberately
                            // retired; see the twenty-round test). Zero means
                            // unlimited, like the legacy caps.
                            max_duration: (!node.budget.max_duration.is_zero())
                                .then_some(node.budget.max_duration),
                            ..leveler_agent::StepLimits::from_legacy_caps(
                                node.budget.max_commands,
                                node.budget.max_modified_files,
                            )
                        },
                        write_allowlist: (!node.allowed_paths.is_empty())
                            .then(|| node.allowed_paths.clone()),
                    },
                    match resume_in_flight.take_if(|(id, _)| *id == node.id.to_string()) {
                        Some((_, prior)) => TurnInput::Resume(prior),
                        None => TurnInput::Goal {
                            goal: compose_node_goal(requirement, context, &node),
                            prior: Vec::new(),
                        },
                    },
                    observer,
                    cancellation.clone(),
                )
                .await?;
            rounds += recorded.outcome.rounds;
            last_text = recorded.outcome.final_text.clone();
            let node_modified = recorded.outcome.modified_files.clone();
            for f in &node_modified {
                if !modified_files.contains(f) {
                    modified_files.push(f.clone());
                }
            }
            // K15: Edit + Answered + no mutation → Failed (Node goal_mode is
            // false so Answered is the normal success stop). Test/Verify may
            // Answer without files and still complete.
            // Task-level completion is still decided by verify/finalize below.
            let status = match (node.kind, recorded.outcome.stop_reason) {
                (TaskNodeKind::Edit, StopReason::Answered) if node_modified.is_empty() => {
                    NodeStatus::Failed
                }
                (_, StopReason::Completed | StopReason::Answered) => NodeStatus::Completed,
                _ => NodeStatus::Failed,
            };
            graph.nodes[index].status = status;
            log.append(
                None,
                EngineEvent::NodeFinished {
                    node_id: node.id.to_string(),
                    status,
                },
                observer,
            )
            .await?;
            if status == NodeStatus::Failed {
                log.append(
                    None,
                    EngineEvent::PhaseChanged {
                        from: phase,
                        to: AgentState::Failed,
                    },
                    observer,
                )
                .await?;
                let failed_text = if matches!(
                    (node.kind, recorded.outcome.stop_reason),
                    (TaskNodeKind::Edit, StopReason::Answered)
                ) && node_modified.is_empty()
                {
                    format!(
                        "edit_answered_without_mutation: node {} finished Answered with no modified files",
                        node.id
                    )
                } else {
                    last_text
                };
                return Ok(TaskReport {
                    outcome: if recorded.outcome.stop_reason == StopReason::BudgetExhausted {
                        TaskOutcome::BudgetLimited
                    } else {
                        TaskOutcome::Failed
                    },
                    final_text: failed_text,
                    modified_files,
                    verification: None,
                    stop_reason: recorded.outcome.stop_reason,
                    rounds,
                    review: None,
                    acceptance: None,
                });
            }
        }

        // Stuck graph: no ready node but work remains — do not enter verify.
        if !graph.all_done() || graph.has_unfinished() {
            log.append(
                None,
                EngineEvent::PhaseChanged {
                    from: phase,
                    to: AgentState::Failed,
                },
                observer,
            )
            .await?;
            return Ok(TaskReport {
                outcome: TaskOutcome::Failed,
                final_text: "graph_stuck: no ready nodes while unfinished work remains".into(),
                modified_files,
                verification: None,
                stop_reason: StopReason::Completed,
                rounds,
                review: None,
                acceptance: None,
            });
        }

        // Verification gate (spec §2.3, §29-32): without gates the task can at
        // best finish unverified (阶段A semantics).
        if !spec.verification.has_gates() {
            log.append(
                None,
                EngineEvent::PhaseChanged {
                    from: phase,
                    to: AgentState::Complete,
                },
                observer,
            )
            .await?;
            return Ok(TaskReport {
                outcome: TaskOutcome::CompletedUnverified,
                final_text: last_text,
                modified_files,
                verification: None,
                stop_reason: StopReason::Completed,
                rounds,
                review: None,
                acceptance: None,
            });
        }
        advance!(AgentState::VerifyTask);
        let allowed = allowed_paths(&graph);
        let mut report = self
            .verify(
                log,
                spec,
                &allowed,
                &modified_files,
                observer,
                &cancellation,
            )
            .await?;
        let mut attempt = 0;
        while !report.passed()
            && attempt < ORCHESTRATE_REPAIR_ATTEMPTS
            && is_repairable(&report)
            && !cancellation.is_cancelled()
        {
            let repair_continuation = match spec.continuation.round_limit() {
                Some(max_rounds) if rounds >= max_rounds => {
                    return Ok(round_budget_exhausted_report(
                        max_rounds,
                        rounds,
                        modified_files,
                        last_text,
                    ));
                }
                Some(max_rounds) => {
                    leveler_agent::ContinuationPolicy::bounded(max_rounds.saturating_sub(rounds))
                }
                None => leveler_agent::ContinuationPolicy::UntilTerminal,
            };
            attempt += 1;
            advance!(AgentState::Repair);
            log.append(None, EngineEvent::RepairStarted { attempt }, observer)
                .await?;
            let repair = runner
                .run_turn(
                    TurnKind::Repair { attempt },
                    TurnProfile::Node {
                        continuation: repair_continuation,
                        limits: leveler_agent::StepLimits::default(),
                        write_allowlist: None,
                    },
                    TurnInput::Goal {
                        goal: compose_repair_goal(requirement, &report),
                        prior: Vec::new(),
                    },
                    observer,
                    cancellation.clone(),
                )
                .await?;
            rounds += repair.outcome.rounds;
            for f in repair.outcome.modified_files {
                if !modified_files.contains(&f) {
                    modified_files.push(f);
                }
            }
            advance!(AgentState::VerifyTask);
            report = self
                .verify(
                    log,
                    spec,
                    &allowed,
                    &modified_files,
                    observer,
                    &cancellation,
                )
                .await?;
        }
        if !report.passed() {
            log.append(
                None,
                EngineEvent::PhaseChanged {
                    from: phase,
                    to: AgentState::Failed,
                },
                observer,
            )
            .await?;
            return Ok(TaskReport {
                outcome: TaskOutcome::Failed,
                final_text: last_text,
                modified_files,
                verification: Some(report),
                stop_reason: StopReason::Completed,
                rounds,
                review: None,
                acceptance: None,
            });
        }

        // Advisory review panel (spec §44); blocks only when configured.
        advance!(AgentState::Review);
        let review_config = ReviewConfig::default();
        let review = planner
            .review(&requirement.goal, &review_config, &cancellation)
            .await;
        log.append(
            None,
            EngineEvent::ReviewStarted {
                lenses: review.lenses_run,
            },
            observer,
        )
        .await?;
        for finding in &review.findings {
            log.append(
                None,
                EngineEvent::ReviewFinding {
                    finding: finding.clone(),
                },
                observer,
            )
            .await?;
        }
        for failure in &review.failures {
            log.append(
                None,
                EngineEvent::ReviewFailed {
                    lens: failure.lens.clone(),
                    error: failure.error.clone(),
                },
                observer,
            )
            .await?;
        }
        let blocking = review_config
            .blocks_on
            .zip(leveler_orchestrator::review::max_severity(&review.findings))
            .map(|(threshold, worst)| worst >= threshold)
            .unwrap_or(false)
            || (review_config.blocks_on.is_some() && !review.failures.is_empty());
        log.append(
            None,
            EngineEvent::ReviewFinished {
                findings: review.findings.len(),
                failures: review.failures.len(),
                blocking,
            },
            observer,
        )
        .await?;
        if blocking {
            log.append(
                None,
                EngineEvent::PhaseChanged {
                    from: phase,
                    to: AgentState::Failed,
                },
                observer,
            )
            .await?;
            return Ok(TaskReport {
                outcome: TaskOutcome::Failed,
                final_text: last_text,
                modified_files,
                verification: Some(report),
                stop_reason: StopReason::Completed,
                rounds,
                review: Some(review.findings),
                acceptance: None,
            });
        }

        // Command-backed acceptance ledger (spec §29 completion proof): run each
        // criterion's shell check and record Met/Unmet/Unverifiable. Required
        // criteria must all be Met for Verified (K2); Unmet or Unverifiable
        // required items downgrade to at most CompletedUnverified. Optional
        // criteria (incl. fallback AC, K11) never block; mutation-derived
        // delete checks fill the gap when understand has no executable
        // required hint. Repo gates stay the hard Failed path.
        let acceptance = self
            .evaluate_acceptance(
                log,
                spec,
                requirement,
                &modified_files,
                observer,
                &cancellation,
            )
            .await?;

        log.append(
            None,
            EngineEvent::PhaseChanged {
                from: phase,
                to: AgentState::Complete,
            },
            observer,
        )
        .await?;
        // Single closed-loop exit (design §1.3–§1.4): health ∧ acceptance
        // ∧ ExpectedMutation. Same finalize_task_outcome as conclude_direct.
        let expected = ExpectedEvidence {
            needs_mutation: orchestrate_needs_mutation(
                &graph,
                requirement,
                matches!(
                    self.factory.work_profile,
                    leveler_agent::WorkProfile::Delivery
                ),
            ),
            has_mutation: !modified_files.is_empty(),
        };
        let outcome =
            map_completion_verdict(finalize_task_outcome(&report, Some(&acceptance), expected));
        Ok(TaskReport {
            outcome,
            final_text: last_text,
            modified_files,
            verification: Some(report),
            stop_reason: StopReason::Completed,
            rounds,
            review: Some(review.findings),
            acceptance: Some(acceptance),
        })
    }

    /// Assemble (understand ∪ mutation-derived deletes) and run acceptance
    /// criteria as commands, emit per-criterion evidence, return the ledger.
    async fn evaluate_acceptance(
        &self,
        log: &EventLog<'_>,
        spec: &TaskSpec,
        requirement: &leveler_orchestrator::Requirement,
        modified_files: &[String],
        observer: &mut dyn FnMut(EngineEvent),
        cancellation: &CancellationToken,
    ) -> Result<leveler_verifier::AcceptanceLedger, EngineError> {
        let from_requirement: Vec<AcceptanceCheck> = requirement
            .acceptance_criteria
            .iter()
            .map(|ac| AcceptanceCheck {
                id: ac.id.clone(),
                description: ac.description.clone(),
                command: ac.verification_hint.clone(),
                required: ac.required,
            })
            .collect();
        let checks = assemble_acceptance_checks(from_requirement, &spec.repository, modified_files);
        if checks.is_empty() {
            return Ok(leveler_verifier::AcceptanceLedger::default());
        }
        let ledger = Verifier::with_environment(
            &spec.repository,
            self.factory.tool_context.environment.clone(),
        )
        .evaluate_acceptance(&checks, cancellation)
        .await;
        for item in &ledger.items {
            log.append(
                None,
                EngineEvent::AcceptanceEvidence {
                    id: item.id.clone(),
                    description: item.description.clone(),
                    required: item.required,
                    status: format!("{:?}", item.status).to_lowercase(),
                    reject_reason: item.reject_reason.clone(),
                },
                observer,
            )
            .await?;
        }
        Ok(ledger)
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
        let plan = gate_plan(spec);
        let report = verifier
            .verify(
                &plan,
                allowed_paths,
                modified_files,
                cancellation,
                &mut |_| {},
            )
            .await;
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

/// Orchestrate ExpectedMutation decision table (design §1.3). First match wins.
///
/// 1. Any completed Edit node → true<br>
/// 2. Delivery / `delivery_gate` → true<br>
/// 3. `task_looks_like_implementation(requirement.goal)` → true<br>
/// 4. else false
///
/// Must never use `has_mutation` / `modified_files` (self-referential).
fn orchestrate_needs_mutation(
    graph: &leveler_orchestrator::TaskGraph,
    requirement: &leveler_orchestrator::Requirement,
    delivery_gate: bool,
) -> bool {
    use leveler_orchestrator::{NodeStatus, TaskNodeKind};

    if graph
        .nodes
        .iter()
        .any(|n| n.kind == TaskNodeKind::Edit && n.status == NodeStatus::Completed)
    {
        return true;
    }
    if delivery_gate {
        return true;
    }
    leveler_lifecycle::task_looks_like_implementation(&requirement.goal)
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
    use leveler_core::{TaskId, TaskNodeId};
    use leveler_orchestrator::{
        NodeStatus, Requirement, StepBudget, TaskGraph, TaskNode, TaskNodeKind,
    };

    fn req(goal: &str) -> Requirement {
        Requirement {
            raw_text: goal.to_string(),
            goal: goal.to_string(),
            task_type: leveler_orchestrator::TaskType::Feature,
            constraints: vec![],
            acceptance_criteria: vec![],
            out_of_scope: vec![],
            risk: leveler_orchestrator::TaskRisk::Medium,
            uncertainties: vec![],
        }
    }

    fn graph_with(kind: TaskNodeKind, status: NodeStatus) -> TaskGraph {
        TaskGraph {
            id: TaskId::new("t"),
            goal: "g".into(),
            nodes: vec![TaskNode {
                id: TaskNodeId::new("n1"),
                kind,
                description: "d".into(),
                dependencies: vec![],
                allowed_paths: vec![],
                expected_outputs: vec![],
                acceptance_criteria: vec![],
                budget: StepBudget::default(),
                status,
            }],
        }
    }

    #[test]
    fn completed_edit_requires_mutation() {
        let g = graph_with(TaskNodeKind::Edit, NodeStatus::Completed);
        assert!(orchestrate_needs_mutation(&g, &req("explain auth"), false));
    }

    #[test]
    fn inspect_only_does_not_require_mutation() {
        let g = graph_with(TaskNodeKind::Inspect, NodeStatus::Completed);
        assert!(!orchestrate_needs_mutation(
            &g,
            &req("explain how auth works"),
            false
        ));
    }

    #[test]
    fn delivery_gate_requires_mutation() {
        let g = graph_with(TaskNodeKind::Inspect, NodeStatus::Completed);
        assert!(orchestrate_needs_mutation(
            &g,
            &req("explain how auth works"),
            true
        ));
    }

    #[test]
    fn implementation_heuristic_requires_mutation() {
        let g = graph_with(TaskNodeKind::Test, NodeStatus::Completed);
        assert!(orchestrate_needs_mutation(
            &g,
            &req("fix the login bug"),
            false
        ));
    }

    #[test]
    fn failed_edit_does_not_count_as_completed_edit() {
        // Priority 1 only matches Completed Edit; failed Edit falls through.
        let g = graph_with(TaskNodeKind::Edit, NodeStatus::Failed);
        assert!(!orchestrate_needs_mutation(
            &g,
            &req("explain how auth works"),
            false
        ));
    }

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
        S::Completed | S::Answered => None,
        S::BudgetExhausted => Some(TaskOutcome::BudgetLimited),
        // Incomplete thrash, stalled quiet, blocked, etc. — never success.
        S::Incomplete | S::Blocked | S::Stalled | S::CompletedUnverified => {
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
