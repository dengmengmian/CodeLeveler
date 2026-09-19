//! Session orchestration: run, chat and resume through the task engine.
//!
//! Every path delegates to `leveler-engine`'s [`TaskEngine`], so turns, tool
//! calls, approvals and verification results are persisted before observers
//! see them, and an interrupted run resumes from its exact transcript. The
//! `AgentEvent` observer signature is kept as a temporary shim until the UIs
//! consume `EngineEvent` directly (plan B6).

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use tokio_util::sync::CancellationToken;

use leveler_agent::coding::{TaskReport, TaskSpec};
use leveler_agent::{AdvisoryKind, AgentEvent, AgentOutcome, AutoClarify, Clarifier, StopReason};
use leveler_engine::{EngineError, EngineEvent, ExecutionKind, TaskOutcome};
use leveler_execution::{Approver, PermissionProfile};
use leveler_model::{ContentPart, ModelRef};
use leveler_storage::SessionRepository;
use leveler_verifier::{CheckStatus, Verdict, VerificationReport};

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
            exit_code,
            stop,
        } => AgentEvent::ToolResult {
            id: call_id,
            name,
            is_error,
            preview,
            applied_diff,
            exit_code,
            stop,
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
        EngineEvent::ModelRetrying {
            attempt,
            max_attempts,
            delay_ms,
        } => AgentEvent::ModelRetrying {
            attempt,
            max_attempts,
            delay_ms,
        },
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
            spec,
        } => AgentEvent::SubAgentStarted {
            id,
            nickname,
            role,
            task,
            profile_id,
            profile_role,
            read_only,
            spec,
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
            outcome,
            stop,
            limit,
        } => AgentEvent::SubAgentFinished {
            id,
            nickname,
            ok,
            summary,
            contribution,
            outcome,
            stop,
            limit,
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
            ..
        } => AgentEvent::VerificationCheck {
            name,
            // Parsed by the vocabulary's owner. A spelling this build cannot
            // read is not renderable as a check, and filing it under `Skipped`
            // would invent the reason it did not run.
            status: CheckStatus::from_wire(&status)?,
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
        EngineError::UnclosedTerminalBoundary(m) => AppError::UnclosedTerminalBoundary(m),
        EngineError::TerminalCommitFailed(m) => AppError::TerminalCommitFailed(m),
        // Pass the diagnostic through verbatim rather than flattening it back
        // to a bare sentence — the whole point of carrying the event type, the
        // producing agent and the capacity is that they reach the user.
        error @ (EngineError::EventBufferOverloaded { .. }
        | EngineError::DuplicateChildSettlement { .. }) => AppError::Engine(error.to_string()),
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
        EngineError::OwnedByLiveBoot { .. } => AppError::SessionRunningElsewhere,
        EngineError::OwnershipUnknown { .. } => AppError::SessionOwnershipUnknown,
    }
}

pub(crate) fn mode_from_str(s: &str) -> Option<PermissionProfile> {
    // parse() covers current wire values and the legacy 0003 names ("plan",
    // "workspace_write") still present as SQLite column DEFAULTs.
    PermissionProfile::parse(s)
}

