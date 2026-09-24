use std::collections::HashSet;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use leveler_context::{load_scoped_rules, render_instructions};
use leveler_lifecycle::{
    EvidenceLedger, FindingKind, ObjectiveAnchor, PlanState, ProgressCaps, TurnPhase,
};
use leveler_model::{
    CompactionRecord, ContentPart, FinishReason, Message, ModelError, Role, ToolCall,
    ToolResultContent,
};
use leveler_tools::ToolRegistry;

use super::agent_spawn::SpawnAdmission;
use super::closeout::{
    CLOSEOUT_NUDGE_BUDGET, CloseoutAction, CloseoutBudget, CloseoutInput, CloseoutReason, decide,
    stalled_detail,
};
use super::dispatch::{
    collect_modified, compact_json, deny_call, extract_applied_diff, extract_image, extract_plan,
    newly_modified_paths, note_tool_side_effects, preview,
};
use super::host::AdmitError;
use super::{
    AbortedFacts, AdvisoryKind, AgentError, AgentEvent, AgentOutcome, ChildToolEvent, DriveAborted,
    Executor, ModelRequestRecord, StopReason, TranscriptSink,
};
use crate::authorization::{collect_scoped_paths_from_call, push_unique_path};
use crate::injected_tools::{
    CLAIM_WRITE_SCOPE_TOOL, GrantScope, PermissionRequestOutcome, REPORT_FINDING_TOOL,
    REQUEST_PERMISSIONS_TOOL, SPAWN_AGENT_TOOL, TurnPermissionGrants, UPDATE_GOAL_TOOL,
    advertise_escalation, apply_turn_grants, ask_user_tool_definition,
    claim_write_scope_tool_definition, escalation_action, escalation_missing_axis_message,
    is_escalatable_tool, is_user_input_tool, parse_escalation, parse_permission_request,
    permission_already_denied_message, report_finding_tool_definition,
    request_permissions_tool_definition, request_user_input_tool_definition,
    spawn_agent_tool_definition, update_goal_tool_definition,
};
use crate::nudges::{finalization_nudge, goal_resolve_nudge};
use crate::sub_agent::{
    AgentRole, ChildProfile, MAX_SUB_AGENT_DEPTH, agent_nickname, lost_children_note,
    multi_agent_steer_hint, new_delegated_agent_id, scopes_overlap, settlement_notice,
    should_inject_delegation_hint,
};
use async_trait::async_trait;
use leveler_context::{COMPACT_KEEP_RECENT, compact_messages, estimate_tokens};

use leveler_agent_core::{
    Agent, AgentCoreError, AgentHarness, BudgetDimension, BudgetExhaustion, Flow, LoopContext,
    LoopStop, ModelRound, ModelStepLimits, SpentBefore, StopReason as KernelStop,
};
use leveler_lifecycle::ProgressLedger;
use leveler_model::ToolDefinition;

/// One still-running background delegation (V2). The join handle is owned by
/// the drive loop — there is no second scheduler; settlement is read at round
/// boundaries and at the explicit drain points.
struct BackgroundChild {
    id: String,
    nickname: String,
    role: AgentRole,
    /// Worker exclusive scope (empty for read-only roles). Held for overlap
    /// admission and the parent write fence until settlement.
    scope: Vec<String>,
    /// This child's own cancellation, so an exit path can stop it and wait
    /// rather than abort it.
    token: CancellationToken,
    handle: tokio::task::JoinHandle<super::handlers::SubAgentRunResult>,
}

/// Abort-on-drop guard: if the drive future is dropped on an unexpected path,
/// spawned children must not keep running as orphans. Every NORMAL exit drains
/// (settles) children first, so this abort is strictly the crash path.
///
/// Review 必改②: an aborted child never reaches a settle fold, so its write
/// ownership must be released HERE too — otherwise a mid-round `return Err`
/// (model decode/length/content-filter/fatal-admission) leaves the registry
/// holding a claim for a child that no longer exists.
#[derive(Default)]
struct BackgroundChildren {
    children: Vec<BackgroundChild>,
    ownership: Option<Arc<crate::ownership::OwnershipRegistry>>,
}

impl Drop for BackgroundChildren {
    fn drop(&mut self) {
        for child in &self.children {
            child.handle.abort();
            if let Some(ownership) = &self.ownership {
                ownership.release_all(&child.id);
            }
        }
    }
}

/// Bounded recovery from a malformed tool call: tool arguments sometimes
/// arrive as invalid JSON (an unescaped backslash from a regex, a raw newline
/// from a multi-line script). Rather than failing the whole turn on that Decode
/// error, feed the parse error back and let the model resend — the error is
/// reported exactly, and nothing guesses what the arguments meant. Reset on any
/// clean round so the cap is on *consecutive* failures.
const MAX_DECODE_RETRIES: u32 = 2;

/// Plain-text output may hit the provider's per-response limit. Continue the
/// same answer a bounded number of times; truncated tool calls are never safe
/// to execute and fail immediately.
const MAX_LENGTH_CONTINUATIONS: u32 = 2;

/// The wall-clock point (on the same task-level axis as `max_duration`) at
/// which a bounded run is asked to stop expanding and return what it has.
///
/// `None` when the run has no duration bound, no finalization reserve, or a
/// reserve that would swallow the whole run. The point is strictly before
/// `max_duration`, so it can request a synthesis but never extend a run.
pub(crate) fn finalization_point(
    max_duration: Option<std::time::Duration>,
    finalization_grace: Option<std::time::Duration>,
) -> Option<std::time::Duration> {
    let max = max_duration?;
    let grace = finalization_grace?;
    max.checked_sub(grace).filter(|at| !at.is_zero())
}

/// The CodeLeveler coding harness around the generic agent kernel.
///
/// The kernel (`leveler-agent-core`) owns the round loop, the model round,
/// admission against the mechanical limits, cancellation and the deadline. It
/// calls back into this type at each round boundary, and everything a coding
/// agent needs beyond a bare tool loop lives here: the ToolHost admission
/// pipeline, write ownership, delegation, the evidence and progress ledgers,
/// closeout, compaction, and the CodeLeveler event projection.
///
/// Every field was a local of the loop this harness replaced; they are fields
/// now because the kernel, not this code, owns the iteration.
pub(crate) struct Drive<'a> {
    executor: &'a Executor,
    observer: &'a mut (dyn FnMut(AgentEvent) + Send),
    sink: &'a mut dyn TranscriptSink,
    /// Active objective used for this drive (host-pinned).
    objective: ObjectiveAnchor,
    /// The tool table every request advertises.
    tools: Vec<ToolDefinition>,
    modified_files: Vec<String>,
    /// Model steps this drive has started. Reported on abort so an
    /// interrupted/failed turn records what it actually spent. A mechanical
    /// count of the agent's request cadence, never a task-progress measure.
    model_steps: u32,
    scoped_paths: Vec<String>,
    progress_caps: ProgressCaps,
    progress: ProgressLedger,
    epoch_model_steps_at_start: u32,
    epoch_tokens_at_start: u64,
    epoch_estimated_at_start: u64,
    epoch_duration_at_start: std::time::Duration,
    model_step_note_sent: bool,
    /// Set after the one deterministic goal-mode reminder that turns an
    /// initial read-only investigation into delivery work.
    delivery_convergence_nudged: bool,
    plan_state: PlanState,
    /// True after an actually dispatched non-plan tool call until the model
    /// successfully publishes another full plan table. This is freshness only:
    /// the final table may truthfully retain open steps.
    plan_needs_reconciliation: bool,
    /// Set whenever the model-visible messages gain something the durable
    /// transcript does not — a transient nudge, a fold.
    context_diverged: bool,
    session_approved: HashSet<String>,
    background_children: BackgroundChildren,
    /// Whether this run's resumed children (if any) were launched yet.
    resumed_children_launched: bool,
    run_agents_semaphore: Arc<tokio::sync::Semaphore>,
    bg_progress_tx: tokio::sync::mpsc::UnboundedSender<AgentEvent>,
    bg_progress_rx: tokio::sync::mpsc::UnboundedReceiver<AgentEvent>,
    /// Accumulated elevations from approved request_permissions this turn.
    turn_grants: TurnPermissionGrants,
    /// Consecutive search calls with no intervening action.
    /// Sources of scoped AGENTS.md rules already appended to the transcript.
    injected_rule_sources: Vec<String>,
    /// The most recent non-empty assistant text.
    last_text: String,
    /// Finalization is a one-way lifecycle boundary for this drive. Keeping
    /// the latch here prevents multiple exit helpers from publishing it twice.
    finalization_started: bool,
    /// Set once the wall-clock finalization request has been put in front of
    /// the model, so a run that ignores it is not told again every round.
    finalization_requested_sent: bool,
    ledger: EvidenceLedger,
    closeout_budget: CloseoutBudget,
    decode_retries: u32,
    length_continuations: u32,
    continued_text: String,
    commands_run: u32,
    /// Human reason + structured dimension when a step limit trips mid-round.
    budget_exceeded: Option<(String, BudgetExhaustion)>,
    /// Provider-reported total for the round just finished, which the
    /// compaction threshold prefers over its own estimate.
    last_model_call_usage_total: u64,
    /// Shared record of the most recent compaction fold. Written here when the
    /// harness folds; read by the kernel when it builds the accounting
    /// snapshot. One handle, two views of the same fact.
    compaction: Arc<std::sync::Mutex<Option<CompactionRecord>>>,
}

impl Executor {
    /// Run the CodeLeveler coding harness over the generic agent kernel.
    ///
    /// The transcript, the objective, the observer and the durable sink go in;
    /// the kernel drives model↔tool rounds until the model resolves the goal,
    /// this harness stops it, or a mechanical limit fires.
    pub(crate) async fn drive(
        &self,
        mut messages: Vec<Message>,
        objective: ObjectiveAnchor,
        observer: &mut (dyn FnMut(AgentEvent) + Send),
        sink: &mut dyn TranscriptSink,
        cancellation: CancellationToken,
        aborted: &mut AbortedFacts,
    ) -> Result<AgentOutcome, DriveAborted> {
        let mut tools = self.registry.definitions();
        // Primary name, plus legacy ask_user for older models and prompts.
        tools.push(request_user_input_tool_definition());
        tools.push(ask_user_tool_definition());
        // Nothing to request under 完全访问 — the elevation it asks for is
        // already granted, so advertising it only invites a pointless round
        // trip and an interruption the user explicitly opted out of.
        if self.tool_context.policy.mode() != leveler_execution::PermissionProfile::FullAccess {
            tools.push(request_permissions_tool_definition());
            // Same reason, applied to the command tools: a denied command can
            // carry its own one-shot elevation on the retry instead of
            // spending a round trip on a separate request.
            advertise_escalation(&mut tools);
        }
        // A sub-agent shouldn't spawn its own sub-agents; product kill-switch
        // can also hide spawn_agent entirely.
        if self.policy.allow_delegation && self.depth < MAX_SUB_AGENT_DEPTH {
            tools.push(spawn_agent_tool_definition());
        }
        // Late-bound ownership: a spawned child starts read-capable and
        // claims its own bounded write scope. Read-only roles never claim.
        if self.depth > 0 && !crate::sub_agent::ChildProfile::resolve(self.agent_role).read_only() {
            tools.push(claim_write_scope_tool_definition());
        }
        // Children report typed findings; the parent reads them in the
        // settlement notice and decides what to do, in its own words.
        if self.depth > 0 {
            tools.push(report_finding_tool_definition());
        }
        // Goal mode: the model resolves the objective explicitly.
        if self.policy.goal_mode {
            tools.push(update_goal_tool_definition());
        }

        let mut progress = self
            .seeded_progress
            .clone()
            .with_objective_version(objective.version);
        progress.phase = TurnPhase::Active;
        // `closing` records that a close was attempted in a previous window.
        // A drive that is starting is not closing, whatever the window before
        // it did; a seeded continuation must not begin inside the closeout it
        // was issued to get out of.
        progress.closing = false;

        // Product steer: top-level runs see keep-vs-delegate once. Parallel
        // keywords are not required — ordinary implementation goals must still
        // evaluate bounded Worker work.
        let mut context_diverged = false;
        if should_inject_delegation_hint(self.policy.allow_delegation, self.depth)
            && !messages.iter().any(|m| {
                m.role == Role::User
                    && m.text_content()
                        .contains(crate::sub_agent::MULTI_AGENT_HINT_HEADER)
            })
        {
            messages.push(Message::text(Role::User, multi_agent_steer_hint()));
            context_diverged = true;
        }

        // MA-RT-3 C10: children that durably SETTLED while the previous
        // window ended are not lost — their recorded outcomes are re-delivered
        // so the parent integrates instead of re-delegating. The host already
        // pruned them out of `outstanding_children`; persisting that pruned
        // ledger here is the once-per-restart mark (a crash before it lands
        // re-delivers again, which repeats a note but never repeats state).
        if self.depth == 0 && !self.restart_settled_children.is_empty() {
            let note = Message::text(
                Role::User,
                crate::sub_agent::settled_children_redelivery_note(&self.restart_settled_children),
            );
            observer(AgentEvent::ProgressUpdated {
                ledger: progress.clone(),
            });
            sink.append(std::slice::from_ref(&note)).await?;
            messages.push(note);
        }
        // In-process children did not survive a restart: tell the model
        // truthfully which delegations were lost, release their scopes, and
        // clear the durable record.
        if self.depth == 0 && !progress.outstanding_children.is_empty() {
            let note = Message::text(
                Role::User,
                lost_children_note(&progress.outstanding_children),
            );
            progress.outstanding_children.clear();
            observer(AgentEvent::ProgressUpdated {
                ledger: progress.clone(),
            });
            // Durable like every other injected turn — the record was just
            // cleared, so the note is the only remaining truth.
            sink.append(std::slice::from_ref(&note)).await?;
            messages.push(note);
        }

        let (bg_progress_tx, bg_progress_rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        // The drive owns ONE plan: `self.seeded_plan`. The evidence ledger
        // mirrors that same plan, never a different one. A resume/continuation
        // seeds a real plan and the ledger carries it; a fresh turn seeds
        // neither, so the previous epoch's plan stays history instead of
        // reappearing inside the ledger. Initializing the two together is what
        // keeps a later tool call from "clearing" a plan this turn never held.
        let mut seeded_ledger = self.seeded_ledger.clone();
        seeded_ledger.plan = self.seeded_plan.clone();
        // One handle for the fold record: the harness writes it when it folds;
        // the kernel reads it when it builds the accounting snapshot.
        let compaction_record: Arc<std::sync::Mutex<Option<CompactionRecord>>> =
            Arc::new(std::sync::Mutex::new(None));
        let mut harness = Drive {
            executor: self,
            observer,
            sink,
            tools,
            modified_files: Vec::new(),
            model_steps: 0,
            scoped_paths: Vec::new(),
            progress_caps: ProgressCaps::default(),
            epoch_model_steps_at_start: progress.cumulative_model_steps,
            epoch_tokens_at_start: progress.cumulative_model_tokens,
            epoch_estimated_at_start: progress.cumulative_estimated_model_tokens,
            epoch_duration_at_start: std::time::Duration::from_millis(
                progress.cumulative_duration_ms,
            ),
            commands_run: progress.cumulative_commands,
            model_step_note_sent: false,
            delivery_convergence_nudged: false,
            plan_state: self.seeded_plan.clone(),
            plan_needs_reconciliation: false,
            context_diverged,
            session_approved: HashSet::new(),
            background_children: BackgroundChildren {
                children: Vec::new(),
                ownership: Some(self.ownership.clone()),
            },
            resumed_children_launched: false,
            run_agents_semaphore: Arc::new(tokio::sync::Semaphore::new(
                self.policy.max_concurrent_agents.max(1),
            )),
            bg_progress_tx,
            bg_progress_rx,
            turn_grants: TurnPermissionGrants::default(),
            injected_rule_sources: Vec::new(),
            last_text: String::new(),
            finalization_started: false,
            finalization_requested_sent: false,
            ledger: seeded_ledger,
            // Unified closeout nudge budget shared by every quiet-round
            // mechanism (goal resolution, empty answer).
            closeout_budget: CloseoutBudget::new(CLOSEOUT_NUDGE_BUDGET),
            decode_retries: 0,
            length_continuations: 0,
            continued_text: String::new(),
            budget_exceeded: None,
            last_model_call_usage_total: 0,
            compaction: compaction_record.clone(),
            progress,
            objective,
        };

        // A resumed unfinished task activates the exact persisted declaration
        // in this turn. Terminal UI state can therefore archive the old plan;
        // it never has to keep stale presentation state around to guess that a
        // real continuation started.
        if !harness.plan_state.is_empty() {
            (harness.observer)(AgentEvent::PlanUpdated {
                steps: harness.plan_state.steps.clone(),
            });
        }

        // Hard step limits (spec §27) as the kernel enforces them: the epoch's
        // prior spend is what makes the resource limits task-level rather than
        // per-drive.
        //
        // A model-step count bounds a run in exactly two shapes, and neither is
        // a task budget:
        //
        // - `StepLimits.max_model_steps` is the SAFETY CEILING: the circuit
        //   breaker that stops a runaway model↔tool loop. The host pins it far
        //   outside the normal operating range (`leveler run` defaults it to
        //   `DEFAULT_MODEL_STEP_CEILING`).
        // - `model_step_window_limit` is a deliberately BOUNDED UNIT OF WORK the
        //   host owns — an eval case, a delegated agent's manifest budget. Its N
        //   is the hard edge of that unit.
        //
        // No hidden count sits under either. A top-level `UntilTerminal` turn
        // with neither ends on a semantic terminal state or a real resource
        // guard (cancellation, the token/cost/duration budgets, the no-progress
        // watchdog). A step count is a property of the model's tool cadence, not
        // of the user's task, so it can never be what decides a task is over.
        let limits = ModelStepLimits {
            model_step_ceiling: self.step_limits.max_model_steps,
            model_step_window_limit: self.continuation.model_step_window_limit(),
            max_model_tokens: self.step_limits.max_model_tokens,
            max_cost_usd_micros: self.step_limits.max_cost_usd_micros,
            max_duration: self.step_limits.max_duration,
            finalize_at: finalization_point(
                self.step_limits.max_duration,
                self.step_limits.finalization_grace,
            ),
            spent_before: SpentBefore {
                model_tokens: harness.epoch_tokens_at_start,
                cost_usd_micros: harness.progress.cumulative_cost_usd_micros,
                duration: harness.epoch_duration_at_start,
            },
        };
        let agent = Agent::new(self.runtime.clone(), self.model.clone())
            .with_limits(limits)
            .with_pricing(self.pricing)
            .with_max_output_tokens(Some(self.max_output_tokens))
            .with_reasoning_effort(self.policy.reasoning_effort)
            .with_context_window(self.policy.context_window)
            .with_compact_at(self.policy.context_budget)
            .with_compaction_record(compaction_record.clone());
        let result = agent.run(messages, &mut harness, cancellation).await;
        if result.is_err() {
            // Every normal exit drains its children; an error exit must too.
            // Aborting instead cannot stop a write already inside a blocking
            // section, and releasing a scope on abort lets that write land
            // after the scope is gone.
            harness.stop_background_children().await;
        }
        // An abort still leaves the loop's proven facts behind: the rounds it
        // started and the files it confirmed it changed. Report them so a
        // failed/interrupted turn never claims it did nothing.
        aborted.model_steps = harness.model_steps;
        aborted.modified_files = harness.modified_files.clone();
        result.map_err(|error| DriveAborted {
            error,
            facts: aborted.clone(),
        })
    }
}

impl<'a> Drive<'a> {
    /// Project a kernel event onto the CodeLeveler event vocabulary. Tool
    /// events never arrive here: this harness dispatches its own tools.
    fn forward_kernel_event(&mut self, event: leveler_agent_core::AgentEvent) {
        use leveler_agent_core::AgentEvent as Kernel;
        let projected = match event {
            Kernel::StreamAttemptStarted => AgentEvent::StreamAttemptStarted,
            Kernel::AssistantDelta(delta) => AgentEvent::AssistantDelta(delta),
            Kernel::ReasoningDelta(delta) => AgentEvent::ReasoningDelta(delta),
            Kernel::Usage(usage) => AgentEvent::Usage {
                input_tokens: usage.input_tokens.min(u32::MAX as u64) as u32,
                output_tokens: usage.output_tokens.min(u32::MAX as u64) as u32,
                cached_input_tokens: usage.cached_input_tokens.min(u32::MAX as u64) as u32,
                // `None` stays `None`: an unreported breakdown must not become
                // a measured zero on its way to a client.
                reasoning_tokens: usage
                    .reasoning_tokens
                    .map(|value| value.min(u32::MAX as u64) as u32),
            },
            Kernel::ContextUsage(accounting) => AgentEvent::ContextUsage { accounting },
            Kernel::ModelRetrying {
                attempt,
                max_attempts,
                delay_ms,
            } => AgentEvent::ModelRetrying {
                attempt,
                max_attempts,
                delay_ms,
            },
            Kernel::ToolCallStarted { .. } | Kernel::ToolCallFinished { .. } => return,
        };
        (self.observer)(projected);
    }

