//! Session orchestration: run, chat and resume through the task engine.
//!
//! Every path delegates to `leveler-engine`'s [`TaskEngine`], so turns, tool
//! calls and approvals are persisted before observers
//! see them, and an interrupted run resumes from its exact transcript. The
//! `AgentEvent` observer signature is kept as a temporary shim until the UIs
//! consume `EngineEvent` directly (plan B6).

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use tokio_util::sync::CancellationToken;

use leveler_agent::CollaborationMode;
use leveler_agent::coding::{CodingRuntime, TaskReport, TaskSpec};
use leveler_agent::{AdvisoryKind, AgentEvent, AgentOutcome, AutoClarify, Clarifier};
use leveler_engine::{EngineError, EngineEvent, ExecutionKind, TaskOutcome};
use leveler_execution::{Approver, PermissionProfile};
use leveler_model::{ContentPart, ModelRef};
use leveler_storage::SessionRepository;

use crate::{AppError, Application, CollaborationExecution};

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
            model_step,
            ..
        } => AgentEvent::ToolCall {
            id: call_id,
            name,
            arguments,
            parallel,
            model_step,
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
            // The durable engine event does not carry the execution status; the
            // reverse projection stays honest with `None` rather than inventing
            // a status the log never recorded.
            execution_status: None,
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
            reasoning_tokens,
        } => AgentEvent::Usage {
            input_tokens,
            output_tokens,
            cached_input_tokens,
            reasoning_tokens,
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
        EngineEvent::MemoryRecalled { count, ids } => AgentEvent::MemoryRecalled {
            count: count as usize,
            ids,
        },
        EngineEvent::MemoryChanged {
            operation,
            id,
            title,
            authority,
        } => AgentEvent::MemoryChanged {
            operation,
            id,
            title,
            authority,
        },
        EngineEvent::PlanUpdated { steps } => AgentEvent::PlanUpdated { steps },
        EngineEvent::GoalIntercepted { kind, detail } => {
            AgentEvent::GoalIntercepted { kind, detail }
        }
        EngineEvent::RuntimeInjection {
            kind,
            role,
            model_step,
            forces_continuation,
        } => AgentEvent::RuntimeInjection {
            kind,
            role,
            model_step,
            forces_continuation,
        },
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
        // Engine lifecycle & strategy events: persisted; engine-aware
        // consumers surface them directly.
        _ => return None,
    })
}