/// Coding's recovery facts for reaped sessions: an interrupted goal
/// checkpoint under each recovery token, which then ends. The checkpoint is
/// cut before the release so it cannot absorb the next execution.
pub(crate) async fn checkpoint_reaped_sessions(
    engine: &leveler_engine::TaskEngine,
    reaped_sessions: &[leveler_engine::ReapedSession],
) {
    let stores = &engine.stores;
    for reaped in reaped_sessions {
        match leveler_agent::coding::create_goal_checkpoint(
            engine,
            &reaped.session_id,
            leveler_lifecycle::CheckpointReason::Interrupted,
            None,
            None,
        )
        .await
        {
            Ok(Some(record)) => {
                let log = leveler_engine::EventLog::new_owned(
                    stores.events.as_ref(),
                    reaped.session_id.clone(),
                    reaped.token.clone(),
                );
                let event = leveler_agent::coding::checkpoint_created_event(&record);
                if let Err(error) = log.append(None, event, &mut |_| {}).await {
                    tracing::warn!(
                        %error,
                        session = %reaped.session_id,
                        "interrupted checkpoint persisted but its announcement failed"
                    );
                }
            }
            Ok(None) => {}
            Err(error) => tracing::warn!(
                %error,
                session = %reaped.session_id,
                "interrupted goal checkpoint failed; the reap stands"
            ),
        }
    }
    leveler_engine::release_reaped(engine, reaped_sessions).await;
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

    /// Clear the zombie `running` turns dead boots left behind, optionally
    /// scoped to one session. Returns how many were reaped.
    ///
    /// A process that is starting up — or creating or reopening a session —
    /// does this: a turn whose boot was killed is not running any more, and a
    /// row that still says it is becomes a live spinner over dead work the
    /// next time anyone opens the session. Only a boot proven dead gives this
    /// authority, which is what makes it safe from any host on a repository
    /// other processes share: a live boot's turn — this process's own
    /// included — and a turn whose boot cannot be probed are left alone.
    pub async fn reap_zombie_turns(
        &self,
        db: &leveler_storage::Database,
        session: Option<&leveler_core::SessionId>,
    ) -> Result<usize, AppError> {
        let engine = self.task_engine(db)?;
        let outcome = leveler_engine::reap_after_restart(
            &engine,
            session,
            leveler_engine::ReapScope::EndedBoots,
        )
        .await
        .map_err(app_error_from_engine)?;
        checkpoint_reaped_sessions(&engine, &outcome.reaped_sessions).await;
        for conflict in &outcome.conflicts {
            tracing::warn!(
                session = conflict.session_id.as_str(),
                refusal = ?conflict.refusal,
                "not reaping running turns without proof their boot has ended"
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
        self.task_engine(db)?
            .create_task(&leveler_engine::NewSession {
                workspace: self.layout.repo_root.display().to_string(),
                goal: goal.to_string(),
                model: model.to_string(),
                mode: PermissionProfile::Assisted.as_str().to_string(),
                sandbox: false,
                kind: ExecutionKind::Direct,
                axes: Some(leveler_engine::NewSessionAxes {
                    collaboration: self.collaboration().as_str().to_string(),
                    work_profile: self.work_profile().as_str().to_string(),
                }),
            })
            .await
            .map_err(app_error_from_engine)
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
            // Headless: nobody is at a keyboard to explicitly cancel a task.
            Arc::new(AtomicBool::new(false)),
            // Legacy AgentEvent observer (headless CLI renderer): adapt from
            // the canonical stream one-way.
            &mut |event| forward_engine_event(event, observer),
            cancellation,
            // Headless goals run under the task round limit when one is set;
            // pinning UntilTerminal over it was the exp8 null result.
            crate::goal_continuation_for(self.task_round_limit),
            unattended_limits(self.top_level_limits()),
            false,
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
        // Set when the user cancels the logical task rather than interrupting.
        task_cancel: Arc<AtomicBool>,
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
            task_cancel,
            observer,
            cancellation,
            leveler_agent::ContinuationPolicy::UntilTerminal,
            self.top_level_limits(),
            false,
        )
        .await
    }

    /// Run one goal through the Develop workflow instead of straight to
    /// Coding. Same session, same axes, same terminal authority as
    /// [`Self::run_in_session_with_clarifier`]; only the harness differs.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_develop_in_session(
        &self,
        session_id: &leveler_core::SessionId,
        model: &ModelRef,
        mode: PermissionProfile,
        goal: &str,
        approver: Arc<dyn Approver>,
        clarifier: Arc<dyn Clarifier>,
        sandbox: bool,
        steering: Option<Arc<dyn leveler_agent::SteeringSource>>,
        task_cancel: Arc<AtomicBool>,
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
            task_cancel,
            observer,
            cancellation,
            leveler_agent::ContinuationPolicy::UntilTerminal,
            self.top_level_limits(),
            true,
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
            // Eval runs never carry an interactive task cancel.
            Arc::new(AtomicBool::new(false)),
            // Legacy AgentEvent observer (eval collectors): adapt one-way.
            &mut |event| forward_engine_event(event, observer),
            cancellation,
            leveler_agent::ContinuationPolicy::bounded(max_rounds),
            leveler_agent::StepLimits::default(),
            false,
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
        task_cancel: Arc<AtomicBool>,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
        cancellation: CancellationToken,
        continuation: leveler_agent::ContinuationPolicy,
        limits: leveler_agent::StepLimits,
        // `/develop` only. An ordinary turn passes false and reaches exactly
        // the code it reached before this flag existed.
        develop: bool,
    ) -> Result<AgentOutcome, AppError> {
        let db = self.open_database().await?;
        let repo = SessionRepository::new(&db);
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
            .with_steering(steering)
            .with_task_cancel(task_cancel);
        // Candidate extraction moved to the caller that owns a client
        // connection (`InteractiveRuntime`), because a pending candidate nobody
        // is told about is the same as none. Doing it here could only log.
        let _ = ();
        let mut spec = self.direct_spec(goal.to_string(), mode, sandbox);
        spec.runtime.continuation = continuation;
        spec.runtime.limits = limits;
        let result = if develop {
            engine
                .run_develop(session_id, &spec, observer, cancellation)
                .await
        } else {
            engine.run(session_id, &spec, observer, cancellation).await
        };
        match result {
            Ok(report) => report_to_result(report),
            Err(error) => Err(app_error_from_engine(error)),
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
        // Mid-turn user text and per-child cancel handles. The chat turn is
        // the one TUI, Web and phone submit, so without it neither a steer
        // nor a "stop this child" can reach the running turn.
        steering: Option<Arc<dyn leveler_agent::SteeringSource>>,
        task_cancel: Arc<AtomicBool>,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
        cancellation: CancellationToken,
    ) -> Result<AgentOutcome, AppError> {
        let db = self.open_database().await?;
        let repo = SessionRepository::new(&db);
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
            .with_steering(steering)
            .with_task_cancel(task_cancel);
        let goal = goal_from_content(&content);
        // See the note in `run_in_session_with_policy`: the notice belongs to
        // the layer with a client to notify.
        let _ = ();
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
        // Canonical recovery write ⇒ ownership-fenced. The TaskEngine is the
        // single authority that resolves legacy task rows, refuses a foreign
        // owner, and advances the fencing epoch.
        let engine = self.task_engine(&db)?;
        let token = engine
            .acquire_ownership(session_id)
            .await
            .map_err(app_error_from_engine)?;
        let closed = leveler_engine::acknowledge_crash_window(&db, &token, session_id).await;
        // Acknowledging starts no execution: whatever runs next acquires anew.
        engine
            .release_ownership(&token)
            .await
            .map_err(app_error_from_engine)?;
        closed.map_err(app_error_from_engine)
    }

    /// Cancel the logical task behind `session_id` when no turn is running.
    ///
    /// Commits a terminal `cancelled` outcome (settling any running goal), so
    /// the task is no longer resumable and a later `继续` cannot silently
    /// reopen it. Returns the terminal event for the caller to publish, or
    /// `None` when there is no task or it was already cancelled (idempotent).
    pub async fn cancel_task(
        &self,
        session_id: &leveler_core::SessionId,
    ) -> Result<Option<EngineEvent>, AppError> {
        use leveler_engine::TaskTerminal;
        use leveler_lifecycle::{AgentState, SessionStatus, VerificationStatus};

        let db = self.open_database().await?;
        let engine = self.task_engine(&db)?;
        let Some(task) = engine.stores.tasks.task_for_session(session_id).await? else {
            return Ok(None);
        };
        // Idempotent: an already-cancelled task writes nothing more.
        if let Some(row) = engine
            .stores
            .events
            .load_last_by_type(session_id, "task_finished", None)
            .await?
            && let Ok(EngineEvent::TaskFinished { outcome, .. }) =
                EngineEvent::from_payload(&row.payload)
            && outcome == TaskOutcome::Cancelled
        {
            return Ok(None);
        }
        let token = engine
            .acquire_ownership(session_id)
            .await
            .map_err(app_error_from_engine)?;
        // The logical task is what ends here: any goal still owing work is
        // settled, not left running for a future continuation to pick up.
        let goal = engine
            .stores
            .goals
            .for_task(&task)
            .await?
            .into_iter()
            .find(|goal| goal.state == leveler_storage::GoalState::Running)
            .map(|goal| leveler_storage::GoalTerminalUpdate {
                goal_id: goal.id,
                // Cancelling is not a work window.
                windows_delta: 0,
                settle: true,
            });
        let mut events = Vec::new();
        engine
            .finish_task(
                &token,
                session_id,
                TaskTerminal {
                    outcome: TaskOutcome::Cancelled,
                    verification: VerificationStatus::NotRun,
                    reason: Some("user_cancelled_task".to_string()),
                    failure: None,
                    stop: None,
                    status: SessionStatus::Cancelled,
                    state: AgentState::Cancelled,
                    goal,
                    warnings: Vec::new(),
                },
                &mut |event| events.push(event),
            )
            .await
            .map_err(app_error_from_engine)?;
        Ok(events.into_iter().next())
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
        let (engine, spec) = self
            .resume_prepare(
                session_id,
                approver,
                Arc::new(AutoClarify),
                Arc::new(AtomicBool::new(false)),
            )
            .await?;
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

    /// Continue an interrupted session with the user's continuation message.
    ///
    /// Same persisted execution config, product axes and goal identity as
    /// [`Self::resume_session`]; the difference is that this path is driveable
    /// from a live client (its clarifier and mid-turn steering), and the
    /// continuation message is recorded as the turn's own user input.
    #[allow(clippy::too_many_arguments)]
    pub async fn resume_in_session_with_instruction(
        &self,
        session_id: &leveler_core::SessionId,
        instruction: leveler_model::Message,
        approver: Arc<dyn Approver>,
        clarifier: Arc<dyn Clarifier>,
        steering: Option<Arc<dyn leveler_agent::SteeringSource>>,
        task_cancel: Arc<AtomicBool>,
        observer: &mut (dyn FnMut(leveler_engine::EngineEvent) + Send),
        cancellation: CancellationToken,
    ) -> Result<AgentOutcome, AppError> {
        let (engine, spec) = self
            .resume_prepare(session_id, approver, clarifier, task_cancel)
            .await?;
        let engine = engine.with_steering(steering);
        let result = engine
            .resume_with_instruction(session_id, &spec, instruction, observer, cancellation)
            .await;
        match result {
            Ok(report) => report_to_result(report),
            Err(error) => Err(app_error_from_engine(error)),
        }
    }

    /// Build the engine and spec a resume runs under, from the persisted
    /// session row — never from caller-supplied config.
    async fn resume_prepare(
        &self,
        session_id: &leveler_core::SessionId,
        approver: Arc<dyn Approver>,
        clarifier: Arc<dyn Clarifier>,
        task_cancel: Arc<AtomicBool>,
    ) -> Result<(leveler_agent::coding::CodingRuntime, TaskSpec), AppError> {
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
                clarifier,
                work_profile,
                read_only,
                Some(session_id.as_str()),
            )
            .await?
            .with_task_cancel(task_cancel);
        let mut spec = self.direct_spec(record.goal.clone(), mode, sandbox);
        // Resume with the persisted strategy, not an assumed one.
        spec.runtime.kind = kind;
        Ok((engine, spec))
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
            execution_duration_ms: 0,
            review: None,
            completion_warnings: Vec::new(),
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

    /// §13 C: the durable spelling reaches the harness event as the status it
    /// means, in the canonical spelling and in the older one.
    #[test]
    fn a_durable_check_status_projects_to_the_status_it_means() {
        for (durable, expected) in [
            ("passed", leveler_verifier::CheckStatus::Passed),
            ("failed", leveler_verifier::CheckStatus::Failed),
            ("skipped", leveler_verifier::CheckStatus::Skipped),
            ("tool_missing", leveler_verifier::CheckStatus::ToolMissing),
            ("toolmissing", leveler_verifier::CheckStatus::ToolMissing),
            (
                "environment_unavailable",
                leveler_verifier::CheckStatus::EnvironmentUnavailable,
            ),
            (
                "environmentunavailable",
                leveler_verifier::CheckStatus::EnvironmentUnavailable,
            ),
        ] {
            let projected = engine_event_to_agent(EngineEvent::VerificationCheck {
                name: "test".to_string(),
                status: durable.to_string(),
                observation: None,
                disposition: None,
                execution: None,
                evidence: None,
            });
            match projected {
                Some(AgentEvent::VerificationCheck { status, .. }) => {
                    assert_eq!(status, expected, "{durable}")
                }
                other => panic!("{durable}: expected a check event, got {other:?}"),
            }
        }
    }

    /// A spelling this build cannot read is dropped rather than filed as a
    /// skip: the CLI cannot render a status it does not know, and calling it
    /// `Skipped` would invent the reason it did not run.
    #[test]
    fn a_check_status_this_build_cannot_read_is_not_projected_as_a_skip() {
        let projected = engine_event_to_agent(EngineEvent::VerificationCheck {
            name: "test".to_string(),
            status: "something-else".to_string(),
            observation: None,
            disposition: None,
            execution: None,
            evidence: None,
        });
        assert!(projected.is_none(), "{projected:?}");
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
                observation: leveler_verifier::CheckObservation::NotRun(
                    leveler_verifier::NotRunReason::ToolMissing,
                ),
                disposition: leveler_verifier::GateDisposition::Required,
                execution: None,
                evidence: String::new(),
                failure: None,
                failed_tests: std::collections::BTreeSet::new(),
            }],
            scope_ok: true,
            scope_violations: Vec::new(),
        });

        let out = report_to_result(task).unwrap();

        assert_eq!(
            out.stop_detail.as_deref(),
            Some("verification incomplete: tsc (tool missing)")
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
        use leveler_verifier::{
            CheckExecution, CheckKind, CheckObservation, CheckOutcome, GateDisposition,
            VerificationReport,
        };
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
                observation: CheckObservation::Failed,
                disposition: GateDisposition::Required,
                execution: Some(CheckExecution {
                    program: "cargo".into(),
                    args: vec!["test".into()],
                    exit_code: Some(1),
                    timed_out: false,
                }),
                evidence: String::new(),
                failure: None,
                failed_tests: ["permission_grants::always_allow_grants_survive_reassembly"]
                    .into_iter()
                    .map(String::from)
                    .collect(),
            }],
            scope_ok: true,
            scope_violations: Vec::new(),
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
        use leveler_verifier::{
            CheckExecution, CheckKind, CheckObservation, CheckOutcome, GateDisposition,
            VerificationReport,
        };
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
                observation: CheckObservation::Failed,
                disposition: GateDisposition::Required,
                execution: Some(CheckExecution {
                    program: "cargo".into(),
                    args: vec!["test".into()],
                    exit_code: Some(1),
                    timed_out: false,
                }),
                evidence: String::new(),
                failure: None,
                failed_tests: ["a::one", "b::two", "c::three", "d::four"]
                    .into_iter()
                    .map(String::from)
                    .collect(),
            }],
            scope_ok: true,
            scope_violations: Vec::new(),
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
        let app = isolated_app(&tmp, WorkProfile::Economy);
        let db = app.open_database().await.unwrap();
        let repo = SessionRepository::new(&db);
        let (work_profile, read_only) = app
            .turn_axes(&repo, &leveler_core::SessionId::new("no-such-session"))
            .await
            .unwrap();
        assert_eq!(work_profile, WorkProfile::Economy);
        assert!(!read_only);
    }
}