    /// Answer a call the host refused BEFORE admission, closing it on both
    /// channels at once.
    ///
    /// The model needs the reason in its transcript; every other reader needs
    /// the announced call to reach a terminal. `ToolCall` is emitted before
    /// admission (and persisted as `ToolCallStarted`), so a refusal that only
    /// wrote the transcript left the durable log claiming the call was still
    /// running — which the engine's crash-window reconciliation reads as "this
    /// may have run and left a side effect", about a command that provably
    /// never ran. A refusal is a fact the runtime holds; it does not get to
    /// decay into an unknown.
    fn settle_refused_call(
        &mut self,
        call: &ToolCall,
        reason: String,
        results: &mut [Option<ContentPart>],
        index: usize,
    ) {
        (self.observer)(AgentEvent::ToolResult {
            exit_code: None,
            stop: None,
            id: call.id.as_str().to_string(),
            name: call.name.clone(),
            is_error: true,
            preview: preview(&reason),
            applied_diff: None,
        });
        results[index] = Some(ContentPart::ToolResult {
            result: ToolResultContent {
                call_id: call.id.clone(),
                content: reason,
                is_error: true,
            },
        });
    }

    /// Write absolute epoch spend into the ledger so continue/resume seeds the
    /// same totals (including tool-phase command/file increments after the
    /// last model stream), and publish it.
    fn flush_epoch(&mut self, rt: &LoopContext) {
        sync_epoch_progress(
            &mut self.progress,
            self.epoch_model_steps_at_start,
            self.epoch_duration_at_start,
            rt.run_started(),
            rt.model_steps(),
            rt.model_tokens_spent(),
            self.epoch_estimated_at_start
                .saturating_add(rt.usage().estimated_model_tokens),
            self.commands_run,
            rt.cost_spent_micros(),
            &self.modified_files,
        );
        (self.observer)(AgentEvent::ProgressUpdated {
            ledger: self.progress.clone(),
        });
    }

    /// Persist a model call the kernel already folded into the run's spend.
    async fn persist_request(&mut self, record: ModelRequestRecord) -> Result<(), AgentError> {
        Ok(self.sink.record_model_request(&record).await?)
    }

    /// Fold a model call the harness made on its own account — a compaction
    /// summary, or a call a delegated child made — into the same spend the
    /// kernel admits against, then write it down. One event, two consumers;
    /// the guard and the bill cannot drift apart.
    async fn record_request(
        &mut self,
        rt: &mut LoopContext,
        record: ModelRequestRecord,
        estimate: Option<u64>,
    ) -> Result<(), AgentError> {
        rt.record_spend(record.usage, record.cost_usd_micros, estimate);
        self.persist_request(record).await
    }

    /// A child runs as an owned `'static` future, so it cannot borrow this
    /// sink; its model-call records ride the progress channel instead and are
    /// written down here, by the one holder of durable storage. They are not
    /// forwarded to the observer: the live progress line already carries the
    /// child's running totals, and this event exists for the ledger, not the
    /// screen.
    async fn forward_child_event(
        &mut self,
        rt: &mut LoopContext,
        event: AgentEvent,
    ) -> Result<(), AgentError> {
        if let AgentEvent::SubAgentModelRequest { record } = event {
            // Already priced by the child against its own model.
            self.record_request(rt, *record, None).await
        } else {
            (self.observer)(event);
            Ok(())
        }
    }

    /// Non-blocking settlement of finished background children — the notice
    /// lands in `messages` before the next model round.
    async fn settle_finished_children(
        &mut self,
        rt: &mut LoopContext,
        messages: &mut Vec<Message>,
    ) -> Result<(), AgentError> {
        while let Ok(event) = self.bg_progress_rx.try_recv() {
            self.forward_child_event(rt, event).await?;
        }
        let mut settled = Vec::new();
        let mut i = 0;
        while i < self.background_children.children.len() {
            if self.background_children.children[i].handle.is_finished() {
                settled.push(self.background_children.children.remove(i));
            } else {
                i += 1;
            }
        }
        for child in settled {
            if !self.plan_state.is_empty() {
                self.plan_needs_reconciliation = true;
            }
            let BackgroundChild {
                id,
                nickname,
                role,
                scope,
                handle,
                ..
            } = child;
            let result = join_settlement(handle.await);
            let (content, _ok) = fold_child_settlement(
                &mut self.progress,
                &mut self.commands_run,
                &mut self.modified_files,
                &mut self.ledger,
                &mut *self.observer,
                &id,
                &nickname,
                role,
                &result,
            );
            clear_outstanding_child(&mut self.progress, &id);
            // Terminal release: the child's exclusive claims end with it,
            // whatever its terminal state (idempotent).
            self.executor.ownership.release_all(&id);
            if let Some(host) = &self.executor.steering {
                host.child_ended(&id);
            }
            // The terminal is durable before the transcript says the child
            // settled: a crash between the two must not leave the parent
            // holding a result the log still calls unfinished.
            self.settlements_durable().await?;
            let notice = Message::text(
                Role::User,
                settlement_notice(&nickname, &id, role, &scope, &content),
            );
            // Persist the notice NOW. The next ContextSnapshot is a whole
            // model round away, and outstanding_children was just cleared — a
            // crash in between would otherwise lose the child's report text
            // with no lost-note either.
            self.sink.append(std::slice::from_ref(&notice)).await?;
            messages.push(notice);
            self.flush_epoch(rt);
        }
        Ok(())
    }

    /// Launch a new background activation for every child the engine recorded
    /// as resumed: same id, same spec, its restored transcript. From here it is
    /// an ordinary background child — permit, budget share, fence, settlement.
    async fn launch_resumed_children(
        &mut self,
        rt: &mut LoopContext,
        messages: &mut Vec<Message>,
    ) -> Result<(), AgentError> {
        let executor = self.executor;
        if executor.depth != 0 || executor.resumed_children.is_empty() {
            return Ok(());
        }
        let share_n = executor.resumed_children.len() as u32;
        let mut launched = Vec::new();
        for (share_of, child) in executor.resumed_children.iter().enumerate() {
            executor
                .ownership
                .register_owner(&child.id, &format!("{} ({})", child.nickname, child.id));
            let model_refusal = match child
                .spec
                .model
                .as_deref()
                .and_then(leveler_model::ModelRef::parse)
            {
                Some(model) => executor.pinned_model_refusal(&model).await,
                None => None,
            };
            let refusal = match model_refusal {
                Some(refusal) => Some(refusal),
                None if child.role == AgentRole::Worker && !child.spec.files.is_empty() => executor
                    .ownership
                    .try_claim(&child.id, &child.spec.files)
                    .err()
                    .map(|rejection| {
                        format!(
                            "its write scope could not be re-claimed: {}",
                            rejection.for_model()
                        )
                    }),
                None => None,
            };
            if let Some(refusal) = refusal {
                // Stale authority is never resurrected, and a child that cannot
                // hold its scope or run on its model cannot continue. Settle it
                // now, truthfully.
                executor.ownership.release_all(&child.id);
                let result = super::handlers::SubAgentRunResult {
                    result: crate::sub_agent::ChildResult::new(false, "", refusal),
                    stop: leveler_lifecycle::ChildStop::Failed,
                    limit: None,
                    progress: ProgressLedger::default(),
                    modified_files: Vec::new(),
                    findings: Vec::new(),
                };
                let (content, _) = fold_child_settlement(
                    &mut self.progress,
                    &mut self.commands_run,
                    &mut self.modified_files,
                    &mut self.ledger,
                    &mut *self.observer,
                    &child.id,
                    &child.nickname,
                    child.role,
                    &result,
                );
                self.settlements_durable().await?;
                let notice = Message::text(
                    Role::User,
                    settlement_notice(
                        &child.nickname,
                        &child.id,
                        child.role,
                        &child.spec.files,
                        &content,
                    ),
                );
                self.sink.append(std::slice::from_ref(&notice)).await?;
                messages.push(notice);
                continue;
            }
            let residual = residual_step_limits(
                executor.step_limits,
                self.commands_run,
                rt.model_tokens_spent(),
                rt.cost_spent_micros(),
                projected_epoch_file_count(&self.progress, &self.modified_files),
                self.epoch_duration_at_start,
                rt.run_started(),
                share_of as u32,
                share_n,
            );
            let parent_wall = super::handlers::ParentWallBudget {
                cap: executor.step_limits.max_duration,
                epoch_duration_at_start: self.epoch_duration_at_start,
                run_started: rt.run_started(),
            };
            let token = rt.cancellation().child_token();
            if let Some(host) = &executor.steering {
                host.child_started(&child.id, token.clone());
            }
            let handle = tokio::spawn(executor.sub_agent_resume_future(
                child,
                self.run_agents_semaphore.clone(),
                self.bg_progress_tx.clone(),
                residual,
                token.clone(),
                parent_wall,
            ));
            self.progress.outstanding_children.push(format!(
                "{}|{}|{}|{}",
                child.id,
                child.nickname,
                child.role.label(),
                child.spec.files.join(",")
            ));
            self.background_children.children.push(BackgroundChild {
                id: child.id.clone(),
                nickname: child.nickname.clone(),
                role: child.role,
                scope: child.spec.files.clone(),
                token,
                handle,
            });
            launched.push(child.clone());
        }
        (self.observer)(AgentEvent::ProgressUpdated {
            ledger: self.progress.clone(),
        });
        if !launched.is_empty() {
            let note = Message::text(
                Role::User,
                crate::sub_agent::resumed_children_note(&launched),
            );
            self.sink.append(std::slice::from_ref(&note)).await?;
            messages.push(note);
        }
        Ok(())
    }

    /// The error-exit drain: cancel every background child, wait for each to
    /// stop, and settle it like any other exit. Its scope is released only
    /// after it has stopped. Best effort on the durable side — the run is
    /// already failing, and that original error is what the caller gets.
    async fn stop_background_children(&mut self) {
        if self.background_children.children.is_empty() {
            return;
        }
        for child in &self.background_children.children {
            child.token.cancel();
        }
        let children = std::mem::take(&mut self.background_children.children);
        for child in children {
            let result = join_settlement(child.handle.await);
            fold_child_settlement(
                &mut self.progress,
                &mut self.commands_run,
                &mut self.modified_files,
                &mut self.ledger,
                &mut *self.observer,
                &child.id,
                &child.nickname,
                child.role,
                &result,
            );
            clear_outstanding_child(&mut self.progress, &child.id);
            self.executor.ownership.release_all(&child.id);
            if let Some(host) = &self.executor.steering {
                host.child_ended(&child.id);
            }
        }
        while let Ok(event) = self.bg_progress_rx.try_recv() {
            match event {
                AgentEvent::SubAgentModelRequest { record } => {
                    if let Err(error) = self.persist_request(*record).await {
                        tracing::warn!(%error, "could not record a stopped child's model call");
                    }
                }
                other => (self.observer)(other),
            }
        }
        (self.observer)(AgentEvent::ProgressUpdated {
            ledger: self.progress.clone(),
        });
        if let Err(error) = self.settlements_durable().await {
            tracing::warn!(%error, "stopped children's terminals may not be durable");
        }
    }

    /// Wait until every child terminal emitted so far is durable. Hosts without
    /// persistence have no barrier and nothing to wait for.
    async fn settlements_durable(&mut self) -> Result<(), AgentError> {
        if let Some(barrier) = &self.executor.event_barrier {
            barrier.flush().await?;
        }
        Ok(())
    }

    fn enter_finalization(&mut self) {
        if self.finalization_started {
            return;
        }
        self.finalization_started = true;
        (self.observer)(AgentEvent::FinalizationStarted);
    }

    /// Drain completion-dependent children with explicit timing. The phase key
    /// is intentionally generic and opaque to the engine; the app maps it onto
    /// its client vocabulary.
    async fn settle_finalization_dependencies(
        &mut self,
        rt: &mut LoopContext,
        messages: &mut Vec<Message>,
    ) -> Result<(), AgentError> {
        self.enter_finalization();
        let started = std::time::Instant::now();
        let phase = "settling_dependencies".to_string();
        (self.observer)(AgentEvent::FinalizationPhaseStarted {
            phase: phase.clone(),
        });
        let result = self.drain_background_children(rt, messages).await;
        tracing::debug!(
            %phase,
            elapsed_ms = started.elapsed().as_millis(),
            "finalization phase finished"
        );
        result
    }

    /// Full drain — await EVERY outstanding background child before the run
    /// returns, so no exit path orphans a running delegation or loses its
    /// result. Children hold cancellation tokens and wall caps, so this
    /// terminates.
    async fn drain_background_children(
        &mut self,
        rt: &mut LoopContext,
        messages: &mut Vec<Message>,
    ) -> Result<(), AgentError> {
        while !self.background_children.children.is_empty() {
            self.settle_finished_children(rt, messages).await?;
            if self.background_children.children.is_empty() {
                break;
            }
            tokio::select! {
                biased;
                Some(event) = self.bg_progress_rx.recv() => self.forward_child_event(rt, event).await?,
                _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
            }
        }
        Ok(())
    }

    /// A progress watchdog giving up: mark the ledger terminal, publish it,
    /// report the model's own last words (or `fallback` when it went quiet),
    /// and flush the epoch before returning.
    async fn stop_now(
        &mut self,
        rt: &mut LoopContext,
        messages: &mut Vec<Message>,
        stop: StopReason,
        detail: &str,
        fallback: &str,
    ) -> Result<AgentOutcome, AgentError> {
        self.progress.enter_closed();
        (self.observer)(AgentEvent::ProgressUpdated {
            ledger: self.progress.clone(),
        });
        let final_text = if self.last_text.trim().is_empty() {
            fallback.to_string()
        } else {
            self.last_text.clone()
        };
        (self.observer)(AgentEvent::Finished(final_text.clone()));
        self.enter_finalization();
        self.flush_epoch(rt);
        self.settle_finalization_dependencies(rt, messages).await?;
        Ok(AgentOutcome::drive_result(
            final_text,
            rt.model_steps(),
            self.modified_files.clone(),
            stop,
            Some(detail.to_string()),
            &self.progress,
            &self.objective,
        ))
    }
}

#[async_trait]
impl AgentHarness for Drive<'_> {
    type Stop = AgentOutcome;
    type Error = AgentError;

    fn on_event(&mut self, event: leveler_agent_core::AgentEvent) {
        self.forward_kernel_event(event);
    }

    fn tool_definitions(&self) -> Vec<ToolDefinition> {
        self.tools.clone()
    }

    fn request_context(&self, rt: &LoopContext) -> Vec<Message> {
        // Project existing owners, never maintain another task state. Plan
        // declarations and mechanical activity stay distinct: neither file
        // writes nor a changed plan establish semantic completion.
        let active: Vec<_> = self
            .plan_state
            .steps
            .iter()
            .filter(|s| s.status == "in_progress")
            .take(3)
            .map(|s| leveler_core::truncate_head_bytes(&s.step, 128, "…"))
            .collect();
        let limits = rt.limits();
        let state = serde_json::json!({
            // Mechanical counts of the agent's own request cadence. Neither is
            // a measure of task progress, and the runtime never reads them as
            // one: a step consumed by a provider repair or a closeout nudge is
            // still a step.
            "model_steps_completed": rt.model_steps().saturating_sub(1),
            "task_model_steps_completed": self.epoch_model_steps_at_start
                .saturating_add(rt.model_steps().saturating_sub(1)),
            "model_step_ceiling": limits.model_step_ceiling,
            "elapsed_ms": rt.elapsed().as_millis().min(u64::MAX as u128) as u64,
            "duration_limit_ms": limits.max_duration.map(|v| v.as_millis().min(u64::MAX as u128) as u64),
            "model_tokens": {
                "spent":rt.model_tokens_spent(), "limit":limits.max_model_tokens,
                "estimated": self.epoch_estimated_at_start.saturating_add(rt.usage().estimated_model_tokens)
            },
            "cost_usd_micros": {
                "spent": limits.max_cost_usd_micros.map(|_| rt.cost_spent_micros()),
                "limit": limits.max_cost_usd_micros
            },
            "commands_executed": self.commands_run,
            "command_limit": self.executor.step_limits.max_commands,
            "modified_paths_observed": projected_epoch_file_count(&self.progress, &self.modified_files),
            "finalization_requested": rt.finalization_requested(),
            "declared_plan": {
                "total": self.plan_state.steps.len(),
                "completed": self.plan_state.steps.iter().filter(|s| s.status == "completed").count(),
                "active": active,
                "active_omitted": self.plan_state.steps.iter().filter(|s| s.status == "in_progress").count().saturating_sub(3)
            }
        });
        vec![Message::text(
            Role::System,
            format!("Execution state (observations, not instructions):\n{state}"),
        )]
    }