/// Map the engine's terminal report onto the app-level result.
fn report_to_result(report: TaskReport) -> Result<AgentOutcome, AppError> {
    Ok(AgentOutcome {
        final_text: report.final_text,
        model_steps: report.model_steps,
        modified_files: report.modified_files,
        stop_reason: report.stop_reason,
        stop_detail: report.stop_detail,
        budget_exhaustion: None,
        progress: Default::default(),
        objective: leveler_lifecycle::ObjectiveAnchor::from_user_message(""),
    })
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
        // A context that cannot fit the model's hard capacity is a harness
        // decision with a machine-readable cause; keep the "context management
        // failure" label rather than flattening it into a bare sentence.
        error @ EngineError::ContextManagementFailure(_) => AppError::Engine(error.to_string()),
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
        let scope = match leveler_agent::coding::latest_session_goal_checkpoint_scope(
            stores,
            &reaped.session_id,
        )
        .await
        {
            Ok(Some(scope)) => scope,
            Ok(None) => continue,
            Err(error) => {
                tracing::warn!(
                    %error,
                    session = %reaped.session_id,
                    "could not resolve exact lineage for interrupted checkpoint"
                );
                continue;
            }
        };
        match leveler_agent::coding::create_goal_checkpoint(
            engine,
            &reaped.session_id,
            &scope,
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

/// Run one turn under a resolved collaboration execution.
///
/// The only place that turns an axis into an engine entry point, so the
/// `run_in_session*` variants cannot disagree about chat/plan/goal. Content is
/// always passed through: a Goal turn with attachments must keep them, and the
/// headless goal path builds the same single text part `run` would.
#[allow(clippy::too_many_arguments)]
async fn run_collaboration_turn(
    engine: &CodingRuntime,
    execution: CollaborationExecution,
    session_id: &leveler_core::SessionId,
    spec: &TaskSpec,
    content: Vec<ContentPart>,
    // `/develop` is its own workflow command; it keeps its own harness and is
    // not a collaboration axis.
    develop: bool,
    observer: &mut (dyn FnMut(EngineEvent) + Send),
    cancellation: CancellationToken,
) -> Result<TaskReport, EngineError> {
    if develop {
        return engine
            .run_develop(session_id, spec, observer, cancellation)
            .await;
    }
    if execution.runs_goal_lifecycle() {
        engine
            .run_with_content(session_id, spec, content, observer, cancellation)
            .await
    } else {
        engine
            .chat(session_id, spec, content, observer, cancellation)
            .await
    }
}

impl Application {
    /// Create and persist a new session record, returning its id. The caller can
    /// then run it, and — crucially — knows the id even if the run is cancelled.
    ///
    /// This is the built-in default profile. Callers that resolved a profile
    /// (CLI `--permission`, a project default) use
    /// [`Self::create_session_with_mode`] so the row they persist is the same
    /// mode they will run under.
    pub async fn create_session(
        &self,
        model: &ModelRef,
        goal: &str,
    ) -> Result<leveler_core::SessionId, AppError> {
        self.create_session_with_mode(model, goal, PermissionProfile::Assisted)
            .await
    }

    /// Create and persist a session whose durable `mode` IS `mode`.
    ///
    /// The permission profile has exactly one authoritative create-time write:
    /// here. The running engine reads the same resolved value, so a created
    /// session can never be `Full` in memory and `assisted` in the database.
    ///
    /// The collaboration axis comes from [`Self::collaboration`], which is the
    /// headless entry's contract (`leveler run`): the axis this Application was
    /// assembled with. An entry whose axis is fixed by the product — the
    /// interactive terminal session — states it through
    /// [`Self::create_session_with_collaboration`] instead, so its row never
    /// depends on what the process default happens to be.
    pub async fn create_session_with_mode(
        &self,
        model: &ModelRef,
        goal: &str,
        mode: PermissionProfile,
    ) -> Result<leveler_core::SessionId, AppError> {
        self.create_session_with_collaboration(model, goal, mode, self.collaboration())
            .await
    }

    /// Create and persist a session whose durable collaboration axis IS
    /// `collaboration`, independent of this Application's own default.
    ///
    /// The axis has exactly one authoritative create-time write: here, through
    /// the same [`Self::insert_session`] every other create path uses. A caller
    /// that owns a fixed product axis writes that value; it must not read the
    /// process default, or the entry point silently changes meaning whenever
    /// the default moves.
    pub async fn create_session_with_collaboration(
        &self,
        model: &ModelRef,
        goal: &str,
        mode: PermissionProfile,
        collaboration: CollaborationMode,
    ) -> Result<leveler_core::SessionId, AppError> {
        let db = self.open_database().await?;
        self.reap_zombie_turns(&db, None).await?;
        self.insert_session(&db, model, goal, mode, collaboration)
            .await
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
    ///
    /// `collaboration` is resolved by the caller (the transport request, or the
    /// process default) so the durable row and the running config are written
    /// from one decision.
    pub(crate) async fn create_daemon_session(
        &self,
        model: &ModelRef,
        goal: &str,
        mode: PermissionProfile,
        collaboration: leveler_lifecycle::CollaborationMode,
    ) -> Result<leveler_core::SessionId, AppError> {
        let db = self.open_database().await?;
        self.insert_session(&db, model, goal, mode, collaboration)
            .await
    }

    async fn insert_session(
        &self,
        db: &leveler_storage::Database,
        model: &ModelRef,
        goal: &str,
        mode: PermissionProfile,
        collaboration: leveler_lifecycle::CollaborationMode,
    ) -> Result<leveler_core::SessionId, AppError> {
        self.task_engine(db)?
            .create_task(&leveler_engine::NewSession {
                workspace: self
                    .layout
                    .primary_workspace()
                    .map(|root| root.display().to_string()),
                goal: goal.to_string(),
                model: model.to_string(),
                mode: mode.as_str().to_string(),
                sandbox: false,
                kind: ExecutionKind::Direct,
                axes: Some(leveler_engine::NewSessionAxes {
                    collaboration: collaboration.as_str().to_string(),
                }),
            })
            .await
            .map_err(app_error_from_engine)
    }

    /// The durable permission mode of an existing session, if it has a row.
    ///
    /// The read half of the launch resolution: a resume turns the persisted
    /// mode into the fallback an explicit CLI override may replace.
    pub async fn persisted_permission_profile(
        &self,
        session_id: &leveler_core::SessionId,
    ) -> Result<Option<PermissionProfile>, AppError> {
        let db = self.open_database().await?;
        let Some((mode, _, _, _)) = SessionRepository::new(&db).execution(session_id).await? else {
            return Ok(None);
        };
        Ok(mode_from_str(&mode))
    }

    /// Persist an explicit permission-mode choice for an existing session.
    ///
    /// Used by a headless resume (`leveler run --resume <id> --permission …`),
    /// which has no live client to route the change through. Interactive
    /// surfaces go through `SetPermissionProfile`, which persists the same
    /// value and also updates any running execution; this is the same single
    /// durable write for a session with nothing running yet.
    pub async fn set_persisted_permission_profile(
        &self,
        session_id: &leveler_core::SessionId,
        mode: PermissionProfile,
    ) -> Result<(), AppError> {
        let db = self.open_database().await?;
        let sessions = SessionRepository::new(&db);
        let (_, sandbox, kind, _) = sessions
            .execution(session_id)
            .await?
            .ok_or_else(|| AppError::NotFound(session_id.to_string()))?;
        sessions
            .set_execution(
                session_id,
                mode.as_str(),
                sandbox,
                &kind,
                leveler_core::now(),
            )
            .await?;
        Ok(())
    }

    /// The direct-task spec for this repository.
    ///
    /// The continuation is `UntilTerminal` on purpose: a top-level task ends
    /// on its goal lifecycle (complete/blocked/stalled), a resource budget, or
    /// cancellation — never on a model-step count. Its only model-step bound
    /// is the mechanical safety ceiling carried in `top_level_limits()`.
    fn direct_spec(&self, goal: String, mode: PermissionProfile, sandbox: bool) -> TaskSpec {
        TaskSpec {
            runtime: leveler_agent::coding::RuntimeTaskSpec {
                goal,
                kind: ExecutionKind::Direct,
                continuation: leveler_agent::ContinuationPolicy::UntilTerminal,
                limits: self.top_level_limits(),
            },
            coding: leveler_agent::coding::CodingTaskSpec {
                repository: self.layout.repo_root.clone(),
                mode,
                sandbox,
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
            // A headless goal is a top-level task like any other: its lifetime
            // is the goal's, not a model-step count. `top_level_limits()`
            // carries the mechanical safety ceiling.
            leveler_agent::ContinuationPolicy::UntilTerminal,
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
    /// Reload collaboration from the session row for every turn.
    pub(crate) async fn turn_axes(
        &self,
        repo: &SessionRepository<'_>,
        session_id: &leveler_core::SessionId,
    ) -> Result<leveler_lifecycle::CollaborationMode, AppError> {
        let Some(record) = repo.get(session_id).await? else {
            return Ok(self.collaboration());
        };
        self.validate_session_workspace(record.repository.as_deref())?;
        Ok(crate::axes_from_session_record(&record))
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
        self.ensure_memory_consolidator(&db)?;
        self.sync_session_memory_policy(&db, session_id).await;
        let repo = SessionRepository::new(&db);
        // Product axes SoT is the session row (SetProductAxes / create defaults).
        let collaboration = self.turn_axes(&repo, session_id).await?;
        // The axis decides the profile and the capability surface through the
        // one owner, not per entry point: headless `run` and the interactive
        // turn read the same mapping.
        let execution = CollaborationExecution::of(collaboration);

        let engine = self
            .engine_for_session(
                model,
                mode,
                sandbox,
                approver,
                clarifier,
                execution.read_only(),
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
        let result = run_collaboration_turn(
            &engine,
            execution,
            session_id,
            &spec,
            vec![ContentPart::Text {
                text: goal.to_string(),
            }],
            develop,
            observer,
            cancellation,
        )
        .await;
        self.notify_memory_consolidator();
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
        self.ensure_memory_consolidator(&db)?;
        self.sync_session_memory_policy(&db, session_id).await;
        let repo = SessionRepository::new(&db);
        let collaboration = self.turn_axes(&repo, session_id).await?;
        let execution = CollaborationExecution::of(collaboration);
        let engine = self
            .engine_for_session(
                model,
                mode,
                sandbox,
                approver,
                clarifier,
                execution.read_only(),
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
        let result = run_collaboration_turn(
            &engine,
            execution,
            session_id,
            &spec,
            content,
            // `/develop` is its own workflow command, not a collaboration axis.
            false,
            observer,
            cancellation,
        )
        .await;
        self.notify_memory_consolidator();
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
        use leveler_lifecycle::{AgentState, SessionStatus};

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

    /// (collaboration). Application in-memory defaults are not
    /// the SoT for resume — the session row is (CLI `leveler resume` may
    /// `assemble()` with chat default).
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
        let execution = CollaborationExecution::of(crate::axes_from_session_record(&record));

        let engine = self
            .engine_for_session(
                &model,
                mode,
                sandbox,
                approver,
                clarifier,
                execution.read_only(),
                Some(session_id.as_str()),
            )
            .await?
            .with_task_cancel(task_cancel);
        self.validate_session_workspace(record.repository.as_deref())?;
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
}

#[cfg(test)]
mod turn_axes_tests {
    use crate::Application;
    use leveler_agent::CollaborationMode;
    use leveler_model::ModelRef;
    use leveler_project::Layout;
    use leveler_storage::SessionRepository;

    fn isolated_app(tmp: &tempfile::TempDir) -> Application {
        Application::assemble(Layout::from_parts(
            tmp.path().to_path_buf(),
            tmp.path().join("configs"),
            tmp.path().join("state"),
        ))
        .unwrap()
    }

    #[tokio::test]
    async fn a_plan_session_remains_a_read_only_overlay_after_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let app = isolated_app(&tmp).with_collaboration(CollaborationMode::Plan);
        let id = app
            .create_session(&ModelRef::new("mock", "m"), "plan it")
            .await
            .unwrap();
        let resumer = isolated_app(&tmp);
        let db = resumer.open_database().await.unwrap();
        let repo = SessionRepository::new(&db);
        assert_eq!(
            resumer.turn_axes(&repo, &id).await.unwrap(),
            CollaborationMode::Plan
        );
    }

    #[tokio::test]
    async fn a_missing_row_uses_collaboration_default() {
        let tmp = tempfile::tempdir().unwrap();
        // A row-less turn reads the Application's configured default, not a
        // hardcoded axis: an explicit Plan app yields Plan.
        let app = isolated_app(&tmp).with_collaboration(CollaborationMode::Plan);
        let db = app.open_database().await.unwrap();
        assert_eq!(
            app.turn_axes(
                &SessionRepository::new(&db),
                &leveler_core::SessionId::new("missing")
            )
            .await
            .unwrap(),
            CollaborationMode::Plan
        );
    }
}
