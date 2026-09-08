use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio_util::sync::CancellationToken;

use leveler_context::{load_scoped_rules, render_instructions};
use leveler_lifecycle::{
    ChangeImpact, DepthUseMetrics, EvidenceLedger, FindingKind, GateConfig, ObjectiveAnchor,
    PlanState, ProgressCaps, TurnPhase, check, is_build_relevant,
};
use leveler_model::{
    ContentPart, FinishReason, Message, ModelError, ModelRequest, Role, ToolCall, ToolChoice,
    ToolResultContent,
};
use leveler_tools::ToolRegistry;

use super::gates;

use super::closeout::{
    CLOSEOUT_NUDGE_BUDGET, CloseoutAction, CloseoutBudget, CloseoutInput, CloseoutReason, decide,
    stalled_detail,
};
use super::dispatch::{
    collect_modified, compact_json, deny_call, extract_image, extract_plan, newly_modified_paths,
    note_tool_side_effects, preview, task_needs_structured_plan,
};
use super::host::AdmitError;
use super::{
    AdvisoryKind, AgentError, AgentEvent, AgentOutcome, ChildToolEvent, Executor,
    ModelRequestRecord, StopReason, TranscriptSink,
};
use crate::authorization::{
    collect_scoped_paths_from_call, is_search_tool, is_verification_program, observe_class,
    push_unique_path, unproven_verification_note,
};
use crate::compaction::{COMPACT_KEEP_RECENT, compact_messages, estimate_tokens};
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
use crate::nudges::{first_user_text, goal_resolve_nudge};
use crate::sub_agent::{
    AgentRole, ChildProfile, MAX_SUB_AGENT_DEPTH, agent_nickname, lost_children_note,
    multi_agent_steer_hint, new_delegated_agent_id, scopes_overlap, settlement_notice,
    should_inject_delegation_hint,
};

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

struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// The one soft reminder a multi-step task gets when the model has worked
/// for a few rounds without registering a plan. Advisory only: no tool is
/// refused for a missing plan, and the model may keep working without one.
pub(crate) const PLAN_SOFT_NUDGE_TEXT: &str = "This task may benefit from a structured plan: if you already have a \
                     clear multi-step execution path, you may register it with update_plan \
                     (one in_progress step, the rest pending) so progress is visible. A \
                     plan is not required — continue exploring or editing as you see fit.";

