//! Session orchestration: run, chat and resume through the task engine.
//!
//! Every path delegates to `leveler-engine`'s [`TaskEngine`], so turns, tool
//! calls, approvals and verification results are persisted before observers
//! see them, and an interrupted run resumes from its exact transcript. The
//! `AgentEvent` observer signature is kept as a temporary shim until the UIs
//! consume `EngineEvent` directly (plan B6).

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use leveler_agent::coding::{TaskReport, TaskSpec, mode_str};
use leveler_agent::{
    AdvisoryKind, AgentEvent, AgentOutcome, AgentVerificationStatus, AutoClarify, Clarifier,
    StopReason,
};
use leveler_engine::{EngineError, EngineEvent, ExecutionKind, TaskOutcome};
use leveler_execution::{Approver, PermissionProfile};
use leveler_model::{ContentPart, ModelRef};
use leveler_storage::{SessionRecord, SessionRepository};
use leveler_verifier::{Verdict, VerificationReport};

use crate::{AppError, Application};

/// Map a run result to the legacy persisted session status/state columns.
/// (The engine additionally stamps the `outcome` column.)
fn verification_failure_summary(report: &VerificationReport) -> String {
    if !report.scope_ok {
        return format!(
            "modified files outside allowed scope: {}",
            report.scope_violations.join(", ")
        );
    }
    let failed_gates = report.failed_gates();
    let failed: Vec<String> = failed_gates
        .iter()
        .map(|check| failed_gate_label(check))
        .collect();
    if failed.is_empty() {
        "verification did not pass".to_string()
    } else {
        format!("failed gate(s): {}", failed.join(", "))
    }
}

/// Terminal-marker label for one failed gate: the check name plus the parsed
/// failing test ids (capped at two, with the remainder as a count) so the
/// marker carries evidence instead of contradicting the agent's own summary
/// unexplained. Falls back to the bare name when no test ids were parsed
/// (build/fmt failures, unparsable output).
fn failed_gate_label(check: &leveler_verifier::CheckOutcome) -> String {
    if check.failed_tests.is_empty() {
        return check.name.clone();
    }
    let shown: Vec<&str> = check
        .failed_tests
        .iter()
        .take(2)
        .map(String::as_str)
        .collect();
    let rest = check.failed_tests.len() - shown.len();
    if rest == 0 {
        format!("{} ({})", check.name, shown.join(", "))
    } else {
        format!("{} ({}, +{} more)", check.name, shown.join(", "), rest)
    }
}

