//! Session orchestration: run, chat and resume through the task engine.
//!
//! Every path delegates to `leveler-engine`'s [`TaskEngine`], so turns, tool
//! calls, approvals and verification results are persisted before observers
//! see them, and an interrupted run resumes from its exact transcript. The
//! `AgentEvent` observer signature is kept as a temporary shim until the UIs
//! consume `EngineEvent` directly (plan B6).

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use leveler_agent::{
    AdvisoryKind, AgentError, AgentEvent, AgentOutcome, AgentVerificationStatus, AutoClarify,
    Clarifier, StopReason,
};
use leveler_engine::{
    EngineError, EngineEvent, ExecutionKind, TaskOutcome, TaskReport, TaskSpec, mode_str,
};
use leveler_execution::{Approver, PermissionProfile};
use leveler_lifecycle::{AgentState, SessionStatus};
use leveler_model::{ContentPart, ModelRef};
use leveler_storage::{SessionRecord, SessionRepository};
use leveler_verifier::{AcceptanceLedger, Verdict, VerificationReport};

use crate::{AppError, Application};

/// Concrete stop_detail when health is Verified/Failed-not-applied but the
/// task still landed CompletedUnverified (usually acceptance / proven-AC).
fn unverified_acceptance_detail(
    acceptance: Option<&AcceptanceLedger>,
    has_mutation: bool,
) -> String {
    match acceptance {
        Some(ledger) => {
            if let Some(unmet) = ledger.unmet_required().first() {
                return format!(
                    "验收未通过：{}{}",
                    unmet.id,
                    acceptance_failure_suffix(unmet)
                );
            }
            if ledger.has_required_unverifiable() {
                return "验收项缺少可执行检查命令".to_string();
            }
            if has_mutation && !ledger.has_proven_required_met() {
                // Optional-only / empty after demotion — no system-backed Met.
                return "有改动但缺少系统级验收背书".to_string();
            }
            "任务已完成，但没有足够的独立验收证据".to_string()
        }
        None if has_mutation => "有改动但缺少系统级验收背书".to_string(),
        None => "任务已完成，但没有足够的独立验收证据".to_string(),
    }
}

/// Concise "why this AC failed" tail for the turn-end detail: the check command
/// and the first line of its failure output. Without this the user only sees a
/// bare criterion id and cannot tell what ran or why it failed.
fn acceptance_failure_suffix(unmet: &leveler_verifier::AcceptanceEvidence) -> String {
    let cmd = unmet
        .command
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    // Prefer the explicit reject reason; fall back to the raw command output.
    let reason = unmet
        .reject_reason
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(unmet.evidence.as_str());
    let reason = first_line_capped(reason, 100);
    match (cmd, reason.is_empty()) {
        (Some(cmd), false) => format!(" · 检查「{}」：{}", first_line_capped(cmd, 80), reason),
        (Some(cmd), true) => format!(" · 检查「{}」", first_line_capped(cmd, 80)),
        (None, false) => format!(" · {reason}"),
        (None, true) => String::new(),
    }
}

/// First non-empty-trimmed line of `s`, capped to `max` chars with an ellipsis.
fn first_line_capped(s: &str, max: usize) -> String {
    let line = s
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    if line.chars().count() > max {
        let head: String = line.chars().take(max).collect();
        format!("{head}…")
    } else {
        line.to_string()
    }
}