    async fn on_round_start(
        &mut self,
        rt: &mut LoopContext,
        messages: &mut Vec<Message>,
    ) -> Result<Flow<AgentOutcome>, AgentError> {
        // Mid-turn user input goes in at the top of the round, before the
        // model is asked anything: a correction that arrives after the work
        // is done is worthless. Empty is the normal case.
        if let Some(source) = &self.executor.steering {
            for text in source.take_pending() {
                let text = text.trim();
                if text.is_empty() {
                    continue;
                }
                let message = Message::text(Role::User, text);
                self.sink.append(std::slice::from_ref(&message)).await?;
                messages.push(message);
            }
        }
        // Interrupted children this turn continues start before the parent's
        // first model call, so the parent is told about them up front and
        // never re-delegates work that is already resuming.
        if !self.resumed_children_launched {
            self.resumed_children_launched = true;
            self.launch_resumed_children(rt, messages).await?;
        }
        // Background settlements land before the model is asked anything —
        // whichever path reached this round top (tool batch, quiet wait,
        // nudge continue), the model never runs a round blind to a child
        // that already finished.
        self.settle_finished_children(rt, messages).await?;
        Ok(Flow::Continue)
    }

    async fn on_round_admitted(
        &mut self,
        rt: &mut LoopContext,
        messages: &mut Vec<Message>,
    ) -> Result<Flow<AgentOutcome>, AgentError> {
        let model_steps = rt.model_steps();
        self.model_steps = model_steps;
        let _ = rt;
        // Nested AGENTS.md rules for directories touched so far. Appended at
        // the tail rather than folded into the system prompt: rewriting the
        // first message would invalidate the provider's prefix cache for the
        // entire transcript on every round.
        let fresh = load_scoped_rules(
            self.executor.tool_context.execution.workspace.root(),
            &self.scoped_paths,
            &self.injected_rule_sources,
        );
        if !fresh.is_empty() {
            self.injected_rule_sources
                .extend(fresh.iter().map(|r| r.source.clone()));
            let rules = Message::text(
                Role::System,
                format!("Project rules:\n{}", render_instructions(&fresh)),
            );
            // Durable like every other injected message: a standing
            // constraint the model saw must survive in the transcript, not
            // only in a snapshot (the seed drops stale System rows on the
            // next turn, so this never duplicates the system prompt).
            self.sink.append(std::slice::from_ref(&rules)).await?;
            messages.push(rules);
        }

        // The wall clock is near its bound: tell the model ONCE to stop
        // expanding and return what it has, rather than let the hard deadline
        // cut off an in-flight round with nothing but partial work. This is a
        // request, not a stop: the kernel's deadline still ends the run if the
        // model keeps working, and it is never issued again after this round.
        //
        // The transcript check makes it once across activations too: a resumed
        // child's saved transcript already holds the request, and repeating it
        // would buy nothing.
        if !self.finalization_requested_sent && rt.finalization_requested() {
            self.finalization_requested_sent = true;
            let text = finalization_nudge();
            if !messages
                .iter()
                .any(|m| m.role == Role::User && m.text_content() == text)
            {
                let nudge = Message::text(Role::User, text);
                self.sink.append(std::slice::from_ref(&nudge)).await?;
                messages.push(nudge);
            }
        }

        // Once the model-step safety ceiling is nearly reached, the model is
        // told so, once — against THIS drive's counter and ceiling, so the
        // number it reads is the bound that actually applies to it. Resumes
        // re-pin the ceiling per drive, so an epoch-relative total would
        // understate what is left.
        //
        // This is a mechanical fact about the run, not a task budget: it does
        // not ask the model to hurry up, and it is not a signal that the task
        // is nearly done.
        if self.executor.depth == 0
            && let Some(ceiling) = self.executor.step_limits.max_model_steps
            && let Some(note) =
                model_step_note(rt.model_steps(), ceiling, self.model_step_note_sent)
        {
            let note = Message::text(Role::User, note);
            self.sink.append(std::slice::from_ref(&note)).await?;
            messages.push(note);
            self.model_step_note_sent = true;
        }
        Ok(Flow::Continue)
    }

    async fn on_model_error(
        &mut self,
        rt: &mut LoopContext,
        error: AgentCoreError,
        messages: &mut Vec<Message>,
    ) -> Result<Flow<AgentOutcome>, AgentError> {
        let cancellation = rt.cancellation().clone();
        match error {
            AgentCoreError::Model(e)
                if e.kind == leveler_model::ModelErrorKind::Decode
                    && self.decode_retries < MAX_DECODE_RETRIES
                    && !cancellation.is_cancelled() =>
            {
                self.decode_retries += 1;
                let feedback = Message::text(
                    Role::User,
                    format!(
                        "你上一次的工具调用参数不是合法 JSON:{}。请重新发起同一个工具调用,\
                             确保 arguments 是严格合法的 JSON——字符串里的反斜杠写成 `\\\\`、\
                             换行写成 `\\n`,不要放裸换行或裸反斜杠。多行脚本请拆成单行,\
                             或改用 write_file / apply_patch 之类不必在命令里塞长文本的工具。",
                        e.message
                    ),
                );
                self.sink.append(std::slice::from_ref(&feedback)).await?;
                messages.push(feedback);
                return Ok(Flow::NextRound);
            }
            other => Err(other.into()),
        }
    }

    async fn on_response(
        &mut self,
        rt: &mut LoopContext,
        round_result: &ModelRound,
        messages: &mut Vec<Message>,
    ) -> Result<Flow<AgentOutcome>, AgentError> {
        let model_steps = rt.model_steps();
        let cancellation = rt.cancellation().clone();
        let has_next_model_step = rt.has_next_model_step();
        // Priced once, by the kernel, against the usage the provider reported
        // — cached share included, because charging every input token at the
        // uncached rate overstated a session's cost by roughly 4x at a 90% hit
        // rate. The same priced record is what admission folded and what the
        // ledger stores.
        self.persist_request(ModelRequestRecord {
            provider_request_id: Some(round_result.request_id.clone()),
            provider: self.executor.model.provider.clone(),
            model: self.executor.model.model.clone(),
            usage: round_result.usage,
            finish_reason: round_result.finish_reason,
            latency_ms: round_result.latency_ms,
            retry_count: round_result.retry_count,
            kind: crate::ModelCallKind::Round,
            agent_id: None,
            cost_usd_micros: round_result.cost_usd_micros,
            // The round's request carried exactly this effort (the agent is
            // built `with_reasoning_effort(self.policy.reasoning_effort)`).
            reasoning_effort: self
                .executor
                .policy
                .reasoning_effort
                .map(|effort| effort.as_wire().to_string()),
        })
        .await?;
        // Cost can cross the limit on the response that tips it; stop after
        // this round's tools (if any) rather than allowing another model call.
        if let Some(max) = self.executor.step_limits.max_cost_usd_micros
            && rt.cost_spent_micros() >= max
        {
            self.budget_exceeded = Some((
                format!(
                    "Stopped: the {max}-micro-USD model cost budget was exhausted after {model_steps} model step(s)."
                ),
                BudgetExhaustion::new(BudgetDimension::Cost, rt.cost_spent_micros(), max),
            ));
        }
        // Epoch totals for continue/resume inheritance (absolute spend).
        // Persist ProgressUpdated so the next turn's seed gate and budget
        // resume see the same ledger (event log is SoT, not in-memory only).
        self.flush_epoch(rt);

        self.decode_retries = 0;
        self.last_model_call_usage_total = round_result.usage.total();
        let assistant = round_result.message.clone();
        let text = assistant.text_content();
        let calls = round_result.tool_calls();
        let finish_reason = round_result.finish_reason;

        match finish_reason {
            FinishReason::Length => {
                if !calls.is_empty() {
                    // The call is incomplete and must never execute; but a
                    // too-large tool call deserves the same bounded second
                    // chance text truncation gets — nudge for a smaller
                    // re-issue instead of killing the whole turn.
                    if self.length_continuations >= MAX_LENGTH_CONTINUATIONS
                        || !has_next_model_step
                        || cancellation.is_cancelled()
                    {
                        return Err(AgentError::Model(ModelError::new(
                            leveler_model::ModelErrorKind::Truncated,
                            "model output ended at the token limit while producing a tool call; the call was not executed",
                        )));
                    }
                    self.length_continuations += 1;
                    let feedback = Message::text(
                        Role::User,
                        "Your output hit the token limit while emitting a tool call — the \
                             call was NOT executed. Re-issue it smaller: split a large patch \
                             into several apply_patch calls, or shorten the arguments.",
                    );
                    self.sink.append(std::slice::from_ref(&feedback)).await?;
                    messages.push(feedback);
                    return Ok(Flow::NextRound);
                }
                if text.trim().is_empty()
                    || self.length_continuations >= MAX_LENGTH_CONTINUATIONS
                    || !has_next_model_step
                {
                    return Err(AgentError::Model(ModelError::new(
                        leveler_model::ModelErrorKind::Truncated,
                        "model output remained truncated after bounded continuation attempts",
                    )));
                }
                self.length_continuations += 1;
                self.continued_text.push_str(&text);
                self.last_text = self.continued_text.clone();
                self.sink.append(std::slice::from_ref(&assistant)).await?;
                messages.push(assistant);
                messages.push(Message::text(
                        Role::User,
                        "Continue exactly from the cutoff. Do not repeat prior text. Complete every open list, code block, sentence, and conclusion.",
                    ));
                self.context_diverged = true;
                return Ok(Flow::NextRound);
            }
            FinishReason::ContentFilter => {
                return Err(AgentError::Model(ModelError::new(
                    leveler_model::ModelErrorKind::ContentFiltered,
                    "provider content filtering stopped the response before a complete answer",
                )));
            }
            FinishReason::Other => {
                return Err(AgentError::Model(ModelError::new(
                    leveler_model::ModelErrorKind::Other,
                    "provider returned an unknown terminal finish reason",
                )));
            }
            FinishReason::ToolCalls if calls.is_empty() => {
                // Provider/gateway glitch (a dropped tool-call fragment),
                // not a model mistake: bounded feedback retry, same budget
                // as parameter-level decode failures.
                if self.decode_retries < MAX_DECODE_RETRIES
                    && has_next_model_step
                    && !cancellation.is_cancelled()
                {
                    self.decode_retries += 1;
                    let feedback = Message::text(
                        Role::User,
                        "Your last response declared a tool call but no complete call \
                             arrived (it was likely cut off in transit). Re-issue the tool \
                             call in full.",
                    );
                    self.sink.append(std::slice::from_ref(&feedback)).await?;
                    messages.push(feedback);
                    return Ok(Flow::NextRound);
                }
                return Err(AgentError::Model(ModelError::new(
                    leveler_model::ModelErrorKind::Decode,
                    "provider reported tool_calls but supplied no complete tool call",
                )));
            }
            FinishReason::Stop if !calls.is_empty() => {
                // Some OpenAI-compatible gateways finish tool-call rounds
                // with `stop`. The calls are complete — execute them.
                tracing::warn!(
                    "provider sent complete tool calls with finish_reason=stop; \
                         treating as tool_calls"
                );
            }
            FinishReason::Stop | FinishReason::ToolCalls => {}
        }

        if !text.trim().is_empty() {
            if self.continued_text.is_empty() {
                self.last_text = text.clone();
            } else {
                self.continued_text.push_str(&text);
                self.last_text = self.continued_text.clone();
            }
            (self.observer)(AgentEvent::AssistantText(self.last_text.clone()));
        }
        Ok(Flow::Continue)
    }

    async fn on_quiet(
        &mut self,
        rt: &mut LoopContext,
        assistant: Message,
        messages: &mut Vec<Message>,
    ) -> Result<Flow<AgentOutcome>, AgentError> {
        let model_steps = rt.model_steps();
        let cancellation = rt.cancellation().clone();
        let has_next_model_step = rt.has_next_model_step();
        // Cost tip-over after this response with no tools: end now (no more steps).
        if let Some((reason, exhaustion)) = self.budget_exceeded.take() {
            self.sink.append(&[assistant]).await?;
            (self.observer)(AgentEvent::Finished(reason.clone()));
            self.enter_finalization();
            self.flush_epoch(rt);
            self.settle_finalization_dependencies(rt, messages).await?;
            return Ok(Flow::Stop(AgentOutcome::drive_budget_exhausted(
                reason,
                model_steps,
                self.modified_files.clone(),
                exhaustion,
                &self.progress,
                &self.objective,
            )));
        }

        if !self.background_children.children.is_empty() {
            // V2: a quiet round while background children run is WAITING,
            // not a stall — hold for the next settlement instead of
            // burning closeout nudges or classifying no-progress. The
            // settlement itself is injected at the next round top.
            self.sink.append(&[assistant]).await?;
            while !self.background_children.children.is_empty()
                && !self
                    .background_children
                    .children
                    .iter()
                    .any(|child| child.handle.is_finished())
                && !cancellation.is_cancelled()
            {
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => {}
                    Some(event) = self.bg_progress_rx.recv() => self.forward_child_event(rt, event).await?,
                    _ = tokio::time::sleep(std::time::Duration::from_millis(200)) => {}
                }
            }
            return Ok(Flow::NextRound);
        }