fn goal_from_content(content: &[ContentPart]) -> String {
    content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// LEGACY one-way adapter: canonical `EngineEvent` → the old `AgentEvent`
/// observer vocabulary. The interactive UI path no longer goes through this —
/// it consumes `EngineEvent` directly (see `EventBridge`). Remaining callers
/// are the headless CLI renderer (`run_in_session` / `resume_session`) and
/// the eval collectors (`run_in_session_bounded`, `eval_signals`). Never add
/// a UI path through here: the mapping is deliberately lossy (engine-only
/// facts return `None`).
fn forward_engine_event(event: EngineEvent, observer: &mut (dyn FnMut(AgentEvent) + Send)) {
    if let Some(agent_event) = engine_event_to_agent(event) {
        observer(agent_event);
    }
}

/// LEGACY: map an engine kernel event to its old `AgentEvent` counterpart.
/// Engine-only events (task/turn lifecycle, approvals, plan strategy) return
/// `None` — they are already persisted in the event log and are surfaced by
/// engine-aware consumers directly. Kept only for the headless CLI renderer
/// and the eval collectors; the client projection is `EventBridge` over
/// `EngineEvent`.
pub fn engine_event_to_agent(event: EngineEvent) -> Option<AgentEvent> {
    Some(match event {
        EngineEvent::StreamAttemptStarted => AgentEvent::StreamAttemptStarted,
        EngineEvent::AssistantDelta { text } => AgentEvent::AssistantDelta(text),
        EngineEvent::ReasoningDelta { text } => AgentEvent::ReasoningDelta(text),
        EngineEvent::AssistantMessage { text } => AgentEvent::AssistantText(text),
        EngineEvent::ToolCallStarted {
            call_id,
            name,
            arguments,
            parallel,
            agent_id: None,
            ..
        } => AgentEvent::ToolCall {
            id: call_id,
            name,
            arguments,
            parallel,
        },
        EngineEvent::ToolCallFinished {
            call_id,
            name,
            is_error,
            preview,
            agent_id: None,
            applied_diff,
        } => AgentEvent::ToolResult {
            id: call_id,
            name,
            is_error,
            preview,
            applied_diff,
        },
        // A delegated agent's canonical events are durable FACTS for recovery,
        // not a second UI stream: the parent already surfaces child work as
        // attributed `SubAgentActivity`. Projecting them here would render
        // every child tool call twice.
        EngineEvent::ToolCallStarted { .. } | EngineEvent::ToolCallFinished { .. } => return None,
        EngineEvent::WorkspaceSnapshotCreated { call_id, snapshot } => {
            AgentEvent::WorkspaceSnapshot { call_id, snapshot }
        }
        EngineEvent::TokenUsage {
            input_tokens,
            output_tokens,
            cached_input_tokens,
        } => AgentEvent::Usage {
            input_tokens,
            output_tokens,
            cached_input_tokens,
        },
        EngineEvent::Compacted { from, to } => AgentEvent::Compacted { from, to },
        EngineEvent::AdvisoryStarted { kind } => AgentEvent::AdvisoryStarted {
            // Unknown keys (older/newer logs) degrade to the audit label.
            kind: AdvisoryKind::from_key(&kind).unwrap_or(AdvisoryKind::ContextCompaction),
        },
        EngineEvent::CommandProgress { label, elapsed_ms } => {
            AgentEvent::CommandProgress { label, elapsed_ms }
        }
        EngineEvent::PlanUpdated { steps } => AgentEvent::PlanUpdated { steps },
        EngineEvent::GoalIntercepted { kind, detail } => {
            AgentEvent::GoalIntercepted { kind, detail }
        }
        EngineEvent::DelegationStage { action, detail } => {
            AgentEvent::DelegationStage { action, detail }
        }
        EngineEvent::EvidenceLedgerUpdated { ledger } => {
            AgentEvent::EvidenceLedgerUpdated { ledger }
        }
        EngineEvent::ProgressUpdated { ledger } => AgentEvent::ProgressUpdated { ledger },
        EngineEvent::SubAgentStarted {
            id,
            nickname,
            role,
            task,
            profile_id,
            profile_role,
            read_only,
        } => AgentEvent::SubAgentStarted {
            id,
            nickname,
            role,
            task,
            profile_id,
            profile_role,
            read_only,
        },
        EngineEvent::SubAgentProgress {
            id,
            active,
            input_tokens,
            output_tokens,
            cached_input_tokens,
        } => AgentEvent::SubAgentProgress {
            id,
            active,
            input_tokens,
            output_tokens,
            cached_input_tokens,
        },
        EngineEvent::SubAgentFinished {
            id,
            nickname,
            ok,
            summary,
            contribution,
        } => AgentEvent::SubAgentFinished {
            id,
            nickname,
            ok,
            summary,
            contribution,
        },
        EngineEvent::SubAgentActivity {
            id,
            phase,
            tool,
            preview,
            is_error,
        } => AgentEvent::SubAgentActivity {
            id,
            phase,
            tool,
            preview,
            is_error,
        },
        EngineEvent::RunFinished { text } => AgentEvent::Finished(text),
        EngineEvent::VerificationStarted => AgentEvent::VerificationStarted,
        EngineEvent::VerificationCheck {
            name,
            status,
            evidence,
        } => AgentEvent::VerificationCheck {
            name,
            status: match status.as_str() {
                "passed" => AgentVerificationStatus::Passed,
                "failed" => AgentVerificationStatus::Failed,
                _ => AgentVerificationStatus::Skipped,
            },
            evidence,
        },
        EngineEvent::VerificationFinished {
            passed,
            verification,
        } => AgentEvent::VerificationFinished {
            passed,
            verification,
        },
        // Engine lifecycle & strategy events: persisted; engine-aware
        // consumers surface them directly.
        _ => return None,
    })
}

/// Map the engine's terminal report onto the app-level stop reason the UI
/// renders. Two orthogonal facts arrive — how the run ended, and what the
/// project's checks said — and both are surfaced without folding one into
/// the other: a completed run with failed checks is "done, checks failed",
/// never "incomplete" and never a bare "done".
fn report_to_result(report: TaskReport) -> Result<AgentOutcome, AppError> {
    use leveler_lifecycle::VerificationStatus;
    // A run "did work" if it claimed completion or actually touched files. A
    // pure conversational answer did neither, so it carries no verification
    // verdict — it just ends as "answered".
    let did_work = report.stop_reason == StopReason::Completed || !report.modified_files.is_empty();
    let (stop_reason, detail) = match report.outcome {
        // A guard-forced Incomplete stop keeps its honest stop reason: the
        // checks describe the tree, not the task (R004 F4).
        TaskOutcome::Completed if did_work && report.stop_reason != StopReason::Incomplete => {
            match report.verification_status {
                VerificationStatus::Passed => (StopReason::Completed, None),
                VerificationStatus::Failed => (
                    StopReason::CompletedChecksFailed,
                    report
                        .verification
                        .as_ref()
                        .map(verification_failure_summary),
                ),
                VerificationStatus::NotRun | VerificationStatus::Unavailable => (
                    StopReason::CompletedUnverified,
                    Some(unverified_detail(&report)),
                ),
            }
        }
        // Pure Q&A, or any other terminal reason: keep the executor's reason.
        _ => (report.stop_reason, None),
    };
    Ok(AgentOutcome {
        final_text: report.final_text,
        rounds: report.rounds,
        modified_files: report.modified_files,
        stop_reason,
        stop_detail: detail.or(report.stop_detail),
        budget_exhaustion: None,
        progress: Default::default(),
        objective: leveler_lifecycle::ObjectiveAnchor::from_user_message(""),
    })
}

/// Why the project's checks produced no verdict, as the stable UI token or
/// the verifier's own reason.
fn unverified_detail(report: &TaskReport) -> String {
    if report.modified_files.is_empty() {
        // Stable token for TUI: "◇ 结束 · 未改源码" (not "未验证" delivery).
        return leveler_client_protocol::REASON_NO_CODE_CHANGES.to_string();
    }
    match &report.verification {
        // The project configured no gating checks, so there was nothing to
        // verify against. That is a calm "not auto-verified" finish, not a
        // warning about THIS task.
        Some(verification) if !verification.has_gating_checks() => {
            leveler_client_protocol::REASON_NO_AUTOMATIC_VERIFICATION.to_string()
        }
        Some(verification) => match verification.verdict() {
            Verdict::Unverified(reason) => reason,
            _ => leveler_client_protocol::REASON_NO_AUTOMATIC_VERIFICATION.to_string(),
        },
        None => leveler_client_protocol::REASON_NO_AUTOMATIC_VERIFICATION.to_string(),
    }
}

pub(crate) fn app_error_from_engine(error: EngineError) -> AppError {
    match error {
        // A provider fault stays typed all the way to the app: the callers
        // that classify infrastructure failures read the error, not the text.
        EngineError::Execution {
            model: Some(error), ..
        } => AppError::Model(error),
        EngineError::Execution { detail, .. } => AppError::Engine(detail),
        EngineError::Cancelled => AppError::Agent(leveler_agent::AgentError::Cancelled),
        EngineError::StaleOwnership(m) => {
            AppError::Agent(leveler_agent::AgentError::StaleOwnership(m))
        }
        EngineError::Storage(e) => AppError::Storage(e),
        EngineError::Serde(e) => AppError::Serde(e.to_string()),
        EngineError::Config(m) | EngineError::Corrupt(m) => AppError::Engine(m),
        // Pass the diagnostic through verbatim rather than flattening it back
        // to a bare sentence — the whole point of carrying the event type, the
        // producing agent and the capacity is that they reach the user.
        error @ EngineError::EventBufferOverloaded { .. } => AppError::Engine(error.to_string()),
        EngineError::RecoveryConfirmationRequired { call_id, tool } => AppError::Engine(format!(
            "crash recovery halted: an interrupted `{tool}` (call {call_id}) may already have \
             run; inspect the workspace, then resume with --confirm-recovery to acknowledge \
             and continue"
        )),
        // Ownership failures stay loud and named: the user (or a supervising
        // layer) must know this was a fencing decision, not a storage fault.
        error @ (EngineError::Ownership(_) | EngineError::OwnershipConflict { .. }) => {
            AppError::Engine(error.to_string())
        }
    }
}

pub(crate) fn mode_from_str(s: &str) -> Option<PermissionProfile> {
    // parse() covers current wire values and the legacy 0003 names ("plan",
    // "workspace_write") still present as SQLite column DEFAULTs.
    PermissionProfile::parse(s)
}

impl Application {
    /// Create and persist a new session record, returning its id. The caller can
    /// then run it, and — crucially — knows the id even if the run is cancelled.
    pub async fn create_session(
        &self,
        model: &ModelRef,
        goal: &str,
    ) -> Result<leveler_core::SessionId, AppError> {
        let db = self.open_database().await?;
        self.reap_zombie_turns(&db, None).await?;
        self.insert_session(&db, model, goal).await
    }

    /// Clear the zombie `running` turns this runtime left behind, optionally
    /// scoped to one session. Returns how many were reaped.
    ///
    /// A process that is starting up does this once: a turn whose owning
    /// process was killed is not running any more, and a row that still says it
    /// is becomes a live spinner over dead work the next time anyone opens the
    /// session. Foreign-owned tasks are reported and never touched — the
    /// ownership check inside the reaper is what makes it safe to call from a
    /// repository where a daemon may be alive.
    pub async fn reap_zombie_turns(
        &self,
        db: &leveler_storage::Database,
        session: Option<&leveler_core::SessionId>,
    ) -> Result<usize, AppError> {
        let runtime_id = self.runtime_id()?;
        let outcome = leveler_engine::reap_after_restart(
            &leveler_storage::EngineStores::from_database(db),
            &runtime_id,
            session,
        )
        .await
        .map_err(app_error_from_engine)?;
        for conflict in &outcome.conflicts {
            tracing::warn!(
                session = conflict.session_id.as_str(),
                owner = ?conflict.owner,
                "not reaping a task owned by another runtime"
            );
        }
        if !outcome.events.is_empty() {
            tracing::warn!(
                reaped = outcome.events.len(),
                session = session.map(|s| s.as_str()),
                "reaped zombie running turns at startup"
            );
        }
        Ok(outcome.events.len())
    }

    /// Create a session inside a long-lived daemon. Startup performs the zombie
    /// reap once; doing it for every new session would interrupt unrelated live
    /// turns owned by the same daemon.
    pub(crate) async fn create_daemon_session(
        &self,
        model: &ModelRef,
        goal: &str,
    ) -> Result<leveler_core::SessionId, AppError> {
        let db = self.open_database().await?;
        self.insert_session(&db, model, goal).await
    }

    async fn insert_session(
        &self,
        db: &leveler_storage::Database,
        model: &ModelRef,
        goal: &str,
    ) -> Result<leveler_core::SessionId, AppError> {
        let record = SessionRecord::new(
            self.layout.repo_root.display().to_string(),
            goal,
            model.to_string(),
            leveler_core::now(),
        )
        .with_axes(self.collaboration().as_str(), self.work_profile().as_str());
        let repo = SessionRepository::new(db);
        repo.create(&record).await?;
        let id = leveler_core::SessionId::new(record.id);
        // Never rely on SQLite DEFAULT 'workspace_write' from migration 0003 —
        // that string is no longer a valid PermissionProfile wire value.
        repo.set_execution(
            &id,
            PermissionProfile::Assisted.as_str(),
            false,
            "direct",
            leveler_core::now(),
        )
        .await?;
        Ok(id)
    }

    /// The direct-task spec for this repository: verification is discovered
    /// from `.leveler/config.yaml` or the repo's manifests.
    fn direct_spec(&self, goal: String, mode: PermissionProfile, sandbox: bool) -> TaskSpec {
        TaskSpec {
            runtime: leveler_agent::coding::RuntimeTaskSpec {
                goal,
                kind: ExecutionKind::Direct,
                continuation: crate::goal_continuation_for(self.task_round_limit),
                limits: self.top_level_limits(),
            },
            coding: leveler_agent::coding::CodingTaskSpec {
                repository: self.layout.repo_root.clone(),
                mode,
                sandbox,
                verification: leveler_verifier::discover::plan_for_repo(&self.layout.repo_root),
                base_commit: None,
            },
        }
    }

    /// Run a previously-created session to completion (or cancellation),
    /// persisting the transcript incrementally so it can be resumed.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_in_session(
        &self,
        session_id: &leveler_core::SessionId,
        model: &ModelRef,
        mode: PermissionProfile,
        goal: &str,
        approver: Arc<dyn Approver>,
        sandbox: bool,
        observer: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: CancellationToken,
    ) -> Result<AgentOutcome, AppError> {
        // Unattended defaults: AutoClarify + a wall-clock ceiling — nobody is
        // watching a headless run to Ctrl+C a stuck task. Interactive UIs go
        // through [`Self::run_in_session_with_clarifier`], which stays
        // until-terminal. Config `limits.max_duration_seconds` overrides.
        self.run_in_session_with_policy(
            session_id,
            model,
            mode,
            goal,
            approver,
            Arc::new(AutoClarify),
            sandbox,
            // Headless: nobody is at a keyboard to steer.
            None,
            // Legacy AgentEvent observer (headless CLI renderer): adapt from
            // the canonical stream one-way.
            &mut |event| forward_engine_event(event, observer),
            cancellation,
            // Headless goals run under the task round limit when one is set;
            // pinning UntilTerminal over it was the exp8 null result.
            crate::goal_continuation_for(self.task_round_limit),
            unattended_limits(self.top_level_limits()),
        )
        .await
    }

    /// Like [`Self::run_in_session`] but with an injectable clarifier (TUI waits;
    /// CLI may pass AutoClarify for unattended runs).
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    pub async fn run_in_session_with_clarifier(
        &self,
        session_id: &leveler_core::SessionId,
        model: &ModelRef,
        mode: PermissionProfile,
        goal: &str,
        approver: Arc<dyn Approver>,
        clarifier: Arc<dyn Clarifier>,
        sandbox: bool,
        // Mid-turn user input; `None` disables steering for this run.
        steering: Option<Arc<dyn leveler_agent::SteeringSource>>,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
        cancellation: CancellationToken,
    ) -> Result<AgentOutcome, AppError> {
        self.run_in_session_with_policy(
            session_id,
            model,
            mode,
            goal,
            approver,
            clarifier,
            sandbox,
            steering,
            observer,
            cancellation,
            leveler_agent::ContinuationPolicy::UntilTerminal,
            self.top_level_limits(),
        )
        .await
    }

    /// Eval-only entry point: the case owns a fixed round budget so results are
    /// comparable. Interactive callers must use [`Self::run_in_session`].
    #[allow(clippy::too_many_arguments)]
    pub async fn run_in_session_bounded(
        &self,
        session_id: &leveler_core::SessionId,
        model: &ModelRef,
        mode: PermissionProfile,
        goal: &str,
        approver: Arc<dyn Approver>,
        sandbox: bool,
        observer: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: CancellationToken,
        max_rounds: u32,
        // Eval runs are unattended, so this is `None` for every normal run.
        // The eval harness passes a source only when an ablation arm needs to
        // inject a message mid-turn; nothing on a product path sets it.
        steering: Option<Arc<dyn leveler_agent::SteeringSource>>,
    ) -> Result<AgentOutcome, AppError> {
        self.run_in_session_with_policy(
            session_id,
            model,
            mode,
            goal,
            approver,
            Arc::new(AutoClarify),
            sandbox,
            steering,
            // Legacy AgentEvent observer (eval collectors): adapt one-way.
            &mut |event| forward_engine_event(event, observer),
            cancellation,
            leveler_agent::ContinuationPolicy::bounded(max_rounds),
            leveler_agent::StepLimits::default(),
        )
        .await
    }

    /// The product axes a turn in `session_id` executes under.
    ///
    /// The SESSION ROW is the single authority: `/work-mode` writes the choice
    /// there, and every turn — interactive, headless, resumed — composes its
    /// tool surface from it. This Application's in-memory work profile is the
    /// value a NEW session is created with; it never competes with the row of
    /// a session that already exists. `run_in_session_with_content` (the
    /// interactive turn) used to read the Application default, so `/work-mode
    /// economy` changed the row and the next turn still composed the balanced
    /// surface — one durable fact with two readers.
    pub(crate) async fn turn_axes(
        &self,
        repo: &SessionRepository<'_>,
        session_id: &leveler_core::SessionId,
    ) -> Result<(leveler_agent::WorkProfile, bool), AppError> {
        let Some(record) = repo.get(session_id).await? else {
            return Ok((self.work_profile(), false));
        };
        let (work_profile, collaboration) = crate::axes_from_session_record(&record);
        Ok((
            work_profile,
            collaboration == leveler_lifecycle::CollaborationMode::Plan,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_in_session_with_policy(
        &self,
        session_id: &leveler_core::SessionId,
        model: &ModelRef,
        mode: PermissionProfile,
        goal: &str,
        approver: Arc<dyn Approver>,
        clarifier: Arc<dyn Clarifier>,
        sandbox: bool,
        steering: Option<Arc<dyn leveler_agent::SteeringSource>>,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
        cancellation: CancellationToken,
        continuation: leveler_agent::ContinuationPolicy,
        limits: leveler_agent::StepLimits,
    ) -> Result<AgentOutcome, AppError> {
        let db = self.open_database().await?;
        let repo = SessionRepository::new(&db);
        // Lifecycle (Running/terminal status+state) is stamped by the engine —
        // the single writer — atomically with outcome and TaskFinished.
        // Persist the execution config so resume never guesses (plan B4).
        repo.set_execution(
            session_id,
            mode_str(mode),
            sandbox,
            ExecutionKind::Direct.as_str(),
            leveler_core::now(),
        )
        .await?;
        // Product axes SoT is the session row (SetProductAxes / create defaults).
        let (work_profile, read_only) = self.turn_axes(&repo, session_id).await?;

        let engine = self
            .engine_for_with_profile(
                model,
                mode,
                sandbox,
                approver,
                clarifier,
                work_profile,
                read_only,
                Some(session_id.as_str()),
            )
            .await?
            .with_steering(steering);
        // System-side memory candidates (explicit intent + package-manager
        // signals). Never writes active memory; user accept is separate (K36).
        self.enqueue_memory_candidates(goal);
        let mut spec = self.direct_spec(goal.to_string(), mode, sandbox);
        spec.runtime.continuation = continuation;
        spec.runtime.limits = limits;
        // Goal identity (long-goal P1/P2). Recorded HERE rather than at the
        // interactive call site because this is the seam both paths cross:
        // `leveler run` (headless) and the TUI's RunGoal reach the engine
        // through it. Wiring only the interactive one left every headless run
        // — the ones most likely to be killed unattended — with no record
        // that work was owed.
        let goal_record = self.open_goal_record(session_id, goal).await;
        let result = engine.run(session_id, &spec, observer, cancellation).await;
        // The engine already decided what this run means; this reads its
        // answer rather than inferring one from "the call returned". A goal
        // whose run stopped at its round budget still owes work, and the whole
        // reason the goal ledger exists is so that fact survives the process.
        self.record_goal_windows(goal_record.as_ref(), &result)
            .await;
        if goal_owes_no_more_work(&result) {
            self.settle_goal_record(goal_record).await;
        }
        match result {
            Ok(report) => report_to_result(report),
            Err(error) => Err(app_error_from_engine(error)),
        }
    }

    /// Record that a long-lived intent exists, before the work starts.
    ///
    /// Best-effort: a goal whose bookkeeping cannot be written must still run.
    /// `None` means "not recorded", and every caller treats that as nothing to
    /// settle rather than as a settled goal.
    async fn open_goal_record(
        &self,
        session_id: &leveler_core::SessionId,
        objective: &str,
    ) -> Option<leveler_core::GoalId> {
        let db = self.open_database().await.ok()?;
        let now = leveler_core::now();
        let task = leveler_storage::TaskStore::ensure_for_session(&db, session_id, now)
            .await
            .ok()?;
        // An objective this task still owes IS this objective: continuing it is
        // what resuming means. Opening a second record for the same intent
        // splits one goal across two — the windows spent on it land half in
        // each, and the record the earlier invocation left owed stays owed
        // forever because nothing will ever settle it again.
        if let Ok(existing) = leveler_storage::GoalStore::for_task(&db, &task).await
            && let Some(owed) = existing.into_iter().find(|goal| {
                goal.state == leveler_storage::GoalState::Running && goal.objective == objective
            })
        {
            return Some(owed.id);
        }
        match leveler_storage::GoalStore::open(&db, &task, objective, now).await {
            Ok(id) => Some(id),
            Err(error) => {
                tracing::warn!(%error, "could not record goal identity; the goal still runs");
                None
            }
        }
    }

    /// Record the work windows this invocation spent on the goal.
    ///
    /// Best-effort, like every other line of goal bookkeeping: a window that
    /// cannot be written down must not fail the run that ran it. The store
    /// counts calls, so a resumed goal accumulates across processes — which is
    /// the only way a count of windows can outlive the windows.
    async fn record_goal_windows(
        &self,
        goal: Option<&leveler_core::GoalId>,
        result: &Result<leveler_agent::coding::TaskReport, leveler_engine::EngineError>,
    ) {
        let Some(goal) = goal else { return };
        // A run that never produced a report still opened one window. Saying
        // "no windows" about a goal that just spent real model calls would be
        // a worse lie than an approximate count.
        let windows = result.as_ref().map(|r| r.windows).unwrap_or(1);
        let Ok(db) = self.open_database().await else {
            tracing::warn!("could not record goal windows: database unavailable");
            return;
        };
        for _ in 0..windows {
            if let Err(error) = leveler_storage::GoalStore::note_window(&db, goal).await {
                tracing::warn!(%error, "could not record a goal work window");
                break;
            }
        }
    }

    /// Mark a goal as owing no further work.
    async fn settle_goal_record(&self, goal: Option<leveler_core::GoalId>) {
        let Some(goal) = goal else { return };
        let Ok(db) = self.open_database().await else {
            tracing::warn!("could not settle goal: database unavailable");
            return;
        };
        if let Err(error) =
            leveler_storage::GoalStore::settle(&db, &goal, leveler_core::now()).await
        {
            tracing::warn!(%error, "could not settle goal record");
        }
    }

    /// Like [`Application::run_in_session`], but the first user message carries
    /// arbitrary content parts (text + images) for multimodal input (spec §43).
    #[allow(clippy::too_many_arguments)]
    pub async fn run_in_session_with_content(
        &self,
        session_id: &leveler_core::SessionId,
        model: &ModelRef,
        mode: PermissionProfile,
        content: Vec<ContentPart>,
        approver: Arc<dyn Approver>,
        clarifier: Arc<dyn Clarifier>,
        sandbox: bool,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
        cancellation: CancellationToken,
    ) -> Result<AgentOutcome, AppError> {
        let db = self.open_database().await?;
        let repo = SessionRepository::new(&db);
        // Lifecycle is stamped by the engine (single writer).
        repo.set_execution(
            session_id,
            mode_str(mode),
            sandbox,
            ExecutionKind::Direct.as_str(),
            leveler_core::now(),
        )
        .await?;

        let (work_profile, read_only) = self.turn_axes(&repo, session_id).await?;
        let engine = self
            .engine_for_with_profile(
                model,
                mode,
                sandbox,
                approver,
                clarifier,
                work_profile,
                read_only,
                Some(session_id.as_str()),
            )
            .await?;
        let goal = goal_from_content(&content);
        // System-side memory candidates (explicit intent + package-manager
        // signals). Never writes active memory; user accept is separate (K36).
        self.enqueue_memory_candidates(&goal);
        let spec = self.direct_spec(goal, mode, sandbox);
        let result = engine
            .chat(session_id, &spec, content, observer, cancellation)
            .await;
        match result {
            Ok(report) => report_to_result(report),
            Err(error) => Err(app_error_from_engine(error)),
        }
    }

    /// Resume an interrupted session from its persisted transcript AND its
    /// persisted execution config (mode/sandbox/kind) **and product axes**
    /// Close a session's dangling tool calls with a user-acknowledged marker
    /// so a resume blocked by crash-recovery confirmation can proceed. The
    /// caller asserts the workspace has been inspected. Returns the count.
    pub async fn acknowledge_crash_window(
        &self,
        session_id: &leveler_core::SessionId,
    ) -> Result<usize, AppError> {
        let db = self.open_database().await?;
        // Canonical recovery write ⇒ ownership-fenced. Resolve the task,
        // refuse a foreign owner explicitly (never auto-steal), reacquire a
        // fresh epoch for this runtime, then acknowledge under that token.
        let runtime_id = self.runtime_id()?;
        let task =
            leveler_storage::TaskStore::ensure_for_session(&db, session_id, leveler_core::now())
                .await?;
        let current = leveler_storage::OwnershipStore::current(&db, &task)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("no task for session {session_id}")))?;
        if let Some(owner) = &current.runtime
            && owner != &runtime_id
        {
            return Err(AppError::Engine(format!(
                "task {task} is owned by runtime {owner} at epoch {}; \
                 this runtime ({runtime_id}) must not acknowledge its crash window",
                current.epoch
            )));
        }
        let token =
            leveler_storage::OwnershipStore::acquire(&db, &task, &runtime_id, current.epoch)
                .await
                .map_err(|e| AppError::Engine(e.to_string()))?;
        leveler_engine::acknowledge_crash_window(&db, &token, session_id)
            .await
            .map_err(app_error_from_engine)
    }

    /// (work_profile / collaboration). Application in-memory defaults are not
    /// the SoT for resume — the session row is (CLI `leveler resume` may
    /// `assemble()` with balanced default).
    pub async fn resume_session(
        &self,
        session_id: &leveler_core::SessionId,
        approver: Arc<dyn Approver>,
        observer: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: CancellationToken,
    ) -> Result<AgentOutcome, AppError> {
        let db = self.open_database().await?;
        let repo = SessionRepository::new(&db);
        let record = repo
            .get(session_id)
            .await?
            .ok_or_else(|| AppError::NotFound(session_id.to_string()))?;
        let model = ModelRef::parse(&record.model)
            .ok_or_else(|| AppError::NotFound(format!("model `{}`", record.model)))?;
        let (mode, sandbox, kind, _) = repo
            .execution(session_id)
            .await?
            .ok_or_else(|| AppError::NotFound(session_id.to_string()))?;
        let mode = mode_from_str(&mode)
            .ok_or_else(|| AppError::Engine(format!("unknown persisted mode `{mode}`")))?;
        let kind = ExecutionKind::parse(&kind).map_err(app_error_from_engine)?;
        // Product axes: SoT is the session row, not Application defaults.
        let (work_profile, collaboration) = crate::axes_from_session_record(&record);
        let read_only = collaboration == leveler_lifecycle::CollaborationMode::Plan;

        let engine = self
            .engine_for_with_profile(
                &model,
                mode,
                sandbox,
                approver,
                Arc::new(AutoClarify),
                work_profile,
                read_only,
                Some(session_id.as_str()),
            )
            .await?;
        let mut spec = self.direct_spec(record.goal.clone(), mode, sandbox);
        // Resume with the persisted strategy, not an assumed one.
        spec.runtime.kind = kind;
        let result = engine
            .resume(
                session_id,
                &spec,
                &mut |event| forward_engine_event(event, observer),
                cancellation,
            )
            .await;
        match result {
            Ok(report) => report_to_result(report),
            Err(error) => Err(app_error_from_engine(error)),
        }
    }
}

/// A headless run's wall-clock ceiling when the project config sets none.
/// Nobody is present to Ctrl+C an unattended task stuck on a slow gateway or
/// a spinning model; interactive runs deliberately have no default ceiling.
const DEFAULT_UNATTENDED_MAX_DURATION: std::time::Duration =
    std::time::Duration::from_secs(60 * 60);

/// Apply the unattended wall-clock default without overriding an explicitly
/// configured `limits.max_duration_seconds`.
fn unattended_limits(mut limits: leveler_agent::StepLimits) -> leveler_agent::StepLimits {
    if limits.max_duration.is_none() {
        limits.max_duration = Some(DEFAULT_UNATTENDED_MAX_DURATION);
    }
    limits
}

#[cfg(test)]
mod unattended_limits_tests {
    use super::*;

    #[test]
    fn headless_runs_get_a_wall_clock_ceiling_by_default() {
        let limits = unattended_limits(leveler_agent::StepLimits::default());
        assert_eq!(
            limits.max_duration,
            Some(DEFAULT_UNATTENDED_MAX_DURATION),
            "an unattended run must never be unbounded in wall-clock time"
        );
    }

    #[test]
    fn configured_duration_wins_over_the_default() {
        let configured = leveler_agent::StepLimits {
            max_duration: Some(std::time::Duration::from_secs(120)),
            ..Default::default()
        };
        assert_eq!(
            unattended_limits(configured).max_duration,
            Some(std::time::Duration::from_secs(120))
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_client_protocol::RuntimeEvent;
    use leveler_engine::TaskOutcome;
    use leveler_execution::PermissionProfile;

    #[test]
    fn mode_from_str_accepts_current_and_legacy_wire_values() {
        assert_eq!(mode_from_str("assisted"), Some(PermissionProfile::Assisted));
        assert_eq!(
            mode_from_str("full_access"),
            Some(PermissionProfile::FullAccess)
        );
        assert_eq!(
            mode_from_str("request_approval"),
            Some(PermissionProfile::RequestApproval)
        );
        // Legacy 0003 defaults still present on some DBs / column DEFAULT.
        assert_eq!(
            mode_from_str("workspace_write"),
            Some(PermissionProfile::Assisted)
        );
        assert_eq!(
            mode_from_str("plan"),
            Some(PermissionProfile::RequestApproval)
        );
        assert_eq!(mode_from_str("bogus"), None);
    }

    fn report(outcome: TaskOutcome, stop_reason: StopReason, modified: &[&str]) -> TaskReport {
        report_with(
            outcome,
            leveler_lifecycle::VerificationStatus::NotRun,
            stop_reason,
            modified,
        )
    }

    fn report_with(
        outcome: TaskOutcome,
        verification_status: leveler_lifecycle::VerificationStatus,
        stop_reason: StopReason,
        modified: &[&str],
    ) -> TaskReport {
        TaskReport {
            outcome,
            final_text: String::new(),
            modified_files: modified.iter().map(|s| s.to_string()).collect(),
            verification: None,
            verification_status,
            stop_reason,
            stop_detail: None,
            rounds: 1,
            windows: 1,
            review: None,
        }
    }

    /// R004 F4: a guard-forced Incomplete stop must never be laundered into
    /// "Completed" by a green gate — gates describe the tree, not the task.
    #[test]
    fn incomplete_stop_survives_a_verified_outcome() {
        let task = report_with(
            TaskOutcome::Completed,
            leveler_lifecycle::VerificationStatus::Passed,
            StopReason::Incomplete,
            &["a.rs"],
        );
        let out = report_to_result(task).unwrap();
        assert_eq!(out.stop_reason, StopReason::Incomplete);

        let task = report(TaskOutcome::Completed, StopReason::Incomplete, &["a.rs"]);
        let out = report_to_result(task).unwrap();
        assert_eq!(out.stop_reason, StopReason::Incomplete);
    }

    #[test]
    fn executor_stop_detail_survives_without_a_stronger_report_detail() {
        let mut task = report(TaskOutcome::BudgetLimited, StopReason::BudgetExhausted, &[]);
        task.stop_detail = Some("budget exhausted: model_tokens spent=120 limit=100".into());

        let out = report_to_result(task).unwrap();

        assert_eq!(
            out.stop_detail.as_deref(),
            Some("budget exhausted: model_tokens spent=120 limit=100")
        );
    }

    #[test]
    fn no_progress_detail_survives_report_and_runtime_event_mapping() {
        const DETAIL: &str = "no-progress streak; all-refused rounds short-circuited";
        let mut task = report(TaskOutcome::Failed, StopReason::Incomplete, &[]);
        task.stop_detail = Some(DETAIL.into());

        let RuntimeEvent::TurnIncomplete { reason } =
            crate::event_bridge::turn_runtime_event(report_to_result(task))
        else {
            panic!("an incomplete report must emit TurnIncomplete");
        };

        assert_eq!(reason, DETAIL);
    }

    #[test]
    fn completed_unverified_work_is_not_reported_as_incomplete() {
        // A finished run that did work but leveler could not independently
        // verify must stay distinct from a genuinely-incomplete run
        // (Stalled/audit-failed), so the UI says "done, unverified" not "not
        // completed".
        let out = report_to_result(report(
            TaskOutcome::Completed,
            StopReason::Completed,
            &["README.md"],
        ))
        .unwrap();
        assert_eq!(out.stop_reason, StopReason::CompletedUnverified);
    }

    #[test]
    fn completed_without_tracked_changes_explains_why_checks_did_not_run() {
        let mut task = report(TaskOutcome::Completed, StopReason::Completed, &[]);
        task.stop_detail = Some("executor fallback".into());
        let out = report_to_result(task).unwrap();

        assert_eq!(out.stop_reason, StopReason::CompletedUnverified);
        assert_eq!(
            out.stop_detail.as_deref(),
            Some(leveler_client_protocol::REASON_NO_CODE_CHANGES)
        );
    }

    #[test]
    fn completed_changes_without_a_plan_explain_that_no_gate_was_found() {
        let out = report_to_result(report(
            TaskOutcome::Completed,
            StopReason::Completed,
            &["README.md"],
        ))
        .unwrap();

        assert_eq!(out.stop_reason, StopReason::CompletedUnverified);
        assert_eq!(
            out.stop_detail.as_deref(),
            Some(leveler_client_protocol::REASON_NO_AUTOMATIC_VERIFICATION)
        );
    }

    #[test]
    fn completed_changes_preserve_the_tool_missing_reason() {
        let mut task = report_with(
            TaskOutcome::Completed,
            leveler_lifecycle::VerificationStatus::Unavailable,
            StopReason::Completed,
            &["src/main.ts"],
        );
        task.verification = Some(leveler_verifier::VerificationReport {
            checks: vec![leveler_verifier::CheckOutcome {
                name: "tsc".to_string(),
                kind: leveler_verifier::CheckKind::Build,
                gating: true,
                status: leveler_verifier::CheckStatus::ToolMissing,
                evidence: String::new(),
                failure: None,
                failed_tests: std::collections::BTreeSet::new(),
            }],
            scope_ok: true,
            scope_violations: Vec::new(),
            baseline_failures: Vec::new(),
        });

        let out = report_to_result(task).unwrap();

        assert_eq!(
            out.stop_detail.as_deref(),
            Some("gating checks did not run: tsc (tool missing)")
        );
    }

    #[test]
    fn zero_gating_checks_report_surfaces_the_token_not_the_raw_reason() {
        // When the project configured no gating checks, the terminal detail
        // must be the stable REASON_NO_AUTOMATIC_VERIFICATION token — not the
        // verifier's raw English reason ("no gating verification checks were
        // configured"), which used to leak into the UI as a ⚠ warning.
        let mut task = report(
            TaskOutcome::Completed,
            StopReason::Completed,
            &["src/lib.rs"],
        );
        task.verification = Some(leveler_verifier::VerificationReport {
            checks: Vec::new(),
            scope_ok: true,
            scope_violations: Vec::new(),
            baseline_failures: Vec::new(),
        });

        let out = report_to_result(task).unwrap();

        assert_eq!(out.stop_reason, StopReason::CompletedUnverified);
        assert_eq!(
            out.stop_detail.as_deref(),
            Some(leveler_client_protocol::REASON_NO_AUTOMATIC_VERIFICATION)
        );
        assert_ne!(
            out.stop_detail.as_deref(),
            Some("no gating verification checks were configured")
        );
    }

    /// Case 3 at the UI seam: the model declared completion and the checks
    /// failed. The marker says both — done, checks failed — and carries the
    /// parsed failing test ids, not just the check name.
    #[test]
    fn checks_failed_marker_names_the_failing_tests() {
        use leveler_verifier::{CheckKind, CheckOutcome, CheckStatus, VerificationReport};
        let mut task = report_with(
            TaskOutcome::Completed,
            leveler_lifecycle::VerificationStatus::Failed,
            StopReason::Completed,
            &["src/lib.rs"],
        );
        task.verification = Some(VerificationReport {
            checks: vec![CheckOutcome {
                name: "cargo test".into(),
                kind: CheckKind::Test,
                gating: true,
                status: CheckStatus::Failed,
                evidence: String::new(),
                failure: None,
                failed_tests: ["permission_grants::always_allow_grants_survive_reassembly"]
                    .into_iter()
                    .map(String::from)
                    .collect(),
            }],
            scope_ok: true,
            scope_violations: Vec::new(),
            baseline_failures: Vec::new(),
        });

        let out = report_to_result(task).unwrap();

        assert_eq!(out.stop_reason, StopReason::CompletedChecksFailed);
        assert_eq!(
            out.stop_detail.as_deref(),
            Some(
                "failed gate(s): cargo test \
                 (permission_grants::always_allow_grants_survive_reassembly)"
            )
        );
    }

    #[test]
    fn checks_failed_marker_caps_the_test_list_and_keeps_the_count() {
        // Many failing tests must not flood the one-line marker: show the
        // first two ids and the size of the remainder.
        use leveler_verifier::{CheckKind, CheckOutcome, CheckStatus, VerificationReport};
        let mut task = report_with(
            TaskOutcome::Completed,
            leveler_lifecycle::VerificationStatus::Failed,
            StopReason::Completed,
            &["src/lib.rs"],
        );
        task.verification = Some(VerificationReport {
            checks: vec![CheckOutcome {
                name: "cargo test".into(),
                kind: CheckKind::Test,
                gating: true,
                status: CheckStatus::Failed,
                evidence: String::new(),
                failure: None,
                failed_tests: ["a::one", "b::two", "c::three", "d::four"]
                    .into_iter()
                    .map(String::from)
                    .collect(),
            }],
            scope_ok: true,
            scope_violations: Vec::new(),
            baseline_failures: Vec::new(),
        });

        let out = report_to_result(task).unwrap();

        assert_eq!(
            out.stop_detail.as_deref(),
            Some("failed gate(s): cargo test (a::one, b::two, +2 more)")
        );
    }

    #[test]
    fn read_only_answer_stays_answered_not_unverified() {
        // A conversational reply that changed nothing must not be stamped
        // "done, unverified" — there was nothing to verify.
        let out =
            report_to_result(report(TaskOutcome::Completed, StopReason::Answered, &[])).unwrap();
        assert_eq!(out.stop_reason, StopReason::Answered);
    }

    #[test]
    fn verified_work_is_reported_completed_even_when_model_only_answered() {
        // leveler's gate passed on real edits, but the model ended with prose
        // instead of update_goal(complete). The passing gate is authoritative:
        // surface it as done, not a bare "answered".
        let out = report_to_result(report_with(
            TaskOutcome::Completed,
            leveler_lifecycle::VerificationStatus::Passed,
            StopReason::Answered,
            &["diff.go"],
        ))
        .unwrap();
        assert_eq!(out.stop_reason, StopReason::Completed);
    }

    #[test]
    fn verified_read_only_answer_stays_answered() {
        // A gate that incidentally passes on a no-edit Q&A must not promote the
        // reply to "completed" — nothing was done.
        let out = report_to_result(report_with(
            TaskOutcome::Completed,
            leveler_lifecycle::VerificationStatus::Passed,
            StopReason::Answered,
            &[],
        ))
        .unwrap();
        assert_eq!(out.stop_reason, StopReason::Answered);
    }
}

/// Does this run's terminal truth mean the goal owes no further work?
///
/// The engine produces a structured verdict for every run and uses it itself —
/// to decide whether to reap the task, and which kind of checkpoint to cut.
/// The durable goal record has to read the same verdict. It used to be settled
/// unconditionally the moment `engine.run` returned, which made "the function
/// came back" the settlement authority and quietly closed the books on the one
/// case the record exists for: a goal stopped at a resource boundary with work
/// still owed.
///
/// Deliberately NOT "anything but Verified stays open". A run that genuinely
/// failed is finished — the goal owes nothing more automatically, and how it
/// went lives on the session row. Only a run that was *cut short* still owes.
pub(crate) fn goal_owes_no_more_work(
    result: &Result<leveler_agent::coding::TaskReport, leveler_engine::EngineError>,
) -> bool {
    use leveler_lifecycle::TaskOutcome;
    match result {
        Ok(report) => match report.outcome {
            // Reached an end, however it went.
            TaskOutcome::Completed | TaskOutcome::Blocked | TaskOutcome::Failed => true,
            // Stopped at an explicit resource boundary: incomplete and
            // resumable, which is precisely "still owed".
            TaskOutcome::BudgetLimited => false,
            // Cut short. No Ok report carries this today; if one ever does,
            // "still owed" is the honest reading of it.
            TaskOutcome::Interrupted => false,
        },
        // Cancelled mid-flight. The work was stopped, not finished.
        Err(leveler_engine::EngineError::Cancelled) => false,
        // The engine could not reach a verdict at all. A goal with no verdict
        // is not a settled goal: leaving it owed is what keeps it discoverable
        // instead of silently dropped.
        Err(_) => false,
    }
}

#[cfg(test)]
mod goal_settlement_tests {
    use super::goal_owes_no_more_work;
    use leveler_agent::coding::TaskReport;
    use leveler_engine::EngineError;
    use leveler_lifecycle::TaskOutcome;

    fn report(outcome: TaskOutcome) -> Result<TaskReport, EngineError> {
        Ok(TaskReport {
            outcome,
            verification_status: leveler_lifecycle::VerificationStatus::NotRun,
            final_text: String::new(),
            modified_files: Vec::new(),
            verification: None,
            stop_reason: leveler_agent::StopReason::Completed,
            stop_detail: None,
            rounds: 1,
            windows: 1,
            review: None,
        })
    }

    #[test]
    fn a_finished_run_settles_however_it_went() {
        assert!(goal_owes_no_more_work(&report(TaskOutcome::Completed)));
        assert!(goal_owes_no_more_work(&report(TaskOutcome::Blocked)));
        assert!(
            goal_owes_no_more_work(&report(TaskOutcome::Failed)),
            "a run that failed is finished; the verdict lives on the session row"
        );
    }

    #[test]
    fn a_run_cut_short_leaves_the_goal_owed() {
        assert!(!goal_owes_no_more_work(&report(TaskOutcome::BudgetLimited)));
        assert!(!goal_owes_no_more_work(&report(TaskOutcome::Interrupted)));
        assert!(!goal_owes_no_more_work(&Err(EngineError::Cancelled)));
    }

    #[test]
    fn no_verdict_is_not_a_settlement() {
        assert!(!goal_owes_no_more_work(&Err(EngineError::Config(
            "nothing ran".to_string()
        ))));
    }
}

#[cfg(test)]
mod turn_axes_tests {
    //! One durable fact, one reader. `/work-mode` writes the session row, so
    //! every turn in that session — interactive included — must compose its
    //! tool surface from the row and never from this process's create-time
    //! default.

    use leveler_agent::WorkProfile;
    use leveler_model::ModelRef;
    use leveler_project::Layout;
    use leveler_storage::SessionRepository;

    use crate::Application;

    /// The axes under test come from the session row and the explicit
    /// create-time default below, so the ambient global config cannot change
    /// the answer either way.
    fn isolated_app(tmp: &tempfile::TempDir, default: WorkProfile) -> Application {
        let layout = Layout::from_parts(
            tmp.path().to_path_buf(),
            tmp.path().join("configs"),
            tmp.path().join("state"),
        );
        Application::assemble(layout)
            .unwrap()
            .with_work_profile(default)
    }

    #[tokio::test]
    async fn a_turn_reads_the_session_row_not_the_application_default() {
        let tmp = tempfile::tempdir().unwrap();
        let creator = isolated_app(&tmp, WorkProfile::Economy);
        let id = creator
            .create_session(&ModelRef::new("mock", "m"), "quick scan")
            .await
            .unwrap();

        // A fresh process: its own default is Balanced, and the row says
        // Economy. The row wins for a session that already exists.
        let next_process = isolated_app(&tmp, WorkProfile::Balanced);
        assert_eq!(next_process.work_profile(), WorkProfile::Balanced);
        let db = next_process.open_database().await.unwrap();
        let repo = SessionRepository::new(&db);
        let (work_profile, read_only) = next_process.turn_axes(&repo, &id).await.unwrap();
        assert_eq!(
            work_profile,
            WorkProfile::Economy,
            "the session row is the single authority for a turn's tool surface"
        );
        assert!(!read_only, "chat collaboration is not a read-only overlay");
    }

    #[tokio::test]
    async fn a_plan_session_is_a_read_only_overlay() {
        let tmp = tempfile::tempdir().unwrap();
        let app = isolated_app(&tmp, WorkProfile::Balanced)
            .with_collaboration(leveler_agent::CollaborationMode::Plan);
        let id = app
            .create_session(&ModelRef::new("mock", "m"), "plan it")
            .await
            .unwrap();
        let db = app.open_database().await.unwrap();
        let repo = SessionRepository::new(&db);
        let (_, read_only) = app.turn_axes(&repo, &id).await.unwrap();
        assert!(read_only);
    }

    /// A session id with no row is not a licence to invent axes: the
    /// create-time default is the only thing left to use, and it must not
    /// silently become a read-only overlay.
    #[tokio::test]
    async fn a_missing_row_falls_back_to_the_create_time_default() {
        let tmp = tempfile::tempdir().unwrap();
        let app = isolated_app(&tmp, WorkProfile::Delivery);
        let db = app.open_database().await.unwrap();
        let repo = SessionRepository::new(&db);
        let (work_profile, read_only) = app
            .turn_axes(&repo, &leveler_core::SessionId::new("no-such-session"))
            .await
            .unwrap();
        assert_eq!(work_profile, WorkProfile::Delivery);
        assert!(!read_only);
    }
}