/// Map a run result to the legacy persisted session status/state columns.
/// (The engine additionally stamps the `outcome` column.)
fn status_for_app(result: &Result<AgentOutcome, AppError>) -> (SessionStatus, AgentState) {
    match result {
        Ok(o) => match o.stop_reason {
            StopReason::Completed => (SessionStatus::Completed, AgentState::Complete),
            StopReason::Answered => (SessionStatus::Completed, AgentState::Complete),
            // Plan finished; a forced closeout stop is an abnormal *end*, not an
            // unfinished *task* — it must NOT fall back to Execute.
            StopReason::CloseoutForced => (SessionStatus::Completed, AgentState::Complete),
            StopReason::Incomplete => (SessionStatus::Incomplete, AgentState::Execute),
            StopReason::BudgetExhausted => (SessionStatus::Incomplete, AgentState::Execute),
            // Same outcome as a spent budget — work so far is real, task unfinished.
            StopReason::TurnLimitReached => (SessionStatus::Incomplete, AgentState::Execute),
            StopReason::Blocked => (SessionStatus::Blocked, AgentState::Execute),
            StopReason::Stalled => (SessionStatus::Incomplete, AgentState::Execute),
            StopReason::CompletedUnverified => (SessionStatus::Completed, AgentState::Complete),
        },
        Err(AppError::Agent(AgentError::Cancelled)) => {
            (SessionStatus::Interrupted, AgentState::Execute)
        }
        Err(_) => (SessionStatus::Failed, AgentState::Failed),
    }
}

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

/// Forward the engine's kernel events onto the legacy `AgentEvent` observer.
fn forward_engine_event(event: EngineEvent, observer: &mut dyn FnMut(AgentEvent)) {
    if let Some(agent_event) = engine_event_to_agent(event) {
        observer(agent_event);
    }
}

/// Map an engine kernel event to its legacy `AgentEvent` counterpart.
/// Engine-only events (task/turn lifecycle, approvals, plan strategy) return
/// `None` — they are already persisted in the event log and are surfaced by
/// engine-aware consumers directly.
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
        } => AgentEvent::ToolResult {
            id: call_id,
            name,
            is_error,
            preview,
        },
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
        EngineEvent::PlanUpdated { steps } => AgentEvent::PlanUpdated { steps },
        EngineEvent::GoalIntercepted { kind, detail } => {
            AgentEvent::GoalIntercepted { kind, detail }
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
        } => AgentEvent::SubAgentStarted {
            id,
            nickname,
            role,
            task,
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
        } => AgentEvent::SubAgentFinished {
            id,
            nickname,
            ok,
            summary,
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
        EngineEvent::VerificationFinished { passed } => AgentEvent::VerificationFinished { passed },
        // Engine lifecycle & strategy events: persisted; engine-aware
        // consumers surface them directly.
        _ => return None,
    })
}

/// A failed gate is a completed turn with an incomplete outcome, not a runtime
/// crash. Full evidence has already been emitted through VerificationCheck;
/// the terminal marker only names the failed gates so the transcript remains
/// usable instead of dumping thousands of test-log characters in red.
fn report_to_result(report: TaskReport) -> Result<AgentOutcome, AppError> {
    let verification_failure = report.verification.as_ref().and_then(|verification| {
        (verification.verdict() == Verdict::Failed)
            .then(|| verification_failure_summary(verification))
    });
    // A run "did work" if it claimed completion or actually touched files. A
    // pure conversational answer did neither, so it carries no verified/
    // unverified verdict — it just ends as "answered".
    let did_work = report.stop_reason == StopReason::Completed || !report.modified_files.is_empty();
    let unverified_detail = if report.outcome == TaskOutcome::CompletedUnverified && did_work {
        if report.modified_files.is_empty() {
            // Stable token for TUI: "◇ 结束 · 未改仓库" (not "未验证" delivery).
            Some(leveler_client_protocol::REASON_NO_CODE_CHANGES.to_string())
        } else if let Some(verification) = &report.verification {
            if !verification.has_gating_checks() {
                // The project configured no gating checks, so there was nothing
                // to verify against. That is a calm "not auto-verified" finish,
                // not a warning about THIS task — route it to the same soft copy
                // as the no-verification-report case instead of leaking the raw
                // verifier string ("no gating verification checks were configured")
                // into the UI as a ⚠ warning.
                Some(leveler_client_protocol::REASON_NO_AUTOMATIC_VERIFICATION.to_string())
            } else {
                match verification.verdict() {
                    Verdict::Unverified(reason) => Some(reason),
                    _ => Some(unverified_acceptance_detail(
                        report.acceptance.as_ref(),
                        !report.modified_files.is_empty(),
                    )),
                }
            }
        } else {
            Some(leveler_client_protocol::REASON_NO_AUTOMATIC_VERIFICATION.to_string())
        }
    } else {
        None
    };
    let stop_reason = if verification_failure.is_some() {
        StopReason::Incomplete
    } else {
        match report.outcome {
            // leveler's own gate passed on real work: surface it as done + verified
            // even when the model merely "answered" instead of signalling
            // completion — a passing gate is stronger evidence than a self-claim.
            TaskOutcome::Verified if did_work => StopReason::Completed,
            // Real work finished, but no gate could confirm it.
            TaskOutcome::CompletedUnverified if did_work => StopReason::CompletedUnverified,
            // Pure Q&A, or any other terminal reason: keep the executor's reason.
            _ => report.stop_reason,
        }
    };
    Ok(AgentOutcome {
        final_text: report.final_text,
        rounds: report.rounds,
        modified_files: report.modified_files,
        stop_reason,
        stop_detail: verification_failure.or(unverified_detail),
        metrics: Default::default(),
        progress: Default::default(),
        objective: leveler_lifecycle::ObjectiveAnchor::from_user_message(""),
    })
}