        {
            // Unified closeout (executor/closeout.rs): a quiet round buys at
            // most ONE protocol repair — the goal run did not call
            // update_goal, or the model produced no text at all. Past that a
            // goal-mode quiet ends as `Stalled` — never as a success, so a
            // model that never learns to call update_goal terminates without
            // the harness declaring completion on its behalf; a non-goal turn
            // ends `Answered`.
            //
            // No-progress is counted once when this drive ends as Stalled
            // (below), not on the repair — so one drive can still use it,
            // while Engine continue is capped across turns.
            let has_final_text = !self.last_text.trim().is_empty();
            let action = decide(&CloseoutInput {
                goal_mode: self.executor.policy.goal_mode,
                has_final_text,
                cancelled: cancellation.is_cancelled(),
                can_continue: has_next_model_step,
                budget_remaining: self.closeout_budget.remaining(),
                human_boundary_seen: self.progress.human_boundary_seen(),
                plan_needs_reconciliation: self.plan_needs_reconciliation,
            });
            // Whether the harness accepted the quiet round or bought itself
            // another model call is the difference between "the model is
            // slow" and "we added a round" — indistinguishable on screen.
            tracing::info!(
                model_steps,
                ?action,
                goal_mode = self.executor.policy.goal_mode,
                has_final_text,
                budget_remaining = self.closeout_budget.remaining(),
                "closeout decided"
            );
            if let CloseoutAction::NudgeOnce(reason) = action {
                self.closeout_budget.consume();
                // Surface the injection: without this the user sees a
                // "final" answer and then an unexplained extra model round.
                (self.observer)(AgentEvent::AdvisoryStarted {
                    kind: AdvisoryKind::CloseoutNudge(reason),
                });
                let nudge = match reason {
                    CloseoutReason::GoalUnresolved => {
                        Message::text(Role::User, goal_resolve_nudge())
                    }
                    CloseoutReason::EmptyAnswer => Message::text(
                        Role::User,
                        "Your last message was empty. Reply with the actual answer to the \
                             request — do not send an empty message.",
                    ),
                    CloseoutReason::PlanUnreconciled => Message::text(
                        Role::User,
                        "Real tool work happened after your latest plan declaration. Before \
                             finishing, call update_plan with the complete final step table. \
                             Report statuses truthfully: leave unfinished work pending or \
                             in_progress; do not mark it completed merely to close the turn. \
                             Then provide the final answer.",
                    ),
                };
                // Persist BOTH the quiet-round assistant text and the nudge:
                // an engine continuation reloads the transcript from the
                // sink, and any gap here makes the resumed model see a
                // different conversation than the one it actually had.
                self.sink.append(&[assistant, nudge.clone()]).await?;
                messages.push(nudge);
                return Ok(Flow::NextRound);
            }
            self.sink.append(&[assistant]).await?;
            (self.observer)(AgentEvent::Finished(self.last_text.clone()));
            self.enter_finalization();
            // In goal mode reaching this point means the model went quiet
            // through every nudge without ever calling update_goal — that
            // is a stall, not a proven completion. The detail carries the
            // closeout reason so an engine continuation knows what the
            // previous turn stalled on.
            let (stop_reason, stop_detail) =
                if self.executor.policy.goal_mode && self.progress.human_boundary_seen() {
                    // User said no, model went quiet without resolving the
                    // goal. That is blocked, not a stall — and it is reported
                    // as such, since nothing re-drives a turn on its own.
                    self.progress.enter_closed();
                    (self.observer)(AgentEvent::ProgressUpdated {
                        ledger: self.progress.clone(),
                    });
                    (
                        StopReason::Blocked,
                        Some("user denied a required permission; goal left unresolved".to_string()),
                    )
                } else if self.executor.policy.goal_mode {
                    // One no-progress tick per stalled drive so Engine
                    // continue_active_goal cannot open unbounded turns.
                    self.progress.note_no_progress_round(model_steps);
                    if self
                        .progress
                        .should_hard_stop_no_progress(self.progress_caps)
                    {
                        self.progress.enter_closed();
                    }
                    (self.observer)(AgentEvent::ProgressUpdated {
                        ledger: self.progress.clone(),
                    });
                    let reason = match action {
                        CloseoutAction::Stall(reason) => reason,
                        _ => CloseoutReason::GoalUnresolved,
                    };
                    (
                        StopReason::Stalled,
                        Some(stalled_detail(
                            reason,
                            "目标模式结束但未调用 update_goal(complete/blocked)",
                        )),
                    )
                } else if action == CloseoutAction::Stall(CloseoutReason::PlanUnreconciled) {
                    (
                        StopReason::Incomplete,
                        Some(stalled_detail(
                            CloseoutReason::PlanUnreconciled,
                            "explicit plan was not reconciled after the last tool work",
                        )),
                    )
                } else {
                    if self.progress.human_boundary_seen() {
                        self.progress.enter_closed();
                        (self.observer)(AgentEvent::ProgressUpdated {
                            ledger: self.progress.clone(),
                        });
                    }
                    (StopReason::Answered, None)
                };
            self.flush_epoch(rt);

            self.settle_finalization_dependencies(rt, messages).await?;
            return Ok(Flow::Stop(AgentOutcome::drive_result(
                self.last_text.clone(),
                model_steps,
                self.modified_files.clone(),
                stop_reason,
                stop_detail,
                &self.progress,
                &self.objective,
            )));
        }
    }

    async fn execute_calls(
        &mut self,
        rt: &mut LoopContext,
        assistant: Message,
        calls: Vec<ToolCall>,
        messages: &mut Vec<Message>,
    ) -> Result<Flow<AgentOutcome>, AgentError> {
        let model_steps = rt.model_steps();
        let cancellation = rt.cancellation().clone();
        let has_next_model_step = rt.has_next_model_step();
        let model_tokens_spent = rt.model_tokens_spent();
        let cost_spent_micros = rt.cost_spent_micros();
        // Tool results, filled by call index. Parallel-safe read-only tools
        // may finish out of order, but the model must receive their results
        // in the original call order — providers reject reordered results.
        let mut results: Vec<Option<ContentPart>> = (0..calls.len()).map(|_| None).collect();
        // Goal mode: set when the model calls update_goal this round; the run
        // ends (Completed/Blocked) once this round's tool results are recorded.
        let mut goal_resolution: Option<(StopReason, String)> = None;
        // Images loaded via view_image this round, injected as a user message
        // after the tool results so a vision model sees them next request.
        let mut pending_images: Vec<ContentPart> = Vec::new();
        // Read-only, side-effect-free tools deferred to run concurrently
        // after this in-order pass. Everything else runs here, serially.
        // Each job carries the AdmittedCall proving the host pipeline ran.
        struct ParallelJob {
            index: usize,
            admitted: crate::executor::host::AdmittedCall,
        }
        let mut parallel_jobs: Vec<ParallelJob> = Vec::new();
        // spawn_agent calls deferred to run concurrently after this pass.
        let mut spawn_jobs: Vec<(usize, ToolCall)> = Vec::new();
        // Calls a guard refused before they ran (budgets, allowlist,
        // permission). A round consisting solely of refusals is no progress —
        // it feeds the all-refused streak.
        let mut denied_calls_this_round: usize = 0;
        // User cancel observed inside this batch. The batch stops, but the
        // round is still committed (results + spend) before Cancelled
        // surfaces — completed tools' side effects are already on disk.
        let mut cancelled_mid_batch = false;
        // A plan emitted in the same assistant response as actual work cannot
        // reconcile that work: the model generated it before seeing any of
        // this batch's tool results. Only a work-free batch may clear an
        // existing freshness obligation.
        let mut actual_work_admitted_this_batch = false;
        // Ids/names survive the consuming loop below so calls the cancel
        // cut short can still be refused in place (transcript pairing).
        let call_snapshot: Vec<ToolCall> = calls.clone();

        for (index, call) in calls.into_iter().enumerate() {
            // A child reports one typed finding: validated at the tool
            // boundary, recorded in ITS ledger (the parent adopts on
            // join), persisted through the same EvidenceLedgerUpdated
            // events as every other ledger change.
            if call.name == REPORT_FINDING_TOOL && self.executor.depth > 0 {
                (self.observer)(AgentEvent::ToolCall {
                    id: call.id.as_str().to_string(),
                    name: REPORT_FINDING_TOOL.to_string(),
                    arguments: compact_json(&call.arguments),
                    parallel: false,
                });
                let kind = call
                    .arguments
                    .get("kind")
                    .and_then(|v| v.as_str())
                    .and_then(FindingKind::parse);
                let summary = call
                    .arguments
                    .get("summary")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .unwrap_or("")
                    .to_string();
                let (ok, msg) = match kind {
                    None => (
                        false,
                        "report_finding requires a `kind` from the documented list.".to_string(),
                    ),
                    Some(_) if summary.is_empty() => (
                        false,
                        "report_finding requires a non-empty `summary`.".to_string(),
                    ),
                    Some(kind) => {
                        let field = |name: &str| {
                            call.arguments
                                .get(name)
                                .and_then(|v| v.as_str())
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .map(String::from)
                        };
                        let id = self.ledger.record_finding(
                            kind,
                            summary,
                            field("file"),
                            field("symbol"),
                        );
                        (self.observer)(AgentEvent::EvidenceLedgerUpdated {
                            ledger: self.ledger.clone(),
                        });
                        (true, format!("Finding {id} recorded."))
                    }
                };
                (self.observer)(AgentEvent::ToolResult {
                    exit_code: None,
                    stop: None,
                    id: call.id.as_str().to_string(),
                    name: REPORT_FINDING_TOOL.to_string(),
                    is_error: !ok,
                    preview: preview(&msg),
                    applied_diff: None,
                });
                results[index] = Some(ContentPart::ToolResult {
                    result: ToolResultContent {
                        call_id: call.id,
                        content: msg,
                        is_error: !ok,
                    },
                });
                continue;
            }

            // Goal mode: the model explicitly resolves the objective. Record
            // the resolution; the run ends after this round's results are
            // committed (so the transcript stays well-formed).
            if self.executor.policy.goal_mode && call.name == UPDATE_GOAL_TOOL {
                // Surface the resolution so the TUI/JSONL shows the goal being
                // closed (special tools otherwise skip the ToolCall event).
                (self.observer)(AgentEvent::ToolCall {
                    id: call.id.as_str().to_string(),
                    name: UPDATE_GOAL_TOOL.to_string(),
                    arguments: compact_json(&call.arguments),
                    parallel: false,
                });
                // A resolution without an explicit status is not accepted as
                // completion — feed the error back so the model resolves
                // deliberately instead of by omission.
                // V2 completion gate: a goal cannot close while delegated
                // children are still running — their results have not been
                // received, let alone integrated. Drain them (they hold
                // wall caps), inject the settlements, and refuse this
                // resolution once so the model integrates first.
                if call.arguments.get("status").and_then(|v| v.as_str()) == Some("complete")
                    && !self.background_children.children.is_empty()
                {
                    let waiting: Vec<String> = self
                        .background_children
                        .children
                        .iter()
                        .map(|c| format!("{} ({})", c.nickname, c.id))
                        .collect();
                    // Review 必改②: this drain runs MID tool batch — pin the
                    // parent's same-batch commands first, or the settlement
                    // fold overwrites local counters with a lagging ledger
                    // (the exact under-count pin_parent_batch_work exists
                    // to prevent on the foreground path).
                    pin_parent_batch_work(
                        &mut self.progress,
                        self.commands_run,
                        &self.modified_files,
                    );
                    self.drain_background_children(rt, messages).await?;
                    let feedback = format!(
                        "Cannot complete: delegated sub-agent(s) {} were still \
                             running. They have now settled — their notices are above. \
                             Inspect and integrate their results (judge any findings), \
                             re-verify, then call update_goal again.",
                        waiting.join(", ")
                    );
                    (self.observer)(AgentEvent::GoalIntercepted {
                        kind: "outstanding_children".to_string(),
                        detail: waiting.join(", "),
                    });
                    (self.observer)(AgentEvent::ToolResult {
                        exit_code: None,
                        stop: None,
                        id: call.id.as_str().to_string(),
                        name: UPDATE_GOAL_TOOL.to_string(),
                        is_error: true,
                        preview: preview(&feedback),
                        applied_diff: None,
                    });
                    results[index] = Some(ContentPart::ToolResult {
                        result: ToolResultContent {
                            call_id: call.id,
                            content: feedback,
                            is_error: true,
                        },
                    });
                    continue;
                }
                if self.plan_needs_reconciliation {
                    let feedback = "Cannot resolve the goal yet: real tool work happened after \
                         the latest plan declaration. Call update_plan with the complete final \
                         table first. Keep unfinished steps pending or in_progress; do not mark \
                         them completed merely to close the goal. Then call update_goal again."
                        .to_string();
                    (self.observer)(AgentEvent::GoalIntercepted {
                        kind: "plan_unreconciled".to_string(),
                        detail: "tool work occurred after the latest plan update".to_string(),
                    });
                    (self.observer)(AgentEvent::ToolResult {
                        exit_code: None,
                        stop: None,
                        id: call.id.as_str().to_string(),
                        name: UPDATE_GOAL_TOOL.to_string(),
                        is_error: true,
                        preview: preview(&feedback),
                        applied_diff: None,
                    });
                    results[index] = Some(ContentPart::ToolResult {
                        result: ToolResultContent {
                            call_id: call.id,
                            content: feedback,
                            is_error: true,
                        },
                    });
                    continue;
                }
                let reason = match call.arguments.get("status").and_then(|v| v.as_str()) {
                    Some("complete") => StopReason::Completed,
                    Some("blocked") => StopReason::Blocked,
                    other => {
                        let feedback = format!(
                            "update_goal requires `status` set to \"complete\" or \
                                 \"blocked\" (got {}). Call update_goal again with an \
                                 explicit status.",
                            other
                                .map(|s| format!("\"{s}\""))
                                .unwrap_or_else(|| "no status".to_string())
                        );
                        (self.observer)(AgentEvent::ToolResult {
                            exit_code: None,
                            stop: None,
                            id: call.id.as_str().to_string(),
                            name: UPDATE_GOAL_TOOL.to_string(),
                            is_error: true,
                            preview: preview(&feedback),
                            applied_diff: None,
                        });
                        results[index] = Some(ContentPart::ToolResult {
                            result: ToolResultContent {
                                call_id: call.id,
                                content: feedback,
                                is_error: true,
                            },
                        });
                        continue;
                    }
                };
                // The plan is the model's declared progress, not a completion
                // authority: the runtime can see that a step is still
                // `pending`, but not whether that step is still required to
                // satisfy the user — which is the model's reading of its own goal.
                if reason == StopReason::Completed {
                    self.ledger.plan = self.plan_state.clone();
                    // HostImplicit single-step completes atomically with the goal.
                    if self.plan_state.is_host_implicit() {
                        self.plan_state.mark_all_completed();
                        (self.observer)(AgentEvent::PlanUpdated {
                            steps: self.plan_state.steps.clone(),
                        });
                    }
                }
                (self.observer)(AgentEvent::ToolResult {
                    exit_code: None,
                    stop: None,
                    id: call.id.as_str().to_string(),
                    name: UPDATE_GOAL_TOOL.to_string(),
                    is_error: false,
                    preview: "Goal resolved.".to_string(),
                    applied_diff: None,
                });
                let summary = call
                    .arguments
                    .get("summary")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                goal_resolution = Some((reason, summary));
                results[index] = Some(ContentPart::ToolResult {
                    result: ToolResultContent {
                        call_id: call.id,
                        content: "Goal resolved.".to_string(),
                        is_error: false,
                    },
                });
                continue;
            }

            // Clarification tools (request_user_input / ask_user) are answered
            // by the clarifier (the UI), not the tool registry (spec §35 / B7).
            //
            // Announced and settled like any other call. A decision the user
            // was asked to make is history: without these two events the
            // question and the answer lived only inside the overlay, so both
            // disappeared the moment the user pressed Enter — absent from the
            // screen, the session log, and every later replay.
            if is_user_input_tool(&call.name) {
                (self.observer)(AgentEvent::ToolCall {
                    id: call.id.as_str().to_string(),
                    name: call.name.clone(),
                    arguments: compact_json(&call.arguments),
                    parallel: false,
                });
                let answer = self.executor.handle_ask_user(&call, &cancellation).await?;
                (self.observer)(AgentEvent::ToolResult {
                    exit_code: None,
                    stop: None,
                    id: call.id.as_str().to_string(),
                    name: call.name.clone(),
                    // The answer IS the result. It is short, user-written, and
                    // the only thing worth reading — never truncated to a
                    // preview budget meant for command output.
                    preview: answer.clone(),
                    is_error: false,
                    applied_diff: None,
                });
                results[index] = Some(ContentPart::ToolResult {
                    result: ToolResultContent {
                        call_id: call.id,
                        content: answer,
                        is_error: false,
                    },
                });
                continue;
            }

            // The task tools manage background shell tasks. A child's id is
            // not one of those, and "unknown task" would read as if the child
            // did not exist. Answer what the id is and how its result arrives;
            // nothing is waited on or killed through the wrong door. An
            // answer, not a refusal: the call is closed with is_error=false.
            if matches!(call.name.as_str(), "wait_task" | "get_task" | "kill_task")
                && let Some(child) = call
                    .arguments
                    .get("task_id")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .and_then(|task_id| {
                        self.background_children
                            .children
                            .iter()
                            .find(|child| child.id == task_id)
                    })
            {
                let child_id = child.id.clone();
                // `wait_task` waits on a child the way it waits on a background
                // task: up to its bounded interval, answering as soon as the
                // child ends. An instant answer turned every poll into a model
                // round (MA4-C: 45 and 82 polls ran parents into the round
                // limit while their children worked).
                if call.name == "wait_task" && !child.handle.is_finished() {
                    let deadline = tokio::time::Instant::now()
                        + leveler_tools::tools::wait_interval(
                            call.arguments
                                .get("timeout_seconds")
                                .and_then(|v| v.as_u64()),
                        );
                    loop {
                        let running = self
                            .background_children
                            .children
                            .iter()
                            .find(|c| c.id == child_id)
                            .is_some_and(|c| !c.handle.is_finished());
                        if !running
                            || cancellation.is_cancelled()
                            || tokio::time::Instant::now() >= deadline
                        {
                            break;
                        }
                        tokio::select! {
                            biased;
                            _ = cancellation.cancelled() => {}
                            Some(event) = self.bg_progress_rx.recv() => self.forward_child_event(rt, event).await?,
                            _ = tokio::time::sleep_until(deadline.min(tokio::time::Instant::now() + std::time::Duration::from_millis(200))) => {}
                        }
                    }
                }
                let Some(child) = self
                    .background_children
                    .children
                    .iter()
                    .find(|c| c.id == child_id)
                else {
                    unreachable!(
                        "a child is only removed at settlement, never inside a tool batch"
                    );
                };
                let state = if child.handle.is_finished() {
                    "It has finished; its result is delivered to you automatically before \
                     your next step."
                } else {
                    "It is still running; its result is delivered to you automatically when \
                     it settles."
                };
                let tail = if call.name == "kill_task" {
                    " A sub-agent cannot be cancelled from here: you have no tool that stops \
                     one, and kill_task stops only background shell tasks."
                } else {
                    " There is nothing to wait on or poll."
                };
                let answer = format!(
                    "`{}` is sub-agent {} ({}), not a background task. {state}{tail}",
                    child.id,
                    child.nickname,
                    child.role.label()
                );
                (self.observer)(AgentEvent::ToolCall {
                    id: call.id.as_str().to_string(),
                    name: call.name.clone(),
                    arguments: compact_json(&call.arguments),
                    parallel: false,
                });
                (self.observer)(AgentEvent::ToolResult {
                    exit_code: None,
                    stop: None,
                    id: call.id.as_str().to_string(),
                    name: call.name.clone(),
                    is_error: false,
                    preview: preview(&answer),
                    applied_diff: None,
                });
                results[index] = Some(ContentPart::ToolResult {
                    result: ToolResultContent {
                        call_id: call.id.clone(),
                        content: answer,
                        is_error: false,
                    },
                });
                continue;
            }

            // spawn_agent defers to the concurrent batch after this pass, so
            // several spawns in one round run in parallel.
            if call.name == SPAWN_AGENT_TOOL {
                spawn_jobs.push((index, call));
                continue;
            }

            // Late-bound ownership: a child claims exclusive write scope
            // against the shared registry. Atomic; a denial is an honest
            // coordination result (is_error=false) the child works around.
            if call.name == CLAIM_WRITE_SCOPE_TOOL {
                // Durable lifecycle FIRST: an ownership transition that
                // decides what a child may mutate has to be
                // reconstructable from the event log like any other tool
                // call. Emitting only a result made a granted claim
                // invisible to an audit keyed on tool starts, and a legal
                // write read as a pre-claim bypass (M7 measurement error).
                (self.observer)(AgentEvent::ToolCall {
                    id: call.id.as_str().to_string(),
                    name: CLAIM_WRITE_SCOPE_TOOL.to_string(),
                    arguments: compact_json(&call.arguments),
                    parallel: false,
                });
                let paths: Vec<String> = call
                    .arguments
                    .get("paths")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                let mut granted = false;
                let mut decision_detail = String::new();
                let (content, is_error) = if self.executor.depth == 0 {
                    (
                        "You are the top-level agent: you already write directly \
                             (outside children's claimed scopes). claim_write_scope is \
                             for spawned children."
                            .to_string(),
                        true,
                    )
                } else if let Some(outside) = {
                    let roots = &self.executor.write_roots;
                    let outside: Vec<&str> = paths
                        .iter()
                        .filter(|p| {
                            !roots.is_empty()
                                && !crate::agent_registry::within_write_roots(p, roots)
                        })
                        .map(String::as_str)
                        .collect();
                    (!outside.is_empty()).then(|| outside.join(", "))
                } {
                    // A declarative agent's definition bounds what it may ever
                    // own; the claim is refused before the ownership registry
                    // is consulted, so it grants nothing.
                    decision_detail = format!("outside write roots: {outside}");
                    (
                        format!(
                            "not granted — {outside} is outside this agent's write roots ({}). \
                             Claim paths inside them, or report what needs changing elsewhere.",
                            self.executor.write_roots.join(", ")
                        ),
                        false,
                    )
                } else {
                    let owner = self
                        .executor
                        .agent_id
                        .clone()
                        .unwrap_or_else(|| format!("child-depth-{}", self.executor.depth));
                    match self.executor.ownership.try_claim(&owner, &paths) {
                        Ok(owned) => {
                            granted = true;
                            decision_detail = owned.join(", ");
                            (
                                format!(
                                    "granted — you exclusively own: {}. Re-read a claimed \
                                         file before your first write to it if time has passed \
                                         since you read it.",
                                    owned.join(", ")
                                ),
                                false,
                            )
                        }
                        Err(rejection) => {
                            decision_detail = match &rejection {
                                crate::ownership::ClaimRejection::Conflicts(conflicts) => conflicts
                                    .iter()
                                    .map(|c| format!("{} owned by {}", c.path, c.owner))
                                    .collect::<Vec<_>>()
                                    .join("; "),
                                other => format!("{other:?}"),
                            };
                            (rejection.for_model(), false)
                        }
                    }
                };
                (self.observer)(AgentEvent::ToolResult {
                    exit_code: None,
                    stop: None,
                    id: call.id.as_str().to_string(),
                    name: CLAIM_WRITE_SCOPE_TOOL.to_string(),
                    is_error,
                    preview: preview(&content),
                    applied_diff: None,
                });
                // Durable ownership provenance. A child's tool events reach
                // the parent as TRANSIENT activity, so without a durable
                // record an offline audit cannot tell an authorized write
                // from a bypass — the M7 measurement failure.
                //
                // It must also be durable BEFORE the write it authorizes:
                // a child's tool calls flush on the child's own task, so a
                // grant emitted only through the activity channel waits for
                // the parent's next drain and can land after the write,
                // which is exactly how a legal write reads as a bypass.
                // Riding the barrier queue is what orders the two.
                //
                // Exactly ONE durable record per transition. The observer
                // copy is forwarded to the parent and persisted from there,
                // so emitting it as well as recording on the barrier writes
                // the same grant twice and an offline audit reads two grants
                // for one claim. The barrier is the primary path; the
                // observer is the fallback when there is no durable host
                // (standalone library use).
                if self.executor.depth > 0 {
                    let action = if granted {
                        "ownership_granted".to_string()
                    } else {
                        "ownership_denied".to_string()
                    };
                    match (&self.executor.event_barrier, &self.executor.agent_id) {
                        (Some(barrier), Some(agent_id)) => {
                            barrier.record_child_tool_event(ChildToolEvent::Ownership {
                                agent_id: agent_id.clone(),
                                action,
                                detail: decision_detail.clone(),
                            });
                            // The registry has already changed, so a flush
                            // failure aborts the run rather than continuing
                            // with owned paths no durable record accounts
                            // for.
                            barrier.flush().await?;
                        }
                        _ => {
                            let owner =
                                self.executor.agent_id.clone().unwrap_or_else(|| {
                                    format!("child-depth-{}", self.executor.depth)
                                });
                            (self.observer)(AgentEvent::DelegationStage {
                                action,
                                detail: format!("{owner}: {decision_detail}"),
                            });
                        }
                    }
                }
                results[index] = Some(ContentPart::ToolResult {
                    result: ToolResultContent {
                        call_id: call.id,
                        content,
                        is_error,
                    },
                });
                continue;
            }

            // request_permissions is answered by the user, not the registry.
            if call.name == REQUEST_PERMISSIONS_TOOL {
                let (_, _, requested) = parse_permission_request(&call.arguments);
                if self.progress.covers_denied_request(
                    requested.network,
                    requested.repository_git || requested.unrestricted_fs,
                ) {
                    results[index] = Some(ContentPart::ToolResult {
                        result: ToolResultContent {
                            call_id: call.id,
                            content: permission_already_denied_message(),
                            is_error: true,
                        },
                    });
                    continue;
                }
                let outcome = self
                    .executor
                    .handle_request_permissions(&call, &cancellation)
                    .await?;
                match &outcome {
                    PermissionRequestOutcome::Granted { grants, .. } => {
                        self.turn_grants = self.turn_grants.merge(*grants);
                    }
                    PermissionRequestOutcome::DeniedByUser { requested, .. } => {
                        self.progress.record_human_denial(
                            requested.network,
                            requested.repository_git || requested.unrestricted_fs,
                        );
                        (self.observer)(AgentEvent::ProgressUpdated {
                            ledger: self.progress.clone(),
                        });
                    }
                    _ => {}
                }
                results[index] = Some(ContentPart::ToolResult {
                    result: ToolResultContent {
                        call_id: call.id,
                        content: outcome.message().to_string(),
                        is_error: outcome.is_error(),
                    },
                });
                continue;
            }

            // A nested AGENTS.md may apply to a file the model tries to edit
            // directly. Refuse that first edit and inject the scoped rules
            // on the next round; otherwise the edit lands before the model
            // ever sees the rules governing it.
            if self.executor.registry.mutates_files(&call.name) {
                let mut target_paths = self.scoped_paths.clone();
                collect_scoped_paths_from_call(&call, &mut target_paths);
                let fresh = load_scoped_rules(
                    self.executor.tool_context.execution.workspace.root(),
                    &target_paths,
                    &self.injected_rule_sources,
                );
                if !fresh.is_empty() {
                    self.scoped_paths = target_paths;
                    let sources = fresh
                        .iter()
                        .map(|rule| rule.source.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    let msg = format!(
                        "Edit paused before execution: project rules were discovered for \
                             this path ({sources}). They will be injected as system rules on \
                             the next model step; review them, then retry the edit."
                    );
                    denied_calls_this_round += 1;
                    results[index] = Some(deny_call(&mut *self.observer, call, msg));
                    continue;
                }
            }

            // Step budgets (spec §27): refuse the call BEFORE it runs once a
            // limit is reached; the run ends after this round's results are
            // committed. File budget also refuses a single multi-file patch
            // that would cross the remaining task-level cap (not only when
            // already exhausted).
            let epoch_file_count = projected_epoch_file_count(&self.progress, &self.modified_files);
            let over_budget = if self.executor.registry.runs_command(&call.name)
                && self
                    .executor
                    .step_limits
                    .max_commands
                    .is_some_and(|max| self.commands_run >= max)
            {
                // All shell paths count (including verify/acceptance-class runs).
                // Some(0) = hard exhausted (not unlimited).
                let cap = u64::from(self.executor.step_limits.max_commands.unwrap_or(0));
                Some((
                    format!("the {cap}-command budget is exhausted"),
                    BudgetExhaustion::new(
                        BudgetDimension::Commands,
                        u64::from(self.commands_run),
                        cap,
                    ),
                ))
            } else if self.executor.registry.mutates_files(&call.name) {
                file_budget_refusal(
                    self.executor.step_limits.max_modified_files,
                    epoch_file_count,
                    &call,
                    &self.progress,
                    &self.modified_files,
                )
                .map(|which| {
                    let cap = self.executor.step_limits.max_modified_files.unwrap_or(0) as u64;
                    (
                        which,
                        BudgetExhaustion::new(
                            BudgetDimension::ModifiedFiles,
                            epoch_file_count as u64,
                            cap,
                        ),
                    )
                })
            } else {
                None
            };
            if let Some((which, exhaustion)) = over_budget {
                let msg = format!("Refused: {which}. The run stops here.");
                self.budget_exceeded = Some((format!("Stopped: {which}."), exhaustion));
                denied_calls_this_round += 1;
                results[index] = Some(deny_call(&mut *self.observer, call, msg));
                continue;
            }
            // Write-ownership fence: a live claim is EXCLUSIVE against
            // everyone, the parent included. The registry is the one
            // authority, so this covers a late-bound child's own
            // claim_write_scope grant as well as a legacy Worker's
            // pre-claim — an edit there would race the child this agent
            // delegated to; integration waits for the settlement notice.
            //
            // A writer at depth 0 checks and commits in ONE locked
            // registry operation and holds the guard for the rest of the
            // call. Asking `conflicts_for` here and taking the guard later
            // left a window in which a background child — a separate task,
            // genuinely parallel on the multi-thread runtime — claimed the
            // same path: its claim saw an empty `parent_active` and was
            // granted, and the later guard marked without re-checking, so
            // both wrote one file.
            let mut _mutation_guard = None;
            if self.executor.registry.mutates_files(&call.name) {
                let owner_key = match (&self.executor.agent_id, self.executor.depth) {
                    (Some(id), _) => id.clone(),
                    (None, 0) => "parent".to_string(),
                    (None, depth) => format!("child-depth-{depth}"),
                };
                let targets = crate::authorization::mutation_targets(&call);
                let hits = if self.executor.depth == 0 {
                    match self
                        .executor
                        .ownership
                        .try_mutation_guard(&owner_key, &targets)
                    {
                        Ok(guard) => {
                            _mutation_guard = Some(guard);
                            Vec::new()
                        }
                        Err(conflicts) => conflicts,
                    }
                } else {
                    self.executor.ownership.conflicts_for(&targets, &owner_key)
                };
                if !hits.is_empty() {
                    let inside: Vec<String> = hits.iter().map(|c| c.path.clone()).collect();
                    let mut owners: Vec<String> = hits.iter().map(|c| c.owner.clone()).collect();
                    owners.dedup();
                    let msg = format!(
                        "Edit refused: {} belongs to the exclusive scope of \
                             still-running sub-agent(s) {}. Wait for the settlement \
                             notice and integrate its result instead of editing its \
                             files while it works; continue on other work meanwhile.",
                        inside.join(", "),
                        owners.join(", ")
                    );
                    denied_calls_this_round += 1;
                    results[index] = Some(deny_call(&mut *self.observer, call, msg));
                    continue;
                }
            }

            // Write authority (late-bound ownership): a child's effective
            // allowlist is what it has CLAIMED (plus any legacy pre-claim);
            // before its first grant that set is empty and EVERY mutating
            // tool is refused — do not wait to parse patch paths
            // (PB2_B_ORCH_1: Update File landed because an empty-target
            // miss skipped this fence). The parent stays unrestricted
            // (fenced above by others' claims).
            // An MCP tool's effect lands in a separate, unsandboxed
            // process and cannot be bounded by a claimed scope, so a
            // delegated agent may not reach one at all.
            if let Some(msg) = self.executor.refuse_unboundable_delegated_tool(&call) {
                denied_calls_this_round += 1;
                results[index] = Some(deny_call(&mut *self.observer, call, msg));
                continue;
            }
            if let Some(msg) = self.executor.refuse_unscoped_mutation(&call) {
                denied_calls_this_round += 1;
                results[index] = Some(deny_call(&mut *self.observer, call, msg));
                continue;
            }

            // Read-only, side-effect-free tools are deferred to the
            // concurrent batch below; mark the event so a UI can render them
            // as one parallel group.
            let parallel = self
                .executor
                .registry
                .get(&call.name)
                .map(|t| t.supports_parallel())
                .unwrap_or(false);
            (self.observer)(AgentEvent::ToolCall {
                id: call.id.as_str().to_string(),
                name: call.name.clone(),
                arguments: compact_json(&call.arguments),
                parallel,
            });
            collect_scoped_paths_from_call(&call, &mut self.scoped_paths);

            // A command may carry its own elevation for THIS call. Settling
            // it here — between the call event and the context — is what
            // makes approval and execution one round trip instead of two.
            let mut call_grants = TurnPermissionGrants::default();
            if is_escalatable_tool(&call.name)
                && let Some((reason, requested)) = parse_escalation(&call.arguments)
                // Already allowed for this turn: the user answered this
                // question, so the call runs under the turn grant unasked.
                && (requested.is_empty() || !self.turn_grants.covers(requested))
            {
                if requested.is_empty() {
                    // Malformed: no axis named. Refuse without interrupting
                    // the user — there is nothing to put in a prompt.
                    self.settle_refused_call(
                        &call,
                        escalation_missing_axis_message(),
                        &mut results,
                        index,
                    );
                    continue;
                }
                if self.progress.covers_denied_request(
                    requested.network,
                    requested.repository_git || requested.unrestricted_fs,
                ) {
                    self.settle_refused_call(
                        &call,
                        permission_already_denied_message(),
                        &mut results,
                        index,
                    );
                    continue;
                }
                let action = escalation_action(&call);
                let outcome = self
                    .executor
                    .decide_permission(
                        &call,
                        &action,
                        &reason,
                        requested,
                        GrantScope::SingleCall,
                        &cancellation,
                    )
                    .await?;
                match &outcome {
                    // "仅允许本次" elevates this call only, so the next call
                    // starts confined again; "本轮对话内允许" keeps the grant
                    // for the rest of the turn, as the option says.
                    PermissionRequestOutcome::Granted {
                        grants,
                        for_turn: true,
                        ..
                    } => {
                        self.turn_grants = self.turn_grants.merge(*grants);
                    }
                    PermissionRequestOutcome::Granted { grants, .. } => {
                        call_grants = *grants;
                    }
                    PermissionRequestOutcome::DeniedByUser { requested, .. } => {
                        self.progress.record_human_denial(
                            requested.network,
                            requested.repository_git || requested.unrestricted_fs,
                        );
                        (self.observer)(AgentEvent::ProgressUpdated {
                            ledger: self.progress.clone(),
                        });
                    }
                    _ => {}
                }
                // A refused elevation stops the command. Running it
                // unelevated would hand the model the same denial it was
                // already answering.
                if outcome.is_error() {
                    self.settle_refused_call(
                        &call,
                        outcome.message().to_string(),
                        &mut results,
                        index,
                    );
                    continue;
                }
            }

            // Build the effective context: turn grants from
            // request_permissions, plus this call's own escalation.
            let ctx = apply_turn_grants(
                self.executor.tool_context.clone(),
                self.turn_grants.merge(call_grants),
            );
            // Full epoch path set so tools count "new" files correctly
            // (re-edits of already-budgeted paths do not consume residual).
            let epoch_paths = epoch_modified_paths(&self.progress, &self.modified_files);
            let remaining_files = self
                .executor
                .step_limits
                .max_modified_files
                .map(|max| max.saturating_sub(epoch_paths.len()));
            let ctx = ctx.with_command_write_constraints(
                self.executor.effective_write_allowlist(),
                remaining_files,
                epoch_paths,
            );
            // What a concurrent sibling owns right now. A command's changes
            // are attributed by diffing the whole workspace, which in a
            // shared tree also sees the sibling's writes; this call cannot
            // have made them, so they must not be charged here and rolled
            // back with it.
            let owner_key = match (&self.executor.agent_id, self.executor.depth) {
                (Some(id), _) => id.clone(),
                (None, 0) => "parent".to_string(),
                (None, depth) => format!("child-depth-{depth}"),
            };
            let ctx = ctx.with_foreign_owned_paths(
                self.executor.ownership.paths_owned_by_others(&owner_key),
            );

            // Admission: the ToolHost pipeline (side-effect barrier →
            // hooks → rules → policy → approval → barrier). Execution
            // requires the AdmittedCall it returns; `parallel` (computed
            // above) decides deferral to the batch — every other admitted
            // call runs here, in order.
            // §19: while the parent executes a mutation, an overlapping
            // child claim is denied retryably. The guard was acquired
            // atomically with the ownership check above and lives until
            // this call resolves — taking it here instead would reopen the
            // window it exists to close.
            let (
                content,
                is_error,
                image,
                workspace_snapshot,
                plan,
                newly_modified,
                call_files,
                _executed_commands,
                applied_diff,
                command_facts,
                call,
            ) = match self
                .executor
                .admit(
                    call,
                    ctx,
                    parallel,
                    &mut self.session_approved,
                    &cancellation,
                )
                .await
            {
                Ok(admitted) if parallel => {
                    if admitted.call.name != "update_plan" && !self.plan_state.is_empty() {
                        self.plan_needs_reconciliation = true;
                    }
                    if admitted.call.name != "update_plan" {
                        actual_work_admitted_this_batch = true;
                    }
                    if self.executor.registry.runs_command(&admitted.call.name) {
                        self.commands_run += 1;
                    }
                    parallel_jobs.push(ParallelJob { index, admitted });
                    continue;
                }
                Ok(admitted) => {
                    if admitted.call.name != "update_plan" && !self.plan_state.is_empty() {
                        self.plan_needs_reconciliation = true;
                    }
                    if admitted.call.name != "update_plan" {
                        actual_work_admitted_this_batch = true;
                    }
                    if self.executor.registry.runs_command(&admitted.call.name) {
                        self.commands_run += 1;
                    }
                    let files_before = self.modified_files.clone();
                    // Heartbeat: while a long command runs, emit a
                    // CommandProgress every few seconds so the UI shows
                    // "运行 cargo test · <elapsed>" instead of a bare
                    // "等待模型". select! keeps this in the same task, so
                    // calling `observer` from the ticker branch is sound.
                    let progress_label =
                        command_progress_label(&self.executor.registry, &admitted.call);
                    let (
                        content,
                        is_error,
                        image,
                        workspace_snapshot,
                        plan,
                        call_files,
                        executed_commands,
                        applied_diff,
                        command_facts,
                    ) = {
                        let started = std::time::Instant::now();
                        // A command's live output is drained here, in the loop's
                        // own task, so it reaches the observer in order with
                        // the call's other events.
                        let call_id = admitted.call.id.as_str().to_string();
                        let (output_tx, mut output_rx) =
                            if self.executor.registry.runs_command(&admitted.call.name) {
                                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                                (Some(tx), Some(rx))
                            } else {
                                (None, None)
                            };
                        let mut lines = super::dispatch::OutputLines::default();
                        let dispatch_fut = self.executor.dispatch(
                            &admitted,
                            &mut self.modified_files,
                            &cancellation,
                            output_tx,
                        );
                        tokio::pin!(dispatch_fut);
                        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(
                            COMMAND_HEARTBEAT_SECS,
                        ));
                        ticker.tick().await; // drop the immediate first tick
                        let result = loop {
                            tokio::select! {
                                r = &mut dispatch_fut => break r,
                                Some(chunk) = async {
                                    match output_rx.as_mut() {
                                        Some(rx) => rx.recv().await,
                                        None => std::future::pending().await,
                                    }
                                } => {
                                    if let Some((stream, text)) = lines.push(chunk) {
                                        (self.observer)(AgentEvent::ToolOutput {
                                            id: call_id.clone(),
                                            stream,
                                            text,
                                        });
                                    }
                                }
                                _ = ticker.tick() => {
                                    if let Some(label) = &progress_label {
                                        (self.observer)(AgentEvent::CommandProgress {
                                            label: label.clone(),
                                            elapsed_ms: started.elapsed().as_millis() as u64,
                                        });
                                    }
                                }
                            }
                        };
                        // The call has returned, so every sender is gone: what
                        // is still queued is the tail of its output.
                        if let Some(rx) = output_rx.as_mut() {
                            while let Ok(chunk) = rx.try_recv() {
                                if let Some((stream, text)) = lines.push(chunk) {
                                    (self.observer)(AgentEvent::ToolOutput {
                                        id: call_id.clone(),
                                        stream,
                                        text,
                                    });
                                }
                            }
                        }
                        for (stream, text) in lines.flush() {
                            (self.observer)(AgentEvent::ToolOutput {
                                id: call_id.clone(),
                                stream,
                                text,
                            });
                        }
                        let mut result = result;
                        // A stopped command's result is only "cancelled"; the
                        // output the user already watched is what tells the
                        // model how far it got.
                        if result.8.1.is_some()
                            && let Some(tail) = lines.stopped_tail()
                        {
                            result.0.push_str(&format!(
                                "\n\nOutput before it stopped (last lines):\n{tail}"
                            ));
                        }
                        result
                    };
                    // Cancel during a long tool must stop the batch — do not
                    // keep running subsequent tools after the user hit Ctrl+C.
                    // This call's own result still flows through: it ran, so
                    // its outcome and spend must reach the transcript/ledger.
                    if cancellation.is_cancelled() && !rt.deadline_expired() {
                        cancelled_mid_batch = true;
                    }
                    let newly = newly_modified_paths(&files_before, &self.modified_files);
                    (
                        content,
                        is_error,
                        image,
                        workspace_snapshot,
                        plan,
                        newly,
                        call_files,
                        executed_commands,
                        applied_diff,
                        command_facts,
                        admitted.into_call(),
                    )
                }
                Err(AdmitError::Fatal(error)) => return Err(error),
                Err(AdmitError::Refused { call, reason }) => {
                    denied_calls_this_round += 1;
                    (
                        format!("action not permitted: {reason}"),
                        true,
                        None,
                        None,
                        None,
                        Vec::new(),
                        Vec::new(),
                        Vec::new(),
                        None,
                        (None, None),
                        call,
                    )
                }
            };
            // The call's own report of what it touched. Kept as paths, not
            // collapsed to a bool: a re-edit of a file already in the set
            // has an empty first-touch delta and would otherwise be
            // invisible to the ledger.
            let call_mutated = !call_files.is_empty();
            if let Some(snapshot) = workspace_snapshot {
                (self.observer)(AgentEvent::WorkspaceSnapshot {
                    call_id: call.id.as_str().to_string(),
                    snapshot,
                });
            }
            if let Some(part) = image {
                pending_images.push(part);
            }

            // A plan update is the model's declaration: the host mirror takes
            // any structurally valid list as sent (order is intent, not a
            // rule) and only advances on success.
            let mut content = content;
            let mut is_error = is_error;
            if let Some(steps) = plan
                && !is_error
            {
                match PlanState::from_model_explicit(steps) {
                    Ok(next) => {
                        self.plan_state = next;
                        if !actual_work_admitted_this_batch {
                            self.plan_needs_reconciliation = false;
                        }
                        (self.observer)(AgentEvent::PlanUpdated {
                            steps: self.plan_state.steps.clone(),
                        });
                    }
                    Err(msg) => {
                        content = msg;
                        is_error = true;
                    }
                }
            }

            (self.observer)(AgentEvent::ToolResult {
                exit_code: command_facts.0,
                stop: command_facts.1,
                id: call.id.as_str().to_string(),
                name: call.name.clone(),
                is_error,
                preview: preview(&content),
                // An edit's real landing site, straight from the tool that
                // made it. A failed call never carries one.
                applied_diff: (!is_error).then_some(applied_diff).flatten(),
            });

            // Any tool that newly modified files records a mutation (not
            // only apply_patch/replace by name). Paths are this call only.
            if !is_error && call_mutated {
                // Record what THIS call touched. `newly_modified` is the
                // first-touch delta and is empty on a re-edit; `call_files`
                // is the call's own report. Together they name every path
                // this change reached, so scope and impact see re-edits.
                let mut touched = newly_modified;
                for path in &call_files {
                    if !touched.iter().any(|p| p == path) {
                        touched.push(path.clone());
                    }
                }
                note_tool_side_effects(
                    &mut self.ledger,
                    call.id.as_str(),
                    call.name.as_str(),
                    touched,
                    &self.plan_state,
                    &mut *self.observer,
                );
            }
            for path in &self.modified_files {
                push_unique_path(&mut self.scoped_paths, path);
            }

            results[index] = Some(ContentPart::ToolResult {
                result: ToolResultContent {
                    call_id: call.id,
                    // Already size-capped centrally by `ToolRegistry::execute`.
                    content,
                    is_error,
                },
            });
            if cancelled_mid_batch {
                break;
            }
        }

        // Run the deferred read-only tools concurrently, then fold their
        // results back into their original call slots so the transcript
        // stays in call order regardless of completion order.
        if !parallel_jobs.is_empty() && !cancelled_mid_batch {
            use futures::stream::{FuturesUnordered, StreamExt};
            let cancellation_ref = &cancellation;
            let executor = self.executor;
            // Leveling knob: bound how many of the batch actually overlap
            // (policy `max_parallel_tools`; 0 = the whole batch at once).
            let permits = match self.executor.policy.max_parallel_tools {
                0 => parallel_jobs.len(),
                n => n,
            };
            let sem = Arc::new(tokio::sync::Semaphore::new(permits.max(1)));
            let mut futs = FuturesUnordered::new();
            for job in &parallel_jobs {
                let sem = sem.clone();
                futs.push(async move {
                    let _permit = sem
                        .acquire()
                        .await
                        .expect("tool-batch semaphore is never closed");
                    let out = executor
                        .dispatch_raw(&job.admitted, cancellation_ref, None)
                        .await;
                    (job.index, out)
                });
            }
            let mut raw: std::collections::HashMap<usize, (String, bool, serde_json::Value)> =
                std::collections::HashMap::new();
            while let Some((idx, out)) = futs.next().await {
                raw.insert(idx, out);
            }
            drop(futs);

            for job in &parallel_jobs {
                let (content, is_error, metadata) = raw
                    .remove(&job.index)
                    .expect("every parallel job produced a result");
                let mut job_files = Vec::new();
                collect_modified(&metadata, &mut job_files);
                for f in &job_files {
                    if !self.modified_files.iter().any(|e| e == f) {
                        self.modified_files.push(f.clone());
                    }
                }
                if !is_error && !job_files.is_empty() {
                    note_tool_side_effects(
                        &mut self.ledger,
                        job.admitted.call.id.as_str(),
                        job.admitted.call.name.as_str(),
                        job_files.clone(),
                        &self.plan_state,
                        &mut *self.observer,
                    );
                }
                if let Some(part) = extract_image(&metadata) {
                    pending_images.push(part);
                }
                let (exit_code, stop) = super::dispatch::extract_command_facts(&metadata);
                (self.observer)(AgentEvent::ToolResult {
                    exit_code,
                    stop,
                    id: job.admitted.call.id.as_str().to_string(),
                    name: job.admitted.call.name.clone(),
                    is_error,
                    preview: preview(&content),
                    applied_diff: (!is_error)
                        .then(|| extract_applied_diff(&metadata))
                        .flatten(),
                });
                if !is_error && let Some(steps) = extract_plan(&metadata) {
                    (self.observer)(AgentEvent::PlanUpdated { steps });
                }
                for path in &self.modified_files {
                    push_unique_path(&mut self.scoped_paths, path);
                }
                results[job.index] = Some(ContentPart::ToolResult {
                    result: ToolResultContent {
                        call_id: job.admitted.call.id.clone(),
                        // Already size-capped centrally by `ToolRegistry::execute`.
                        content,
                        is_error,
                    },
                });
            }
        }

        // Concurrent sub-agent batch (CC-style star delegation): several
        // spawn_agent calls in one round run in parallel, bounded by
        // max_concurrent_agents. Each sub-agent's result folds into its call
        // slot; start/finish bubbles to the observer for the UI.
        if !spawn_jobs.is_empty() && !cancelled_mid_batch {
            use futures::stream::{FuturesUnordered, StreamExt};
            let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
            let executor = self.executor;
            let mut futs = FuturesUnordered::new();
            // Parent may have run shells/edits in this same tool batch
            // before children. Pin that spend on the ledger now — otherwise
            // absorb_child_work + `commands_run = progress.cumulative_*`
            // overwrites local counters with a lagging ledger (mixed batch
            // under-counts parent commands).
            pin_parent_batch_work(&mut self.progress, self.commands_run, &self.modified_files);
            // Pass 1: reject invalid spawns. Pass 2: split residual only
            // across *accepted* children (rejected slots must not dilute
            // the share — and must not let accepted children oversell).
            #[allow(clippy::type_complexity)]
            let mut accepted: Vec<(
                usize,
                leveler_core::ToolCallId,
                AgentRole,
                Vec<String>,
                String,
                String,
                String,
                leveler_lifecycle::ChildSpawnSpec,
                Option<String>,
                bool, // run in background (runtime-resolved default: true)
                CancellationToken,
            )> = Vec::new();
            // Loaded on the first call in this batch that names an agent.
            let mut agent_registry: Option<crate::agent_registry::AgentRegistry> = None;
            // Exclusive scopes of workers already admitted in THIS batch.
            // "Exclusive" is only true if admission enforces it: two
            // overlapping scopes in one batch is last-writer-wins waiting
            // to happen, so the second one is refused honestly.
            let mut admitted_worker_scopes: Vec<Vec<String>> = Vec::new();
            for (index, call) in spawn_jobs {
                let background = crate::injected_tools::resolve_run_in_background(&call.arguments);
                let task = call
                    .arguments
                    .get("task")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                // Spawn-time task identity, distinct from the instructions in
                // `task`. Absent on calls from clients that predate it; the
                // renderer then falls back to a projection of the task.
                let title = call
                    .arguments
                    .get("title")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                let agent_name = call
                    .arguments
                    .get("agent")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let profile_arg = call
                    .arguments
                    .get("profile")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let explicit_role = call.arguments.get("role").and_then(|v| v.as_str());
                let files: Vec<String> = call
                    .arguments
                    .get("files")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                // A capability field that is present but not a string is not
                // "omitted": read that way it would buy the default child, a
                // writer.
                let malformed_capability = ["role", "profile", "agent"].into_iter().find(|field| {
                    call.arguments
                        .get(*field)
                        .is_some_and(|v| !v.is_string() && !v.is_null())
                });
                // Declarative agents are read once per batch, and only when a
                // call names one: a definition edited between two batches
                // applies to the later spawn, never to a running child.
                if agent_name.is_some() && agent_registry.is_none() {
                    agent_registry = Some(self.executor.load_agent_registry());
                }
                // Capability negotiation: the requested agent/profile/role +
                // scope against the contract. Honest denial, never a silent
                // downgrade.
                let admission = if malformed_capability.is_some() {
                    Err(String::new())
                } else {
                    self.executor
                        .admit_spawn_call(
                            agent_name,
                            profile_arg,
                            explicit_role,
                            &files,
                            agent_registry.as_ref(),
                        )
                        .await
                };

                // Reject (no agent started) for depth, empty task, or cap.
                let reject = if let Some(field) = malformed_capability {
                    Some(format!(
                        "spawn_agent `{field}` must be a string (or omitted); it was {}.",
                        call.arguments[field]
                    ))
                } else if self.executor.depth >= MAX_SUB_AGENT_DEPTH {
                    Some("Sub-agents may not spawn their own sub-agents.".to_string())
                } else if task.is_empty() {
                    Some("spawn_agent requires a non-empty task.".to_string())
                } else if let Err(msg) = &admission {
                    Some(msg.clone())
                } else if admission
                    .as_ref()
                    .is_ok_and(|a| a.profile.role == AgentRole::Worker)
                    && admitted_worker_scopes
                        .iter()
                        .any(|scope| scopes_overlap(scope, &files))
                {
                    Some(format!(
                        "Worker scope {} overlaps a worker already admitted in this \
                             batch. Parallel workers must own DISJOINT files; fold the \
                             overlapping work into one worker or re-scope it.",
                        files.join(", ")
                    ))
                } else if admission
                    .as_ref()
                    .is_ok_and(|a| a.profile.role == AgentRole::Worker)
                    && !self.executor.ownership.conflicts_for(&files, "").is_empty()
                {
                    // Legacy pre-scoped Worker: its files are an exclusive
                    // claim in the SAME registry late-bound children use —
                    // a conflict with any live claim is an honest denial.
                    let hits = self.executor.ownership.conflicts_for(&files, "");
                    Some(format!(
                        "Worker scope {} overlaps exclusive ownership held by {}. \
                             Wait for its settlement notice, or scope this worker to \
                             disjoint files.",
                        files.join(", "),
                        hits.iter()
                            .map(|c| c.owner.clone())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                } else if (self.progress.children_spawned_total as usize)
                    >= self.executor.policy.max_total_agents
                {
                    // MA-RT-1: the durable task-epoch total, not a
                    // drive-local counter — a turn, window, or restart
                    // boundary must not hand the model a fresh quota.
                    Some(format!(
                        "Sub-agent limit reached ({} total for this task). Do the \
                             remaining work directly.",
                        self.executor.policy.max_total_agents
                    ))
                } else {
                    None
                };
                if let Some(msg) = reject {
                    (self.observer)(AgentEvent::ToolResult {
                        exit_code: None,
                        stop: None,
                        id: call.id.as_str().to_string(),
                        name: SPAWN_AGENT_TOOL.to_string(),
                        is_error: true,
                        preview: msg.clone(),
                        applied_diff: None,
                    });
                    results[index] = Some(ContentPart::ToolResult {
                        result: ToolResultContent {
                            call_id: call.id,
                            content: msg,
                            is_error: true,
                        },
                    });
                    continue;
                }

                let Ok(SpawnAdmission {
                    profile,
                    spec: admitted_spec,
                    brief,
                }) = admission
                else {
                    unreachable!("admission already succeeded");
                };
                let role = profile.role;
                if role == AgentRole::Worker {
                    admitted_worker_scopes.push(files.clone());
                }
                self.progress.children_spawned_total += 1;
                let id = new_delegated_agent_id();
                let nickname = agent_nickname(self.progress.children_spawned_total as usize);
                self.executor
                    .ownership
                    .register_owner(&id, &format!("{nickname} ({id})"));
                if role == AgentRole::Worker && !files.is_empty() {
                    // Atomic legacy pre-claim; admission above already
                    // verified there is no live conflict, and this batch's
                    // earlier spawns claim before later ones are admitted.
                    if let Err(rejection) = self.executor.ownership.try_claim(&id, &files) {
                        let msg = rejection.for_model();
                        self.executor.ownership.release_all(&id);
                        (self.observer)(AgentEvent::ToolResult {
                            exit_code: None,
                            stop: None,
                            id: call.id.as_str().to_string(),
                            name: SPAWN_AGENT_TOOL.to_string(),
                            is_error: true,
                            preview: preview(&msg),
                            applied_diff: None,
                        });
                        results[index] = Some(ContentPart::ToolResult {
                            result: ToolResultContent {
                                call_id: call.id,
                                content: msg,
                                is_error: true,
                            },
                        });
                        self.progress.children_spawned_total =
                            self.progress.children_spawned_total.saturating_sub(1);
                        continue;
                    }
                }
                let started_task = if role == AgentRole::Worker && !files.is_empty() {
                    format!("{task}\n[scope: {}]", files.join(", "))
                } else {
                    task.clone()
                };
                (self.observer)(AgentEvent::ProgressUpdated {
                    ledger: self.progress.clone(),
                });
                let (profile_id, profile_role, read_only) = profile.trace_fields();
                let spec = leveler_lifecycle::ChildSpawnSpec {
                    title,
                    files: files.clone(),
                    background,
                    ..admitted_spec
                };
                // The host holds the child's cancel handle before anyone can
                // see the child: a client that reacts to the start event with
                // "stop this child" must find it.
                let token = cancellation.child_token();
                if let Some(host) = &self.executor.steering {
                    host.child_started(&id, token.clone());
                }
                (self.observer)(AgentEvent::SubAgentStarted {
                    id: id.clone(),
                    nickname: nickname.clone(),
                    role: role.label().to_string(),
                    task: started_task,
                    profile_id: Some(profile_id),
                    profile_role: Some(profile_role),
                    read_only,
                    spec: Some(spec.clone()),
                });
                if !self.plan_state.is_empty() {
                    self.plan_needs_reconciliation = true;
                }
                accepted.push((
                    index, call.id, role, files, task, id, nickname, spec, brief, background, token,
                ));
            }
            // A child may only begin once its `SubAgentStarted` is durable
            // — the same truth-before-effect rule tool dispatch already
            // follows. Without the flush, a crash between spawn and the
            // next pump commit leaves children running that no durable
            // record can reconcile. A flush failure aborts the run before
            // any accepted child executes (fail closed); hosts without
            // persistence leave the barrier unset and proceed as before.
            if !accepted.is_empty()
                && let Some(barrier) = &self.executor.event_barrier
                && let Err(error) = barrier.flush().await
            {
                if let Some(host) = &self.executor.steering {
                    for accepted_child in &accepted {
                        host.child_ended(&accepted_child.5);
                    }
                }
                return Err(error.into());
            }
            let share_n = accepted.len() as u32;
            // Foreground children's own tokens, so an error in this batch can
            // stop and wait for them instead of dropping them mid-write.
            let mut foreground_tokens: Vec<CancellationToken> = Vec::new();
            for (
                share_of,
                (index, call_id, role, files, task, id, nickname, spec, brief, background, token),
            ) in accepted.into_iter().enumerate()
            {
                let sem = self.run_agents_semaphore.clone();
                // Residual parent budgets split across concurrent spawns.
                let residual = residual_step_limits(
                    self.executor.step_limits,
                    self.commands_run,
                    model_tokens_spent,
                    cost_spent_micros,
                    projected_epoch_file_count(&self.progress, &self.modified_files),
                    self.epoch_duration_at_start,
                    rt.run_started(),
                    share_of as u32,
                    share_n,
                );
                let parent_wall = super::handlers::ParentWallBudget {
                    cap: self.executor.step_limits.max_duration,
                    epoch_duration_at_start: self.epoch_duration_at_start,
                    run_started: rt.run_started(),
                };
                if background {
                    // V2 background-first: spawn the owned child future and
                    // return the tool result immediately. Settlement is
                    // injected at a later round boundary; the child's
                    // progress/activity events flow through the run-level
                    // channel.
                    let fut = self.executor.sub_agent_run_future(
                        id.clone(),
                        role,
                        spec,
                        brief,
                        task,
                        sem,
                        self.bg_progress_tx.clone(),
                        residual,
                        token.clone(),
                        parent_wall,
                    );
                    let handle = tokio::spawn(fut);
                    self.progress.outstanding_children.push(format!(
                        "{id}|{nickname}|{}|{}",
                        role.label(),
                        files.join(",")
                    ));
                    (self.observer)(AgentEvent::ProgressUpdated {
                        ledger: self.progress.clone(),
                    });
                    let scope_line = if files.is_empty() {
                        String::new()
                    } else {
                        format!(
                            " It exclusively owns {} — your own edits there are \
                                 refused until it settles.",
                            files.join(", ")
                        )
                    };
                    let content = format!(
                        "[sub-agent {nickname} ({id}, role={})] started in the \
                             background.{scope_line} You will be told when it settles — \
                             continue useful work; do not poll.",
                        role.label()
                    );
                    // The immediate acknowledgment is model-visible, so it
                    // must be UI-visible too (P2 disclosure): pair the
                    // spawn call with its result like any other tool.
                    (self.observer)(AgentEvent::ToolResult {
                        exit_code: None,
                        stop: None,
                        id: call_id.as_str().to_string(),
                        name: SPAWN_AGENT_TOOL.to_string(),
                        is_error: false,
                        preview: preview(&content),
                        applied_diff: None,
                    });
                    self.background_children.children.push(BackgroundChild {
                        id,
                        nickname,
                        role,
                        scope: files,
                        token,
                        handle,
                    });
                    results[index] = Some(ContentPart::ToolResult {
                        result: ToolResultContent {
                            call_id,
                            content,
                            is_error: false,
                        },
                    });
                    continue;
                }
                let progress_ch = progress_tx.clone();
                foreground_tokens.push(token.clone());
                futs.push(async move {
                    let result = executor
                        .run_one_sub_agent_on(
                            id.clone(),
                            role,
                            spec,
                            brief,
                            task,
                            sem,
                            progress_ch,
                            residual,
                            token,
                            parent_wall,
                        )
                        .await;
                    (index, call_id, id, nickname, role, result)
                });
            }
            drop(progress_tx);

            let mut batch_error: Option<AgentError> = None;
            while !futs.is_empty() {
                tokio::select! {
                    biased;
                    Some(progress_ev) = progress_rx.recv(), if batch_error.is_none() => {
                        if let Err(error) = self.forward_child_event(rt, progress_ev).await {
                            // The batch is failing. Stop every child still
                            // running and keep settling them below, so each
                            // ends, is released after it stopped, and is
                            // ended for the host before the error returns.
                            for child_token in &foreground_tokens {
                                child_token.cancel();
                            }
                            batch_error = Some(error);
                        }
                    }
                    Some((index, call_id, id, nickname, role, result)) = futs.next() => {
                        self.executor.ownership.release_all(&id);
                        if let Some(host) = &self.executor.steering {
                            host.child_ended(&id);
                        }
                        let (content, ok) = fold_child_settlement(
                            &mut self.progress,
                            &mut self.commands_run,
                            &mut self.modified_files,
                            &mut self.ledger,
                            &mut *self.observer,
                            &id,
                            &nickname,
                            role,
                            &result,
                        );
                        results[index] = Some(ContentPart::ToolResult {
                            result: ToolResultContent {
                                call_id,
                                // Sub-agent results bypass the registry, so apply
                                // the central cap here.
                                content: leveler_tools::registry::cap_output_with(
                                    &content,
                                    self.executor.tool_context.policy.tool_output_budget,
                                ),
                                is_error: !ok,
                            },
                        });
                    }
                }
            }
            if let Some(error) = batch_error {
                return Err(error);
            }
            while let Ok(event) = progress_rx.try_recv() {
                self.forward_child_event(rt, event).await?;
            }
            drop(futs);
            // Foreground results reach the transcript with this round's tool
            // message below; their terminals must be durable first.
            self.settlements_durable().await?;
            // Always flush after sub-agent batch so absorbed spend is durable
            // even when the parent is about to cancel.
            self.flush_epoch(rt);
            if cancellation.is_cancelled() && !rt.deadline_expired() {
                // All sub-agent results are folded in by now; commit them
                // with the round below before surfacing Cancelled.
                cancelled_mid_batch = true;
            }
        }

        // Cancel cut the batch short: refuse every remaining call in place
        // so each tool_use still gets a paired result in the transcript.
        if cancelled_mid_batch {
            for (index, slot) in results.iter_mut().enumerate() {
                if slot.is_none() {
                    *slot = Some(deny_call(
                        &mut *self.observer,
                        call_snapshot[index].clone(),
                        "cancelled by the user before this call ran".to_string(),
                    ));
                }
            }
        }

        let results: Vec<ContentPart> = results
            .into_iter()
            .map(|r| r.expect("every tool call produced a result"))
            .collect();

        let tool_message = Message {
            role: Role::Tool,
            content: results,
        };
        messages.push(tool_message.clone());
        // Flush spend BEFORE transcript persistence: tools already ran, so
        // a sink I/O failure must not drop this batch's command/file totals.
        self.flush_epoch(rt);
        self.sink.append(&[assistant, tool_message]).await?;
        // Batch was cancelled: the round is durable (results paired, spend
        // flushed above) — exit now instead of starting another model round.
        if cancelled_mid_batch {
            self.drain_background_children(rt, messages).await?;
            return Err(AgentError::Cancelled);
        }

        // V2: forward background children's live progress and settle any
        // that finished — the notice must be in context before the next
        // model round ("you are told when one finishes"; no polling).
        self.settle_finished_children(rt, messages).await?;

        // Plan fully completed → closing phase (a lifecycle fact for the
        // UI and for continuation seeding; nothing is refused for it).
        if self.plan_state.is_fully_completed() {
            self.progress.enter_closing();
        }
        // Mechanical no-progress watchdog: a round in which EVERY attempted
        // call was refused before it ran is not progress. Enough of them in
        // a row and the turn stops, so an `UntilTerminal` run cannot spin
        // forever re-issuing guarded actions. Nothing here reads the
        // model's work for meaning: rounds with executed tools — failed or
        // not — always count as progress.
        // Unconditional: a run in which every attempted action is refused is
        // making no progress, and stopping is lifecycle correctness rather
        // than advice to the model. Nothing here reads a policy flag, because
        // a safety boundary is not an experiment.
        let all_refused =
            !call_snapshot.is_empty() && denied_calls_this_round == call_snapshot.len();
        if all_refused {
            self.progress.note_no_progress_round(model_steps);
            (self.observer)(AgentEvent::ProgressUpdated {
                ledger: self.progress.clone(),
            });
            if self.executor.depth == 0
                && self
                    .progress
                    .should_hard_stop_no_progress(self.progress_caps)
            {
                return Ok(Flow::Stop(
                    self.stop_now(
                        rt,
                        messages,
                        StopReason::Incomplete,
                        "no-progress streak; all-refused model steps short-circuited",
                        "Stopped: no progress (every attempted action was refused).",
                    )
                    .await?,
                ));
            }
        } else if !call_snapshot.is_empty() {
            self.progress.note_progress(model_steps);
        }

        // Goal mode: an explicit update_goal this round ends the run now that
        // its result is committed.
        if let Some((reason, summary)) = goal_resolution {
            let final_text = if summary.is_empty() {
                self.last_text.clone()
            } else {
                summary
            };
            // Epoch terminal: next Content turn must not inherit Closing state.
            if matches!(reason, StopReason::Completed | StopReason::Blocked) {
                self.progress.enter_closed();
                (self.observer)(AgentEvent::ProgressUpdated {
                    ledger: self.progress.clone(),
                });
            }
            (self.observer)(AgentEvent::Finished(final_text.clone()));
            self.enter_finalization();
            self.flush_epoch(rt);

            self.settle_finalization_dependencies(rt, messages).await?;
            return Ok(Flow::Stop(AgentOutcome::drive_result(
                final_text,
                model_steps,
                self.modified_files.clone(),
                reason,
                None,
                &self.progress,
                &self.objective,
            )));
        }

        // A step limit tripped this round: results are committed, stop now.
        if let Some((reason, exhaustion)) = self.budget_exceeded.take() {
            (self.observer)(AgentEvent::Finished(reason.clone()));
            self.enter_finalization();
            self.flush_epoch(rt);

            self.settle_finalization_dependencies(rt, messages).await?;
            return Ok(Flow::Stop(AgentOutcome::drive_budget_exhausted(
                reason,
                model_steps,
                self.modified_files.clone(),
                exhaustion,
                &self.progress,
                &self.objective,
            )));
        }

        // Surface any images loaded this round to the model (image content
        // parts live in a user message, not a tool result, for OpenAI-style
        // providers).
        if !pending_images.is_empty() {
            let image_message = Message {
                role: Role::User,
                content: pending_images,
            };
            messages.push(image_message.clone());
            self.sink.append(&[image_message]).await?;
        }

        if has_next_model_step
            && should_inject_delivery_convergence_nudge(
                self.executor.policy.goal_mode,
                self.delivery_convergence_nudged,
                model_steps,
                !self.modified_files.is_empty(),
                !call_snapshot.is_empty(),
            )
        {
            let nudge = Message::text(
                Role::User,
                "DELIVERY CONVERGENCE: If the objective asks for workspace edits and the source reads above reveal a concrete requested edit, the next tool call must make that edit. Do not inspect more files, git state, test-runner configuration, temporary/probe/audit artifacts, or run exploratory probes first. Write the requested test/change, then use its result to discover remaining edge cases. If the objective is genuinely read-only analysis, ignore this message.",
            );
            self.sink.append(std::slice::from_ref(&nudge)).await?;
            messages.push(nudge);
            self.delivery_convergence_nudged = true;
        }

        // Auto-compaction (spec §53): when the context size exceeds the
        // budget, fold the in-memory transcript before the next request so a
        // long task never overflows the window. Prefer the provider's
        // reported token count, but fall back to a char/4 estimate — many
        // gateways don't report streaming usage, and without a fallback
        // compaction would silently never fire. The persisted transcript
        // (sink) is untouched — only what we resend shrinks.
        let context_tokens = self
            .last_model_call_usage_total
            .max(estimate_tokens(messages));
        // Fold when the last request's estimate crossed the budget. One
        // threshold, one action: the runtime does not read the model's
        // re-reads as evidence that it "deserves" a bigger window.
        let over_budget = self.executor.policy.context_budget > 0
            && context_tokens > u64::from(self.executor.policy.context_budget);
        if has_next_model_step && over_budget {
            let before = messages.len();
            // Cap the retained working set at half the live budget so a
            // huge recent tool output can't keep the fold over the window;
            // the other half leaves room for the head, summary, and next
            // response. (current_budget > 0 is guaranteed by the decision.)
            let keep_recent_tokens = u64::from(self.executor.policy.context_budget) / 2;
            // Name this extra round trip so the UI shows "compacting…" instead
            // of a bare "waiting for model" during the summary call.
            (self.observer)(AgentEvent::AdvisoryStarted {
                kind: AdvisoryKind::ContextCompaction,
            });
            // Compaction discards detail by design; this is the only moment
            // a project can keep something first (export it, push it to
            // memory) without the loop guessing what matters.
            if self.executor.hook_runner.has_lifecycle() {
                self.executor
                    .hook_runner
                    .run_lifecycle(
                        leveler_execution::LifecycleEvent::PreCompact,
                        &format!(
                            r#"{{"context_tokens":{context_tokens},"budget":{}}}"#,
                            self.executor.policy.context_budget
                        ),
                        &cancellation,
                    )
                    .await;
            }
            let summarized = self
                .executor
                .summarize_for_compaction(
                    messages,
                    COMPACT_KEEP_RECENT,
                    keep_recent_tokens,
                    &cancellation,
                )
                .await;
            // A fold's summarization is a provider call, and until it was
            // recorded a session that folded reported fewer tokens than it
            // spent — precisely in the lane a fold is the cost of.
            if let Some(summarized) = &summarized {
                self.record_request(
                    rt,
                    ModelRequestRecord {
                        provider_request_id: Some(summarized.request_id.to_string()),
                        provider: self.executor.model.provider.clone(),
                        model: self.executor.model.model.clone(),
                        usage: summarized.usage,
                        finish_reason: summarized.finish_reason,
                        latency_ms: summarized.latency_ms,
                        retry_count: 0,
                        kind: crate::ModelCallKind::Compaction,
                        agent_id: None,
                        cost_usd_micros: None,
                        reasoning_effort: None,
                    }
                    .priced(self.executor.pricing.as_ref()),
                    None,
                )
                .await?;
            }
            let mut summary = summarized.map(|s| s.text);
            // Long-goal P3: before old context is folded away, the host
            // cuts a durable checkpoint and hands back its context block
            // — the fold's summary becomes persisted truth. If the
            // checkpoint cannot be made durable, we keep the context this
            // round instead of dropping continuity (fail closed); the
            // fold retries at the next boundary.
            let mut fold_permitted = true;
            if let Some(port) = &self.executor.compaction_checkpoint {
                match port.checkpoint_before_compaction(summary.as_deref()).await {
                    Ok(Some(block)) => summary = Some(block),
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            "goal checkpoint failed; keeping context uncompacted this model step"
                        );
                        fold_permitted = false;
                    }
                }
            }
            if fold_permitted {
                *messages = compact_messages(
                    messages,
                    COMPACT_KEEP_RECENT,
                    keep_recent_tokens,
                    summary.as_deref(),
                    Some(self.objective.text()),
                );
                self.context_diverged = true;
                // Record the fold as estimated tokens so the accounting can
                // show `before → after → reclaimed` without the TUI deriving
                // it from a second transcript.
                let after_tokens = estimate_tokens(messages);
                if let Ok(mut slot) = self.compaction.lock() {
                    *slot = Some(CompactionRecord {
                        before_tokens: context_tokens,
                        after_tokens,
                    });
                }
            }
            if self.executor.hook_runner.has_lifecycle() {
                self.executor
                    .hook_runner
                    .run_lifecycle(
                        leveler_execution::LifecycleEvent::PostCompact,
                        &format!(
                            r#"{{"messages_before":{before},"messages_after":{}}}"#,
                            messages.len()
                        ),
                        &cancellation,
                    )
                    .await;
            }
            if messages.len() < before {
                (self.observer)(AgentEvent::Compacted {
                    from: before,
                    to: messages.len(),
                });
            }
        }

        // Persist the exact next-request context through the engine event
        // log — but only when it holds something the append-only raw
        // transcript does not: a compaction fold or a transient nudge.
        // A round that merely appended durable messages is reconstructed
        // from the transcript, and snapshotting it anyway made the log
        // grow by the whole context every round. `context_trace` (eval
        // measurement) restores the per-round copy on request.
        if has_next_model_step && (self.context_diverged || self.executor.policy.context_trace) {
            (self.observer)(AgentEvent::ContextSnapshot {
                messages: messages.clone(),
            });
            self.context_diverged = false;
        }
        Ok(Flow::Continue)
    }

    async fn on_stop(
        &mut self,
        rt: &mut LoopContext,
        stop: LoopStop,
    ) -> Result<AgentOutcome, AgentError> {
        let model_steps = stop.model_steps;
        let mut messages = stop.messages;
        match stop.reason {
            KernelStop::Cancelled => {
                // Flush epoch spend before Cancelled so resume/event-log keep
                // command/file/token totals (including absorbed children).
                self.flush_epoch(rt);
                self.drain_background_children(rt, &mut messages).await?;
                Err(AgentError::Cancelled)
            }
            // Unconditional circuit breaker: even a busy loop that evades
            // every progress watchdog terminates here.
            KernelStop::ModelStepCeiling { ceiling } => {
                let reason = format!(
                    "Stopped: reached the {ceiling} model-step safety ceiling for a single turn. \
                     This is a mechanical limit on the run's model/tool loop, not a task budget."
                );
                (self.observer)(AgentEvent::Finished(reason.clone()));
                self.enter_finalization();
                self.flush_epoch(rt);
                self.settle_finalization_dependencies(rt, &mut messages)
                    .await?;
                Ok(AgentOutcome::drive_result(
                    reason,
                    model_steps,
                    self.modified_files.clone(),
                    StopReason::TurnLimitReached,
                    Some(format!(
                        "model_step_ceiling={ceiling}; mechanical safety breaker, not a task budget"
                    )),
                    &self.progress,
                    &self.objective,
                ))
            }
            KernelStop::BudgetExhausted(exhaustion) => {
                let dimension = exhaustion.dimension;
                let reason = match dimension {
                    BudgetDimension::Cost => format!(
                        "Stopped: the {}-micro-USD model cost budget was exhausted after {model_steps} model step(s).",
                        exhaustion.cap
                    ),
                    BudgetDimension::Duration => format!(
                        "Stopped: the {}s duration budget was exhausted after {model_steps} model step(s).",
                        std::time::Duration::from_millis(exhaustion.cap).as_secs_f64()
                    ),
                    _ => format!(
                        "Stopped: the {}-token model budget was exhausted after {model_steps} model step(s).",
                        exhaustion.cap
                    ),
                };
                (self.observer)(AgentEvent::Finished(reason.clone()));
                self.enter_finalization();
                self.flush_epoch(rt);
                self.settle_finalization_dependencies(rt, &mut messages)
                    .await?;
                Ok(AgentOutcome::drive_budget_exhausted(
                    reason,
                    model_steps,
                    self.modified_files.clone(),
                    exhaustion,
                    &self.progress,
                    &self.objective,
                ))
            }
            // The window's pinned round limit: this is the COMMON bounded
            // exit, so it drains running background children like every other
            // one, or the abort-on-drop backstop hard-kills them (spend and
            // findings lost).
            KernelStop::ModelStepWindowLimit { limit: round_limit } => {
                // A host-pinned, deliberately bounded unit of work (an eval
                // case, a delegated agent's budget) used up its model steps.
                // Never return an empty answer: surface the last thing the
                // model said plus how far it got, so the caller/UI shows real
                // state.
                let summary = {
                    let mut s = format!(
                        "Reached the {round_limit}-model-step limit pinned for this bounded unit \
                         of work before finishing."
                    );
                    if !self.modified_files.is_empty() {
                        s.push_str(&format!(
                            " Files changed so far: {}.",
                            self.modified_files.join(", ")
                        ));
                    }
                    if !self.last_text.trim().is_empty() {
                        s.push_str(&format!("\n\nLatest note: {}", self.last_text.trim()));
                    }
                    s
                };
                (self.observer)(AgentEvent::Finished(summary.clone()));
                self.enter_finalization();
                self.settle_finalization_dependencies(rt, &mut messages)
                    .await?;
                self.flush_epoch(rt);
                Ok(AgentOutcome::drive_result(
                    summary,
                    round_limit,
                    self.modified_files.clone(),
                    StopReason::BudgetExhausted,
                    Some(format!(
                        "model_step_window_limit={round_limit}; host-pinned bounded unit of work, \
                         not a resource budget"
                    )),
                    &self.progress,
                    &self.objective,
                ))
            }
            // `on_quiet` owns every quiet exit and returns its own outcome, so
            // the kernel's neutral model-end never decides anything here.
            KernelStop::ModelEnd => {
                (self.observer)(AgentEvent::Finished(self.last_text.clone()));
                self.enter_finalization();
                self.flush_epoch(rt);
                self.settle_finalization_dependencies(rt, &mut messages)
                    .await?;
                Ok(AgentOutcome::drive_result(
                    self.last_text.clone(),
                    model_steps,
                    self.modified_files.clone(),
                    StopReason::Answered,
                    None,
                    &self.progress,
                    &self.objective,
                ))
            }
        }
    }
}