impl Executor {
    /// The core loop over a growing message transcript.
    pub(crate) async fn drive(
        &self,
        mut messages: Vec<Message>,
        objective: ObjectiveAnchor,
        observer: &mut (dyn FnMut(AgentEvent) + Send),
        sink: &mut dyn TranscriptSink,
        cancellation: CancellationToken,
    ) -> Result<AgentOutcome, AgentError> {
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
        let mut modified_files: Vec<String> = Vec::new();
        let mut scoped_paths: Vec<String> = Vec::new();
        let mut metrics = DepthUseMetrics::default();
        // Active objective is host-pinned (this turn / session goal).
        let original_task = if objective.is_empty() {
            first_user_text(&messages)
        } else {
            objective.text().to_string()
        };
        let progress_caps = ProgressCaps::default();
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
        // Spend admission reads ONE authority: the finalized model-request
        // records, folded here as they are handed to the sink that makes them
        // `model_requests` rows. Whatever this drive spends is
        // `<what the epoch already spent> + <this drive's projection>` —
        // nothing re-derives a token count or re-applies a price table.
        let mut usage = crate::usage::RuntimeUsageProjection::default();
        let epoch_tokens_at_start = progress.cumulative_model_tokens;
        let epoch_estimated_at_start = progress.cumulative_estimated_model_tokens;
        let epoch_cost_at_start = progress.cumulative_cost_usd_micros;
        let mut model_tokens_spent = epoch_tokens_at_start;
        let mut estimated_tokens_spent = epoch_estimated_at_start;
        let mut cost_spent_micros = epoch_cost_at_start;
        // The one way a model call becomes spend. Every provider call this
        // drive is responsible for — its own rounds, the folds it triggers,
        // the bounded advisory calls it makes, and every call its children
        // make — arrives here as an already-priced record, is projected into
        // the runtime's admission totals, and is then written down. One event,
        // two consumers; the guard and the bill cannot drift apart because
        // there is nothing left to drift.
        //
        // `$estimate` stands in for a provider that reported no usage at all:
        // an admission input with no durable row behind it, tracked apart so
        // the reconciliation stays exact.
        macro_rules! record_request {
            ($record:expr) => {{ record_request!($record, None) }};
            ($record:expr, $estimate:expr) => {{
                let record = $record;
                usage.record(&record, $estimate);
                model_tokens_spent =
                    epoch_tokens_at_start.saturating_add(usage.admission_model_tokens());
                estimated_tokens_spent =
                    epoch_estimated_at_start.saturating_add(usage.estimated_model_tokens);
                cost_spent_micros =
                    epoch_cost_at_start.saturating_add(usage.admission_cost_usd_micros());
                sink.record_model_request(&record).await?;
            }};
        }
        let structured_plan_required =
            self.policy.require_explicit_plan && task_needs_structured_plan(&original_task);
        // Soft plan nudge only: after this many rounds without a plan on a
        // task that reads as multi-step, inject one advisory. Never used to
        // refuse a tool or force ToolChoice — the plan is the model's
        // cognitive aid, not a mutation license.
        const PLAN_SOFT_NUDGE_AFTER_ROUNDS: u32 = 2;
        let mut plan_rounds_without_plan = 0u32;
        let mut plan_soft_nudge_sent = false;
        let mut budget_note_sent = false;
        let mut plan_state = self.seeded_plan.clone();
        let mut structured_plan_started = !plan_state.is_empty();
        // Short tasks: no host-seeded one-step plan shell. Plan UI appears only when
        // the model calls update_plan, or resume rehydrates a prior PlanUpdated.
        // HostImplicit remains for resume of older sessions that already
        // persisted that origin.
        // Set whenever the model-visible `messages` gain something the durable
        // transcript (`sink`) does not hold — a transient nudge, a fold. That
        // is the only time a `ContextSnapshot` carries information; a round
        // that only appended durable messages is reconstructible from the
        // transcript alone.
        let mut context_diverged = false;
        // Product steer: top-level runs see keep-vs-delegate once. Parallel
        // keywords are not required — ordinary implementation goals must still
        // evaluate bounded Worker work.
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
        let mut session_approved: HashSet<String> = HashSet::new();
        // V2 background delegation state: children the model started with
        // run_in_background (the default). One run-level concurrency semaphore
        // covers foreground batches AND background children; the progress
        // channel outlives any single spawn batch and is drained at round
        // boundaries.
        let mut background_children = BackgroundChildren {
            children: Vec::new(),
            ownership: Some(self.ownership.clone()),
        };
        let run_agents_semaphore = Arc::new(tokio::sync::Semaphore::new(
            self.policy.max_concurrent_agents.max(1),
        ));
        let (bg_progress_tx, mut bg_progress_rx) =
            tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
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
            // 必改④: durable like every other injected turn — the record was
            // just cleared, so the note is the only remaining truth.
            sink.append(std::slice::from_ref(&note)).await?;
            messages.push(note);
        }
        // Accumulated elevations from approved request_permissions this turn.
        let mut turn_grants = crate::injected_tools::TurnPermissionGrants::default();
        // Leveling: consecutive search calls with no intervening action. Reset
        // whenever the model does something concrete (read/edit/run).
        let mut consecutive_searches = 0usize;
        // Sources of scoped AGENTS.md rules already appended to the transcript,
        // so a second file in the same directory does not re-inject them.
        let mut injected_rule_sources: Vec<String> = Vec::new();
        // The most recent non-empty assistant text, surfaced if the round budget
        // runs out so a truncated run still reports where it got to.
        let mut last_text = String::new();
        // Completion-evidence gate: whether a verification command has passed,
        // and how many times we have refused an unverified completion.
        let mut verification_ran = false;
        // EvidenceLedger (mutations / verifications / findings).
        // Resume may seed prior mutations/verifies from last EvidenceLedgerUpdated.
        let mut ledger = {
            let mut led = self.seeded_ledger.clone();
            if !plan_state.is_empty() {
                led.plan = plan_state.clone();
            }
            led
        };
        // Unified closeout nudge budget shared by every quiet-round mechanism
        // (goal resolution, completion evidence, empty answer, audit repair) —
        // the old per-mechanism caps (3 + 2 + 2 + 1) are deliberately gone.
        let mut closeout_budget = CloseoutBudget::new(CLOSEOUT_NUDGE_BUDGET);
        // Re-arm guard for the evidence nudge: it fires again only after real
        // progress (any successful tool call), never on idle repeats.
        // No-progress loop guard: "name\0args" -> (last result content, identical
        // repeat count, novelty epoch at that count). Blocks a call that keeps
        // producing the same output WHILE nothing else moved.
        //
        // The epoch is what keeps the refusal from being permanent. The guard
        // refuses BEFORE running, on the previous count — and a refused call
        // never executes, so it can never record new content and can never
        // clear its own entry. Without the epoch a zero-argument tool whose
        // result legitimately changes later (browser_snapshot is the canonical
        // case: re-snapshotting is the ONLY documented stale-ref recovery) is
        // locked out for the rest of the turn once it twice returned the same
        // text. The epoch advances whenever ANY call produces content it has
        // not produced before, so genuine alternating thrash (every result
        // identical) still freezes it and still trips the guard.
        let mut call_history: std::collections::HashMap<String, (String, u32, u64)> =
            std::collections::HashMap::new();
        let mut novelty_epoch: u64 = 0;
        // Bounded recovery from a malformed tool call: a weak model sometimes
        // emits `run_command`/tool arguments that aren't valid JSON (an
        // unescaped backslash from a regex, or a raw newline from a multi-line
        // script). Rather than failing the whole turn on that Decode error,
        // feed the parse error back and let the model resend. Reset on any
        // clean round so the cap is on *consecutive* failures.
        let mut decode_retries = 0u32;
        const MAX_DECODE_RETRIES: u32 = 2;
        // Plain-text output may hit the provider's per-response limit. Continue
        // the same answer a bounded number of times; truncated tool calls are
        // never safe to execute and fail immediately below.
        let mut length_continuations = 0u32;
        let mut continued_text = String::new();
        const MAX_LENGTH_CONTINUATIONS: u32 = 2;
        // Absolute per-turn round ceiling — the unconditional circuit breaker.
        // The loop guards are mechanical and narrow (identical results, every
        // call refused), so a busy loop that keeps issuing novel-looking calls
        // evades them and, under `UntilTerminal`, never ends. This ceiling
        // guarantees termination regardless of progress or continuation
        // policy. Set well above any legitimate single turn.
        const MAX_TURN_ROUNDS: u32 = 100;
        let round_ceiling = self.step_limits.max_rounds.unwrap_or(MAX_TURN_ROUNDS);
        // Hard step limits (spec §27): wall clock from run start, commands
        // executed so far, and the reason once a limit trips. The round that
        // trips a limit still commits its tool results (well-formed transcript)
        // and then the run returns BudgetExhausted.
        // Epoch spend: continue/resume seeds prior cumulative totals so limits
        // are task-level, not per-drive zeros.
        let run_started = std::time::Instant::now();
        let epoch_duration_at_start =
            std::time::Duration::from_millis(progress.cumulative_duration_ms);
        // Turn-wide deadline: cancel the same token passed to model streams,
        // hooks, reviewers, approvers and tools. The existing round-boundary
        // budget check then produces the normal BudgetExhausted outcome.
        let deadline_expired = Arc::new(AtomicBool::new(false));
        let turn_cancellation = cancellation.child_token();
        let deadline_done = CancellationToken::new();
        let _deadline_guard = CancelOnDrop(deadline_done.clone());
        if let Some(max) = self.step_limits.max_duration {
            let remaining = max.saturating_sub(epoch_duration_at_start);
            let expired = Arc::clone(&deadline_expired);
            let deadline_token = turn_cancellation.clone();
            let external = cancellation.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = external.cancelled() => {}
                    _ = deadline_done.cancelled() => {}
                    _ = tokio::time::sleep(remaining) => {
                        expired.store(true, Ordering::Release);
                        deadline_token.cancel();
                    }
                }
            });
        }
        let cancellation = turn_cancellation;
        let mut commands_run = progress.cumulative_commands;
        // Human reason + structured dimension when a step limit trips mid-round.
        let mut budget_exceeded: Option<(String, crate::budget::BudgetExhaustion)> = None;
        let epoch_rounds_at_start = progress.cumulative_rounds;

        if self.step_limits.max_cost_usd_micros.is_some() && self.pricing.is_none() {
            return Err(AgentError::InvalidBudget(
                "a cost limit requires pricing in the selected model profile".to_string(),
            ));
        }

        // Every exit from this drive — and every round boundary — must fold the
        // round's spend into the epoch ledger and publish it, or resume and the
        // TUI footer fall behind what the outcome reports. Seventeen call sites
        // used to repeat the same eleven arguments by hand; a macro (not a
        // closure — these are all live `&mut` borrows) keeps them in lockstep.
        macro_rules! flush_epoch {
            ($rounds:expr) => {{
                sync_epoch_progress(
                    &mut progress,
                    &mut metrics,
                    epoch_rounds_at_start,
                    epoch_tokens_at_start,
                    epoch_duration_at_start,
                    run_started,
                    $rounds,
                    model_tokens_spent,
                    estimated_tokens_spent,
                    commands_run,
                    cost_spent_micros,
                    &modified_files,
                );
                observer(AgentEvent::ProgressUpdated {
                    ledger: progress.clone(),
                });
            }};
        }

        // A child runs as an owned `'static` future, so it cannot borrow this
        // sink; its model-call records ride the progress channel instead and
        // are written down here, by the one holder of durable storage. They
        // are not forwarded to the observer: the live progress line already
        // carries the child's running totals, and this event exists for the
        // ledger, not the screen.
        macro_rules! forward_child_event {
            ($event:expr) => {{
                let event = $event;
                if let AgentEvent::SubAgentModelRequest { record } = &event {
                    // Already priced by the child against its own model.
                    record_request!((**record).clone());
                } else {
                    observer(event);
                }
            }};
        }

        // V2: non-blocking settlement of finished background children — the
        // notice lands in `messages` before the next model round.
        macro_rules! settle_finished_children {
            ($rounds:expr) => {{
                while let Ok(event) = bg_progress_rx.try_recv() {
                    forward_child_event!(event);
                }
                let mut settled = Vec::new();
                let mut i = 0;
                while i < background_children.children.len() {
                    if background_children.children[i].handle.is_finished() {
                        settled.push(background_children.children.remove(i));
                    } else {
                        i += 1;
                    }
                }
                for child in settled {
                    let BackgroundChild {
                        id,
                        nickname,
                        role,
                        scope,
                        handle,
                    } = child;
                    let result = join_settlement(handle.await);
                    let (content, _ok) = fold_child_settlement(
                        &mut progress,
                        &mut commands_run,
                        &mut modified_files,
                        &mut ledger,
                        observer,
                        &id,
                        &nickname,
                        role,
                        &result,
                    );
                    clear_outstanding_child(&mut progress, &id);
                    // Terminal release: the child's exclusive claims end with
                    // it, whatever its terminal state (idempotent).
                    self.ownership.release_all(&id);
                    let notice = Message::text(
                        Role::User,
                        settlement_notice(&nickname, &id, role, &scope, &content),
                    );
                    // 必改④: persist the notice NOW. The next ContextSnapshot is
                    // a whole model round away, and outstanding_children was
                    // just cleared — a crash in between would otherwise lose
                    // the child's report text with no lost-note either.
                    sink.append(std::slice::from_ref(&notice)).await?;
                    messages.push(notice);
                    flush_epoch!($rounds);
                }
            }};
        }

        // V2: full drain — await EVERY outstanding background child before the
        // run returns, so no exit path orphans a running delegation or loses
        // its result (findings adoption in the fold is durable even when the
        // run is ending). Children hold child cancellation tokens and wall
        // caps, so this terminates.
        macro_rules! drain_background_children {
            ($rounds:expr) => {{
                while !background_children.children.is_empty() {
                    settle_finished_children!($rounds);
                    if background_children.children.is_empty() {
                        break;
                    }
                    tokio::select! {
                        biased;
                        Some(event) = bg_progress_rx.recv() => forward_child_event!(event),
                        _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
                    }
                }
            }};
        }

        let mut round = 0u32;

        // A progress watchdog giving up: mark the ledger terminal, publish it,
        // report the model's own last words (or `$fallback` when it went quiet),
        // and flush the epoch before returning. Defined after `round` so the
        // macro body resolves it; the watchdogs below each used to spell out all
        // six steps, and one of them drifted into a hand-built `AgentOutcome`.
        macro_rules! stop_now {
            ($stop:expr, $detail:expr, $fallback:expr) => {{
                progress.enter_terminal();
                observer(AgentEvent::ProgressUpdated {
                    ledger: progress.clone(),
                });
                let final_text = if last_text.trim().is_empty() {
                    $fallback.to_string()
                } else {
                    last_text.clone()
                };
                observer(AgentEvent::Finished(final_text.clone()));
                flush_epoch!(round);
                drain_background_children!(round);
                return Ok(AgentOutcome::drive_result(
                    final_text,
                    round,
                    modified_files,
                    $stop,
                    Some($detail.to_string()),
                    &metrics,
                    &progress,
                    &objective,
                ));
            }};
        }

        loop {
            // Mid-turn user input goes in at the top of the round, before the
            // model is asked anything: a correction that arrives after the work
            // is done is worthless. Empty is the normal case.
            if let Some(source) = &self.steering {
                for text in source.take_pending() {
                    let text = text.trim();
                    if text.is_empty() {
                        continue;
                    }
                    let message = Message::text(Role::User, text);
                    sink.append(std::slice::from_ref(&message)).await?;
                    messages.push(message);
                }
            }
            // Background settlements land before the model is asked anything —
            // whichever path reached this round top (tool batch, quiet wait,
            // nudge continue), the model never runs a round blind to a child
            // that already finished.
            settle_finished_children!(round);
            // The one place that decides whether another main-task model call
            // happens. These six predicates ran inline here, each with its own
            // early return, and every path that wanted another round reached
            // them only by falling back to the top of the loop. Same
            // predicates, same order, same verdicts — named, and now with one
            // input a later policy can be handed.
            let verdict =
                crate::admission::admit_next_round(&crate::admission::RoundAdmissionInput {
                    round,
                    round_ceiling,
                    window_round_limit: self.continuation.round_limit(),
                    model_tokens_spent,
                    max_model_tokens: self.step_limits.max_model_tokens,
                    cost_spent_micros,
                    max_cost_usd_micros: self.step_limits.max_cost_usd_micros,
                    elapsed: epoch_duration_at_start.saturating_add(run_started.elapsed()),
                    max_duration: self.step_limits.max_duration,
                    cancelled: cancellation.is_cancelled(),
                    deadline_expired: deadline_expired.load(Ordering::Acquire),
                });
            // Applied in two phases because the round counter advances between
            // them, and what a stop reports depends on which side of that it
            // falls: a spent budget names the rounds that COMPLETED, while the
            // cancel and deadline paths flush the round they were about to
            // start. One decision, the loop's own bookkeeping.
            match &verdict {
                crate::admission::RoundAdmission::StopBudget(exhaustion)
                    if !matches!(
                        exhaustion.dimension,
                        crate::budget::BudgetDimension::Duration
                    ) =>
                {
                    let reason = match exhaustion.dimension {
                        crate::budget::BudgetDimension::Cost => format!(
                            "Stopped: the {}-micro-USD model cost budget was exhausted after {round} round(s).",
                            exhaustion.cap
                        ),
                        _ => format!(
                            "Stopped: the {}-token model budget was exhausted after {round} round(s).",
                            exhaustion.cap
                        ),
                    };
                    observer(AgentEvent::Finished(reason.clone()));
                    flush_epoch!(round);
                    drain_background_children!(round);
                    return Ok(AgentOutcome::drive_budget_exhausted(
                        reason,
                        round,
                        modified_files,
                        exhaustion.clone(),
                        &metrics,
                        &progress,
                        &objective,
                    ));
                }
                crate::admission::RoundAdmission::StopRoundCeiling { ceiling } => {
                    // Unconditional circuit breaker: even a busy loop that
                    // evades every progress watchdog terminates here.
                    let reason =
                        format!("Stopped: reached the {ceiling}-round ceiling for a single turn.");
                    observer(AgentEvent::Finished(reason.clone()));
                    flush_epoch!(round);
                    drain_background_children!(round);
                    return Ok(AgentOutcome::drive_result(
                        reason,
                        round,
                        modified_files,
                        StopReason::TurnLimitReached,
                        Some("round ceiling reached".to_string()),
                        &metrics,
                        &progress,
                        &objective,
                    ));
                }
                crate::admission::RoundAdmission::StopWindowLimit => break,
                _ => {}
            }
            round = round.saturating_add(1);
            let has_next_round = self.continuation.allows_round_after(round);
            match verdict {
                crate::admission::RoundAdmission::Cancelled => {
                    // Flush epoch spend before Cancelled so resume/event-log keep
                    // command/file/token totals (including any absorbed children).
                    flush_epoch!(round);
                    drain_background_children!(round);
                    return Err(AgentError::Cancelled);
                }
                // Only the duration dimension can reach here: the token and
                // cost caps returned in the phase above. Matched explicitly so
                // a dimension added later cannot inherit this message.
                crate::admission::RoundAdmission::StopBudget(exhaustion)
                    if exhaustion.dimension == crate::budget::BudgetDimension::Duration =>
                {
                    let reason = format!(
                        "Stopped: the {}s duration budget was exhausted after {} round(s).",
                        std::time::Duration::from_millis(exhaustion.cap).as_secs_f64(),
                        round.saturating_sub(1)
                    );
                    observer(AgentEvent::Finished(reason.clone()));
                    flush_epoch!(round);
                    drain_background_children!(round);
                    return Ok(AgentOutcome::drive_budget_exhausted(
                        reason,
                        round - 1,
                        modified_files,
                        exhaustion,
                        &metrics,
                        &progress,
                        &objective,
                    ));
                }
                _ => {}
            }

            // Nested AGENTS.md rules for directories touched so far. Appended at
            // the tail rather than folded into the system prompt: rewriting the
            // first message would invalidate the provider's prefix cache for the
            // entire transcript on every round.
            let fresh = load_scoped_rules(
                self.tool_context.execution.workspace.root(),
                &scoped_paths,
                &injected_rule_sources,
            );
            if !fresh.is_empty() {
                injected_rule_sources.extend(fresh.iter().map(|r| r.source.clone()));
                let rules = Message::text(
                    Role::System,
                    format!("Project rules:\n{}", render_instructions(&fresh)),
                );
                // Durable like every other injected message: a standing
                // constraint the model saw must survive in the transcript, not
                // only in a snapshot (the seed drops stale System rows on the
                // next turn, so this never duplicates the system prompt).
                sink.append(std::slice::from_ref(&rules)).await?;
                messages.push(rules);
            }

            // One soft plan reminder for a multi-step task the model has been
            // working on without a plan. Advisory: nothing is refused.
            if structured_plan_required
                && !structured_plan_started
                && plan_rounds_without_plan >= PLAN_SOFT_NUDGE_AFTER_ROUNDS
                && !plan_soft_nudge_sent
            {
                let nudge = Message::text(Role::User, PLAN_SOFT_NUDGE_TEXT);
                sink.append(std::slice::from_ref(&nudge)).await?;
                messages.push(nudge);
                plan_soft_nudge_sent = true;
            }

            // A pinned task budget is the model's to spend: at 80% it is told
            // where it stands, once. `epoch_rounds_at_start + round_limit` is
            // the task total the engine clamped this window to.
            if self.depth == 0
                && let Some(remaining) = self.continuation.round_limit()
                && let Some(note) = budget_note(
                    epoch_rounds_at_start.saturating_add(round),
                    epoch_rounds_at_start.saturating_add(remaining),
                    budget_note_sent,
                )
            {
                let note = Message::text(Role::User, note);
                sink.append(std::slice::from_ref(&note)).await?;
                messages.push(note);
                budget_note_sent = true;
            }

            let mut request = ModelRequest::new(self.model.clone(), messages.clone());
            // Full tool table + Auto for every normal navigation round. Plan is
            // never a license to navigate: forced update_plan tool_choice and
            // "tools = [update_plan]" are gone (C2.3A).
            request.tools = tools.clone();
            request.tool_choice = ToolChoice::Auto;
            request.max_output_tokens = Some(self.max_output_tokens);
            request.reasoning_effort = self.policy.reasoning_effort;

            let stream_result = match self
                .stream_round_with_retry(request, observer, &cancellation)
                .await
            {
                Ok(v) => {
                    decode_retries = 0;
                    v
                }
                // Malformed tool-call JSON: feed the error back and retry the
                // round instead of aborting, up to a small cap.
                Err(AgentError::Model(e))
                    if e.kind == leveler_model::ModelErrorKind::Decode
                        && decode_retries < MAX_DECODE_RETRIES
                        && !cancellation.is_cancelled() =>
                {
                    decode_retries += 1;
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
                    sink.append(std::slice::from_ref(&feedback)).await?;
                    messages.push(feedback);
                    continue;
                }
                Err(AgentError::Cancelled) if deadline_expired.load(Ordering::Acquire) => {
                    // Re-enter at the round boundary, which records progress
                    // and returns the standard duration-budget outcome.
                    continue;
                }
                Err(e) => return Err(e),
            };

            // Zero-usage gateways must not disable the token budget: fall back
            // to the transcript estimate (same fallback compaction uses), so
            // `max_model_tokens` still binds. Estimated spend is per-round
            // request + response, mirroring what the provider actually bills.
            let round_estimate = if stream_result.usage.total() > 0 {
                None
            } else {
                Some(estimate_tokens(&messages).saturating_add(estimate_tokens(
                    std::slice::from_ref(&stream_result.message),
                )))
            };
            // Priced once, here, against the usage the provider reported —
            // cached share included, because charging every input token at the
            // uncached rate overstated a session's cost by roughly 4x at a 90%
            // hit rate and made a cost budget bind long before the money was
            // actually spent. The same priced record is what admission folds
            // and what the ledger stores.
            record_request!(
                ModelRequestRecord {
                    provider_request_id: Some(stream_result.request_id.clone()),
                    provider: self.model.provider.clone(),
                    model: self.model.model.clone(),
                    usage: stream_result.usage,
                    finish_reason: stream_result.finish_reason,
                    latency_ms: stream_result.latency_ms,
                    retry_count: stream_result.retry_count,
                    kind: crate::ModelCallKind::Round,
                    agent_id: None,
                    cost_usd_micros: None,
                }
                .priced(self.pricing.as_ref()),
                round_estimate
            );
            // Cost can cross the limit on the response that tips it; stop after
            // this round's tools (if any) rather than allowing another model call.
            if let Some(max) = self.step_limits.max_cost_usd_micros
                && cost_spent_micros >= max
            {
                budget_exceeded = Some((
                    format!(
                        "Stopped: the {max}-micro-USD model cost budget was exhausted after {round} round(s)."
                    ),
                    crate::budget::BudgetExhaustion::new(
                        crate::budget::BudgetDimension::Cost,
                        cost_spent_micros,
                        max,
                    ),
                ));
            }
            // Epoch totals for continue/resume inheritance (absolute spend).
            // Persist ProgressUpdated so the next turn's seed gate and budget
            // resume see the same ledger (event log is SoT, not in-memory only).
            // Tool-phase increments are re-synced on every exit via
            // `sync_epoch_progress` so resume never under-counts commands/files.
            flush_epoch!(round);

            let assistant = stream_result.message;
            let used_tokens = stream_result.usage.total();
            let finish_reason = stream_result.finish_reason;

            let text = assistant.text_content();
            let calls: Vec<ToolCall> = assistant
                .content
                .iter()
                .filter_map(|p| match p {
                    ContentPart::ToolCall { call } => Some(call.clone()),
                    _ => None,
                })
                .collect();

            match finish_reason {
                FinishReason::Length => {
                    if !calls.is_empty() {
                        // The call is incomplete and must never execute; but a
                        // too-large tool call deserves the same bounded second
                        // chance text truncation gets — nudge for a smaller
                        // re-issue instead of killing the whole turn.
                        if length_continuations >= MAX_LENGTH_CONTINUATIONS
                            || !has_next_round
                            || cancellation.is_cancelled()
                        {
                            return Err(AgentError::Model(ModelError::new(
                                leveler_model::ModelErrorKind::Truncated,
                                "model output ended at the token limit while producing a tool call; the call was not executed",
                            )));
                        }
                        length_continuations += 1;
                        let feedback = Message::text(
                            Role::User,
                            "Your output hit the token limit while emitting a tool call — the \
                             call was NOT executed. Re-issue it smaller: split a large patch \
                             into several apply_patch calls, or shorten the arguments.",
                        );
                        sink.append(std::slice::from_ref(&feedback)).await?;
                        messages.push(feedback);
                        continue;
                    }
                    if text.trim().is_empty()
                        || length_continuations >= MAX_LENGTH_CONTINUATIONS
                        || !has_next_round
                    {
                        return Err(AgentError::Model(ModelError::new(
                            leveler_model::ModelErrorKind::Truncated,
                            "model output remained truncated after bounded continuation attempts",
                        )));
                    }
                    length_continuations += 1;
                    continued_text.push_str(&text);
                    last_text = continued_text.clone();
                    sink.append(std::slice::from_ref(&assistant)).await?;
                    messages.push(assistant);
                    messages.push(Message::text(
                        Role::User,
                        "Continue exactly from the cutoff. Do not repeat prior text. Complete every open list, code block, sentence, and conclusion.",
                    ));
                    context_diverged = true;
                    continue;
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
                    if decode_retries < MAX_DECODE_RETRIES
                        && has_next_round
                        && !cancellation.is_cancelled()
                    {
                        decode_retries += 1;
                        let feedback = Message::text(
                            Role::User,
                            "Your last response declared a tool call but no complete call \
                             arrived (it was likely cut off in transit). Re-issue the tool \
                             call in full.",
                        );
                        sink.append(std::slice::from_ref(&feedback)).await?;
                        messages.push(feedback);
                        continue;
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
                if continued_text.is_empty() {
                    last_text = text.clone();
                } else {
                    continued_text.push_str(&text);
                    last_text = continued_text.clone();
                }
                observer(AgentEvent::AssistantText(last_text.clone()));
            }

            messages.push(assistant.clone());

            // Cost tip-over after this response with no tools: end now (no more rounds).
            if calls.is_empty()
                && let Some((reason, exhaustion)) = budget_exceeded.take()
            {
                sink.append(&[assistant]).await?;
                observer(AgentEvent::Finished(reason.clone()));
                flush_epoch!(round);
                drain_background_children!(round);
                return Ok(AgentOutcome::drive_budget_exhausted(
                    reason,
                    round,
                    modified_files,
                    exhaustion,
                    &metrics,
                    &progress,
                    &objective,
                ));
            }

            if calls.is_empty() && !background_children.children.is_empty() {
                // V2: a quiet round while background children run is WAITING,
                // not a stall — hold for the next settlement instead of
                // burning closeout nudges or classifying no-progress. The
                // settlement itself is injected at the next round top.
                sink.append(&[assistant]).await?;
                while !background_children.children.is_empty()
                    && !background_children
                        .children
                        .iter()
                        .any(|child| child.handle.is_finished())
                    && !cancellation.is_cancelled()
                {
                    tokio::select! {
                        biased;
                        _ = cancellation.cancelled() => {}
                        Some(event) = bg_progress_rx.recv() => forward_child_event!(event),
                        _ = tokio::time::sleep(std::time::Duration::from_millis(200)) => {}
                    }
                }
                continue;
            }

            if calls.is_empty() {
                // Unified closeout (executor/closeout.rs): a quiet round
                // produces AT MOST one nudge — chosen by priority
                // (EmptyAnswer > GoalUnresolved >
                // AnswerIncomplete) — and every mechanism draws from the same
                // per-turn budget. Past the budget a goal-mode quiet ends as
                // `Stalled` — never as a success, so a model that never learns
                // to call update_goal terminates without the harness declaring
                // completion on its behalf; a non-goal turn ends `Answered`.
                //
                // No-progress is counted once when this drive ends as Stalled
                // (below), not on every quiet nudge — so one drive can still
                // use its nudge budget, while Engine continue is capped across
                // turns.
                let impact = ChangeImpact {
                    has_mutation: !modified_files.is_empty(),
                    verified_after_last_mutation: verification_ran,
                    build_relevant: modified_files.is_empty()
                        || modified_files.iter().any(|f| is_build_relevant(f)),
                    modified_files: modified_files.clone(),
                };
                let has_final_text = !last_text.trim().is_empty();
                let action = decide(&CloseoutInput {
                    goal_mode: self.policy.goal_mode,
                    has_final_text,
                    impact: &impact,
                    cancelled: cancellation.is_cancelled(),
                    can_continue: has_next_round,
                    budget_remaining: closeout_budget.remaining(),
                    human_boundary_seen: progress.human_boundary_seen(),
                });
                // Whether the harness accepted the quiet round or bought itself
                // another model call is the difference between "the model is
                // slow" and "we added a round" — indistinguishable on screen.
                tracing::info!(
                    round,
                    ?action,
                    goal_mode = self.policy.goal_mode,
                    has_final_text,
                    has_mutation = impact.has_mutation,
                    build_relevant = impact.build_relevant,
                    verified = impact.verified_after_last_mutation,
                    budget_remaining = closeout_budget.remaining(),
                    "closeout decided"
                );
                if let CloseoutAction::NudgeOnce(reason) = action {
                    closeout_budget.consume();
                    metrics.extra_model_calls += 1;
                    // Surface the injection: without this the user sees a
                    // "final" answer and then an unexplained extra model round.
                    observer(AgentEvent::AdvisoryStarted {
                        kind: AdvisoryKind::CloseoutNudge(reason),
                    });
                    let nudge = match reason {
                        CloseoutReason::GoalUnresolved => {
                            Message::text(Role::User, goal_resolve_nudge(&original_task))
                        }
                        CloseoutReason::EmptyAnswer => Message::text(
                            Role::User,
                            "Your last message was empty. Reply with the actual answer to the \
                             request — do not send an empty message.",
                        ),
                    };
                    // Persist BOTH the quiet-round assistant text and the nudge:
                    // an engine continuation reloads the transcript from the
                    // sink, and any gap here makes the resumed model see a
                    // different conversation than the one it actually had.
                    sink.append(&[assistant, nudge.clone()]).await?;
                    messages.push(nudge);
                    continue;
                }
                sink.append(&[assistant]).await?;
                observer(AgentEvent::Finished(last_text.clone()));
                // In goal mode reaching this point means the model went quiet
                // through every nudge without ever calling update_goal — that
                // is a stall, not a proven completion. The detail carries the
                // closeout reason so an engine continuation knows what the
                // previous turn stalled on.
                let (stop_reason, stop_detail) = if self.policy.goal_mode
                    && progress.human_boundary_seen()
                {
                    // User said no, model went quiet without resolving the
                    // goal. That is blocked, not a stall — and it is reported
                    // as such, since nothing re-drives a turn on its own.
                    progress.enter_terminal();
                    observer(AgentEvent::ProgressUpdated {
                        ledger: progress.clone(),
                    });
                    (
                        StopReason::Blocked,
                        Some("user denied a required permission; goal left unresolved".to_string()),
                    )
                } else if self.policy.goal_mode {
                    // One no-progress tick per stalled drive so Engine
                    // continue_active_goal cannot open unbounded turns.
                    progress.note_no_progress_round(round);
                    if progress.should_hard_stop_no_progress(progress_caps) {
                        progress.enter_terminal();
                    }
                    observer(AgentEvent::ProgressUpdated {
                        ledger: progress.clone(),
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
                } else {
                    if progress.human_boundary_seen() {
                        progress.enter_terminal();
                        observer(AgentEvent::ProgressUpdated {
                            ledger: progress.clone(),
                        });
                    }
                    (StopReason::Answered, None)
                };
                flush_epoch!(round);

                drain_background_children!(round);
                return Ok(AgentOutcome::drive_result(
                    last_text,
                    round,
                    modified_files,
                    stop_reason,
                    stop_detail,
                    &metrics,
                    &progress,
                    &objective,
                ));
            }

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
                loop_key: String,
            }
            let mut parallel_jobs: Vec<ParallelJob> = Vec::new();
            // spawn_agent calls deferred to run concurrently after this pass.
            let mut spawn_jobs: Vec<(usize, ToolCall)> = Vec::new();
            // Calls a guard refused before they ran (loop guard, budgets,
            // allowlist, permission). A round consisting solely of refusals is
            // no progress — it feeds the all-refused streak.
            let mut denied_calls_this_round: usize = 0;
            // User cancel observed inside this batch. The batch stops, but the
            // round is still committed (results + spend) before Cancelled
            // surfaces — completed tools' side effects are already on disk.
            let mut cancelled_mid_batch = false;
            // Ids/names survive the consuming loop below so calls the cancel
            // cut short can still be refused in place (transcript pairing).
            let call_snapshot: Vec<ToolCall> = calls.clone();

            for (index, call) in calls.into_iter().enumerate() {
                // Cap consecutive search calls so the model acts on
                // what they have instead of searching in circles (spec §17).
                let tool_is_search = is_search_tool(&call.name);
                if tool_is_search && self.policy.max_search_calls_per_step > 0 {
                    consecutive_searches += 1;
                }
                if let gates::GateVerdict::Refuse(msg) = gates::search_budget_gate(
                    tool_is_search,
                    consecutive_searches,
                    self.policy.max_search_calls_per_step,
                ) {
                    denied_calls_this_round += 1;
                    results[index] = Some(deny_call(observer, call, msg));
                    continue;
                }

                // A child reports one typed finding: validated at the tool
                // boundary, recorded in ITS ledger (the parent adopts on
                // join), persisted through the same EvidenceLedgerUpdated
                // events as every other ledger change.
                if call.name == REPORT_FINDING_TOOL && self.depth > 0 {
                    observer(AgentEvent::ToolCall {
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
                            "report_finding requires a `kind` from the documented list."
                                .to_string(),
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
                            let id = ledger.record_finding(
                                kind,
                                summary,
                                field("file"),
                                field("symbol"),
                            );
                            observer(AgentEvent::EvidenceLedgerUpdated {
                                ledger: ledger.clone(),
                            });
                            (true, format!("Finding {id} recorded."))
                        }
                    };
                    observer(AgentEvent::ToolResult {
                        id: call.id.as_str().to_string(),
                        name: REPORT_FINDING_TOOL.to_string(),
                        is_error: !ok,
                        preview: preview(&msg),
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
                if self.policy.goal_mode && call.name == UPDATE_GOAL_TOOL {
                    // Surface the resolution so the TUI/JSONL shows the goal being
                    // closed (special tools otherwise skip the ToolCall event).
                    observer(AgentEvent::ToolCall {
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
                        && !background_children.children.is_empty()
                    {
                        let waiting: Vec<String> = background_children
                            .children
                            .iter()
                            .map(|c| format!("{} ({})", c.nickname, c.id))
                            .collect();
                        // Review 必改②: this drain runs MID tool batch — pin the
                        // parent's same-batch commands first, or the settlement
                        // fold overwrites local counters with a lagging ledger
                        // (the exact under-count pin_parent_batch_work exists
                        // to prevent on the foreground path).
                        pin_parent_batch_work(&mut progress, commands_run, &modified_files);
                        drain_background_children!(round);
                        let feedback = format!(
                            "Cannot complete: delegated sub-agent(s) {} were still \
                             running. They have now settled — their notices are above. \
                             Inspect and integrate their results (judge any findings), \
                             re-verify, then call update_goal again.",
                            waiting.join(", ")
                        );
                        observer(AgentEvent::GoalIntercepted {
                            kind: "outstanding_children".to_string(),
                            detail: waiting.join(", "),
                        });
                        observer(AgentEvent::ToolResult {
                            id: call.id.as_str().to_string(),
                            name: UPDATE_GOAL_TOOL.to_string(),
                            is_error: true,
                            preview: preview(&feedback),
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
                            observer(AgentEvent::ToolResult {
                                id: call.id.as_str().to_string(),
                                name: UPDATE_GOAL_TOOL.to_string(),
                                is_error: true,
                                preview: preview(&feedback),
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
                    // Mechanical readiness only: the model's own open todos
                    // and the acceptance commands the USER wrote down. What a
                    // child reported is information the model already read.
                    if reason == StopReason::Completed {
                        let gate = GateConfig {
                            goal_todo_gate: self.policy.goal_todo_gate,
                            todo_override_allowed: true,
                        };
                        ledger.plan = plan_state.clone();
                        // Explicit structured flag only — never attempt-count bypass.
                        let explicit_todo_override = call
                            .arguments
                            .get("override_incomplete_todos")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        if let Err(fail) = check(&plan_state, &gate, explicit_todo_override) {
                            ledger.record_intercept("update_goal", fail.to_string());
                            ledger.plan = plan_state.clone();
                            observer(AgentEvent::GoalIntercepted {
                                kind: "update_goal".into(),
                                detail: fail.to_string(),
                            });
                            observer(AgentEvent::EvidenceLedgerUpdated {
                                ledger: ledger.clone(),
                            });
                            let feedback = format!(
                                "update_goal(complete) refused: {fail}. Finish the remaining \
                                 plan steps (or mark them with update_plan) and run any \
                                 acceptance command the task named, then try again. \
                                 Incomplete todos require override_incomplete_todos=true \
                                 (only when override is allowed) — a second bare complete is not enough."
                            );
                            observer(AgentEvent::ToolResult {
                                id: call.id.as_str().to_string(),
                                name: UPDATE_GOAL_TOOL.to_string(),
                                is_error: true,
                                preview: preview(&feedback),
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
                        // HostImplicit single-step completes atomically with the goal.
                        if plan_state.is_host_implicit() {
                            plan_state.mark_all_completed();
                            metrics.plan_updated += 1;
                            observer(AgentEvent::PlanUpdated {
                                steps: plan_state.steps.clone(),
                            });
                        }
                    }
                    observer(AgentEvent::ToolResult {
                        id: call.id.as_str().to_string(),
                        name: UPDATE_GOAL_TOOL.to_string(),
                        is_error: false,
                        preview: "Goal resolved.".to_string(),
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
                if is_user_input_tool(&call.name) {
                    let answer = self.handle_ask_user(&call, &cancellation).await?;
                    results[index] = Some(ContentPart::ToolResult {
                        result: ToolResultContent {
                            call_id: call.id,
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
                    observer(AgentEvent::ToolCall {
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
                    let (content, is_error) = if self.depth == 0 {
                        (
                            "You are the top-level agent: you already write directly \
                             (outside children's claimed scopes). claim_write_scope is \
                             for spawned children."
                                .to_string(),
                            true,
                        )
                    } else {
                        let owner = self
                            .agent_id
                            .clone()
                            .unwrap_or_else(|| format!("child-depth-{}", self.depth));
                        match self.ownership.try_claim(&owner, &paths) {
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
                                    crate::ownership::ClaimRejection::Conflicts(conflicts) => {
                                        conflicts
                                            .iter()
                                            .map(|c| format!("{} owned by {}", c.path, c.owner))
                                            .collect::<Vec<_>>()
                                            .join("; ")
                                    }
                                    other => format!("{other:?}"),
                                };
                                (rejection.for_model(), false)
                            }
                        }
                    };
                    observer(AgentEvent::ToolResult {
                        id: call.id.as_str().to_string(),
                        name: CLAIM_WRITE_SCOPE_TOOL.to_string(),
                        is_error,
                        preview: preview(&content),
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
                    if self.depth > 0 {
                        let action = if granted {
                            "ownership_granted".to_string()
                        } else {
                            "ownership_denied".to_string()
                        };
                        match (&self.event_barrier, &self.agent_id) {
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
                                let owner = self
                                    .agent_id
                                    .clone()
                                    .unwrap_or_else(|| format!("child-depth-{}", self.depth));
                                observer(AgentEvent::DelegationStage {
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
                    if progress.covers_denied_request(requested.network, requested.unrestricted_fs)
                    {
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
                        .handle_request_permissions(&call, &cancellation)
                        .await?;
                    match &outcome {
                        PermissionRequestOutcome::Granted { grants, .. } => {
                            turn_grants = turn_grants.merge(*grants);
                        }
                        PermissionRequestOutcome::DeniedByUser { requested, .. } => {
                            progress
                                .record_human_denial(requested.network, requested.unrestricted_fs);
                            observer(AgentEvent::ProgressUpdated {
                                ledger: progress.clone(),
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
                if self.registry.mutates_files(&call.name) {
                    let mut target_paths = scoped_paths.clone();
                    collect_scoped_paths_from_call(&call, &mut target_paths);
                    let fresh = load_scoped_rules(
                        self.tool_context.execution.workspace.root(),
                        &target_paths,
                        &injected_rule_sources,
                    );
                    if !fresh.is_empty() {
                        scoped_paths = target_paths;
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
                        results[index] = Some(deny_call(observer, call, msg));
                        continue;
                    }
                }

                // No-progress loop guard: same observe class (e.g. git status via
                // run_command vs shell_command) or exact (tool, args) already
                // produced an identical result LOOP_GUARD_THRESHOLD times.
                let loop_key = observe_class(&call.name, &call.arguments)
                    .unwrap_or_else(|| format!("{}\0{}", call.name, compact_json(&call.arguments)));
                let repeats = if self.policy.progress_guards {
                    match call_history.get(&loop_key) {
                        // Something novel happened since this key last repeated:
                        // let it run and let the RESULT decide, instead of
                        // predicting that the world stood still.
                        Some((_, _, epoch)) if *epoch != novelty_epoch => 0,
                        Some((_, n, _)) => *n,
                        None => 0,
                    }
                } else {
                    0
                };
                if let gates::GateVerdict::Refuse(msg) = gates::loop_guard(&call.name, repeats) {
                    // The loop guard only refuses after the SAME key produced
                    // IDENTICAL content twice: mechanical repetition, counted
                    // on the all-refused track like every other refusal.
                    denied_calls_this_round += 1;
                    results[index] = Some(deny_call(observer, call, msg));
                    continue;
                }

                // Step budgets (spec §27): refuse the call BEFORE it runs once a
                // limit is reached; the run ends after this round's results are
                // committed. File budget also refuses a single multi-file patch
                // that would cross the remaining task-level cap (not only when
                // already exhausted).
                let epoch_file_count = projected_epoch_file_count(&progress, &modified_files);
                let over_budget = if self.registry.runs_command(&call.name)
                    && self
                        .step_limits
                        .max_commands
                        .is_some_and(|max| commands_run >= max)
                {
                    // All shell paths count (including verify/acceptance-class runs).
                    // Some(0) = hard exhausted (not unlimited).
                    let cap = u64::from(self.step_limits.max_commands.unwrap_or(0));
                    Some((
                        format!("the {cap}-command budget is exhausted"),
                        crate::budget::BudgetExhaustion::new(
                            crate::budget::BudgetDimension::Commands,
                            u64::from(commands_run),
                            cap,
                        ),
                    ))
                } else if self.registry.mutates_files(&call.name) {
                    file_budget_refusal(
                        self.step_limits.max_modified_files,
                        epoch_file_count,
                        &call,
                        &progress,
                        &modified_files,
                    )
                    .map(|which| {
                        let cap = self.step_limits.max_modified_files.unwrap_or(0) as u64;
                        (
                            which,
                            crate::budget::BudgetExhaustion::new(
                                crate::budget::BudgetDimension::ModifiedFiles,
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
                    budget_exceeded = Some((format!("Stopped: {which}."), exhaustion));
                    denied_calls_this_round += 1;
                    results[index] = Some(deny_call(observer, call, msg));
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
                if self.registry.mutates_files(&call.name) {
                    let owner_key = match (&self.agent_id, self.depth) {
                        (Some(id), _) => id.clone(),
                        (None, 0) => "parent".to_string(),
                        (None, depth) => format!("child-depth-{depth}"),
                    };
                    let targets = crate::authorization::mutation_targets(&call);
                    let hits = if self.depth == 0 {
                        match self.ownership.try_mutation_guard(&owner_key, &targets) {
                            Ok(guard) => {
                                _mutation_guard = Some(guard);
                                Vec::new()
                            }
                            Err(conflicts) => conflicts,
                        }
                    } else {
                        self.ownership.conflicts_for(&targets, &owner_key)
                    };
                    if !hits.is_empty() {
                        let inside: Vec<String> = hits.iter().map(|c| c.path.clone()).collect();
                        let mut owners: Vec<String> =
                            hits.iter().map(|c| c.owner.clone()).collect();
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
                        results[index] = Some(deny_call(observer, call, msg));
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
                if let Some(msg) = self.refuse_unboundable_delegated_tool(&call) {
                    denied_calls_this_round += 1;
                    results[index] = Some(deny_call(observer, call, msg));
                    continue;
                }
                if let Some(msg) = self.refuse_unscoped_mutation(&call) {
                    denied_calls_this_round += 1;
                    results[index] = Some(deny_call(observer, call, msg));
                    continue;
                }

                // Read-only, side-effect-free tools are deferred to the
                // concurrent batch below; mark the event so a UI can render them
                // as one parallel group.
                let parallel = self
                    .registry
                    .get(&call.name)
                    .map(|t| t.supports_parallel())
                    .unwrap_or(false);
                observer(AgentEvent::ToolCall {
                    id: call.id.as_str().to_string(),
                    name: call.name.clone(),
                    arguments: compact_json(&call.arguments),
                    parallel,
                });
                collect_scoped_paths_from_call(&call, &mut scoped_paths);

                // A command may carry its own elevation for THIS call. Settling
                // it here — between the call event and the context — is what
                // makes approval and execution one round trip instead of two.
                let mut call_grants = TurnPermissionGrants::default();
                if is_escalatable_tool(&call.name)
                    && let Some((reason, requested)) = parse_escalation(&call.arguments)
                {
                    if requested.is_empty() {
                        // Malformed: no axis named. Refuse without interrupting
                        // the user — there is nothing to put in a prompt.
                        results[index] = Some(ContentPart::ToolResult {
                            result: ToolResultContent {
                                call_id: call.id,
                                content: escalation_missing_axis_message(),
                                is_error: true,
                            },
                        });
                        continue;
                    }
                    if progress.covers_denied_request(requested.network, requested.unrestricted_fs)
                    {
                        results[index] = Some(ContentPart::ToolResult {
                            result: ToolResultContent {
                                call_id: call.id,
                                content: permission_already_denied_message(),
                                is_error: true,
                            },
                        });
                        continue;
                    }
                    let action = escalation_action(&call);
                    let outcome = self
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
                        // One-shot by construction: this never touches
                        // `turn_grants`, so the next call starts confined again.
                        PermissionRequestOutcome::Granted { grants, .. } => {
                            call_grants = *grants;
                        }
                        PermissionRequestOutcome::DeniedByUser { requested, .. } => {
                            progress
                                .record_human_denial(requested.network, requested.unrestricted_fs);
                            observer(AgentEvent::ProgressUpdated {
                                ledger: progress.clone(),
                            });
                        }
                        _ => {}
                    }
                    // A refused elevation stops the command. Running it
                    // unelevated would hand the model the same denial it was
                    // already answering.
                    if outcome.is_error() {
                        results[index] = Some(ContentPart::ToolResult {
                            result: ToolResultContent {
                                call_id: call.id,
                                content: outcome.message().to_string(),
                                is_error: true,
                            },
                        });
                        continue;
                    }
                }

                // Build the effective context: turn grants from
                // request_permissions, plus this call's own escalation.
                let ctx =
                    apply_turn_grants(self.tool_context.clone(), turn_grants.merge(call_grants));
                // Full epoch path set so tools count "new" files correctly
                // (re-edits of already-budgeted paths do not consume residual).
                let epoch_paths = epoch_modified_paths(&progress, &modified_files);
                let remaining_files = self
                    .step_limits
                    .max_modified_files
                    .map(|max| max.saturating_sub(epoch_paths.len()));
                let ctx = ctx.with_command_write_constraints(
                    self.effective_write_allowlist(),
                    remaining_files,
                    epoch_paths,
                );
                // What a concurrent sibling owns right now. A command's changes
                // are attributed by diffing the whole workspace, which in a
                // shared tree also sees the sibling's writes; this call cannot
                // have made them, so they must not be charged here and rolled
                // back with it.
                let owner_key = match (&self.agent_id, self.depth) {
                    (Some(id), _) => id.clone(),
                    (None, 0) => "parent".to_string(),
                    (None, depth) => format!("child-depth-{depth}"),
                };
                let ctx =
                    ctx.with_foreign_owned_paths(self.ownership.paths_owned_by_others(&owner_key));

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
                    executed_commands,
                    call,
                ) = match self
                    .admit(call, ctx, parallel, &mut session_approved, &cancellation)
                    .await
                {
                    Ok(admitted) if parallel => {
                        if self.registry.runs_command(&admitted.call.name) {
                            commands_run += 1;
                        }
                        parallel_jobs.push(ParallelJob {
                            index,
                            admitted,
                            loop_key,
                        });
                        continue;
                    }
                    Ok(admitted) => {
                        if self.registry.runs_command(&admitted.call.name) {
                            commands_run += 1;
                        }
                        let files_before = modified_files.clone();
                        // Heartbeat: while a long command runs, emit a
                        // CommandProgress every few seconds so the UI shows
                        // "运行 cargo test · <elapsed>" instead of a bare
                        // "等待模型". select! keeps this in the same task, so
                        // calling `observer` from the ticker branch is sound.
                        let progress_label = command_progress_label(&self.registry, &admitted.call);
                        let (
                            content,
                            is_error,
                            image,
                            workspace_snapshot,
                            plan,
                            call_files,
                            executed_commands,
                        ) = {
                            let started = std::time::Instant::now();
                            let dispatch_fut =
                                self.dispatch(&admitted, &mut modified_files, &cancellation);
                            tokio::pin!(dispatch_fut);
                            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(
                                COMMAND_HEARTBEAT_SECS,
                            ));
                            ticker.tick().await; // drop the immediate first tick
                            loop {
                                tokio::select! {
                                    r = &mut dispatch_fut => break r,
                                    _ = ticker.tick() => {
                                        if let Some(label) = &progress_label {
                                            observer(AgentEvent::CommandProgress {
                                                label: label.clone(),
                                                elapsed_ms: started.elapsed().as_millis() as u64,
                                            });
                                        }
                                    }
                                }
                            }
                        };
                        // Cancel during a long tool must stop the batch — do not
                        // keep running subsequent tools after the user hit Ctrl+C.
                        // This call's own result still flows through: it ran, so
                        // its outcome and spend must reach the transcript/ledger.
                        if cancellation.is_cancelled() && !deadline_expired.load(Ordering::Acquire)
                        {
                            cancelled_mid_batch = true;
                        }
                        let newly = newly_modified_paths(&files_before, &modified_files);
                        (
                            content,
                            is_error,
                            image,
                            workspace_snapshot,
                            plan,
                            newly,
                            call_files,
                            executed_commands,
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
                    observer(AgentEvent::WorkspaceSnapshot {
                        call_id: call.id.as_str().to_string(),
                        snapshot,
                    });
                }
                if let Some(part) = image {
                    pending_images.push(part);
                }

                // Update the loop-guard window: identical output → count up,
                // any change (progress) → reset to this new result.
                match call_history.get_mut(&loop_key) {
                    Some(entry) if entry.0 == content => {
                        entry.1 += 1;
                        entry.2 = novelty_epoch;
                    }
                    _ => {
                        novelty_epoch = novelty_epoch.saturating_add(1);
                        call_history.insert(loop_key, (content.clone(), 1, novelty_epoch));
                    }
                }

                // Validate plan updates against the in-memory mirror before
                // accepting them (skip-step / origin rules). Host mirror only
                // advances on success; tool text is rewritten on rejection.
                let mut content = content;
                let mut is_error = is_error;
                if let Some(steps) = plan
                    && !is_error
                {
                    match PlanState::from_model_explicit(steps) {
                        Ok(next) => {
                            if let Err(msg) =
                                PlanState::validate_no_skip_complete(&plan_state, &next)
                            {
                                content = msg;
                                is_error = true;
                            } else {
                                plan_state = next;
                                structured_plan_started = true;
                                metrics.plan_updated += 1;
                                observer(AgentEvent::PlanUpdated {
                                    steps: plan_state.steps.clone(),
                                });
                            }
                        }
                        Err(msg) => {
                            content = msg;
                            is_error = true;
                        }
                    }
                }

                observer(AgentEvent::ToolResult {
                    id: call.id.as_str().to_string(),
                    name: call.name.clone(),
                    is_error,
                    preview: preview(&content),
                });

                // A concrete action clears the consecutive-search counter.
                if !is_search_tool(&call.name) {
                    consecutive_searches = 0;
                }
                // A passing verification-class command is completion evidence;
                // an arbitrary command (echo, ls, …) is not. What ran comes
                // from the execution layer's own report, so the SAME
                // `go build ./...` counts whether the model routed it through
                // `run_command` or `shell_command` (HC-002 F1). A shape whose
                // zero exit proves nothing about its members reports nothing,
                // and one execution is recorded once.
                if !is_error {
                    let mut recorded: Vec<String> = Vec::new();
                    for command in &executed_commands {
                        let (program, args) = command.split_first().expect("non-empty command");
                        if !is_verification_program(program) {
                            continue;
                        }
                        let fp = EvidenceLedger::normalize_command_fingerprint(program, args);
                        if recorded.contains(&fp) {
                            continue;
                        }
                        ledger.record_verify(call.id.as_str(), fp.clone(), 0);
                        recorded.push(fp);
                    }
                    // A verification program that ran in a shape whose exit
                    // proves nothing is real to the model and invisible to the
                    // ledger. Say so on the result, where the model is looking,
                    // instead of at the next refused close.
                    if let Some(note) = unproven_verification_note(
                        &call.name,
                        &call.arguments,
                        &executed_commands,
                        is_error,
                    ) {
                        ledger.record_intercept("unproven_verification", note.clone());
                        content.push_str("\n\n");
                        content.push_str(&note);
                    }
                    if !recorded.is_empty() {
                        verification_ran = true;
                        ledger.plan = plan_state.clone();
                        observer(AgentEvent::EvidenceLedgerUpdated {
                            ledger: ledger.clone(),
                        });
                    }
                }
                // Any tool that newly modified files records a mutation (not
                // only apply_patch/replace by name). Paths are this call only.
                if !is_error && call_mutated {
                    if !newly_modified.is_empty() {
                        verification_ran = false;
                    }
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
                        &mut ledger,
                        call.id.as_str(),
                        call.name.as_str(),
                        touched,
                        &plan_state,
                        observer,
                    );
                    if self.registry.mutates_files(&call.name) && !structured_plan_started {
                        metrics.first_write_before_plan = true;
                    }
                }
                for path in &modified_files {
                    push_unique_path(&mut scoped_paths, path);
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
                // Leveling knob: bound how many of the batch actually overlap
                // (policy `max_parallel_tools`; 0 = the whole batch at once).
                let permits = match self.policy.max_parallel_tools {
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
                        let out = self.dispatch_raw(&job.admitted, cancellation_ref).await;
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
                        if !modified_files.iter().any(|e| e == f) {
                            modified_files.push(f.clone());
                        }
                    }
                    if !is_error && !job_files.is_empty() {
                        verification_ran = false;
                        note_tool_side_effects(
                            &mut ledger,
                            job.admitted.call.id.as_str(),
                            job.admitted.call.name.as_str(),
                            job_files.clone(),
                            &plan_state,
                            observer,
                        );
                    }
                    if let Some(part) = extract_image(&metadata) {
                        pending_images.push(part);
                    }
                    match call_history.get_mut(&job.loop_key) {
                        Some(entry) if entry.0 == content => {
                            entry.1 += 1;
                            entry.2 = novelty_epoch;
                        }
                        _ => {
                            novelty_epoch = novelty_epoch.saturating_add(1);
                            call_history
                                .insert(job.loop_key.clone(), (content.clone(), 1, novelty_epoch));
                        }
                    }
                    observer(AgentEvent::ToolResult {
                        id: job.admitted.call.id.as_str().to_string(),
                        name: job.admitted.call.name.clone(),
                        is_error,
                        preview: preview(&content),
                    });
                    if !is_error && let Some(steps) = extract_plan(&metadata) {
                        observer(AgentEvent::PlanUpdated { steps });
                    }
                    if !is_search_tool(&job.admitted.call.name) {
                        consecutive_searches = 0;
                    }
                    for path in &modified_files {
                        push_unique_path(&mut scoped_paths, path);
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
                let mut futs = FuturesUnordered::new();
                // Parent may have run shells/edits in this same tool batch
                // before children. Pin that spend on the ledger now — otherwise
                // absorb_child_work + `commands_run = progress.cumulative_*`
                // overwrites local counters with a lagging ledger (mixed batch
                // under-counts parent commands).
                pin_parent_batch_work(&mut progress, commands_run, &modified_files);
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
                    Option<leveler_model::ModelRef>,
                    Vec<String>,
                    u32,
                    bool, // run in background (runtime-resolved default: true)
                )> = Vec::new();
                // Exclusive scopes of workers already admitted in THIS batch.
                // "Exclusive" is only true if admission enforces it: two
                // overlapping scopes in one batch is last-writer-wins waiting
                // to happen, so the second one is refused honestly.
                let mut admitted_worker_scopes: Vec<Vec<String>> = Vec::new();
                for (index, call) in spawn_jobs {
                    let background =
                        crate::injected_tools::resolve_run_in_background(&call.arguments);
                    let task = call
                        .arguments
                        .get("task")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    // A named persona supplies the instructions and, unless the
                    // caller overrides it, the role.
                    let agent_name = call
                        .arguments
                        .get("agent")
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|s| !s.is_empty());
                    let named = agent_name.map(|name| {
                        (
                            name,
                            crate::named_agent::load(
                                self.tool_context.execution.workspace.root(),
                                name,
                            ),
                        )
                    });
                    let profile_arg = call
                        .arguments
                        .get("profile")
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|s| !s.is_empty());
                    let explicit_role = call.arguments.get("role").and_then(|v| v.as_str());
                    // A named persona supplies the role only when the caller
                    // did not override it with `profile` or `role`.
                    let named_role = match &named {
                        Some((_, Some(agent)))
                            if explicit_role.is_none() && profile_arg.is_none() =>
                        {
                            Some(agent.role.as_str())
                        }
                        _ => None,
                    };
                    let role_hint = explicit_role.or(named_role);
                    let task = match &named {
                        Some((_, Some(agent))) => agent.compose_task(&task),
                        _ => task,
                    };
                    // A definition may pin its own model (e.g. run investigation
                    // on a cheaper one). An unparsable ref is rejected below
                    // rather than silently falling back to the parent's model.
                    let pinned_model: Option<&str> = match &named {
                        Some((_, Some(agent))) if !agent.model.trim().is_empty() => {
                            Some(agent.model.trim())
                        }
                        _ => None,
                    };
                    let model_override = pinned_model.and_then(leveler_model::ModelRef::parse);
                    // A definition's own policy: the tools it may hold and how
                    // long it may run. Empty / 0 means inherit.
                    let (agent_tools, agent_max_rounds) = match &named {
                        Some((_, Some(agent))) => (agent.tools.clone(), agent.max_rounds),
                        _ => (Vec::new(), 0),
                    };
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
                    let admitted_profile =
                        ChildProfile::admit_spawn(profile_arg, role_hint, &files);

                    // Reject (no agent started) for depth, empty task, or cap.
                    let reject = if let Some((requested, None)) = &named {
                        // Never fall back to a personaless spawn: the run would
                        // look successful while doing something else entirely.
                        let known = crate::named_agent::discover(
                            self.tool_context.execution.workspace.root(),
                        )
                        .into_iter()
                        .map(|a| a.name)
                        .collect::<Vec<_>>()
                        .join(", ");
                        Some(format!(
                            "Unknown agent `{requested}`. Available: {known}. Omit `agent` to \
                             spawn with an inline task instead."
                        ))
                    } else if self.depth >= MAX_SUB_AGENT_DEPTH {
                        Some("Sub-agents may not spawn their own sub-agents.".to_string())
                    } else if let (Some(raw), None) = (pinned_model, &model_override) {
                        Some(format!(
                            "Agent `{}` pins model `{raw}`, which is not a valid `provider/model` \
                             reference. Fix the agent definition.",
                            agent_name.unwrap_or("?")
                        ))
                    } else if task.is_empty() {
                        Some("spawn_agent requires a non-empty task.".to_string())
                    } else if let Err(msg) = &admitted_profile {
                        // Capability negotiation: the requested profile/role +
                        // scope against the contract. Honest denial, never a
                        // silent downgrade.
                        Some(msg.clone())
                    } else if admitted_profile
                        .as_ref()
                        .is_ok_and(|p| p.role == AgentRole::Worker)
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
                    } else if admitted_profile
                        .as_ref()
                        .is_ok_and(|p| p.role == AgentRole::Worker)
                        && !self.ownership.conflicts_for(&files, "").is_empty()
                    {
                        // Legacy pre-scoped Worker: its files are an exclusive
                        // claim in the SAME registry late-bound children use —
                        // a conflict with any live claim is an honest denial.
                        let hits = self.ownership.conflicts_for(&files, "");
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
                    } else if (progress.children_spawned_total as usize)
                        >= self.policy.max_total_agents
                    {
                        // MA-RT-1: the durable task-epoch total, not a
                        // drive-local counter — a turn, window, or restart
                        // boundary must not hand the model a fresh quota.
                        Some(format!(
                            "Sub-agent limit reached ({} total for this task). Do the \
                             remaining work directly.",
                            self.policy.max_total_agents
                        ))
                    } else {
                        None
                    };
                    if let Some(msg) = reject {
                        observer(AgentEvent::ToolResult {
                            id: call.id.as_str().to_string(),
                            name: SPAWN_AGENT_TOOL.to_string(),
                            is_error: true,
                            preview: msg.clone(),
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

                    let Ok(profile) = admitted_profile else {
                        unreachable!("admission already succeeded");
                    };
                    let role = profile.role;
                    if role == AgentRole::Worker {
                        admitted_worker_scopes.push(files.clone());
                    }
                    progress.children_spawned_total += 1;
                    let id = new_delegated_agent_id();
                    let nickname = agent_nickname(progress.children_spawned_total as usize);
                    self.ownership
                        .register_owner(&id, &format!("{nickname} ({id})"));
                    if role == AgentRole::Worker && !files.is_empty() {
                        // Atomic legacy pre-claim; admission above already
                        // verified there is no live conflict, and this batch's
                        // earlier spawns claim before later ones are admitted.
                        if let Err(rejection) = self.ownership.try_claim(&id, &files) {
                            let msg = rejection.for_model();
                            self.ownership.release_all(&id);
                            observer(AgentEvent::ToolResult {
                                id: call.id.as_str().to_string(),
                                name: SPAWN_AGENT_TOOL.to_string(),
                                is_error: true,
                                preview: preview(&msg),
                            });
                            results[index] = Some(ContentPart::ToolResult {
                                result: ToolResultContent {
                                    call_id: call.id,
                                    content: msg,
                                    is_error: true,
                                },
                            });
                            progress.children_spawned_total =
                                progress.children_spawned_total.saturating_sub(1);
                            continue;
                        }
                    }
                    let started_task = if role == AgentRole::Worker && !files.is_empty() {
                        format!("{task}\n[scope: {}]", files.join(", "))
                    } else {
                        task.clone()
                    };
                    observer(AgentEvent::ProgressUpdated {
                        ledger: progress.clone(),
                    });
                    let (profile_id, profile_role, read_only) = profile.trace_fields();
                    observer(AgentEvent::SubAgentStarted {
                        id: id.clone(),
                        nickname: nickname.clone(),
                        role: role.label().to_string(),
                        task: started_task,
                        profile_id: Some(profile_id),
                        profile_role: Some(profile_role),
                        read_only,
                    });
                    accepted.push((
                        index,
                        call.id,
                        role,
                        files,
                        task,
                        id,
                        nickname,
                        model_override,
                        agent_tools,
                        agent_max_rounds,
                        background,
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
                    && let Some(barrier) = &self.event_barrier
                {
                    barrier.flush().await?;
                }
                let share_n = accepted.len() as u32;
                for (
                    share_of,
                    (
                        index,
                        call_id,
                        role,
                        files,
                        task,
                        id,
                        nickname,
                        model_override,
                        agent_tools,
                        agent_max_rounds,
                        background,
                    ),
                ) in accepted.into_iter().enumerate()
                {
                    let sem = run_agents_semaphore.clone();
                    let token = cancellation.child_token();
                    // Residual parent budgets split across concurrent spawns.
                    let residual = residual_step_limits(
                        self.step_limits,
                        commands_run,
                        model_tokens_spent,
                        cost_spent_micros,
                        projected_epoch_file_count(&progress, &modified_files),
                        epoch_duration_at_start,
                        run_started,
                        share_of as u32,
                        share_n,
                    );
                    let parent_wall = super::handlers::ParentWallBudget {
                        cap: self.step_limits.max_duration,
                        epoch_duration_at_start,
                        run_started,
                    };
                    if background {
                        // V2 background-first: spawn the owned child future and
                        // return the tool result immediately. Settlement is
                        // injected at a later round boundary; the child's
                        // progress/activity events flow through the run-level
                        // channel.
                        let fut = self.sub_agent_run_future(
                            id.clone(),
                            role,
                            files.clone(),
                            model_override,
                            agent_tools,
                            agent_max_rounds,
                            task,
                            sem,
                            bg_progress_tx.clone(),
                            residual,
                            token,
                            parent_wall,
                        );
                        let handle = tokio::spawn(fut);
                        progress.outstanding_children.push(format!(
                            "{id}|{nickname}|{}|{}",
                            role.label(),
                            files.join(",")
                        ));
                        observer(AgentEvent::ProgressUpdated {
                            ledger: progress.clone(),
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
                        observer(AgentEvent::ToolResult {
                            id: call_id.as_str().to_string(),
                            name: SPAWN_AGENT_TOOL.to_string(),
                            is_error: false,
                            preview: preview(&content),
                        });
                        background_children.children.push(BackgroundChild {
                            id,
                            nickname,
                            role,
                            scope: files,
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
                    futs.push(async move {
                        let result = self
                            .run_one_sub_agent_on(
                                id.clone(),
                                role,
                                files,
                                model_override,
                                agent_tools,
                                agent_max_rounds,
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

                while !futs.is_empty() {
                    tokio::select! {
                        biased;
                        Some(progress_ev) = progress_rx.recv() => forward_child_event!(progress_ev),
                        Some((index, call_id, id, nickname, role, result)) = futs.next() => {
                            self.ownership.release_all(&id);
                            let (content, ok) = fold_child_settlement(
                                &mut progress,
                                &mut commands_run,
                                &mut modified_files,
                                &mut ledger,
                                observer,
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
                                        self.tool_context.policy.tool_output_budget,
                                    ),
                                    is_error: !ok,
                                },
                            });
                        }
                    }
                }
                while let Ok(progress) = progress_rx.try_recv() {
                    forward_child_event!(progress);
                }
                drop(futs);
                // Always flush after sub-agent batch so absorbed spend is durable
                // even when the parent is about to cancel.
                flush_epoch!(round);
                if cancellation.is_cancelled() && !deadline_expired.load(Ordering::Acquire) {
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
                            observer,
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
            flush_epoch!(round);
            sink.append(&[assistant, tool_message]).await?;
            // Batch was cancelled: the round is durable (results paired, spend
            // flushed above) — exit now instead of starting another model round.
            if cancelled_mid_batch {
                drain_background_children!(round);
                return Err(AgentError::Cancelled);
            }

            // V2: forward background children's live progress and settle any
            // that finished — the notice must be in context before the next
            // model round ("you are told when one finishes"; no polling).
            settle_finished_children!(round);

            // Plan fully completed → closing phase (a lifecycle fact for the
            // UI and for continuation seeding; nothing is refused for it).
            if plan_state.is_fully_completed() {
                progress.enter_closing();
            }
            // Mechanical no-progress watchdog: a round in which EVERY attempted
            // call was refused before it ran is not progress. Enough of them in
            // a row and the turn stops, so an `UntilTerminal` run cannot spin
            // forever re-issuing guarded actions. Nothing here reads the
            // model's work for meaning: rounds with executed tools — failed or
            // not — always count as progress, and identical repeats are
            // already bounded by the per-key loop guard above.
            let all_refused = self.policy.progress_guards
                && !call_snapshot.is_empty()
                && denied_calls_this_round == call_snapshot.len();
            if all_refused {
                progress.note_no_progress_round(round);
                observer(AgentEvent::ProgressUpdated {
                    ledger: progress.clone(),
                });
                if self.depth == 0 && progress.should_hard_stop_no_progress(progress_caps) {
                    stop_now!(
                        StopReason::Incomplete,
                        "no-progress streak; all-refused rounds short-circuited",
                        "Stopped: no progress (every attempted action was refused)."
                    );
                }
            } else if !call_snapshot.is_empty() {
                progress.note_progress(round);
            }

            // Count rounds without a plan so a single soft nudge can fire
            // later. Never caps anything; not a budget.
            if structured_plan_required && !structured_plan_started {
                plan_rounds_without_plan = plan_rounds_without_plan.saturating_add(1);
            }

            // Goal mode: an explicit update_goal this round ends the run now that
            // its result is committed.
            if let Some((reason, summary)) = goal_resolution {
                let final_text = if summary.is_empty() {
                    last_text.clone()
                } else {
                    summary
                };
                // Epoch terminal: next Content turn must not inherit Closing state.
                if matches!(reason, StopReason::Completed | StopReason::Blocked) {
                    progress.enter_terminal();
                    observer(AgentEvent::ProgressUpdated {
                        ledger: progress.clone(),
                    });
                }
                observer(AgentEvent::Finished(final_text.clone()));
                flush_epoch!(round);

                drain_background_children!(round);
                return Ok(AgentOutcome::drive_result(
                    final_text,
                    round,
                    modified_files,
                    reason,
                    None,
                    &metrics,
                    &progress,
                    &objective,
                ));
            }

            // A step limit tripped this round: results are committed, stop now.
            if let Some((reason, exhaustion)) = budget_exceeded {
                observer(AgentEvent::Finished(reason.clone()));
                flush_epoch!(round);

                drain_background_children!(round);
                return Ok(AgentOutcome::drive_budget_exhausted(
                    reason,
                    round,
                    modified_files,
                    exhaustion,
                    &metrics,
                    &progress,
                    &objective,
                ));
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
                sink.append(&[image_message]).await?;
            }

            // Auto-compaction (spec §53): when the context size exceeds the
            // budget, fold the in-memory transcript before the next request so a
            // long task never overflows the window. Prefer the provider's
            // reported token count, but fall back to a char/4 estimate — many
            // gateways don't report streaming usage, and without a fallback
            // compaction would silently never fire. The persisted transcript
            // (sink) is untouched — only what we resend shrinks.
            let context_tokens = used_tokens.max(estimate_tokens(&messages));
            // Fold when the last request's estimate crossed the budget. One
            // threshold, one action: the runtime does not read the model's
            // re-reads as evidence that it "deserves" a bigger window.
            let over_budget = self.policy.context_budget > 0
                && context_tokens > u64::from(self.policy.context_budget);
            if has_next_round && over_budget {
                let before = messages.len();
                // Cap the retained working set at half the live budget so a
                // huge recent tool output can't keep the fold over the window;
                // the other half leaves room for the head, summary, and next
                // response. (current_budget > 0 is guaranteed by the decision.)
                let keep_recent_tokens = u64::from(self.policy.context_budget) / 2;
                // Name this extra round trip so the UI shows "compacting…" instead
                // of a bare "waiting for model" during the summary call.
                observer(AgentEvent::AdvisoryStarted {
                    kind: AdvisoryKind::ContextCompaction,
                });
                // Compaction discards detail by design; this is the only moment
                // a project can keep something first (export it, push it to
                // memory) without the loop guessing what matters.
                if self.hook_runner.has_lifecycle() {
                    self.hook_runner
                        .run_lifecycle(
                            leveler_execution::LifecycleEvent::PreCompact,
                            &format!(
                                r#"{{"context_tokens":{context_tokens},"budget":{}}}"#,
                                self.policy.context_budget
                            ),
                            &cancellation,
                        )
                        .await;
                }
                let summarized = self
                    .summarize_for_compaction(
                        &messages,
                        COMPACT_KEEP_RECENT,
                        keep_recent_tokens,
                        &cancellation,
                    )
                    .await;
                // A fold's summarization is a provider call, and until it was
                // recorded a session that folded reported fewer tokens than it
                // spent — precisely in the lane a fold is the cost of.
                if let Some(summarized) = &summarized {
                    record_request!(
                        ModelRequestRecord {
                            provider_request_id: Some(summarized.request_id.to_string()),
                            provider: self.model.provider.clone(),
                            model: self.model.model.clone(),
                            usage: summarized.usage,
                            finish_reason: summarized.finish_reason,
                            latency_ms: summarized.latency_ms,
                            retry_count: 0,
                            kind: crate::ModelCallKind::Compaction,
                            agent_id: None,
                            cost_usd_micros: None,
                        }
                        .priced(self.pricing.as_ref())
                    );
                }
                let mut summary = summarized.map(|s| s.text);
                // Long-goal P3: before old context is folded away, the host
                // cuts a durable checkpoint and hands back its context block
                // — the fold's summary becomes persisted truth. If the
                // checkpoint cannot be made durable, we keep the context this
                // round instead of dropping continuity (fail closed); the
                // fold retries at the next boundary.
                let mut fold_permitted = true;
                if let Some(port) = &self.compaction_checkpoint {
                    match port.checkpoint_before_compaction(summary.as_deref()).await {
                        Ok(Some(block)) => summary = Some(block),
                        Ok(None) => {}
                        Err(error) => {
                            tracing::warn!(
                                %error,
                                "goal checkpoint failed; keeping context uncompacted this round"
                            );
                            fold_permitted = false;
                        }
                    }
                }
                if fold_permitted {
                    messages = compact_messages(
                        &messages,
                        COMPACT_KEEP_RECENT,
                        keep_recent_tokens,
                        summary.as_deref(),
                        Some(objective.text()),
                    );
                    context_diverged = true;
                }
                if self.hook_runner.has_lifecycle() {
                    self.hook_runner
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
                    observer(AgentEvent::Compacted {
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
            if has_next_round && (context_diverged || self.policy.context_trace) {
                observer(AgentEvent::ContextSnapshot {
                    messages: messages.clone(),
                });
                context_diverged = false;
            }
        }

        let round_limit = self
            .continuation
            .round_limit()
            .expect("only bounded continuation exits the loop by round count");
        // Review 必改①: this `break` tail is the COMMON bounded exit — drain
        // running background children here like every `return` does, or the
        // abort-on-drop backstop hard-kills them (spend and findings lost).
        drain_background_children!(round);
        // Budget exhausted: never return an empty answer. Surface the last thing
        // the model said plus how far it got, so the caller/UI shows real state.
        let summary = {
            let mut s = format!("Reached the {round_limit}-round limit before finishing.");
            if !modified_files.is_empty() {
                s.push_str(&format!(
                    " Files changed so far: {}.",
                    modified_files.join(", ")
                ));
            }
            if !last_text.trim().is_empty() {
                s.push_str(&format!("\n\nLatest note: {}", last_text.trim()));
            }
            s
        };
        flush_epoch!(round_limit);
        Ok(AgentOutcome::drive_result(
            summary,
            round_limit,
            modified_files,
            StopReason::BudgetExhausted,
            None,
            &metrics,
            &progress,
            &objective,
        ))
    }
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

/// Once a pinned task budget is 80% spent, the model is told so, once. The
/// budget is the model's to spend and it cannot see it otherwise. Tiny budgets
/// (tests, evals with a handful of rounds) get no note — there is nothing to
/// pace. A resource fact, not a judgement about the work.
pub(crate) const BUDGET_NOTE_MIN_TOTAL: u32 = 20;

pub(crate) fn budget_note(used: u32, total: u32, already_sent: bool) -> Option<String> {
    if already_sent
        || total < BUDGET_NOTE_MIN_TOTAL
        || used.saturating_mul(5) < total.saturating_mul(4)
    {
        return None;
    }
    Some(format!(
        "Budget: {used} of {total} rounds for this task are used. Converge now: land \
         the change, run the check that proves it, and call update_goal. If the \
         goal cannot be reached in what remains, say so with update_goal(blocked)."
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
    metrics: &mut DepthUseMetrics,
    epoch_rounds_at_start: u32,
    epoch_tokens_at_start: u64,
    epoch_duration_at_start: std::time::Duration,
    run_started: std::time::Instant,
    round: u32,
    model_tokens_spent: u64,
    estimated_tokens_spent: u64,
    commands_run: u32,
    cost_spent_micros: u64,
    modified_files: &[String],
) {
    metrics.model_tokens = model_tokens_spent.saturating_sub(epoch_tokens_at_start);
    let duration_ms = epoch_duration_at_start
        .saturating_add(run_started.elapsed())
        .as_millis() as u64;
    // Distinct paths across the epoch (re-edits of the same file do not inflate).
    progress.merge_modified_paths(modified_files.iter().cloned());
    let files_total = progress.cumulative_modified_files;
    progress.set_epoch_spend(
        epoch_rounds_at_start.saturating_add(round),
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
}