pub(crate) fn app_error_from_engine(error: EngineError) -> AppError {
    match error {
        EngineError::Agent(e) => AppError::Agent(e),
        EngineError::Storage(e) => AppError::Storage(e),
        EngineError::Serde(e) => AppError::Serde(e.to_string()),
        EngineError::Planner(e) => AppError::Orchestrator(e),
        EngineError::Config(m) | EngineError::Corrupt(m) => AppError::Engine(m),
        EngineError::EventBufferOverloaded => {
            AppError::Engine("engine event buffer overloaded".to_string())
        }
        EngineError::RecoveryConfirmationRequired { call_id, tool } => AppError::Engine(format!(
            "crash recovery halted: an interrupted `{tool}` (call {call_id}) may already have \
             run; inspect the workspace, then resume with --confirm-recovery to acknowledge \
             and continue"
        )),
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
        // Local single-user CLI: clear zombie `running` turns left by a prior
        // process kill before starting a fresh interactive session.
        let reaped = leveler_engine::reap_running_turns(&db, None)
            .await
            .map_err(app_error_from_engine)?
            .len();
        if reaped > 0 {
            tracing::warn!(reaped, "reaped zombie running turns on session create");
        }
        self.insert_session(&db, model, goal).await
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
            repository: self.layout.repo_root.clone(),
            goal,
            mode,
            sandbox,
            kind: ExecutionKind::Direct,
            continuation: leveler_agent::ContinuationPolicy::UntilTerminal,
            limits: self.top_level_limits(),
            verification: crate::orchestrate::verification_plan_for_root(&self.layout.repo_root),
            base_commit: None,
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
        observer: &mut dyn FnMut(AgentEvent),
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
            observer,
            cancellation,
            leveler_agent::ContinuationPolicy::UntilTerminal,
            unattended_limits(self.top_level_limits()),
        )
        .await
    }

    /// Like [`Self::run_in_session`] but with an injectable clarifier (TUI waits;
    /// CLI may pass AutoClarify for unattended runs).
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
        observer: &mut dyn FnMut(AgentEvent),
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
        observer: &mut dyn FnMut(AgentEvent),
        cancellation: CancellationToken,
        max_rounds: u32,
    ) -> Result<AgentOutcome, AppError> {
        self.run_in_session_with_policy(
            session_id,
            model,
            mode,
            goal,
            approver,
            Arc::new(AutoClarify),
            sandbox,
            observer,
            cancellation,
            leveler_agent::ContinuationPolicy::bounded(max_rounds),
            leveler_agent::StepLimits::default(),
        )
        .await
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
        observer: &mut dyn FnMut(AgentEvent),
        cancellation: CancellationToken,
        continuation: leveler_agent::ContinuationPolicy,
        limits: leveler_agent::StepLimits,
    ) -> Result<AgentOutcome, AppError> {
        let db = self.open_database().await?;
        let repo = SessionRepository::new(&db);
        repo.update_status(
            session_id,
            SessionStatus::Running,
            AgentState::Execute,
            leveler_core::now(),
        )
        .await?;
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
        let record = repo.get(session_id).await?;
        let work_profile = record
            .as_ref()
            .map(|r| crate::axes_from_session_record(r).0)
            .unwrap_or_else(|| self.work_profile());
        let read_only = record.as_ref().is_some_and(|r| r.collaboration == "plan");

        let engine = self
            .engine_for_with_profile(
                model,
                mode,
                sandbox,
                approver,
                clarifier,
                work_profile,
                read_only,
            )
            .await?;
        let mut spec = self.direct_spec(goal.to_string(), mode, sandbox);
        spec.continuation = continuation;
        spec.limits = limits;
        let result = engine
            .run(
                session_id,
                &spec,
                &mut |event| forward_engine_event(event, observer),
                cancellation,
            )
            .await;
        let result = match result {
            Ok(report) => report_to_result(report),
            Err(error) => Err(app_error_from_engine(error)),
        };

        let (status, state) = status_for_app(&result);
        SessionRepository::new(&db)
            .update_status(session_id, status, state, leveler_core::now())
            .await?;
        result
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
        observer: &mut dyn FnMut(AgentEvent),
        cancellation: CancellationToken,
    ) -> Result<AgentOutcome, AppError> {
        let db = self.open_database().await?;
        let repo = SessionRepository::new(&db);
        repo.update_status(
            session_id,
            SessionStatus::Running,
            AgentState::Execute,
            leveler_core::now(),
        )
        .await?;
        repo.set_execution(
            session_id,
            mode_str(mode),
            sandbox,
            ExecutionKind::Direct.as_str(),
            leveler_core::now(),
        )
        .await?;

        let read_only = repo
            .get(session_id)
            .await?
            .is_some_and(|r| r.collaboration == "plan");
        let engine = self
            .engine_for_with_profile(
                model,
                mode,
                sandbox,
                approver,
                clarifier,
                self.work_profile(),
                read_only,
            )
            .await?;
        let spec = self.direct_spec(goal_from_content(&content), mode, sandbox);
        let result = engine
            .chat(
                session_id,
                &spec,
                content,
                &mut |event| forward_engine_event(event, observer),
                cancellation,
            )
            .await;
        let result = match result {
            Ok(report) => report_to_result(report),
            Err(error) => Err(app_error_from_engine(error)),
        };

        let (status, state) = status_for_app(&result);
        SessionRepository::new(&db)
            .update_status(session_id, status, state, leveler_core::now())
            .await?;
        result
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
        leveler_engine::acknowledge_crash_window(&db, session_id)
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
        observer: &mut dyn FnMut(AgentEvent),
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

        repo.update_status(
            session_id,
            SessionStatus::Running,
            AgentState::Execute,
            leveler_core::now(),
        )
        .await?;

        let engine = self
            .engine_for_with_profile(
                &model,
                mode,
                sandbox,
                approver,
                Arc::new(AutoClarify),
                work_profile,
                read_only,
            )
            .await?;
        let mut spec = self.direct_spec(record.goal.clone(), mode, sandbox);
        // Resume with the persisted strategy, not an assumed one.
        spec.kind = kind;
        let result = engine
            .resume(
                session_id,
                &spec,
                &mut |event| forward_engine_event(event, observer),
                cancellation,
            )
            .await;
        let result = match result {
            Ok(report) => report_to_result(report),
            Err(error) => Err(app_error_from_engine(error)),
        };

        let (status, state) = status_for_app(&result);
        SessionRepository::new(&db)
            .update_status(session_id, status, state, leveler_core::now())
            .await?;
        result
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
        TaskReport {
            outcome,
            final_text: String::new(),
            modified_files: modified.iter().map(|s| s.to_string()).collect(),
            verification: None,
            stop_reason,
            rounds: 1,
            review: None,
            acceptance: None,
        }
    }

    #[test]
    fn completed_unverified_work_is_not_reported_as_incomplete() {
        // A finished run that did work but leveler could not independently
        // verify must stay distinct from a genuinely-incomplete run
        // (Stalled/audit-failed), so the UI says "done, unverified" not "not
        // completed".
        let out = report_to_result(report(
            TaskOutcome::CompletedUnverified,
            StopReason::Completed,
            &["README.md"],
        ))
        .unwrap();
        assert_eq!(out.stop_reason, StopReason::CompletedUnverified);
    }

    #[test]
    fn completed_without_tracked_changes_explains_why_checks_did_not_run() {
        let out = report_to_result(report(
            TaskOutcome::CompletedUnverified,
            StopReason::Completed,
            &[],
        ))
        .unwrap();

        assert_eq!(out.stop_reason, StopReason::CompletedUnverified);
        assert_eq!(
            out.stop_detail.as_deref(),
            Some(leveler_client_protocol::REASON_NO_CODE_CHANGES)
        );
    }

    #[test]
    fn completed_changes_without_a_plan_explain_that_no_gate_was_found() {
        let out = report_to_result(report(
            TaskOutcome::CompletedUnverified,
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
    fn unverified_with_green_health_names_missing_acceptance_not_only_catchall() {
        use leveler_verifier::{
            AcceptanceEvidence, AcceptanceLedger, AcceptanceStatus, CheckKind, CheckOutcome,
            CheckStatus, VerificationReport,
        };
        let mut task = report(
            TaskOutcome::CompletedUnverified,
            StopReason::Completed,
            &["src/lib.rs"],
        );
        task.verification = Some(VerificationReport {
            checks: vec![CheckOutcome {
                name: "ok".into(),
                kind: CheckKind::Test,
                gating: true,
                status: CheckStatus::Passed,
                evidence: String::new(),
                failure: None,
                failed_tests: std::collections::BTreeSet::new(),
            }],
            scope_ok: true,
            scope_violations: Vec::new(),
            baseline_failures: Vec::new(),
        });
        task.acceptance = Some(AcceptanceLedger {
            items: vec![AcceptanceEvidence {
                id: "AC-1".into(),
                description: "fallback".into(),
                required: false,
                status: AcceptanceStatus::Unverifiable,
                command: None,
                evidence: String::new(),
                reject_reason: Some("no_command".into()),
            }],
        });
        let out = report_to_result(task).unwrap();
        assert_eq!(out.stop_reason, StopReason::CompletedUnverified);
        assert_eq!(
            out.stop_detail.as_deref(),
            Some("有改动但缺少系统级验收背书"),
            "must not use only the catch-all phrase"
        );
    }

    #[test]
    fn unverified_with_required_unmet_names_the_criterion_id() {
        use leveler_verifier::{
            AcceptanceEvidence, AcceptanceLedger, AcceptanceStatus, CheckKind, CheckOutcome,
            CheckStatus, VerificationReport,
        };
        let mut task = report(
            TaskOutcome::CompletedUnverified,
            StopReason::Completed,
            &["src/lib.rs"],
        );
        task.verification = Some(VerificationReport {
            checks: vec![CheckOutcome {
                name: "ok".into(),
                kind: CheckKind::Test,
                gating: true,
                status: CheckStatus::Passed,
                evidence: String::new(),
                failure: None,
                failed_tests: std::collections::BTreeSet::new(),
            }],
            scope_ok: true,
            scope_violations: Vec::new(),
            baseline_failures: Vec::new(),
        });
        task.acceptance = Some(AcceptanceLedger {
            items: vec![AcceptanceEvidence {
                id: "AC-1".into(),
                description: "must pass".into(),
                required: true,
                status: AcceptanceStatus::Unmet,
                command: Some("false".into()),
                evidence: "exit 1".into(),
                reject_reason: None,
            }],
        });
        let out = report_to_result(task).unwrap();
        // Surface the check command + its failure, not just the bare id, so the
        // user can tell what was run and why it failed.
        assert_eq!(
            out.stop_detail.as_deref(),
            Some("验收未通过：AC-1 · 检查「false」：exit 1")
        );
    }

    #[test]
    fn unverified_detail_prefers_reject_reason_over_evidence() {
        use leveler_verifier::{
            AcceptanceEvidence, AcceptanceLedger, AcceptanceStatus, CheckKind, CheckOutcome,
            CheckStatus, VerificationReport,
        };
        let mut task = report(
            TaskOutcome::CompletedUnverified,
            StopReason::Completed,
            &["src/lib.rs"],
        );
        task.verification = Some(VerificationReport {
            checks: vec![CheckOutcome {
                name: "ok".into(),
                kind: CheckKind::Test,
                gating: true,
                status: CheckStatus::Passed,
                evidence: String::new(),
                failure: None,
                failed_tests: std::collections::BTreeSet::new(),
            }],
            scope_ok: true,
            scope_violations: Vec::new(),
            baseline_failures: Vec::new(),
        });
        task.acceptance = Some(AcceptanceLedger {
            items: vec![AcceptanceEvidence {
                id: "AC-2".into(),
                description: "endpoint returns 200".into(),
                required: true,
                status: AcceptanceStatus::Unmet,
                command: Some("curl -sf localhost:8080/health".into()),
                evidence: "long stdout\nsecond line".into(),
                reject_reason: Some("connection refused".into()),
            }],
        });
        let out = report_to_result(task).unwrap();
        assert_eq!(
            out.stop_detail.as_deref(),
            Some("验收未通过：AC-2 · 检查「curl -sf localhost:8080/health」：connection refused")
        );
    }

    #[test]
    fn completed_changes_preserve_the_tool_missing_reason() {
        let mut task = report(
            TaskOutcome::CompletedUnverified,
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
            TaskOutcome::CompletedUnverified,
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

    #[test]
    fn incomplete_marker_names_the_failing_tests() {
        // The terminal marker must carry the parsed failing test ids, not just
        // the check name — "failed gate(s): cargo test" right after the agent
        // said "all green" reads as a contradiction with zero evidence.
        use leveler_verifier::{CheckKind, CheckOutcome, CheckStatus, VerificationReport};
        let mut task = report(TaskOutcome::Failed, StopReason::Completed, &["src/lib.rs"]);
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

        assert_eq!(out.stop_reason, StopReason::Incomplete);
        assert_eq!(
            out.stop_detail.as_deref(),
            Some(
                "failed gate(s): cargo test \
                 (permission_grants::always_allow_grants_survive_reassembly)"
            )
        );
    }

    #[test]
    fn incomplete_marker_caps_the_test_list_and_keeps_the_count() {
        // Many failing tests must not flood the one-line marker: show the
        // first two ids and the size of the remainder.
        use leveler_verifier::{CheckKind, CheckOutcome, CheckStatus, VerificationReport};
        let mut task = report(TaskOutcome::Failed, StopReason::Completed, &["src/lib.rs"]);
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
        let out = report_to_result(report(
            TaskOutcome::CompletedUnverified,
            StopReason::Answered,
            &[],
        ))
        .unwrap();
        assert_eq!(out.stop_reason, StopReason::Answered);
    }

    #[test]
    fn verified_work_is_reported_completed_even_when_model_only_answered() {
        // leveler's gate passed on real edits, but the model ended with prose
        // instead of update_goal(complete). The passing gate is authoritative:
        // surface it as done, not a bare "answered".
        let out = report_to_result(report(
            TaskOutcome::Verified,
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
        let out =
            report_to_result(report(TaskOutcome::Verified, StopReason::Answered, &[])).unwrap();
        assert_eq!(out.stop_reason, StopReason::Answered);
    }
}