fn should_inject_delivery_convergence_nudge(
    goal_mode: bool,
    already_sent: bool,
    round: u32,
    has_modified_files: bool,
    had_tool_calls: bool,
) -> bool {
    goal_mode && !already_sent && round == 1 && !has_modified_files && had_tool_calls
}

/// Roll one settled child into the parent epoch: spend absorb, modified-file
/// merge, typed-finding adoption at Acknowledged (receipt is not judgment),
/// Worker-incomplete goal debt, the parent-facing content, and the
/// `SubAgentFinished` event. Shared verbatim by the foreground batch, the
/// background settlement path, and the exit drains, so background execution
/// cannot reduce truthfulness.
#[allow(clippy::too_many_arguments)]
fn fold_child_settlement(
    progress: &mut leveler_lifecycle::ProgressLedger,
    commands_run: &mut u32,
    modified_files: &mut Vec<String>,
    ledger: &mut EvidenceLedger,
    observer: &mut (dyn FnMut(AgentEvent) + Send),
    id: &str,
    nickname: &str,
    role: AgentRole,
    result: &super::handlers::SubAgentRunResult,
) -> (String, bool) {
    // Roll the sub-agent's work into the parent task epoch. Its SPEND does not
    // travel this way: every model call it made already reached the parent as a
    // record and was folded into the usage projection when it arrived.
    progress.absorb_child_work(&result.progress);
    *commands_run = progress.cumulative_commands;
    for path in &result.modified_files {
        if !modified_files.iter().any(|p| p == path) {
            modified_files.push(path.clone());
        }
    }
    // Adopt the child's typed findings into the parent ledger and persist the
    // snapshot; the parent-facing text names the adopted ids.
    //
    // A Worker that did not finish used to also get a host-authored BLOCKING
    // finding here, so completion could be refused over it. The mechanical
    // fact — this child started and did not report — is the `ok: false`
    // terminal below; what to do about it is the model's call.
    let adopted: Vec<String> = result
        .findings
        .iter()
        .map(|f| ledger.adopt_finding(id, role.label(), f))
        .collect();
    if !adopted.is_empty() {
        observer(AgentEvent::EvidenceLedgerUpdated {
            ledger: ledger.clone(),
        });
    }
    // N1: the status line leads, so the parent can tell "finished, nothing to
    // flag" from "stopped before it found anything".
    let mut content = result.result.for_parent(nickname);
    if !result.modified_files.is_empty() {
        content.push_str(&format!(
            "\nFiles touched: {}",
            result.modified_files.join(", ")
        ));
    }
    if !adopted.is_empty() {
        let adopted_line = format!("Structured findings adopted: {}.", adopted.join(", "));
        if let Some(pos) = content.find('\n') {
            content.insert_str(pos + 1, &format!("{adopted_line}\n"));
        } else {
            content.push('\n');
            content.push_str(&adopted_line);
        }
    }
    // Project this child's contribution out of the parent ledger. Computed
    // here because this is the one point that owns both the child's identity
    // and the ledger its findings were adopted into — and computed at
    // settlement rather than read later, so a replay of the terminal event
    // alone can answer "did the parent act on what this child found".
    let profile = ChildProfile::resolve(role);
    let (profile_id, profile_role, read_only) = profile.trace_fields();
    let contribution =
        leveler_lifecycle::ChildResultProjection::from_findings(id, role.label(), &ledger.findings)
            .with_profile(profile_id, profile_role, read_only);
    observer(AgentEvent::SubAgentFinished {
        id: id.to_string(),
        nickname: nickname.to_string(),
        ok: result.result.status.completed(),
        summary: preview(&content),
        contribution: Some(contribution),
        outcome: Some(result.result.status),
        stop: Some(result.stop),
        limit: result.limit,
    });
    (content, result.result.status.completed())
}

/// Remove `id` from the durable outstanding-children record.
fn clear_outstanding_child(progress: &mut leveler_lifecycle::ProgressLedger, id: &str) {
    progress
        .outstanding_children
        .retain(|entry| entry.split('|').next() != Some(id));
}

/// A joined background child. A panicked/aborted task is reported as an
/// honest no-result failure, never silently dropped.
fn join_settlement(
    joined: Result<super::handlers::SubAgentRunResult, tokio::task::JoinError>,
) -> super::handlers::SubAgentRunResult {
    match joined {
        Ok(result) => result,
        Err(join_error) => super::handlers::SubAgentRunResult {
            result: crate::sub_agent::ChildResult::new(
                false,
                "",
                format!("its background task ended abnormally: {join_error}"),
            ),
            stop: leveler_lifecycle::ChildStop::Failed,
            limit: None,
            progress: leveler_lifecycle::ProgressLedger::default(),
            modified_files: Vec::new(),
            findings: Vec::new(),
        },
    }
}

/// Pin parent-local batch work onto the ledger before child absorb.
///
/// Local `commands_run` advances when parent tools run in the same assistant
/// batch as `spawn_agent`; the ledger may still lag until the next
/// `sync_epoch_progress`. Without this pin, `absorb_child_work` + reassignment
/// from `progress.cumulative_*` drops the parent's same-batch commands.
///
/// Tokens and cost need no pin: they are an absolute projection of the records
/// seen so far, not a running local the ledger can overwrite.
fn pin_parent_batch_work(
    progress: &mut leveler_lifecycle::ProgressLedger,
    commands_run: u32,
    modified_files: &[String],
) {
    progress.cumulative_commands = progress.cumulative_commands.max(commands_run);
    progress.merge_modified_paths(modified_files.iter().cloned());
}

/// Residual step limits for one child. When the parent is capped, residual is
/// always `Some(_)` including `Some(0)` (hard block) — never re-opens unlimited.
///
/// `share_of` is the 0-based index of this child among `share_n` concurrent
/// spawns so the residual is split (no parallel oversell).
///
/// **Duration note:** wall residual is computed at queue time and refreshed
/// again after the concurrency semaphore is acquired (see `run_one_sub_agent`)
/// so a child that waited behind others cannot keep a pre-queue residual that
/// already exceeds the parent deadline.
fn residual_step_limits(
    parent: super::StepLimits,
    commands_run: u32,
    model_tokens_spent: u64,
    cost_spent_micros: u64,
    epoch_files: usize,
    epoch_duration_at_start: std::time::Duration,
    run_started: std::time::Instant,
    share_of: u32,
    share_n: u32,
) -> super::StepLimits {
    use super::StepLimits;
    let n = share_n.max(1);
    let idx = share_of.min(n - 1);
    let split = |total: u32| -> u32 {
        let base = total / n;
        let rem = total % n;
        base + u32::from(idx < rem)
    };
    let split_usize = |total: usize| -> usize {
        let n = n as usize;
        let idx = idx as usize;
        let base = total / n;
        let rem = total % n;
        base + usize::from(idx < rem)
    };
    let split_u64 = |total: u64| -> u64 {
        let n = u64::from(n);
        let idx = u64::from(idx);
        let base = total / n;
        let rem = total % n;
        base + u64::from(idx < rem)
    };

    let mut limits = StepLimits {
        max_duration: Some(crate::sub_agent::SUB_AGENT_MAX_DURATION),
        // A parent that configured a reserve (a test, or a future profile
        // policy) passes it down; when it did not, the child runner applies
        // the standard reserve. The child wall clock is the child's to
        // finalize against, so this is inherited, not re-decided here.
        finalization_grace: parent.finalization_grace,
        ..StepLimits::default()
    };
    if let Some(max) = parent.max_commands {
        // Some(0) when exhausted — child cannot run any command.
        let remaining = max.saturating_sub(commands_run);
        limits.max_commands = Some(split(remaining));
    }
    if let Some(max) = parent.max_model_tokens {
        let remaining = max.saturating_sub(model_tokens_spent);
        limits.max_model_tokens = Some(split_u64(remaining));
    }
    if let Some(max) = parent.max_cost_usd_micros {
        let remaining = max.saturating_sub(cost_spent_micros);
        limits.max_cost_usd_micros = Some(split_u64(remaining));
    }
    if let Some(max) = parent.max_modified_files {
        let remaining = max.saturating_sub(epoch_files);
        limits.max_modified_files = Some(split_usize(remaining));
    }
    // Child wall clock: min(sub-agent cap, parent residual). Exhausted parent
    // duration → Some(0) hard stop (not a free 1s grant).
    if let Some(parent_max) = parent.max_duration {
        let elapsed = epoch_duration_at_start.saturating_add(run_started.elapsed());
        let residual = parent_max.saturating_sub(elapsed);
        let child_cap = limits
            .max_duration
            .unwrap_or(crate::sub_agent::SUB_AGENT_MAX_DURATION);
        limits.max_duration = Some(child_cap.min(residual));
    }
    limits
}

/// Distinct file count if `drive_files` were merged into the epoch path set.
fn projected_epoch_file_count(
    progress: &leveler_lifecycle::ProgressLedger,
    drive_files: &[String],
) -> usize {
    epoch_modified_paths(progress, drive_files).len()
}

/// Once the model-step safety ceiling is 80% reached, the model is told so,
/// once. Tiny ceilings (tests, evals with a handful of steps) get no note —
/// there is nothing to state. It reports a mechanical limit of the run, never
/// a task budget and never a prompt to converge.
pub(crate) const MODEL_STEP_NOTE_MIN_TOTAL: u32 = 20;

pub(crate) fn model_step_note(used: u32, ceiling: u32, already_sent: bool) -> Option<String> {
    if already_sent
        || ceiling < MODEL_STEP_NOTE_MIN_TOTAL
        || used.saturating_mul(5) < ceiling.saturating_mul(4)
    {
        return None;
    }
    Some(format!(
        "Model steps: {used} of {ceiling} used. This is a mechanical safety \
         ceiling on the run's model/tool loop, not a budget for the task."
    ))
}

/// How often to emit a [`AgentEvent::CommandProgress`] heartbeat while a command
/// tool is still running. Short enough to prove liveness, long enough to be quiet.
const COMMAND_HEARTBEAT_SECS: u64 = 3;

/// The command line a heartbeat should name, or `None` if this call is not a
/// long-running command tool (only those get a heartbeat). Reads the `cmd`
/// argument the command tools take; falls back to the tool name.
fn command_progress_label(registry: &ToolRegistry, call: &ToolCall) -> Option<String> {
    if !registry.runs_command(&call.name) {
        return None;
    }
    let cmd = call
        .arguments
        .get("cmd")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    Some(cmd.map_or_else(|| call.name.clone(), str::to_string))
}

/// Epoch + this-drive distinct modified paths (source of truth for residual).
fn epoch_modified_paths(
    progress: &leveler_lifecycle::ProgressLedger,
    drive_files: &[String],
) -> Vec<String> {
    let mut paths = progress.cumulative_modified_paths.clone();
    for path in drive_files {
        if !paths.iter().any(|p| p == path) {
            paths.push(path.clone());
        }
    }
    paths
}

/// Task-level file budget gate for `apply_patch` / `replace`.
///
/// Refuses when this call would introduce more *new* paths than residual
/// allows (including multi-file patch oversell). Re-edits of paths already in
/// the epoch set are free and allowed even when residual is 0.
fn file_budget_refusal(
    max_modified_files: Option<usize>,
    epoch_file_count: usize,
    call: &ToolCall,
    progress: &leveler_lifecycle::ProgressLedger,
    drive_files: &[String],
) -> Option<String> {
    let max = max_modified_files?;
    let mut targets = Vec::new();
    collect_scoped_paths_from_call(call, &mut targets);
    let new_n = targets
        .iter()
        .filter(|path| {
            !progress
                .cumulative_modified_paths
                .iter()
                .any(|p| p == *path)
                && !drive_files.iter().any(|p| p == *path)
        })
        .count();
    if new_n == 0 {
        // Pure re-edit (or no paths parsed): does not grow the epoch set.
        return None;
    }
    let remaining = max.saturating_sub(epoch_file_count);
    if remaining == 0 {
        return Some(format!("the {max}-modified-file budget is exhausted"));
    }
    if new_n > remaining {
        return Some(format!(
            "this edit would modify {new_n} new file(s) but only {remaining} remain in the \
             {max}-modified-file budget"
        ));
    }
    None
}

/// Write absolute epoch spend into the ledger so continue/resume seeds the
/// same totals (including tool-phase command/file increments after the last
/// model stream).
#[allow(clippy::too_many_arguments)]
fn sync_epoch_progress(
    progress: &mut leveler_lifecycle::ProgressLedger,
    epoch_model_steps_at_start: u32,
    epoch_duration_at_start: std::time::Duration,
    run_started: std::time::Instant,
    model_steps: u32,
    model_tokens_spent: u64,
    estimated_tokens_spent: u64,
    commands_run: u32,
    cost_spent_micros: u64,
    modified_files: &[String],
) {
    let duration_ms = epoch_duration_at_start
        .saturating_add(run_started.elapsed())
        .as_millis() as u64;
    // Distinct paths across the epoch (re-edits of the same file do not inflate).
    progress.merge_modified_paths(modified_files.iter().cloned());
    let files_total = progress.cumulative_modified_files;
    progress.set_epoch_spend(
        epoch_model_steps_at_start.saturating_add(model_steps),
        model_tokens_spent,
        estimated_tokens_spent,
        commands_run,
        cost_spent_micros,
        duration_ms,
        files_total,
    );
}

#[cfg(test)]
mod residual_budget_tests {
    use super::*;

    #[test]
    fn delivery_convergence_nudge_fires_once_after_the_first_read_only_goal_round() {
        assert!(should_inject_delivery_convergence_nudge(
            true, false, 1, false, true
        ));
        assert!(!should_inject_delivery_convergence_nudge(
            false, false, 1, false, true
        ));
        assert!(!should_inject_delivery_convergence_nudge(
            true, true, 1, false, true
        ));
        assert!(!should_inject_delivery_convergence_nudge(
            true, false, 2, false, true
        ));
        assert!(!should_inject_delivery_convergence_nudge(
            true, false, 1, true, true
        ));
        assert!(!should_inject_delivery_convergence_nudge(
            true, false, 1, false, false
        ));
    }
    use crate::executor::StepLimits;
    use std::time::{Duration, Instant};

    #[test]
    fn exhausted_parent_commands_yield_some_zero_not_unlimited() {
        let parent = StepLimits {
            max_commands: Some(3),
            ..StepLimits::default()
        };
        let residual = residual_step_limits(
            parent,
            3, // already used all
            0,
            0,
            0,
            Duration::ZERO,
            Instant::now(),
            0,
            1,
        );
        assert_eq!(residual.max_commands, Some(0));
    }

    #[test]
    fn parallel_share_splits_without_oversell() {
        let parent = StepLimits {
            max_commands: Some(3),
            max_modified_files: Some(2),
            ..StepLimits::default()
        };
        let a = residual_step_limits(parent, 0, 0, 0, 0, Duration::ZERO, Instant::now(), 0, 2);
        let b = residual_step_limits(parent, 0, 0, 0, 0, Duration::ZERO, Instant::now(), 1, 2);
        // 3 commands / 2 children → 2 + 1
        assert_eq!(a.max_commands.unwrap() + b.max_commands.unwrap(), 3);
        // 2 files / 2 → 1 + 1
        assert_eq!(
            a.max_modified_files.unwrap() + b.max_modified_files.unwrap(),
            2
        );
    }

    #[test]
    fn exhausted_parent_duration_is_zero_not_one_second_grant() {
        let parent = StepLimits {
            max_duration: Some(Duration::from_secs(5)),
            ..StepLimits::default()
        };
        // Simulate parent already past the cap.
        let residual = residual_step_limits(
            parent,
            0,
            0,
            0,
            0,
            Duration::from_secs(10),
            Instant::now(),
            0,
            1,
        );
        assert_eq!(residual.max_duration, Some(Duration::ZERO));
    }

    #[test]
    fn unlimited_parent_stays_unlimited_for_child() {
        let parent = StepLimits::default();
        let residual =
            residual_step_limits(parent, 99, 0, 0, 0, Duration::ZERO, Instant::now(), 0, 1);
        assert_eq!(residual.max_commands, None);
        assert_eq!(residual.max_modified_files, None);
    }

    #[test]
    fn share_n_must_use_accepted_count_not_raw_job_count() {
        // Two accepted children of a 2-command residual: 1+1, not 2/3 + 0.
        let parent = StepLimits {
            max_commands: Some(2),
            ..StepLimits::default()
        };
        let a = residual_step_limits(parent, 0, 0, 0, 0, Duration::ZERO, Instant::now(), 0, 2);
        let b = residual_step_limits(parent, 0, 0, 0, 0, Duration::ZERO, Instant::now(), 1, 2);
        assert_eq!(a.max_commands, Some(1));
        assert_eq!(b.max_commands, Some(1));
        // If share_n wrongly included a rejected third job, each would get 0 or 1
        // with sum still 2 but uneven waste — sum of accepted shares must equal residual.
        assert_eq!(
            a.max_commands.unwrap() + b.max_commands.unwrap(),
            2,
            "accepted shares must exhaust residual without oversell"
        );
    }

    #[test]
    fn pin_parent_batch_work_keeps_local_commands_before_absorb() {
        use leveler_lifecycle::ProgressLedger;
        // Ledger lags (0); local batch already spent 1 command.
        let mut progress = ProgressLedger {
            cumulative_commands: 0,
            ..Default::default()
        };
        pin_parent_batch_work(&mut progress, 1, &[]);
        assert_eq!(progress.cumulative_commands, 1);
        let child = ProgressLedger {
            cumulative_commands: 1,
            ..Default::default()
        };
        progress.absorb_child_work(&child);
        assert_eq!(
            progress.cumulative_commands, 2,
            "parent same-batch + child must both count"
        );
    }

    #[test]
    fn multi_file_patch_refused_when_residual_file_budget_is_one() {
        use leveler_core::ToolCallId;
        use leveler_lifecycle::ProgressLedger;
        use leveler_model::ToolCall;

        let progress = ProgressLedger::default();
        let call = ToolCall {
            id: ToolCallId::new("c"),
            name: "apply_patch".into(),
            arguments: serde_json::json!({
                "patch": "*** Begin Patch\n*** Update File: a.rs\n*** Update File: b.rs\n*** End Patch"
            }),
        };
        let reason = file_budget_refusal(Some(1), 0, &call, &progress, &[]);
        assert!(
            reason
                .as_deref()
                .is_some_and(|r| r.contains("2 new file") && r.contains("only 1 remain")),
            "expected multi-file residual refusal, got {reason:?}"
        );
        // Re-edit of an already-budgeted path does not consume residual.
        let mut with_a = ProgressLedger::default();
        with_a.merge_modified_paths(["a.rs"]);
        let reedit = ToolCall {
            id: ToolCallId::new("c2"),
            name: "apply_patch".into(),
            arguments: serde_json::json!({
                "patch": "*** Begin Patch\n*** Update File: a.rs\n*** End Patch"
            }),
        };
        assert_eq!(
            file_budget_refusal(Some(1), 1, &reedit, &with_a, &[]),
            None,
            "re-edit of counted path must be allowed at residual 0 new"
        );
    }

    /// The finalization point is strictly before the hard bound and never
    /// exists without both a duration bound and a reserve that fits inside it.
    #[test]
    fn the_finalization_point_is_before_the_hard_bound_or_absent() {
        assert_eq!(
            finalization_point(
                Some(Duration::from_secs(1200)),
                Some(Duration::from_secs(180))
            ),
            Some(Duration::from_secs(1020))
        );
        assert_eq!(
            finalization_point(Some(Duration::from_secs(1200)), None),
            None,
            "no reserve configured means no request"
        );
        assert_eq!(
            finalization_point(None, Some(Duration::from_secs(180))),
            None,
            "an unbounded run has nothing to finalize for"
        );
        assert_eq!(
            finalization_point(
                Some(Duration::from_secs(60)),
                Some(Duration::from_secs(180))
            ),
            None,
            "a reserve larger than the run would eat the whole budget"
        );
        assert_eq!(
            finalization_point(
                Some(Duration::from_secs(180)),
                Some(Duration::from_secs(180))
            ),
            None,
            "a point at the hard bound is not before it"
        );
    }

    /// Every child inherits a finalization reserve even though the top-level
    /// parent leaves it unset; a parent that configured one passes it down.
    #[test]
    fn a_child_inherits_the_configured_finalization_reserve() {
        let overridden = StepLimits {
            finalization_grace: Some(Duration::from_secs(5)),
            ..StepLimits::default()
        };
        let child =
            residual_step_limits(overridden, 0, 0, 0, 0, Duration::ZERO, Instant::now(), 0, 1);
        assert_eq!(child.finalization_grace, Some(Duration::from_secs(5)));
        // An unconfigured parent leaves it unset; the child runner applies
        // the standard reserve (see `CHILD_FINALIZATION_GRACE_SECS`), so both
        // launch paths get it without each deciding it here.
        let child = residual_step_limits(
            StepLimits::default(),
            0,
            0,
            0,
            0,
            Duration::ZERO,
            Instant::now(),
            0,
            1,
        );
        assert_eq!(child.finalization_grace, None);
    }
}
