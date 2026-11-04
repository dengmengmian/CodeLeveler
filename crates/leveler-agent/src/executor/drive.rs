use std::collections::HashSet;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use leveler_context::{
    FoldRequirement, ProjectInstruction, render_instructions, scoped_rule_sources,
};
use leveler_lifecycle::{
    EvidenceLedger, FindingKind, ObjectiveAnchor, PlanState, ProgressCaps, TurnPhase,
};
use leveler_model::{
    CompactionRecord, ContentPart, ControlContext, FinishReason, Message, ModelError,
    PromptAuthority, PromptSegment, PromptSource, ProtocolRepairKind, Role, RuntimeNoticeKind,
    SegmentLifecycle, ToolCall, ToolResultContent, TranscriptOrigin,
    scoped_rule_paths_in_legacy_system,
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
use crate::executor::goal_scope;
use crate::injected_tools::{
    CLAIM_WRITE_SCOPE_TOOL, COMMIT_TASK_TOOL, GrantScope, PermissionRequestOutcome,
    REPORT_FINDING_TOOL, REQUEST_PERMISSIONS_TOOL, SPAWN_AGENT_TOOL, TurnPermissionGrants,
    UPDATE_GOAL_TOOL, advertise_escalation, apply_turn_grants, claim_write_scope_tool_definition,
    commit_task_tool_definition, escalation_action, escalation_missing_axis_message,
    is_escalatable_tool, is_user_input_tool, parse_escalation, parse_permission_request,
    permission_already_denied_message, report_finding_tool_definition,
    request_permissions_tool_definition, request_user_input_tool_definition,
    spawn_agent_tool_definition, update_goal_tool_definition,
};
use crate::nudges::{
    goal_continuation_nudge, goal_settled_delivery_nudge, plan_unresolved_nudge,
    task_unregistered_nudge,
};

use crate::sub_agent::{
    AgentRole, ChildProfile, MAX_SUB_AGENT_DEPTH, agent_nickname, lost_children_note,
    multi_agent_steer_hint, new_delegated_agent_id, scopes_overlap, settlement_notice,
    should_inject_delegation_hint,
};
use async_trait::async_trait;
use leveler_context::compact_messages;

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
/// A dropped future cannot prove its background processes are gone. Scope
/// release belongs to the explicit asynchronous settlement path.
#[derive(Default)]
/// One refused `commit_task` declaration, kept only to answer a repetition.
///
/// Identity is the pair the model sent (objective text plus acceptance list);
/// a changed declaration is a different attempt and resets the counter. The
/// uncovered count is stored because the closeout quotes it back when the model
/// goes quiet after the refusal — the user's request never became a goal, and
/// the turn must not end as if the refusal had settled it.
struct CommitRefusal {
    objective: String,
    acceptance: Vec<String>,
    repeats: u32,
    uncovered: usize,
}

struct BackgroundChildren {
    children: Vec<BackgroundChild>,
}

impl Drop for BackgroundChildren {
    fn drop(&mut self) {
        for child in &self.children {
            child.handle.abort();
            // Aborting cannot prove background processes settled; retain the
            // ownership registry's lease until async settlement or recovery.
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

/// Consecutive rounds whose every call was a goal-verification refusal.
///
/// Two is the failure this bound exists to avoid: the second refusal ends
/// the turn before the model can read it and cite the recorded command or
/// run the check again. Three lets that correction land, then stops.
/// A round that executes a tool resets the count.
const MAX_VERIFICATION_RECOVERY_ROUNDS: u32 = 3;

/// Tool rounds the drive may run AFTER `update_goal` resolved the goal, before
/// it owes one delivery request that already contains those tool results.
///
/// Accepting a resolution settles the goal, not the delivery. A round that
/// runs a tool is wrap-up: ending the turn on its result drops the answer,
/// because the model has not seen that result yet. This bound limits the
/// wrap-up. Reaching it schedules one more model request. If that request
/// runs tools again, those tools execute and the turn ends honestly — the
/// follow-up was the delivery, and spending it on more tools does not buy
/// another one.
const MAX_SETTLED_DELIVERY_ROUNDS: u32 = 2;
/// How many started-but-unresolved commands the completion gate remembers.
/// Feedback only: the newest starts are the ones a completion can cite.
const MAX_PENDING_COMMANDS: usize = 16;

/// How often the goal gate may return the SAME refusal before the run stops.
///
/// The recovery bound above only fires when a round does nothing else. A gate
/// that refuses for a reason the model cannot remove — a requirement set the
/// runtime itself made contradictory, or an artifact check reading a state the
/// task legitimately changed — can be re-issued forever while the model keeps
/// doing work in between (`DF-06`: 30 rounds, 661k tokens, then Blocked). Three
/// identical verdicts on the same facts means retrying is not the fix, so the
/// run ends as incomplete and reports the gate's own words.
const MAX_IDENTICAL_GATE_REFUSALS: u32 = 3;

/// Tokens held back from each call's share of the remaining context capacity.
/// The estimator deliberately prices message content only, not the per-message
/// role framing or the JSON envelope a provider also counts (see
/// `leveler_model::estimate`). This is a lower-bound allowance for that known
/// omission, not a measured calibration; the provider's reported count is the
/// authority once a request has been made.
const TOOL_RESULT_FRAMING_ALLOWANCE: u64 = 256;

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
    /// Turn control and discovered scoped context, never conversation rows.
    control_context: ControlContext,
    modified_files: Vec<String>,
    /// Model steps this drive has started. Reported on abort so an
    /// interrupted/failed turn records what it actually spent. A mechanical
    /// count of the agent's request cadence, never a task-progress measure.
    model_steps: u32,
    scoped_paths: Vec<String>,
    /// Scoped rule sources injected during the CURRENT round. Another call in
    /// the same round cannot have read them (the request already left), so a
    /// mutating call that needs one is refused as well — but the source is
    /// loaded once, not once per call.
    staged_scoped_rules: std::collections::HashSet<String>,
    progress_caps: ProgressCaps,
    progress: ProgressLedger,
    epoch_model_steps_at_start: u32,
    epoch_tokens_at_start: u64,
    epoch_estimated_at_start: u64,
    epoch_duration_at_start: std::time::Duration,
    model_step_note_sent: bool,
    plan_state: PlanState,
    /// Set whenever the model-visible messages gain something the durable
    /// transcript does not — a transient nudge, a fold.
    context_diverged: bool,
    session_approved: HashSet<String>,
    background_children: BackgroundChildren,
    /// Cache of terminal result identities already committed to the transcript.
    delivered_background_tasks: Option<HashSet<String>>,
    /// Whether this run's resumed children (if any) were launched yet.
    resumed_children_launched: bool,
    run_agents_semaphore: Arc<tokio::sync::Semaphore>,
    bg_progress_tx: tokio::sync::mpsc::UnboundedSender<AgentEvent>,
    bg_progress_rx: tokio::sync::mpsc::UnboundedReceiver<AgentEvent>,
    /// Accumulated elevations from approved request_permissions this turn.
    turn_grants: TurnPermissionGrants,
    /// The most recent non-empty assistant text.
    last_text: String,
    /// Finalization is a one-way lifecycle boundary for this drive. Keeping
    /// the latch here prevents multiple exit helpers from publishing it twice.
    finalization_started: bool,
    ledger: EvidenceLedger,
    closeout_budget: CloseoutBudget,
    /// Command executions this drive observed, chronological and bounded.
    /// Used only by the Goal completion gate: a cited command must match one
    /// of these facts, and a failed test/build is hard evidence against
    /// `update_goal(complete)`.
    verification_runs: Vec<super::goal_verification::VerificationRun>,
    /// Commands this drive STARTED and has not seen a result for, newest last.
    ///
    /// Not evidence — a running process proves nothing yet — but a fact the
    /// completion gate must read, so a citation of it is refused for the true
    /// reason (no result yet) and names the task to wait on. Bounded: this is
    /// feedback, not a ledger.
    pending_commands: Vec<super::goal_verification::PendingCommand>,
    /// Consecutive rounds with no executed tool call in which a
    /// goal-verification refusal arrived, including a round that also refused
    /// another call before it ran. Not the generic no-progress streak: the
    /// model must be able to read the refusal and cite the recorded command
    /// or run the check again. A denial in that round does not clear this
    /// count and does not tick the denial streak. A round that executes a
    /// tool resets it. A new drive starts it over.
    goal_gate_recovery_rounds: u32,
    /// The last goal-gate refusal text and how many times in a row it has been
    /// returned unchanged. Set to the gate's own message when the bound is
    /// reached, so the round can stop after its results are durable.
    repeated_gate_refusal: Option<(String, u32)>,
    /// The last acceptance-scope refusal text this drive returned.
    ///
    /// Kept apart from [`Self::repeated_gate_refusal`] on purpose: that slot is
    /// shared with the verification gate, which clears it on a pass, so reading
    /// it here made every requirements refusal look new.
    requirements_refusal: Option<String>,
    /// The last `commit_task` scope refusal. A deterministic refusal changes
    /// nothing when retried with the same objective and acceptance, so the
    /// model is told it is repeating itself instead of being left to guess.
    commit_refusal: Option<CommitRefusal>,
    /// How many of the user's deliverables the LAST refused declaration left
    /// uncovered, remembered past the closeout that answers it. A Chat turn
    /// that ends after such a refusal did not deliver a verified task: the
    /// report says so instead of letting an `Answered` turn be read as a
    /// completed user request.
    registration_refused: Option<usize>,
    /// Facts the terminal must carry even though the Goal resolved: accepted
    /// completions with no executable verification. Read by the engine and
    /// persisted on `TaskFinished.warnings`.
    completion_warnings: Vec<String>,
    /// The resolution accepted this drive (`update_goal` completed/blocked) and
    /// its summary.
    ///
    /// Accepting a resolution settles the GOAL; it does not write anything for
    /// the person. The run therefore keeps going for a delivery round instead
    /// of ending on the tool call, and this remembers what the terminal must
    /// report when it eventually stops.
    settled_goal: Option<(StopReason, String)>,
    /// Tool rounds spent AFTER the goal settled. A quiet answer does not
    /// count: the bound limits wrap-up, not the delivery itself.
    settled_rounds: u32,
    /// Prose the turn can end on. Set when a round speaks and does not run a
    /// work tool; cleared when a later work tool runs. A process sentence
    /// that a tool call followed is not this, and neither is [`Self::last_text`].
    terminal_answer: Option<String>,
    /// The one model request issued after wrap-up tool results, so the model
    /// can write the answer it has not written yet. A further tool round does
    /// not schedule another one.
    delivery_followup_issued: bool,
    decode_retries: u32,
    length_continuations: u32,
    continued_text: String,
    commands_run: u32,
    /// Human reason + structured dimension when a step limit trips mid-round.
    budget_exceeded: Option<(String, BudgetExhaustion)>,
    /// Shared record of the most recent compaction fold. Written here when the
    /// harness folds; read by the kernel when it builds the accounting
    /// snapshot. One handle, two views of the same fact.
    compaction: Arc<std::sync::Mutex<Option<CompactionRecord>>>,
    /// What the kernel projected the CURRENT round's request to cost, taken
    /// from the accounting snapshot it published for that exact request.
    /// Stamped onto the round's ledger row so the estimate sits beside the
    /// provider's own prompt count instead of being recomputed offline.
    last_projected_input_tokens: Option<u64>,
    /// The provider's own prompt count for that same request, when it reported
    /// one. The estimator is a documented LOWER bound (role framing and JSON
    /// envelopes are not priced); this is the only measurement of the gap, so
    /// the next request's pressure is judged with it instead of pretending the
    /// estimate is the whole truth.
    last_actual_input_tokens: Option<u64>,
    /// Of that projection, how much was the historical-reasoning channel —
    /// priced over the SAME projection, so it can never claim reasoning the
    /// provider did not receive.
    last_projected_reasoning_tokens: Option<u64>,
    /// The projected size at which a soft fold's briefing last failed. A soft
    /// fold is optional — the uncompacted history is still legal to send — so
    /// retrying at the same size costs an extra model call that cannot succeed
    /// any differently. The next attempt is due once the transcript has grown
    /// by what the failed fold would have reclaimed; the hard bound always
    /// retries, because there the fold is mandatory.
    soft_fold_retry_at: Option<u64>,
}

/// What the delivery round is told once `update_goal` resolved the goal.
///
/// Facts only: the resolution is recorded, the person has not been answered,
/// and these background tasks are still running under the lifetime they were
/// started with. A running task is not unfinished goal work. The model decides
/// what to say; the runtime never writes the answer.
fn settled_delivery_notice(reason: StopReason, running: &[String]) -> String {
    let resolution = match reason {
        StopReason::Blocked => "blocked",
        _ => "complete",
    };
    let mut notice = format!(
        "update_goal(status=\"{resolution}\") was accepted and is recorded for this goal. \
         Nothing has been said to the person who asked yet: this round is the delivery, not \
         more goal work.\n\n\
         Write the answer for them now, from what actually happened: what you did, which checks \
         really ran and what they returned, what is still true afterwards, and anything that did \
         not work. Do not describe a step as verified unless its result is in this \
         conversation. Do not call update_goal again.\n"
    );
    if !running.is_empty() {
        notice.push_str(&format!(
            "\nBackground tasks still running:\n- {}\n\
             A task that is still running is not unfinished goal work, and the answer does not \
             wait for it to exit. Report the lifetime already recorded for each one. Do not stop \
             or restart a service unless the person asked for that.\n",
            running.join("\n- ")
        ));
    }
    notice.push_str(
        "\nIf a check is still required, run it. Its result comes back before the answer is \
         due, and the answer is still owed after that result returns.\n",
    );
    notice
}

/// The one request after wrap-up tool results. The checks have already run;
/// this states that fact and that the answer is still missing.
fn delivery_followup_notice() -> String {
    "The checks requested after the goal was accepted have returned and are in this \
     conversation. The person still has no answer: this round is the delivery. Write it now \
     from those results. Do not call update_goal again.\n"
        .to_string()
}

/// `update_goal` and `update_plan` record a decision. They do not act on the
/// prose in front of them, so they do not spend a delivery round and do not
/// erase an answer.
fn is_goal_bookkeeping(name: &str) -> bool {
    name == UPDATE_GOAL_TOOL || name == "update_plan"
}

fn background_task_result_message(
    task_id: String,
    session: &str,
    goal_id: Option<&leveler_core::GoalId>,
    status: leveler_execution::BackgroundTaskStatus,
    exit_code: Option<i32>,
    output: &str,
    output_truncated: bool,
) -> Message {
    use leveler_execution::BackgroundTaskStatus;
    let outcome = match (status, exit_code) {
        (BackgroundTaskStatus::Exited, Some(0)) => "success",
        (BackgroundTaskStatus::Exited, _) => "failure",
        (BackgroundTaskStatus::Killed, _) => "cancelled",
        _ => "unknown",
    };
    const MAX_OUTPUT_BYTES: usize = 2 * 1024;
    let start =
        leveler_core::ceil_char_boundary(output, output.len().saturating_sub(MAX_OUTPUT_BYTES));
    let result = serde_json::json!({
        "task_id": task_id,
        "session_id": session,
        "goal_id": goal_id,
        "status": status,
        "outcome": outcome,
        "exit_code": exit_code,
        "output_tail": &output[start..],
        "output_truncated": output_truncated || start > 0,
        "retained_log_reference": {
            "tool": "get_task",
            "task_id": task_id,
            "availability": "while the execution registry retains this task",
        },
    });
    Message::user(
        format!("Background task terminal result: {result}"),
        TranscriptOrigin::BackgroundTaskResults {
            task_ids: vec![task_id],
        },
    )
}

/// Summarize one running task for a delivery notice.
fn task_line(task: &leveler_execution::BackgroundTaskSnapshot) -> String {
    let line = if task.args.is_empty() {
        task.program.clone()
    } else {
        format!("{} {}", task.program, task.args.join(" "))
    };
    let mut shown = line;
    if shown.chars().count() > 120 {
        shown = shown.chars().take(117).collect::<String>();
        shown.push_str("...");
    }
    format!(
        "{} ({}; stops when {})",
        task.id,
        shown,
        task.lifetime.stop_condition()
    )
}

struct CompactionObserver<'a, 'b> {
    drive: &'a mut Drive<'b>,
    rt: &'a mut LoopContext,
    reasoning_effort: Option<leveler_model::ReasoningEffort>,
    request: &'a leveler_model::ModelRequest,
}
#[async_trait]
impl leveler_agent_core::ModelRoundObserver for CompactionObserver<'_, '_> {
    type Error = AgentError;
    fn on_event(&mut self, _: leveler_agent_core::AgentEvent) {}
    async fn before_attempt(&mut self) -> Result<(), AgentError> {
        if let Some(progress) = self.drive.executor.shared_model_progress().await? {
            let limits = super::StepLimits {
                max_duration: None,
                ..self.drive.executor.task_model_limits
            };
            if !super::auxiliary_budget_available(
                limits,
                &progress,
                self.request,
                self.drive.executor.pricing.as_ref(),
            ) {
                return Err(AgentError::AuxiliaryBudgetUnavailable);
            }
        }
        if !super::auxiliary_budget_available(
            self.drive.executor.step_limits,
            &self.drive.progress,
            self.request,
            self.drive.executor.pricing.as_ref(),
        ) {
            return Err(AgentError::AuxiliaryBudgetUnavailable);
        }
        Ok(())
    }
    async fn on_attempt(&mut self, attempt: leveler_model::ModelAttempt) -> Result<(), AgentError> {
        let record = super::model_attempt_record(
            &attempt,
            &self.drive.executor.model,
            crate::ModelCallKind::Compaction,
            self.reasoning_effort,
        )
        .priced(self.drive.executor.pricing.as_ref());
        self.drive
            .record_request(self.rt, record, attempt.estimated_tokens)
            .await?;
        self.drive.flush_epoch(self.rt);
        if self
            .drive
            .executor
            .step_limits
            .max_cost_usd_micros
            .is_some()
            && self.drive.progress.has_unpriced_model_attempt
        {
            return Err(AgentError::Model(leveler_model::ModelError::new(
                leveler_model::ModelErrorKind::Other,
                "summary attempt has no auditable cost under active cap",
            )));
        }
        Ok(())
    }
}

/// Why a fold is being attempted is classified once, from the policy's TWO
/// bounds, by [`leveler_context::FoldRequirement`]. The drive, chat and resume
/// all share that contract; this wrapper only supplies the drive's resolved
/// policy as facts.
fn fold_requirement(
    policy: &crate::coding::policy::ResolvedContextPolicy,
    projected_tokens: u64,
) -> FoldRequirement {
    FoldRequirement::classify(
        projected_tokens,
        u64::from(policy.pressure_threshold),
        policy.hard_capacity(),
    )
}

/// A stable class for a compaction summary failure: never the provider's raw
/// diagnostic, which may carry internal detail.
fn compaction_failure_class(error: &AgentError) -> &'static str {
    match error {
        AgentError::Model(error) if error.kind == leveler_model::ModelErrorKind::Timeout => {
            "timeout"
        }
        AgentError::Model(_) => "provider",
        AgentError::Cancelled => "timeout",
        _ => "other",
    }
}

/// Record, in one structured line, that a briefing was not produced and what
/// the runtime did about it. No model-visible text: this is observability, not
/// a prompt, and a failed summary must not become instruction.
fn log_compaction_summary_failure(
    requirement: FoldRequirement,
    failure: &'static str,
    projected_tokens: u64,
    context_policy: &crate::coding::policy::ResolvedContextPolicy,
    continued_without_compaction: bool,
) {
    tracing::warn!(
        operation = "compaction",
        trigger = ?requirement,
        failure,
        continued_without_compaction,
        projected_tokens,
        quality_threshold = context_policy.pressure_threshold,
        hard_capacity = ?context_policy.hard_capacity(),
        "context compaction summary was not produced"
    );
}

impl Executor {
    /// The tool table this executor would ask a model for, before capability
    /// exposure filtering.
    ///
    /// Two consumers: [`Self::request_tool_definitions`] (what the model sees)
    /// and [`Self::unexposed_pack_inventory`] (what enabling a pack would add).
    /// They must come from one list, or the stated size of a pack becomes a
    /// second opinion about the tool table.
    fn candidate_tool_definitions(&self) -> Vec<ToolDefinition> {
        let mut tools = self.projected_tool_registry().definitions();
        // Tools discovered during this turn (an external catalog that came up
        // mid-turn) join the surface before capability filtering below, so an
        // unexposed pack stays unexposed.
        if self.depth == 0 {
            let discovered = self
                .discovered
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .definitions();
            tools.extend(discovered);
        }
        // A child clarifier is unattended. Only the top-level turn can ask
        // the user. `ask_user` remains a parser alias of this tool.
        if self.depth == 0
            && (self.tool_context.policy.unrestricted_execution() || self.policy.allow_host_input)
        {
            tools.push(request_user_input_tool_definition());
        }
        // Nothing to request under 完全访问 — the elevation it asks for is
        // already granted, so advertising it only invites a pointless round
        // trip and an interruption the user explicitly opted out of.
        // A child advertises it only when it holds a tool the grant can
        // change. A read-only child has no such tool.
        let permission_changes_a_tool = self.depth == 0
            || tools.iter().any(|tool| {
                is_escalatable_tool(&tool.name)
                    || matches!(tool.name.as_str(), "web_fetch" | "web_search")
                    || tool.name.starts_with("browser_")
            });
        if self.tool_context.policy.mode() != leveler_execution::PermissionProfile::FullAccess
            && permission_changes_a_tool
        {
            tools.push(request_permissions_tool_definition());
            // Same reason, applied to the command tools: a denied command can
            // carry its own one-shot elevation on the retry instead of
            // spending a round trip on a separate request.
            advertise_escalation(&mut tools);
        }
        // A sub-agent shouldn't spawn its own sub-agents; product kill-switch
        // can also hide spawn_agent entirely.
        if (self.tool_context.policy.unrestricted_execution() || self.policy.allow_delegation)
            && self.depth < MAX_SUB_AGENT_DEPTH
        {
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
        // A Chat turn can be delegated as a task. The model declares it; the
        // runtime never classifies the user's text. Read-only surfaces have
        // no authoring, so they cannot declare execution work at all.
        if !self.goal_mode()
            && self.depth == 0
            && self.goal_binder().is_some()
            && !self.tool_context.policy.read_only
        {
            tools.push(commit_task_tool_definition());
        }
        // Goal mode: the model resolves the objective explicitly.
        if self.goal_mode() {
            tools.push(update_goal_tool_definition());
        }
        tools
    }

    /// Packs that are on offer on this request, with how many tools enabling
    /// each one would add to the surface.
    ///
    /// This is the host fact a model cannot observe: an unexposed pack's tools
    /// are absent from the tool table, so nothing in the conversation says the
    /// pack exists or how much is behind it. "Some capabilities are available
    /// on demand" is not an inventory — a model told to check a connected
    /// external surface has no way to learn that the surface is callable at
    /// all, and goes around the harness instead.
    ///
    /// Counts, never tool names: the pack is the unit the model enables.
    fn unexposed_pack_inventory(&self) -> Vec<(crate::capability::CapabilityId, usize)> {
        use crate::capability::tool_capability;
        let Some(state) = &self.capabilities else {
            return Vec::new();
        };
        let mut counts: std::collections::BTreeMap<crate::capability::CapabilityId, usize> =
            std::collections::BTreeMap::new();
        for tool in self.candidate_tool_definitions() {
            let Some(id) = tool_capability(&tool.name) else {
                continue;
            };
            // Only a pack the host can really give the model: availability and
            // permission are host facts, exposure belongs to this request.
            if !state.available(id) || !state.is_permitted(id) || state.exposed(id) {
                continue;
            }
            *counts.entry(id).or_default() += 1;
        }
        counts.into_iter().filter(|(_, count)| *count > 0).collect()
    }

    pub(crate) fn request_tool_definitions(&self) -> Vec<ToolDefinition> {
        let mut tools = self.candidate_tool_definitions();

        if let Some(state) = &self.capabilities {
            // Exposure is the same in every permission mode: FullAccess
            // removes the ASK/DENY boundary, not the model's ability to
            // discover an optional pack through the catalog.
            tools.retain(|tool| state.permits_tool(&tool.name));
            tools.push(crate::capability::control_definition());
        }
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        tools
    }

    /// Ask the host catalog once, before the turn's opening surface is
    /// composed, and record the availability of every pack whose tools arrived.
    ///
    /// Ownership, exposure and permission are untouched: this only makes the
    /// catalog's existence a fact the first request can state.
    async fn absorb_initial_catalog(&self) {
        use crate::capability::tool_capability;
        let added = self.absorb_discovered_tools().await;
        if added.is_empty() {
            return;
        }
        if let Some(state) = &self.capabilities {
            for id in added.iter().filter_map(|name| tool_capability(name)) {
                state.note_available(id);
            }
        }
    }

    /// Run the CodeLeveler coding harness over the generic agent kernel.
    ///
    /// The transcript, the objective, the observer and the durable sink go in;
    /// the kernel drives model↔tool rounds until the model resolves the goal,
    /// this harness stops it, or a mechanical limit fires.
    pub(crate) async fn drive(
        &self,
        mut messages: Vec<Message>,
        mut control_context: ControlContext,
        objective: ObjectiveAnchor,
        observer: &mut (dyn FnMut(AgentEvent) + Send),
        sink: &mut dyn TranscriptSink,
        cancellation: CancellationToken,
        aborted: &mut AbortedFacts,
    ) -> Result<AgentOutcome, DriveAborted> {
        // A host catalog that is already reachable is part of the FIRST
        // request's surface. Absorbing it only from round two meant the opening
        // tool table was composed from a host fact the runtime had not asked
        // for yet: the pack looked empty, so the request could not say what was
        // on offer even though the server had been up since before the turn.
        // A catalog that comes up later is still absorbed per round
        // (`on_round_start`) and announced as a growth.
        self.absorb_initial_catalog().await;
        let tools = self.request_tool_definitions();
        if let Some(state) = &self.capabilities {
            // The pack inventory is a host fact the model receives on every
            // request; recording it makes "the request stated what was on
            // offer" checkable offline instead of inferred.
            let on_offer = self.unexposed_pack_inventory();
            tracing::info!(event="capability_available",catalog_tokens=leveler_model::estimate_text(&state.catalog().to_string()),catalog=%state.catalog(),packs_on_offer=?on_offer.iter().map(|(id, n)| format!("{}:{n}", id.as_str())).collect::<Vec<_>>());
            tracing::info!(event="tool_surface_changed",reason="initial",active_capabilities=?state.active(),tool_count=tools.len(),schema_tokens=leveler_model::estimate_tool_definitions(&tools),tool_names=?tools.iter().map(|tool|&tool.name).collect::<Vec<_>>());
        }
        if self.capabilities.is_some() {
            control_context.blocks.push(PromptSegment::control(
                "capability_discovery",
                PromptSource::ExecutionState,
                PromptAuthority::CoreContract,
                SegmentLifecycle::SessionPrefix,
                true,
                capability_discovery_fact(&self.unexposed_pack_inventory()),
            ));
        }
        // Legacy sessions contain System rows. Recover only their source
        // identities to reload applicable files, never their stale contents.
        let mut scoped_paths = Vec::new();
        for source in &self.seeded_progress.scoped_rule_sources {
            push_unique_path(&mut scoped_paths, source);
        }
        for message in &messages {
            for part in &message.content {
                if let ContentPart::ToolCall { call } = part {
                    collect_scoped_paths_from_call(call, &mut scoped_paths);
                }
            }
            if message.role == Role::System {
                // The stored body is not an instruction. A scoped-rule marker
                // is only a path; the current file is re-read later.
                for source in scoped_rule_paths_in_legacy_system(&message.text_content()) {
                    push_unique_path(&mut scoped_paths, &source);
                }
            }
        }
        messages.retain(|message| message.role != Role::System);

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
        let context_diverged = false;
        if should_inject_delegation_hint(
            (self.tool_context.policy.unrestricted_execution() || self.policy.allow_delegation)
                && self.capability_exposed(crate::capability::CapabilityId::MultiAgent, true),
            self.depth,
        ) && !messages.iter().any(|m| {
            m.role == Role::User
                && m.text_content()
                    .contains(crate::sub_agent::MULTI_AGENT_HINT_HEADER)
        }) {
            control_context.blocks.push(PromptSegment::control(
                "multi_agent_hint",
                PromptSource::DelegationHint,
                PromptAuthority::CoreContract,
                SegmentLifecycle::SessionPrefix,
                true,
                multi_agent_steer_hint(),
            ));
            observer(AgentEvent::runtime_injection(
                crate::executor::RuntimeInjectionKind::MultiAgentHint,
                1,
            ));
        }

        // MA-RT-3 C10: children that durably SETTLED while the previous
        // window ended are not lost — their recorded outcomes are re-delivered
        // so the parent integrates instead of re-delegating. The host already
        // pruned them out of `outstanding_children`; persisting that pruned
        // ledger here is the once-per-restart mark (a crash before it lands
        // re-delivers again, which repeats a note but never repeats state).
        if self.depth == 0 && !self.restart_settled_children.is_empty() {
            let note = Message::user(
                crate::sub_agent::settled_children_redelivery_note(&self.restart_settled_children),
                TranscriptOrigin::RuntimeNotice {
                    notice: RuntimeNoticeKind::ChildRedelivery,
                },
            );
            observer(AgentEvent::ProgressUpdated {
                ledger: progress.clone(),
            });
            sink.append(std::slice::from_ref(&note)).await?;
            messages.push(note);
            observer(AgentEvent::runtime_injection(
                crate::executor::RuntimeInjectionKind::ChildRecovery,
                1,
            ));
        }
        // In-process children did not survive a restart: tell the model
        // truthfully which delegations were lost, release their scopes, and
        // clear the durable record.
        //
        // A child this window RESUMES is not lost, whatever the crashed
        // window's durable ledger still says: the engine's resume decision is
        // the newer fact. Emitting both notices for one child made a real run
        // read them as a contradiction and spend rounds deciding which held.
        let resumed_ids: Vec<&str> = self
            .resumed_children
            .iter()
            .map(|child| child.id.as_str())
            .collect();
        let lost =
            crate::sub_agent::lost_children_among(&progress.outstanding_children, &resumed_ids);
        if self.depth == 0 && !lost.is_empty() {
            let note = Message::user(
                lost_children_note(&lost),
                TranscriptOrigin::RuntimeNotice {
                    notice: RuntimeNoticeKind::ChildLost,
                },
            );
            progress
                .outstanding_children
                .retain(|entry| !lost.contains(entry));
            observer(AgentEvent::ProgressUpdated {
                ledger: progress.clone(),
            });
            // Durable like every other injected turn — the record was just
            // cleared, so the note is the only remaining truth.
            sink.append(std::slice::from_ref(&note)).await?;
            messages.push(note);
            observer(AgentEvent::runtime_injection(
                crate::executor::RuntimeInjectionKind::ChildRecovery,
                1,
            ));
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
            control_context,
            modified_files: Vec::new(),
            model_steps: 0,
            scoped_paths,
            staged_scoped_rules: std::collections::HashSet::new(),
            progress_caps: ProgressCaps::default(),
            epoch_model_steps_at_start: progress.cumulative_model_steps,
            epoch_tokens_at_start: progress.cumulative_model_tokens,
            epoch_estimated_at_start: progress.cumulative_estimated_model_tokens,
            epoch_duration_at_start: std::time::Duration::from_millis(
                progress.cumulative_duration_ms,
            ),
            commands_run: progress.cumulative_commands,
            model_step_note_sent: false,
            plan_state: self.seeded_plan.clone(),
            context_diverged,
            session_approved: HashSet::new(),
            background_children: BackgroundChildren {
                children: Vec::new(),
            },
            delivered_background_tasks: None,
            resumed_children_launched: false,
            run_agents_semaphore: Arc::new(tokio::sync::Semaphore::new(
                self.policy.max_concurrent_agents.max(1),
            )),
            bg_progress_tx,
            bg_progress_rx,
            turn_grants: TurnPermissionGrants::default(),
            last_text: String::new(),
            finalization_started: false,
            ledger: seeded_ledger,
            // Unified closeout nudge budget shared by every quiet-round
            // mechanism (goal resolution, empty answer).
            closeout_budget: CloseoutBudget::new(CLOSEOUT_NUDGE_BUDGET),
            verification_runs: self.seeded_verification_runs.clone(),
            pending_commands: Vec::new(),
            goal_gate_recovery_rounds: 0,
            repeated_gate_refusal: None,
            requirements_refusal: None,
            commit_refusal: None,
            registration_refused: None,
            completion_warnings: Vec::new(),
            settled_goal: None,
            settled_rounds: 0,
            terminal_answer: None,
            delivery_followup_issued: false,
            decode_retries: 0,
            length_continuations: 0,
            continued_text: String::new(),
            budget_exceeded: None,
            last_projected_input_tokens: None,
            last_actual_input_tokens: None,
            last_projected_reasoning_tokens: None,
            soft_fold_retry_at: None,
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
            .with_thinking_disabled(self.policy.thinking_disabled)
            .with_context_window(self.policy.context_policy.context_window)
            .with_reasoning_replay(self.policy.reasoning_replay)
            .with_compact_at(self.policy.context_policy.pressure_threshold)
            // The accounting publishes the SAME resolved budget the fold
            // decisions read: effective input capacity, the completion
            // reservation and the safety headroom. One policy, one snapshot.
            .with_input_budget(
                self.policy
                    .context_policy
                    .hard_capacity()
                    .map(|capacity| capacity.min(u64::from(u32::MAX)) as u32),
                self.policy.context_policy.output_reservation,
                self.policy.context_policy.headroom,
            )
            .with_reasoning_retention(self.policy.reasoning_retention)
            .with_compaction_record(compaction_record.clone());
        let mut result = agent.run(messages, &mut harness, cancellation).await;
        if result.is_err() {
            // Every normal exit drains its children; an error exit must too.
            // Aborting instead cannot stop a write already inside a blocking
            // section, and releasing a scope on abort lets that write land
            // after the scope is gone.
            if let Err(error) = harness.stop_background_children().await {
                result = Err(error);
            }
        }
        if let Err(AgentError::BudgetExhausted(exhaustion)) = result {
            if self.depth == 0
                && let (Some((store, session)), Some(scope)) =
                    (&self.model_request_store, self.model_budget_scope())
            {
                harness.progress = crate::coding::turn::reconcile_model_spend(
                    harness.progress.clone(),
                    store.as_ref(),
                    session,
                    &scope,
                )
                .await
                .map_err(|error| AgentError::Persistence(error.to_string()))?;
            }
            harness.enter_finalization();
            let detail = exhaustion.stop_detail();
            (harness.observer)(AgentEvent::ProgressUpdated {
                ledger: harness.progress.clone(),
            });
            (harness.observer)(AgentEvent::Finished(detail.clone()));
            result = Ok(AgentOutcome::drive_budget_exhausted(
                detail,
                harness.model_steps,
                harness.modified_files.clone(),
                exhaustion,
                &harness.progress,
                &harness.objective,
            ));
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
            Kernel::ReasoningStarted => AgentEvent::ReasoningStarted,
            Kernel::ReasoningDelta(delta) => AgentEvent::ReasoningDelta(delta),
            Kernel::ReasoningCompleted { elapsed_ms } => {
                AgentEvent::ReasoningCompleted { elapsed_ms }
            }
            Kernel::Usage(usage) => {
                // The provider's count for the request whose estimate
                // `ContextUsage` just recorded. Kept together, they are the
                // measured gap the estimator cannot derive on its own.
                self.last_actual_input_tokens = Some(usage.input_tokens);
                AgentEvent::Usage {
                    input_tokens: usage.input_tokens.min(u32::MAX as u64) as u32,
                    output_tokens: usage.output_tokens.min(u32::MAX as u64) as u32,
                    cached_input_tokens: usage.cached_input_tokens.min(u32::MAX as u64) as u32,
                    // `None` stays `None`: an unreported breakdown must not become
                    // a measured zero on its way to a client.
                    reasoning_tokens: usage
                        .reasoning_tokens
                        .map(|value| value.min(u32::MAX as u64) as u32),
                }
            }
            Kernel::ContextUsage(accounting) => {
                // The one place the estimate is captured: it describes the
                // request this round is about to send, so the ledger row for
                // that round can carry it. The kernel boxes the value; this
                // event still carries the accounting itself.
                self.last_projected_input_tokens = Some(accounting.used_tokens);
                self.last_projected_reasoning_tokens =
                    Some(accounting.projected_reasoning_tokens());
                AgentEvent::ContextUsage {
                    accounting: *accounting,
                }
            }
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
            execution_status: None,
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
    async fn persist_request(&mut self, mut record: ModelRequestRecord) -> Result<(), AgentError> {
        // Resolved at write time: a mid-turn `/goal` upgrade changes the
        // owner of the calls that follow it.
        record.budget_scope = self.executor.model_budget_scope();
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
        self.progress.has_unpriced_model_attempt |= record.cost_usd_micros.is_none();
        rt.record_spend(record.usage, record.cost_usd_micros, estimate);
        self.persist_request(record).await
    }

    /// Children persist invocation facts before publishing this live spend
    /// notification. Hosts without a shared store retain the sink route;
    /// durable hosts fold the already-written fact without inserting twice.
    async fn forward_child_event(
        &mut self,
        rt: &mut LoopContext,
        event: AgentEvent,
    ) -> Result<(), AgentError> {
        if let AgentEvent::SubAgentModelRequest { record } = event {
            self.progress.has_unpriced_model_attempt |= record.cost_usd_micros.is_none();
            // Already priced by the child against its own model.
            if self.executor.model_request_store.is_some() {
                rt.record_spend(
                    record.usage,
                    record.cost_usd_micros,
                    record.estimated_tokens,
                );
                Ok(())
            } else {
                {
                    let estimate = record.estimated_tokens;
                    self.record_request(rt, *record, estimate).await
                }
            }
        } else {
            (self.observer)(event);
            Ok(())
        }
    }

    /// Reconcile at request admission against the registry and durable log.
    /// An exit concurrent with this read remains available for the next
    /// admission. The transcript append is the delivery commit, so recovery
    /// replays the same row instead of minting another notice.
    async fn deliver_background_task_results(
        &mut self,
        messages: &mut Vec<Message>,
    ) -> Result<(), AgentError> {
        let (Some(registry), Some(owner)) = (
            self.executor.background_tasks.as_ref(),
            self.executor.tool_context.session_scope.as_deref(),
        ) else {
            return Ok(());
        };
        if self.executor.depth != 0 || self.executor.tool_context.writer_scope() != "parent" {
            return Ok(());
        }
        let delivered = match self.delivered_background_tasks.as_mut() {
            Some(delivered) => delivered,
            None => {
                let ids = self.sink.delivered_background_task_ids().await?;
                self.delivered_background_tasks
                    .insert(ids.into_iter().collect())
            }
        };
        // Bound fresh notices independently of each task's retained log.
        const MAX_FACTS: usize = 8;
        let mut results = Vec::new();
        let mut admitted = HashSet::new();
        for event in self.sink.background_task_results().await? {
            let leveler_engine::EngineEvent::BackgroundTaskFinished {
                task_id,
                goal_id,
                exit_code,
                status,
                output,
                output_truncated,
                ..
            } = event
            else {
                continue;
            };
            if delivered.contains(&task_id) || !admitted.insert(task_id.clone()) {
                continue;
            }
            results.push(background_task_result_message(
                task_id,
                owner,
                goal_id.as_ref(),
                status.unwrap_or(leveler_execution::BackgroundTaskStatus::Exited),
                exit_code,
                &output,
                output_truncated,
            ));
            if results.len() == MAX_FACTS {
                break;
            }
        }
        if results.len() < MAX_FACTS {
            for task in registry
                .local_terminal_snapshots_for_scope(
                    Some(owner),
                    |id| !delivered.contains(id) && !admitted.contains(id),
                    MAX_FACTS - results.len(),
                )
                .await
            {
                admitted.insert(task.id.clone());
                results.push(background_task_result_message(
                    task.id,
                    owner,
                    task.goal_id.as_ref(),
                    task.status,
                    task.exit_code,
                    &task.log,
                    task.log_truncated,
                ));
            }
        }
        if !results.is_empty() {
            self.sink.append(&results).await?;
            delivered.extend(admitted);
            messages.extend(results);
        }
        Ok(())
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
            let BackgroundChild {
                id,
                nickname,
                role,
                scope,
                handle,
                ..
            } = child;
            let result = join_settlement(handle.await);
            self.executor.release_child_scope(&id).await?;
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
            if let Some(host) = &self.executor.steering {
                host.child_ended(&id);
            }
            // The terminal is durable before the transcript says the child
            // settled: a crash between the two must not leave the parent
            // holding a result the log still calls unfinished.
            self.settlements_durable().await?;
            let notice = Message::user(
                settlement_notice(&nickname, &id, role, &scope, &content),
                TranscriptOrigin::RuntimeNotice {
                    notice: RuntimeNoticeKind::ChildSettlement,
                },
            );
            // Persist the notice NOW. The next ContextSnapshot is a whole
            // model round away, and outstanding_children was just cleared — a
            // crash in between would otherwise lose the child's report text
            // with no lost-note either.
            self.sink.append(std::slice::from_ref(&notice)).await?;
            messages.push(notice);
            (self.observer)(AgentEvent::runtime_injection(
                crate::executor::RuntimeInjectionKind::ChildSettlement,
                rt.model_steps().saturating_add(1),
            ));
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
            let ownership_gate = executor
                .tool_context
                .execution
                .command_gate
                .clone()
                .lock_owned()
                .await;
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
            let model_refusal = if !executor
                .background_write_conflicts(&child.spec.files)
                .await
                .is_empty()
            {
                Some("its write scope is held by a live background command".to_string())
            } else {
                model_refusal
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
            drop(ownership_gate);
            if let Some(refusal) = refusal {
                // Stale authority is never resurrected, and a child that cannot
                // hold its scope or run on its model cannot continue. Settle it
                // now, truthfully.
                executor.release_child_scope(&child.id).await?;
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
                let notice = Message::user(
                    settlement_notice(
                        &child.nickname,
                        &child.id,
                        child.role,
                        &child.spec.files,
                        &content,
                    ),
                    TranscriptOrigin::RuntimeNotice {
                        notice: RuntimeNoticeKind::ChildSettlement,
                    },
                );
                self.sink.append(std::slice::from_ref(&notice)).await?;
                messages.push(notice);
                (self.observer)(AgentEvent::runtime_injection(
                    crate::executor::RuntimeInjectionKind::ChildSettlement,
                    rt.model_steps().saturating_add(1),
                ));
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
            let note = Message::user(
                crate::sub_agent::resumed_children_note(&launched),
                TranscriptOrigin::RuntimeNotice {
                    notice: RuntimeNoticeKind::ChildResumed,
                },
            );
            self.sink.append(std::slice::from_ref(&note)).await?;
            messages.push(note);
            (self.observer)(AgentEvent::runtime_injection(
                crate::executor::RuntimeInjectionKind::ChildRecovery,
                rt.model_steps().saturating_add(1),
            ));
        }
        Ok(())
    }

    /// The error-exit drain: cancel every background child, wait for each to
    /// stop, and settle it like any other exit. Its scope is released only
    /// after it has stopped. Best effort on the durable side — the run is
    /// already failing, and that original error is what the caller gets.
    async fn stop_background_children(&mut self) -> Result<(), AgentError> {
        if self.background_children.children.is_empty() {
            return Ok(());
        }
        let mut settlement_error = None;
        for child in &self.background_children.children {
            child.token.cancel();
        }
        let children = std::mem::take(&mut self.background_children.children);
        for child in children {
            let result = join_settlement(child.handle.await);
            if let Err(error) = self.executor.release_child_scope(&child.id).await {
                settlement_error = Some(error);
                continue;
            }
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
            if let Some(host) = &self.executor.steering {
                host.child_ended(&child.id);
            }
        }
        while let Ok(event) = self.bg_progress_rx.try_recv() {
            match event {
                AgentEvent::SubAgentModelRequest { record } => {
                    if self.executor.model_request_store.is_none()
                        && let Err(error) = self.persist_request(*record).await
                    {
                        settlement_error = Some(error);
                    }
                }
                other => (self.observer)(other),
            }
        }
        (self.observer)(AgentEvent::ProgressUpdated {
            ledger: self.progress.clone(),
        });
        self.settlements_durable().await?;
        match settlement_error {
            Some(error) => Err(error),
            None => Ok(()),
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

    /// Record a delivery repair and give the model another request.
    async fn inject_settled_delivery(
        &mut self,
        messages: &mut Vec<Message>,
        model_steps: u32,
        text: String,
    ) -> Result<(), AgentError> {
        let message = Message::user(
            text,
            TranscriptOrigin::ProtocolRepair {
                repair: ProtocolRepairKind::GoalSettledDelivery,
            },
        );
        self.sink.append(std::slice::from_ref(&message)).await?;
        messages.push(message);
        (self.observer)(AgentEvent::AdvisoryStarted {
            kind: AdvisoryKind::CloseoutNudge(CloseoutReason::GoalSettledDelivery),
        });
        (self.observer)(AgentEvent::runtime_injection(
            crate::executor::RuntimeInjectionKind::CloseoutNudge(
                CloseoutReason::GoalSettledDelivery,
            ),
            model_steps.saturating_add(1),
        ));
        Ok(())
    }

    /// Publish the terminal for a goal whose resolution this drive accepted.
    ///
    /// The person's answer is the model's own final text; the goal summary is
    /// the fallback that already existed and is never dressed up as an answer.
    /// When neither exists the terminal carries that fact as a warning instead
    /// of pretending a delivery happened.
    async fn finish_settled_goal(
        &mut self,
        rt: &mut LoopContext,
        messages: &mut Vec<Message>,
        model_steps: u32,
    ) -> Result<Flow<AgentOutcome>, AgentError> {
        let Some((reason, summary)) = self.settled_goal.take() else {
            return Err(AgentError::ContextManagementFailure(
                "settled goal terminal requested without a resolution".to_string(),
            ));
        };
        let delivered = self
            .terminal_answer
            .as_ref()
            .is_some_and(|text| !text.trim().is_empty());
        let final_text = if delivered {
            self.terminal_answer.clone().unwrap_or_default()
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
        let mut outcome = AgentOutcome::drive_result(
            final_text,
            model_steps,
            self.modified_files.clone(),
            reason,
            None,
            &self.progress,
            &self.objective,
        );
        if !delivered {
            outcome.completion_warnings.push(
                "the goal was resolved but the model wrote no final answer for the user; the \
                 goal summary is all that was delivered"
                    .to_string(),
            );
        }
        outcome
            .completion_warnings
            .extend(std::mem::take(&mut self.completion_warnings));
        Ok(Flow::Stop(outcome))
    }

    /// A progress watchdog giving up: mark the ledger terminal, publish it,
    /// report the model's own last words (or `fallback` when it went quiet),
    /// and flush the epoch before returning.
    ///
    /// `detail` is the machine-readable reason carried on the outcome. It is
    /// never substituted for the model's own final text: the runtime's reason
    /// belongs in the terminal report, not in a fabricated assistant message.
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

impl Drive<'_> {
    /// Drop the "you can enable this" hint once nothing it mentions is still
    /// unexposed. A hint that outlives its fact is worse than none.
    fn retract_surface_growth_notice(&mut self) {
        let Some(state) = &self.executor.capabilities else {
            return;
        };
        use crate::capability::tool_capability;
        let still_hidden = self
            .executor
            .discovered_names()
            .iter()
            .filter_map(|name| tool_capability(name))
            .any(|id| !state.exposed(id));
        if !still_hidden {
            self.control_context
                .blocks
                .retain(|block| block.name != "tool_surface_grew");
        }
    }

    /// Tell the model that this request gained tools it did not start with.
    ///
    /// The fact belongs to the harness: a host catalog that came up mid-turn is
    /// invisible in the conversation, and a model that cannot see it either
    /// invents a workaround or claims the capability was never there. This is a
    /// request-epoch observation, not an instruction about what to test.
    fn note_surface_growth(&mut self, added: &[String]) {
        use crate::capability::{CapabilityId, tool_capability};
        // Whether the tools are ON the request's surface is a different fact
        // from whether the catalog grew. A discovered tool whose pack is still
        // unexposed is refused if the model calls it; saying it "is now part of
        // your surface" made a real agent believe the pack had already been
        // enabled — it called, was refused, called again, and the turn ended on
        // the no-progress guard without ever enabling the pack.
        let exposed = |id: CapabilityId| -> bool {
            match self.executor.capabilities.as_ref() {
                Some(state) => state.exposed(id),
                // No disclosure means no pack gating: every discovered tool is
                // callable, so nothing is "waiting for an enable".
                None => true,
            }
        };
        let mut usable: Vec<String> = Vec::new();
        let mut gated: std::collections::BTreeMap<CapabilityId, usize> =
            std::collections::BTreeMap::new();
        for name in added {
            match tool_capability(name) {
                Some(id) if exposed(id) => usable.push(name.clone()),
                Some(id) => *gated.entry(id).or_default() += 1,
                // A discovered tool outside every pack is already callable:
                // there is nothing to enable.
                None => usable.push(name.clone()),
            }
        }
        let mut text = String::new();
        if !usable.is_empty() {
            text.push_str(&format!(
                "Tools became available during this request and are now part of your surface: {}.",
                usable.join(", ")
            ));
        }
        if !gated.is_empty() {
            let packs: Vec<String> = gated
                .iter()
                .map(|(id, count)| {
                    let noun = if *count == 1 { "tool" } else { "tools" };
                    format!("{} ({count} {noun})", id.as_str())
                })
                .collect();
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(&format!(
                "The host catalog offered more tools during this request, but they are NOT on your \
                 surface yet because their optional pack is disabled: {}. A call to those tools is \
                 refused until you call capability(action=\"enable\", id=<pack>); enabling puts them \
                 on the following request.",
                packs.join(", ")
            ));
        }
        if text.is_empty() {
            return;
        }
        self.control_context
            .blocks
            .retain(|block| block.name != "tool_surface_grew");
        self.control_context
            .blocks
            .push(crate::executor::PromptSegment::control(
                "tool_surface_grew",
                crate::executor::PromptSource::ExecutionState,
                crate::executor::PromptAuthority::RuntimeFact,
                crate::executor::SegmentLifecycle::RequestEphemeral,
                false,
                text,
            ));
    }

    fn refresh_capability_surface(&mut self, reason: &str) {
        let Some(state) = &self.executor.capabilities else {
            return;
        };
        let tools = self.executor.request_tool_definitions();
        if tools == self.tools {
            return;
        }
        let previous_schema_tokens = leveler_model::estimate_tool_definitions(&self.tools);
        self.tools = tools;
        let request = self.objective.text();
        let mut optional = self.executor.system_segments(request);
        optional.retain(|segment| {
            matches!(
                segment.name.as_str(),
                "skills_guidance" | "multi_agent_guidance" | "host_interaction_guidance"
            )
        });
        let extra = self
            .executor
            .turn_control_context_inner(request, request, false);
        optional.extend(extra.blocks.into_iter().filter(|segment| {
            matches!(
                segment.name.as_str(),
                "selected_skills" | "skill_index" | "agent_catalog"
            )
        }));
        for segment in optional {
            if !self
                .control_context
                .blocks
                .iter()
                .any(|current| current.name == segment.name)
            {
                self.control_context.blocks.push(segment);
            }
        }
        if self.executor.depth == 0
            && state.exposed(crate::capability::CapabilityId::MultiAgent)
            && !self
                .control_context
                .blocks
                .iter()
                .any(|segment| segment.name == "multi_agent_hint")
        {
            self.control_context.blocks.push(PromptSegment::control(
                "multi_agent_hint",
                PromptSource::DelegationHint,
                PromptAuthority::CoreContract,
                SegmentLifecycle::SessionPrefix,
                true,
                multi_agent_steer_hint(),
            ));
        }
        self.retract_surface_growth_notice();
        tracing::info!(event="tool_surface_changed",reason,active_capabilities=?state.active(),tool_count=self.tools.len(),schema_tokens=leveler_model::estimate_tool_definitions(&self.tools),loaded_schema_tokens=leveler_model::estimate_tool_definitions(&self.tools).saturating_sub(previous_schema_tokens),tool_names=?self.tools.iter().map(|tool|&tool.name).collect::<Vec<_>>());
    }

    /// How much the provider counted above the estimator on the request that
    /// just completed. Zero until both figures exist, and never negative: a
    /// provider that reports fewer prompt tokens than estimated is not
    /// evidence the estimator is too high (cache accounting differs by
    /// route), so this only ever widens the safety margin.
    fn measured_estimate_gap(&self) -> u64 {
        match (
            self.last_projected_input_tokens,
            self.last_actual_input_tokens,
        ) {
            (Some(estimated), Some(actual)) => {
                // A provider's prompt count is a measurement of THIS request
                // only when a window was declared and the count fits in it.
                // Without a window there is nothing to check the figure
                // against, so last reported usage must not become the next
                // request's size. A count past a declared window is the same
                // kind of non-measurement (cumulative accounting, a
                // mislabelled field, a route that reports another quantity):
                // carrying it forward as a fixed gap would make every later
                // fold fail against a cost that folding cannot remove.
                let window = u64::from(self.executor.policy.context_policy.context_window);
                if window == 0 {
                    return 0;
                }
                if actual > window {
                    tracing::warn!(
                        estimated,
                        actual,
                        window,
                        "provider-reported prompt exceeds the model window; ignoring it as an estimate gap"
                    );
                    return 0;
                }
                actual.saturating_sub(estimated)
            }
            _ => 0,
        }
    }

    /// The projected request size with the measured gap applied. The gap was
    /// measured against the previous request, and the next request carries the
    /// same fixed prefix plus messages the same estimator priced, so applying
    /// it here judges pressure against what the provider will actually count.
    fn projected_with_measured_gap(&self, estimate: u64) -> u64 {
        estimate.saturating_add(self.measured_estimate_gap())
    }

    /// Bytes one result in this batch may occupy so the NEXT request stays
    /// inside the model's hard capacity.
    ///
    /// The static per-model ceiling answers "how large may one result be?";
    /// this answers "how much room is actually left?" — the question T8 showed
    /// was missing. The room is split evenly across the batch's calls because
    /// their results arrive together in one request: one call must not consume
    /// another's share.
    ///
    /// Bytes convert at the estimator's tool density (2/5), less a fixed
    /// allowance for the per-message role framing and JSON envelope the
    /// estimator deliberately does not price. With no hard capacity (a route
    /// that declares no window) the static ceiling is the only honest bound.
    fn tool_result_budget_bytes(
        &self,
        rt: &LoopContext,
        messages: &[Message],
        calls: usize,
    ) -> usize {
        let static_budget = self.executor.tool_context.policy.tool_output_budget;
        let policy = self.executor.policy.context_policy;
        if calls == 0 || !policy.folding_enabled() {
            return static_budget;
        }
        let Some(capacity) = policy.hard_capacity() else {
            return static_budget;
        };
        let base = self.projected_with_measured_gap(
            leveler_model::RequestProjection::project_with_control_context(
                messages,
                &self.tools,
                self.executor.policy.reasoning_replay,
                self.executor.policy.reasoning_retention,
                &self.request_context(rt),
                self.executor.route_identity().as_ref(),
            )
            .estimated_tokens(),
        );
        let share = capacity.saturating_sub(base) / calls as u64;
        let bytes = share
            .saturating_sub(TOOL_RESULT_FRAMING_ALLOWANCE)
            .saturating_mul(5)
            / 2;
        usize::try_from(bytes)
            .unwrap_or(usize::MAX)
            .clamp(leveler_tools::registry::MIN_TOOL_OUTPUT, static_budget)
    }
    /// Reconcile the scoped rule blocks with what is on disk right now.
    ///
    /// Only the CURRENT set is valid state: a new file is added, a changed
    /// file replaces its old block, a deleted file's block disappears, and an
    /// unchanged file is left untouched. Identity is the exact prompt text the
    /// block would carry, so there is no version counter or digest table that
    /// can disagree with the file. Blocks live in the control context (not the
    /// transcript), so a fold cannot restore a stale copy.
    ///
    /// Returns the sources that were injected.
    fn reconcile_scoped_rules(&mut self) -> Vec<String> {
        let Some(root) = self.scoped_rule_root() else {
            return Vec::new();
        };
        let candidates = scoped_rule_sources(&root, &self.scoped_paths);
        let mut dropped = false;
        let mut retained: Vec<leveler_model::PromptSegment> =
            Vec::with_capacity(self.control_context.blocks.len());
        for block in self.control_context.blocks.drain(..) {
            if let Some(source) = block.name.strip_prefix("scoped_rules:") {
                let still_valid = candidates
                    .iter()
                    .any(|rule| rule.source == source && scoped_rule_body(rule) == block.text);
                if !still_valid {
                    dropped = true;
                    continue;
                }
            }
            retained.push(block);
        }
        self.control_context.blocks = retained;
        let injected = self.inject_scoped_rules(&candidates);
        if dropped {
            // The rules that no longer apply are gone from the request; say so
            // without quoting them.
            (self.observer)(AgentEvent::runtime_injection(
                crate::executor::RuntimeInjectionKind::ScopedRules,
                self.model_steps,
            ));
        }
        injected
    }

    /// The nested rule documents that scope over `paths` (whether or not they
    /// are already in the context).
    fn applicable_scoped_rules(&self, paths: &[String]) -> Vec<ProjectInstruction> {
        let Some(root) = self.scoped_rule_root() else {
            return Vec::new();
        };
        scoped_rule_sources(&root, paths)
    }

    fn scoped_rule_root(&self) -> Option<std::path::PathBuf> {
        self.executor
            .tool_context
            .execution
            .workspace
            .as_ref()
            .map(|workspace| workspace.root().to_path_buf())
    }

    fn has_scoped_rule(&self, source: &str) -> bool {
        let name = format!("scoped_rules:{source}");
        self.control_context
            .blocks
            .iter()
            .any(|block| block.name == name)
    }

    /// Push the scoped rules that are not in the context yet. Returns the
    /// sources that were added.
    fn inject_scoped_rules(&mut self, rules: &[ProjectInstruction]) -> Vec<String> {
        let mut added = Vec::new();
        for rule in rules {
            if self.has_scoped_rule(&rule.source) {
                continue;
            }
            self.control_context.blocks.push(PromptSegment::control(
                format!("scoped_rules:{}", rule.source),
                PromptSource::ScopedRule {
                    path: rule.source.clone(),
                },
                PromptAuthority::ProjectInstruction,
                SegmentLifecycle::Scoped,
                false,
                scoped_rule_body(rule),
            ));
            added.push(rule.source.clone());
        }
        added
    }
}

/// The user's pause authorization for the bound goal, in the two forms the
/// harness needs.
///
/// `goal_paused` is the DURABLE fact recorded when an earlier window reached
/// the boundary; `reached` is the same boundary re-proved by THIS window's
/// plan. Both read the user's own instruction sequence (the objective plus
/// every durable amendment), never the model's prose, and an instruction that
/// is neither a pause nor a continuation leaves the previous boundary in force.
async fn user_pause_state(
    executor: &Executor,
    objective: &leveler_lifecycle::ObjectiveAnchor,
    plan: &PlanState,
) -> Result<(bool, bool), AgentError> {
    if !executor.goal_mode() {
        // A Chat turn with no goal has only this turn's objective anchor.
        let boundary = leveler_lifecycle::effective_user_pause(anchor_user_texts(objective));
        let reached = plan.is_model_explicit()
            && boundary
                .as_ref()
                .is_some_and(|boundary| boundary.reached(plan));
        return Ok((false, reached));
    }
    let scope = match executor.goal_binder() {
        Some(binder) => binder
            .scope()
            .await
            .map_err(|error| AgentError::Persistence(error.to_string()))?,
        None => None,
    };
    let mut texts: Vec<&str> = anchor_user_texts(objective);
    if let Some(scope) = scope.as_ref() {
        texts.extend(scope.amendments.iter().map(String::as_str));
    }
    // The durable column says an earlier window reached the user's boundary.
    // The user's own NEWEST directive decides whether it is still in force:
    // the column is cleared only at a window's end, so during the window they
    // just authorized it still reads as paused. Only an explicit continuation
    // releases it; a status question or a supplement leaves it in force.
    let goal_paused = scope.as_ref().is_some_and(|scope| {
        leveler_lifecycle::pause_in_force(scope.user_paused_at.is_some(), texts.iter().copied())
    });
    let boundary = leveler_lifecycle::effective_user_pause(texts);
    let reached = plan.is_model_explicit()
        && boundary
            .as_ref()
            .is_some_and(|boundary| boundary.reached(plan));
    Ok((goal_paused, reached))
}

/// The user-authored texts of an objective anchor, in recorded order.
fn anchor_user_texts(anchor: &leveler_lifecycle::ObjectiveAnchor) -> Vec<&str> {
    let mut texts: Vec<&str> = vec![anchor.primary.as_deref().unwrap_or(anchor.text.as_str())];
    texts.extend(anchor.amendments.iter().map(String::as_str));
    texts
}

#[async_trait]
impl AgentHarness for Drive<'_> {
    type Stop = AgentOutcome;
    type Error = AgentError;

    fn on_event(&mut self, event: leveler_agent_core::AgentEvent) {
        self.forward_kernel_event(event);
    }

    async fn before_model_attempt(&mut self, rt: &mut LoopContext) -> Result<(), AgentError> {
        let input = self.last_projected_input_tokens.ok_or_else(|| {
            AgentError::InvalidBudget(
                "model request has no authoritative projection for admission".into(),
            )
        })?;
        let output = u64::from(self.executor.max_output_tokens);
        let local = ProgressLedger {
            cumulative_model_tokens: rt.model_tokens_spent(),
            cumulative_cost_usd_micros: rt.cost_spent_micros(),
            has_unpriced_model_attempt: self.progress.has_unpriced_model_attempt,
            ..ProgressLedger::default()
        };
        super::model_request_budget_check(
            self.executor.step_limits,
            &local,
            input,
            output,
            self.executor.pricing.as_ref(),
        )?;
        self.executor.check_shared_model_budget(input, output).await
    }

    async fn on_model_attempt(
        &mut self,
        rt: &mut LoopContext,
        attempt: &leveler_model::ModelAttempt,
    ) -> Result<(), AgentError> {
        let mut record = super::model_attempt_record(
            attempt,
            &self.executor.model,
            crate::ModelCallKind::Round,
            self.executor.policy.reasoning_effort,
        );
        // The reasoning channel of the SAME projection this round admitted
        // itself on. The drive captured it from the `ContextUsage` of the
        // request it is about to send — the one place the estimate exists —
        // and the ledger row for that round is where it belongs. Without this
        // the number was computed every round and dropped, so "how much of
        // what we sent was replayed thinking?" could only be answered by
        // reconstructing the request offline.
        record.projected_reasoning_tokens = self.last_projected_reasoning_tokens;
        self.progress.has_unpriced_model_attempt |= record.cost_usd_micros.is_none();
        self.persist_request(record).await?;
        if let Some(text) = &attempt.partial_text {
            // Only the text already shown to the user is recoverable here.
            // Do not turn incomplete tool arguments or reasoning into replay
            // content, and do not emit a successful AssistantText event.
            self.sink.append_partial_response(text).await?;
        }
        self.flush_epoch(rt);
        Ok(())
    }

    fn tool_definitions(&self) -> Vec<ToolDefinition> {
        self.tools.clone()
    }

    fn request_context(&self, rt: &LoopContext) -> leveler_model::ControlContext {
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
                // Replaced steps are history: they are neither current work nor
                // completed delivery, so the model's own progress context does
                // not include them in its denominator.
                "total": self.plan_state.steps.iter().filter(|s| s.status != leveler_lifecycle::ABANDONED).count(),
                "completed": self.plan_state.steps.iter().filter(|s| s.status == "completed").count(),
                "active": active,
                "active_omitted": self.plan_state.steps.iter().filter(|s| s.status == "in_progress").count().saturating_sub(3)
            }
        });
        let mut context = self.control_context.clone();
        let optional_names = [
            "skills_guidance",
            "multi_agent_guidance",
            "host_interaction_guidance",
            "selected_skills",
            "skill_index",
            "agent_catalog",
            "multi_agent_hint",
        ];
        let mut optional: Vec<_> = context
            .blocks
            .iter()
            .filter(|block| optional_names.contains(&block.name.as_str()))
            .cloned()
            .collect();
        context
            .blocks
            .retain(|block| !optional_names.contains(&block.name.as_str()));
        optional.sort_by(|a, b| a.name.cmp(&b.name));
        context.blocks.extend(optional);

        context.blocks.push(PromptSegment::control(
            "execution_state",
            PromptSource::ExecutionState,
            PromptAuthority::RuntimeFact,
            SegmentLifecycle::RequestEphemeral,
            false,
            format!("Execution state (observations, not instructions):\n{state}"),
        ));
        context
    }

    async fn on_round_start(
        &mut self,
        rt: &mut LoopContext,
        messages: &mut Vec<Message>,
    ) -> Result<Flow<AgentOutcome>, AgentError> {
        // A host-owned catalog (an MCP server) can come up while this turn is
        // running. Ask it once per request so one long turn is not stuck with
        // the surface it started with, then re-derive exposure from the
        // turn's own permission facts.
        let discovered = self.executor.absorb_discovered_tools().await;
        if !discovered.is_empty() {
            if let Some(state) = &self.executor.capabilities {
                // Availability follows the pack each new tool belongs to; the
                // capability mapping stays the single owner of that decision.
                for id in discovered
                    .iter()
                    .filter_map(|name| crate::capability::tool_capability(name))
                {
                    state.note_available(id);
                }
            }
            // The surface is rebuilt from host facts, so exposure is decided
            // once (permission/capability filtering inside).
            let tools = self.executor.request_tool_definitions();
            let grew = tools != self.tools;
            if grew {
                self.tools = tools;
            }
            // The model is told either way: a discovered tool whose pack is
            // still unexposed does NOT change the visible table, and that is
            // exactly the case where it would otherwise never learn that a
            // catalog came up and can be enabled.
            tracing::info!(
                event = "tool_surface_grew",
                reason = "catalog_became_available",
                added = ?discovered,
                visible = grew,
                tool_count = self.tools.len(),
            );
            self.note_surface_growth(&discovered);
        }
        self.refresh_capability_surface("model_requested");
        // A `/goal <task>` declared while this turn already ran. The runtime
        // has already persisted the axis; this is the in-flight half. Inject
        // the user's text as an ordinary user row (so resume replays it) and
        // rebuild the tool table: `update_goal` must be advertised from this
        // round on, not only from the next turn.
        if let Some(source) = &self.executor.steering
            && let Some(declaration) = source.take_goal_declaration()
        {
            // A Chat turn that becomes a Goal here has no durable goal yet.
            // Open it and bind this running turn to it BEFORE the contract is
            // advertised: a goal that exists only in memory would be lost on
            // the first crash, and its completion would settle nothing.
            //
            // A turn that was ALREADY a Goal keeps its recorded objective: the
            // declaration is an instruction inside that goal, so the durable
            // pending copy is consumed instead of opening a second identity.
            if let Some(binder) = self.executor.goal_binder() {
                if self.executor.starts_in_goal_mode() || binder.bound().is_some() {
                    // The declaration is an instruction inside a goal this
                    // turn already owns. Keep it as user scope authority
                    // before consuming the pending copy.
                    binder.append_amendment(&declaration).await?;
                    binder.discard_declaration(&declaration).await?;
                } else {
                    binder.bind(&declaration, &[]).await?;
                }
            }
            let tools = self.executor.request_tool_definitions();
            if tools != self.tools {
                self.tools = tools;
            }
            let message = Message::user_input(declaration);
            self.sink.append(std::slice::from_ref(&message)).await?;
            messages.push(message);
        }
        // The upgraded window's ledger moves to the goal it now serves. Its
        // request rows were reassigned by the binder; duration and model-step
        // counts live only in this ledger, so resume must find them under the
        // same scope the goal reconciles.
        if let Some(goal_id) = self
            .executor
            .goal_binder()
            .and_then(|binder| binder.bound())
            && self.progress.budget_scope.as_deref() != Some(goal_id.as_str())
        {
            self.flush_epoch(rt);
            self.progress.budget_scope = Some(goal_id.to_string());
            (self.observer)(AgentEvent::ProgressUpdated {
                ledger: self.progress.clone(),
            });
        }
        // Mid-turn user input goes in at the top of the round, before the
        // model is asked anything: a correction that arrives after the work
        // is done is worthless. Empty is the normal case.
        if let Some(source) = &self.executor.steering {
            for text in source.take_pending() {
                let text = text.trim();
                if text.is_empty() {
                    continue;
                }
                // A user message during an active goal is user authority over
                // the scope (it may narrow or add a requirement). Keep it with
                // the goal so a later window cannot forget it.
                if let Some(binder) = self.executor.goal_binder() {
                    binder.append_amendment(text).await?;
                }
                let message = Message::user_input(text);
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
        if self.executor.step_limits.max_cost_usd_micros.is_some()
            && self.progress.has_unpriced_model_attempt
        {
            return Err(AgentError::Model(leveler_model::ModelError::new(
                leveler_model::ModelErrorKind::Other,
                "cannot admit another request under a cost cap: task has an unpriced model attempt",
            )));
        }
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
        // Reconcile scoped rules before this request, keeping them outside the
        // transcript so folding history cannot remove current constraints. A
        // source that changed is replaced and one that disappeared is dropped:
        // the request never carries a rule the repository no longer states.
        self.staged_scoped_rules.clear();
        let injected = self.reconcile_scoped_rules();
        if !injected.is_empty() {
            for source in &injected {
                push_unique_path(&mut self.progress.scoped_rule_sources, source);
            }
            (self.observer)(AgentEvent::ProgressUpdated {
                ledger: self.progress.clone(),
            });
            (self.observer)(AgentEvent::runtime_injection(
                crate::executor::RuntimeInjectionKind::ScopedRules,
                model_steps,
            ));
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
            self.control_context.blocks.push(PromptSegment::control(
                "model_step_ceiling",
                PromptSource::ModelStepCeiling,
                PromptAuthority::RuntimeFact,
                SegmentLifecycle::Turn,
                false,
                note,
            ));
            (self.observer)(AgentEvent::runtime_injection(
                crate::executor::RuntimeInjectionKind::ModelStepCeiling,
                model_steps,
            ));
            self.model_step_note_sent = true;
        }
        self.deliver_background_task_results(messages).await?;
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
                let feedback = Message::user(
                    format!(
                        "你上一次的工具调用参数不是合法 JSON:{}。请重新发起同一个工具调用,\
                             确保 arguments 是严格合法的 JSON——字符串里的反斜杠写成 `\\\\`、\
                             换行写成 `\\n`,不要放裸换行或裸反斜杠。多行脚本请拆成单行,\
                             或改用 write_file / apply_patch 之类不必在命令里塞长文本的工具。",
                        e.message
                    ),
                    TranscriptOrigin::ProtocolRepair {
                        repair: ProtocolRepairKind::InvalidToolJson,
                    },
                );
                self.sink.append(std::slice::from_ref(&feedback)).await?;
                messages.push(feedback);
                (self.observer)(AgentEvent::runtime_injection(
                    crate::executor::RuntimeInjectionKind::ProtocolRepair,
                    rt.model_steps().saturating_add(1),
                ));
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
                    let feedback = Message::user(
                        "Your output hit the token limit while emitting a tool call — the \
                             call was NOT executed. Re-issue it smaller: split a large patch \
                             into several apply_patch calls, or shorten the arguments.",
                        TranscriptOrigin::ProtocolRepair {
                            repair: ProtocolRepairKind::TruncatedToolCall,
                        },
                    );
                    self.sink.append(std::slice::from_ref(&feedback)).await?;
                    messages.push(feedback);
                    (self.observer)(AgentEvent::runtime_injection(
                        crate::executor::RuntimeInjectionKind::ProtocolRepair,
                        model_steps.saturating_add(1),
                    ));
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
                messages.push(Message::user(
                    "Continue exactly from the cutoff. Do not repeat prior text. Complete every open list, code block, sentence, and conclusion.",
                    TranscriptOrigin::ProtocolRepair {
                        repair: ProtocolRepairKind::LengthContinuation,
                    },
                ));
                (self.observer)(AgentEvent::runtime_injection(
                    crate::executor::RuntimeInjectionKind::LengthContinuation,
                    model_steps.saturating_add(1),
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
                    let feedback = Message::user(
                        "Your last response declared a tool call but no complete call \
                             arrived (it was likely cut off in transit). Re-issue the tool \
                             call in full.",
                        TranscriptOrigin::ProtocolRepair {
                            repair: ProtocolRepairKind::MissingToolCall,
                        },
                    );
                    self.sink.append(std::slice::from_ref(&feedback)).await?;
                    messages.push(feedback);
                    (self.observer)(AgentEvent::runtime_injection(
                        crate::executor::RuntimeInjectionKind::ProtocolRepair,
                        model_steps.saturating_add(1),
                    ));
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

        self.decode_retries = 0;
        if !text.trim().is_empty() {
            if self.continued_text.is_empty() {
                self.last_text = text.clone();
            } else {
                self.continued_text.push_str(&text);
                self.last_text = self.continued_text.clone();
            }
        }
        // A round that reasoned but produced no answer prose still has a durable
        // projection: its reasoning segments must survive a resume. Emitting the
        // event with empty text is what carries them — the projection renders a
        // Thought and no assistant block. `last_text` is left alone so an empty
        // round cannot claim the turn's answer.
        if !text.trim().is_empty() || !round_result.reasoning_segments.is_empty() {
            (self.observer)(AgentEvent::AssistantText {
                text: if text.trim().is_empty() {
                    String::new()
                } else {
                    self.last_text.clone()
                },
                reasoning: round_result.reasoning_segments.clone(),
            });
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
            // Unified closeout (executor/closeout.rs): ONE decision per quiet
            // round. A non-Goal turn ends `Answered` — the model answered, and
            // that is the whole of the fact. A Goal turn that went quiet
            // without calling `update_goal` is NOT resolved: the goal is still
            // active, so the harness drives another round on that lifecycle
            // fact alone. Continuation is bounded by the no-progress guard
            // (consecutive quiet rounds), not by a per-turn reminder budget, so
            // a model that resumes real work resets it while a model that only
            // re-emits a premature summary exhausts it. Past the bound the goal
            // ends `Stalled` — never as a success.
            let has_final_text = !self.last_text.trim().is_empty();
            // A goal this drive already resolved: this quiet round IS the
            // delivery the person was waiting for. Publishing here — instead of
            // at the `update_goal` call — is what makes the turn end with the
            // model's answer rather than with a bare tool call. A silent round
            // still gets one request for that answer, then the turn ends with
            // the facts it has.
            if self.settled_goal.is_some() {
                // This quiet round's own prose is the answer. `last_text` is
                // not: a process sentence from before a tool call stays there,
                // and the turn did not end on it.
                let prose = assistant.text_content();
                if !prose.trim().is_empty() {
                    self.terminal_answer = Some(if self.continued_text.is_empty() {
                        prose.trim().to_string()
                    } else {
                        self.last_text.trim().to_string()
                    });
                }
                if self.terminal_answer.is_none()
                    && self.settled_rounds == 0
                    && !self.delivery_followup_issued
                {
                    self.settled_rounds = self.settled_rounds.saturating_add(1);
                    let nudge = Message::user(
                        goal_settled_delivery_nudge(),
                        TranscriptOrigin::ProtocolRepair {
                            repair: ProtocolRepairKind::GoalSettledDelivery,
                        },
                    );
                    self.sink.append(&[assistant, nudge.clone()]).await?;
                    messages.push(nudge);
                    (self.observer)(AgentEvent::AdvisoryStarted {
                        kind: AdvisoryKind::CloseoutNudge(CloseoutReason::GoalSettledDelivery),
                    });
                    (self.observer)(AgentEvent::runtime_injection(
                        crate::executor::RuntimeInjectionKind::CloseoutNudge(
                            CloseoutReason::GoalSettledDelivery,
                        ),
                        model_steps.saturating_add(1),
                    ));
                    return Ok(Flow::NextRound);
                }
                // The kernel already holds this assistant in memory. Resume
                // reloads the sink, so a settled quiet round has to be written
                // before the turn ends. Skipping it leaves session_messages on
                // the delivery notice after the live event already showed the
                // answer. Do not push it again: on_quiet is called after that
                // in-memory push.
                self.sink.append(&[assistant]).await?;
                return self.finish_settled_goal(rt, messages, model_steps).await;
            }
            // The model's OWN latest plan, and only when it declared one: a
            // host-implicit single step is the harness's shell, not a promise
            // the model made, so it can never buy a continuation.
            let open_plan_steps: Vec<String> = if self.plan_state.is_model_explicit() {
                self.plan_state
                    .open_steps()
                    .into_iter()
                    .map(|step| step.step.clone())
                    .collect()
            } else {
                Vec::new()
            };
            let refused_registration_gaps = if self.executor.goal_mode() {
                // A goal turn is bound to the goal lifecycle; the refused
                // declaration is already answered by the goal closeout.
                0
            } else {
                self.commit_refusal
                    .as_ref()
                    .map(|refusal| refusal.uncovered)
                    .unwrap_or(0)
            };
            // The user's own authorization sequence for this goal, in recorded
            // order. In Goal mode the durable scope contributes the cross-turn
            // user turns; the goal's own objective is already the anchor's
            // primary, so it is not read a second time.
            //
            // Reading the sequence — not the newest message alone — is what lets
            // a status question leave the pause in force while an explicit
            // "继续阶段 2" releases it.
            let (goal_user_paused, user_pause_reached) =
                user_pause_state(self.executor, &self.objective, &self.plan_state).await?;
            let pause_finish =
                self.executor.goal_mode() && (user_pause_reached || goal_user_paused);
            let action = decide(&CloseoutInput {
                goal_mode: self.executor.goal_mode(),
                has_final_text,
                cancelled: cancellation.is_cancelled(),
                can_continue: has_next_model_step,
                budget_remaining: self.closeout_budget.remaining(),
                human_boundary_seen: self.progress.human_boundary_seen(),
                continuations_exhausted: self
                    .progress
                    .should_hard_stop_no_progress(self.progress_caps),
                open_plan_steps: open_plan_steps.len(),
                refused_registration_gaps,
                user_pause_reached,
                goal_user_paused,
            });
            // Whether the harness accepted the quiet round or bought itself
            // another model call is the difference between "the model is
            // slow" and "we added a round" — indistinguishable on screen.
            tracing::info!(
                model_steps,
                ?action,
                goal_mode = self.executor.goal_mode(),
                has_final_text,
                budget_remaining = self.closeout_budget.remaining(),
                no_progress_streak = self.progress.no_progress_streak,
                open_plan_steps = open_plan_steps.len(),
                user_pause_reached,
                goal_user_paused,
                "closeout decided"
            );
            // A Goal continuation is a lifecycle drive, not a protocol repair:
            // it counts one no-progress tick (the mechanical bound) and does not
            // spend the one-shot repair budget. The injected text states only
            // that the goal is still active.
            if let CloseoutAction::ContinueGoal = action {
                self.progress.note_no_progress_round(model_steps);
                (self.observer)(AgentEvent::ProgressUpdated {
                    ledger: self.progress.clone(),
                });
                (self.observer)(AgentEvent::AdvisoryStarted {
                    kind: AdvisoryKind::CloseoutNudge(CloseoutReason::GoalUnresolved),
                });
                let nudge = Message::user(
                    goal_continuation_nudge(),
                    TranscriptOrigin::ProtocolRepair {
                        repair: ProtocolRepairKind::GoalUnresolved,
                    },
                );
                // Persist BOTH the quiet-round assistant text and the nudge:
                // a resume reloads the transcript from the sink, and any gap
                // here makes the resumed model see a different conversation
                // than the one it actually had.
                self.sink.append(&[assistant, nudge.clone()]).await?;
                messages.push(nudge);
                (self.observer)(AgentEvent::runtime_injection(
                    crate::executor::RuntimeInjectionKind::CloseoutNudge(
                        CloseoutReason::GoalUnresolved,
                    ),
                    model_steps.saturating_add(1),
                ));
                return Ok(Flow::NextRound);
            }
            if let CloseoutAction::ContinuePlan = action {
                // Same lifecycle shape as the goal drive, one level down: the
                // plan is still open, so the harness states its own record and
                // ticks the no-progress bound. It never spends the one-shot
                // protocol-repair budget, and it never names a step as "next".
                self.progress.note_no_progress_round(model_steps);
                (self.observer)(AgentEvent::ProgressUpdated {
                    ledger: self.progress.clone(),
                });
                (self.observer)(AgentEvent::AdvisoryStarted {
                    kind: AdvisoryKind::CloseoutNudge(CloseoutReason::PlanUnresolved),
                });
                let nudge = Message::user(
                    plan_unresolved_nudge(&open_plan_steps),
                    TranscriptOrigin::ProtocolRepair {
                        repair: ProtocolRepairKind::PlanUnresolved,
                    },
                );
                // Persist BOTH the quiet-round assistant text and the nudge: a
                // resume reloads the transcript from the sink, and any gap here
                // makes the resumed model see a different conversation than the
                // one it actually had.
                self.sink.append(&[assistant, nudge.clone()]).await?;
                messages.push(nudge);
                (self.observer)(AgentEvent::runtime_injection(
                    crate::executor::RuntimeInjectionKind::CloseoutNudge(
                        CloseoutReason::PlanUnresolved,
                    ),
                    model_steps.saturating_add(1),
                ));
                return Ok(Flow::NextRound);
            }
            if let CloseoutAction::ContinueTask = action {
                // The model declared this request as work to finish and the
                // harness refused the declaration for missing acceptance
                // coverage. That refusal settled the CONTRACT, not the user's
                // request: the turn may not end as though the work were done.
                self.progress.note_no_progress_round(model_steps);
                (self.observer)(AgentEvent::ProgressUpdated {
                    ledger: self.progress.clone(),
                });
                (self.observer)(AgentEvent::AdvisoryStarted {
                    kind: AdvisoryKind::CloseoutNudge(CloseoutReason::TaskUnregistered),
                });
                let uncovered = self
                    .commit_refusal
                    .as_ref()
                    .map(|refusal| refusal.uncovered)
                    .unwrap_or(0);
                // State it ONCE. The harness has said the only thing it knows;
                // repeating it on the next quiet round is pressure, not
                // information, and the model's next declaration is a fresh
                // attempt that needs its own comparison.
                self.commit_refusal = None;
                let nudge = Message::user(
                    task_unregistered_nudge(uncovered),
                    TranscriptOrigin::ProtocolRepair {
                        repair: ProtocolRepairKind::TaskRegistrationRefused,
                    },
                );
                self.sink.append(&[assistant, nudge.clone()]).await?;
                messages.push(nudge);
                (self.observer)(AgentEvent::runtime_injection(
                    crate::executor::RuntimeInjectionKind::CloseoutNudge(
                        CloseoutReason::TaskUnregistered,
                    ),
                    model_steps.saturating_add(1),
                ));
                return Ok(Flow::NextRound);
            }
            if let CloseoutAction::NudgeOnce(reason) = action {
                self.closeout_budget.consume();
                // Surface the injection: without this the user sees a
                // "final" answer and then an unexplained extra model round.
                (self.observer)(AgentEvent::AdvisoryStarted {
                    kind: AdvisoryKind::CloseoutNudge(reason),
                });
                let nudge = match reason {
                    CloseoutReason::GoalUnresolved => Message::user(
                        goal_continuation_nudge(),
                        TranscriptOrigin::ProtocolRepair {
                            repair: ProtocolRepairKind::GoalUnresolved,
                        },
                    ),
                    CloseoutReason::PlanUnresolved => Message::user(
                        plan_unresolved_nudge(&open_plan_steps),
                        TranscriptOrigin::ProtocolRepair {
                            repair: ProtocolRepairKind::PlanUnresolved,
                        },
                    ),
                    // TaskUnregistered is a continuation, not a one-shot
                    // repair, so this arm is unreachable through `decide`.
                    // The text is the same fact the continuation states.
                    CloseoutReason::TaskUnregistered => Message::user(
                        task_unregistered_nudge(
                            self.commit_refusal
                                .as_ref()
                                .map(|refusal| refusal.uncovered)
                                .unwrap_or(0),
                        ),
                        TranscriptOrigin::ProtocolRepair {
                            repair: ProtocolRepairKind::TaskRegistrationRefused,
                        },
                    ),
                    CloseoutReason::EmptyAnswer => Message::user(
                        "Your last message was empty. Reply with the actual answer to the \
                             request — do not send an empty message.",
                        TranscriptOrigin::ProtocolRepair {
                            repair: ProtocolRepairKind::EmptyAnswer,
                        },
                    ),
                    CloseoutReason::GoalSettledDelivery => Message::user(
                        goal_settled_delivery_nudge(),
                        TranscriptOrigin::ProtocolRepair {
                            repair: ProtocolRepairKind::GoalSettledDelivery,
                        },
                    ),
                };
                // Persist BOTH the quiet-round assistant text and the nudge:
                // an engine continuation reloads the transcript from the
                // sink, and any gap here makes the resumed model see a
                // different conversation than the one it actually had.
                self.sink.append(&[assistant, nudge.clone()]).await?;
                messages.push(nudge);
                (self.observer)(AgentEvent::runtime_injection(
                    crate::executor::RuntimeInjectionKind::CloseoutNudge(reason),
                    model_steps.saturating_add(1),
                ));
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
                if self.executor.goal_mode() && self.progress.human_boundary_seen() {
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
                } else if self.executor.goal_mode() && pause_finish {
                    // The user's own pause ended the authorized range. Nothing
                    // failed and nothing is resolved: the goal keeps owing the
                    // withheld work and stays recoverable. The stop is the
                    // ordinary `Answered` (the model answered inside the
                    // range) with a durable detail that says WHY the round
                    // ended, so the UI can show a pause instead of a completed
                    // goal and a replay can reach the same state.
                    (
                        StopReason::Answered,
                        Some(super::closeout::user_pause_detail()),
                    )
                } else if self.executor.goal_mode() {
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
            let mut outcome = AgentOutcome::drive_result(
                self.last_text.clone(),
                model_steps,
                self.modified_files.clone(),
                stop_reason,
                stop_detail,
                &self.progress,
                &self.objective,
            );
            // A Chat turn that ends after its own declaration was refused did
            // not deliver a verified task. The stop reason stays `Answered`
            // (the model did answer, and there is no goal to verify), but the
            // terminal must carry the fact so the answer is not read as
            // acceptance of the user's request.
            if !self.executor.goal_mode()
                && self.registration_refused.is_some()
                && matches!(outcome.stop_reason, StopReason::Answered)
            {
                outcome.completion_warnings.push(format!(
                    "no execution task was registered: a commit_task declaration was refused \
                     because {} of the user's explicitly named deliverables had no acceptance \
                     check. This turn ended with an answer, not with a verified delivery.",
                    self.registration_refused.unwrap_or(0)
                ));
            }
            return Ok(Flow::Stop(outcome));
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
        // Measured BEFORE any result is appended: the request these results
        // will join already carries the assistant message and its tool calls,
        // so what is left is exactly the room they may use. Every call in the
        // batch shares it; a static ceiling that ignores the window is what
        // let one `grep` result crowd out the task (G7/T8).
        let tool_result_budget =
            self.tool_result_budget_bytes(rt, messages.as_slice(), calls.len());
        // Tool results, filled by call index. Parallel-safe read-only tools
        // may finish out of order, but the model must receive their results
        // in the original call order — providers reject reordered results.
        let mut observed_terminal_tasks = Vec::new();
        let mut results: Vec<Option<ContentPart>> = (0..calls.len()).map(|_| None).collect();
        // Whether this round's assistant message said anything to the person.
        // A round that resolved the goal without a word is the case the delivery
        // round exists for.
        let round_prose = assistant.text_content();
        let round_spoke = !round_prose.trim().is_empty();
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
        // Calls that produced no durable effect: refused before they ran
        // (budgets, allowlist, permission) or rejected by the goal gate
        // (a resolution the runtime would not accept). A round consisting
        // solely of these is no progress — it feeds the all-refused streak,
        // so a model repeating the same refused resolution cannot spin under
        // an unbounded continuation policy. A goal-verification refusal is
        // counted in `verification_refusals_this_round` instead: that
        // feedback is recoverable, and it has its own bound.
        let mut denied_calls_this_round: usize = 0;
        let mut verification_refusals_this_round: usize = 0;
        // A requirements refusal whose verdict is DIFFERENT from the previous
        // one is the model acting on the feedback, not repeating itself. It is
        // bounded by the same recovery budget as a citation correction, so an
        // accounting the gate can satisfy is never stopped by a streak that
        // said "only repeated refused calls".
        let mut gate_verdict_advanced_this_round = false;
        // User cancel observed inside this batch. The batch stops, but the
        // round is still committed (results + spend) before Cancelled
        // surfaces — completed tools' side effects are already on disk.
        let mut cancelled_mid_batch = false;
        // A goal resolution refused because delegated children were still
        // running must wait for their settlements, but a settlement notice is a
        // USER-role message: appending it here would land between this round's
        // assistant tool calls and their results, which providers reject
        // ("assistant tool call … has no tool result"). The drain therefore
        // runs after the results are paired.
        let mut drain_before_next_round = false;
        // Ids/names survive the consuming loop below so calls the cancel
        // cut short can still be refused in place (transcript pairing).
        let call_snapshot: Vec<ToolCall> = calls.clone();

        let advertised: std::collections::HashSet<String> =
            self.tools.iter().map(|tool| tool.name.clone()).collect();
        for (index, call) in calls.into_iter().enumerate() {
            if cancellation.is_cancelled() && !rt.deadline_expired() {
                cancelled_mid_batch = true;
                break;
            }
            if self.executor.capabilities.is_some()
                && !advertised.contains(&call.name)
                && !(call.name == "ask_user" && advertised.contains("request_user_input"))
            {
                let content = format!(
                    "Tool {} is not exposed on this request. Discover and enable its capability first.",
                    call.name
                );
                (self.observer)(AgentEvent::ToolCall {
                    id: call.id.to_string(),
                    name: call.name.clone(),
                    arguments: compact_json(&call.arguments),
                    parallel: false,
                    model_step: Some(model_steps),
                });
                (self.observer)(AgentEvent::ToolResult {
                    exit_code: None,
                    stop: None,
                    execution_status: None,
                    id: call.id.to_string(),
                    name: call.name.clone(),
                    is_error: true,
                    preview: content.clone(),
                    applied_diff: None,
                });
                results[index] = Some(ContentPart::ToolResult {
                    result: ToolResultContent {
                        call_id: call.id,
                        content,
                        is_error: true,
                    },
                });
                denied_calls_this_round += 1;
                continue;
            }
            if call.name == "capability" && self.executor.capabilities.is_some() {
                (self.observer)(AgentEvent::ToolCall {
                    id: call.id.to_string(),
                    name: call.name.clone(),
                    arguments: compact_json(&call.arguments),
                    parallel: false,
                    model_step: Some(model_steps),
                });
                if let Some(barrier) = &self.executor.event_barrier {
                    barrier.flush().await?;
                }
                if cancellation.is_cancelled() && !rt.deadline_expired() {
                    results[index] = Some(deny_call(
                        &mut *self.observer,
                        call,
                        "cancelled before capability loading".into(),
                        model_steps,
                    ));
                    cancelled_mid_batch = true;
                    break;
                }
                if let Some(fence) = &self.executor.execution_fence {
                    fence
                        .ensure_current()
                        .await
                        .map_err(AgentError::StaleOwnership)?;
                }
                let state = self
                    .executor
                    .capabilities
                    .as_ref()
                    .expect("disclosure state");
                let valid = call.arguments.as_object().is_some_and(|args| {
                    args.keys().all(|key| key == "action" || key == "id")
                        && args.get("id").is_none_or(serde_json::Value::is_string)
                });
                let action = call
                    .arguments
                    .get("action")
                    .and_then(|value| value.as_str());
                let result = if !valid {
                    Err("capability expects action and optional id only".to_string())
                } else {
                    match action {
                        Some("list" | "status") => Ok((state.catalog_with_authority(self.executor.tool_context.policy.unrestricted_execution()).to_string(), false)),
                        Some("enable") => match call
                            .arguments
                            .get("id")
                            .and_then(|value| value.as_str())
                        {
                            Some(id) => match crate::capability::CapabilityId::parse(id) {
                                Ok(id) => state.enable_with_authority(id, self.executor.tool_context.policy.unrestricted_execution()).await.map(|changed| {
                                    (
                                        format!(
                                            "{} enabled; tools available on the following request.",
                                            id.as_str()
                                        ),
                                        changed,
                                    )
                                }),
                                Err(error) => Err(error),
                            },
                            None => Err("capability enable requires string id".into()),
                        },
                        _ => Err("capability action must be list, status, or enable".into()),
                    }
                };
                let (content, is_error) = match result {
                    Ok((content, changed)) => {
                        if changed {
                            self.refresh_capability_surface("model_requested");
                        }
                        (content, false)
                    }
                    Err(error) => {
                        tracing::info!(event="capability_enable_denied",reason=%error);
                        (error, true)
                    }
                };
                if is_error {
                    denied_calls_this_round += 1;
                }
                (self.observer)(AgentEvent::ToolResult {
                    exit_code: None,
                    stop: None,
                    execution_status: None,
                    id: call.id.to_string(),
                    name: call.name.clone(),
                    is_error,
                    preview: content.clone(),
                    applied_diff: None,
                });
                results[index] = Some(ContentPart::ToolResult {
                    result: ToolResultContent {
                        call_id: call.id,
                        content,
                        is_error,
                    },
                });
                continue;
            }

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
                    model_step: Some(model_steps),
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
                    execution_status: None,
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

            // Chat delegation: the model declares that the user's request is
            // work this turn must carry to completion. The declaration is the
            // ONLY authority that turns a Chat window into a task window; the
            // runtime does not infer it from the user's words. Binding is
            // durable (goal row + turn WAL + budget scope) because the work
            // may outlive this process.
            if call.name == COMMIT_TASK_TOOL && self.executor.depth == 0 {
                (self.observer)(AgentEvent::ToolCall {
                    id: call.id.as_str().to_string(),
                    name: COMMIT_TASK_TOOL.to_string(),
                    arguments: compact_json(&call.arguments),
                    parallel: false,
                    model_step: Some(model_steps),
                });
                let objective = call
                    .arguments
                    .get("objective")
                    .and_then(|value| value.as_str())
                    .map(str::trim)
                    .filter(|text| !text.is_empty() && text.chars().count() <= 4000)
                    .map(str::to_string);
                let acceptance: Option<Vec<String>> = call
                    .arguments
                    .get("acceptance")
                    .and_then(|value| value.as_array())
                    .filter(|items| (1..=20).contains(&items.len()))
                    .and_then(|items| {
                        items
                            .iter()
                            .map(|item| {
                                item.as_str()
                                    .map(str::trim)
                                    .filter(|text| !text.is_empty() && text.chars().count() <= 500)
                                    .map(str::to_string)
                            })
                            .collect()
                    });
                let (ok, content) = if self.executor.goal_mode() {
                    (
                        false,
                        "This turn is already executing a committed task. Continue the work \
                         and resolve it with update_goal; do not declare it again."
                            .to_string(),
                    )
                } else if let (Some(objective), Some(acceptance)) = (objective, acceptance) {
                    // The durable objective is the user's own request, not the
                    // model's restatement. A model cannot narrow the task by
                    // declaring a smaller objective; its `acceptance` checks
                    // are the part it owns — but they must still cover every
                    // deliverable the user explicitly enumerated.
                    let user_objective = self.objective.text().trim();
                    let goal_objective = if user_objective.is_empty() {
                        objective.clone()
                    } else {
                        user_objective.to_string()
                    };
                    let requirements =
                        goal_scope::extract(&goal_objective, &self.objective.amendments);
                    let uncovered = goal_scope::uncovered(&requirements, &acceptance);
                    if !uncovered.is_empty() {
                        let repeats = match &self.commit_refusal {
                            Some(previous)
                                if previous.objective == goal_objective
                                    && previous.acceptance == acceptance =>
                            {
                                previous.repeats.saturating_add(1)
                            }
                            _ => 1,
                        };
                        self.registration_refused = Some(uncovered.len());
                        self.commit_refusal = Some(CommitRefusal {
                            objective: goal_objective.clone(),
                            acceptance: acceptance.clone(),
                            repeats,
                            uncovered: uncovered.len(),
                        });
                        // Structured refusal: the model needs to know WHAT is
                        // uncovered and HOW to fix it, not a multi-kilobyte
                        // dump it cannot act on. The full requirement set stays
                        // in the goal row and is readable through the model's
                        // own objective; this lists a bounded, stable sample
                        // with the source ids the runtime itself assigned.
                        const REFUSAL_SAMPLE: usize = 8;
                        let deliverables = requirements
                            .iter()
                            .filter(|requirement| requirement.kind.is_deliverable())
                            .count();
                        let mut detail = format!(
                            "commit_task refused [acceptance_missing_deliverables]: {} of {} \
                             explicit user deliverables have no covering `acceptance` check.\n",
                            uncovered.len(),
                            deliverables
                        );
                        for text in uncovered.iter().take(REFUSAL_SAMPLE) {
                            match requirements
                                .iter()
                                .find(|requirement| requirement.text == *text)
                            {
                                Some(requirement) => detail.push_str(&format!(
                                    "- {} [{}] {}\n",
                                    requirement.id,
                                    requirement.kind.label(),
                                    requirement.text
                                )),
                                None => detail.push_str(&format!("- {text}\n")),
                            }
                        }
                        if uncovered.len() > REFUSAL_SAMPLE {
                            detail.push_str(&format!(
                                "- …and {} more (the complete list is the user's own request in \
                                 this session)\n",
                                uncovered.len() - REFUSAL_SAMPLE
                            ));
                        }
                        detail.push_str(
                            "Fix: add one concrete `acceptance` check per item above, quoting \
                             the deliverable's own words. One check may name several related \
                             deliverables. Do not drop or reword the user's deliverables to make \
                             them match the checks you already have — the refusal conditions are \
                             the user's text, not the plan. Prohibitions (`forbidden`) never need \
                             an acceptance check.",
                        );
                        if repeats > 1 {
                            detail.push_str(&format!(
                                "\nThis is refusal #{repeats} for the identical objective and \
                                 acceptance: the conditions have not changed, so repeating the \
                                 same call cannot succeed. Either add the missing checks or \
                                 continue the work without committing."
                            ));
                        }
                        (self.observer)(AgentEvent::GoalIntercepted {
                            kind: "acceptance_narrowed".to_string(),
                            detail: detail.clone(),
                        });
                        (false, detail)
                    } else {
                        match self.executor.goal_binder().cloned() {
                            Some(binder) => {
                                match binder.bind(&goal_objective, &acceptance).await {
                                    Ok(goal) => {
                                        self.commit_refusal = None;
                                        self.registration_refused = None;
                                        // The committed window carries the goal
                                        // contract from the next request on.
                                        self.tools = self.executor.request_tool_definitions();
                                        // Report the DURABLE scope, not the
                                        // model's restatement. When this turn
                                        // joined a goal the task already owed,
                                        // that goal's objective and acceptance
                                        // are what the completion gate reads,
                                        // and the model must not believe it
                                        // declared a second task.
                                        let bound = binder.scope().await.ok().flatten();
                                        let (bound_objective, bound_acceptance) = bound
                                            .map(|scope| (scope.objective, scope.acceptance))
                                            .unwrap_or_else(|| {
                                                (goal_objective.clone(), acceptance.clone())
                                            });
                                        let joined = bound_objective != goal_objective;
                                        let checks = bound_acceptance
                                            .iter()
                                            .map(|check| format!("- {check}"))
                                            .collect::<Vec<_>>()
                                            .join("\n");
                                        let requirements = goal_scope::extract(
                                            &bound_objective,
                                            &self.objective.amendments,
                                        );
                                        let header = if joined {
                                            format!(
                                                "Execution task joined: goal {goal}. This session \
                                                 already owed this task, so the runtime did not \
                                                 open a second one.\nObjective: {bound_objective}\n"
                                            )
                                        } else {
                                            format!(
                                                "Execution task committed as goal {goal}.\n\
                                                 Objective: {bound_objective}\n"
                                            )
                                        };
                                        let deliverables: Vec<String> = requirements
                                            .iter()
                                            .filter(|requirement| requirement.kind.is_deliverable())
                                            .map(|requirement| format!("- {}", requirement.text))
                                            .collect();
                                        let constraints: Vec<String> = requirements
                                            .iter()
                                            .filter(|requirement| {
                                                !requirement.kind.is_deliverable()
                                            })
                                            .map(|requirement| format!("- {}", requirement.text))
                                            .collect();
                                        let mut scope = String::new();
                                        if !deliverables.is_empty() {
                                            scope.push_str(&format!(
                                                "User requirements (explicit deliverables):\n{}\n",
                                                deliverables.join("\n")
                                            ));
                                        }
                                        if !constraints.is_empty() {
                                            scope.push_str(&format!(
                                                "User constraints (prohibitions: honour them, do \
                                                 not report them as delivered work):\n{}\n",
                                                constraints.join("\n")
                                            ));
                                        }
                                        (
                                            true,
                                            format!(
                                                "{header}{scope}Acceptance:\n{checks}\n\
                                                 This turn stays running until the task is \
                                                 resolved. A stage summary does not end it. \
                                                 Continue the work, then call \
                                                 update_goal(status=\"complete\", \
                                                 verification=..., requirements=[...]) with one \
                                                 entry per explicit user requirement, or \
                                                 update_goal(status=\"blocked\", ...) if the \
                                                 stated objective cannot be met. Do not narrow \
                                                 the objective."
                                            ),
                                        )
                                    }
                                    Err(error) => (
                                        false,
                                        format!("commit_task could not record the task: {error}"),
                                    ),
                                }
                            }
                            None => (
                                false,
                                "commit_task is unavailable here: this execution has no durable \
                                 task store."
                                    .to_string(),
                            ),
                        }
                    }
                } else {
                    (
                        false,
                        "commit_task requires a non-empty `objective` (up to 4000 characters) \
                         and 1-20 non-empty `acceptance` checks (up to 500 characters each)."
                            .to_string(),
                    )
                };
                (self.observer)(AgentEvent::ToolResult {
                    exit_code: None,
                    stop: None,
                    execution_status: None,
                    id: call.id.as_str().to_string(),
                    name: COMMIT_TASK_TOOL.to_string(),
                    is_error: !ok,
                    preview: preview(&content),
                    applied_diff: None,
                });
                results[index] = Some(ContentPart::ToolResult {
                    result: ToolResultContent {
                        call_id: call.id,
                        content,
                        is_error: !ok,
                    },
                });
                continue;
            }

            // Goal mode: the model explicitly resolves the objective. Record
            // the resolution; the run ends after this round's results are
            // committed (so the transcript stays well-formed).
            if self.executor.goal_mode() && call.name == UPDATE_GOAL_TOOL {
                // Surface the resolution so the TUI/JSONL shows the goal being
                // closed (special tools otherwise skip the ToolCall event).
                (self.observer)(AgentEvent::ToolCall {
                    id: call.id.as_str().to_string(),
                    name: UPDATE_GOAL_TOOL.to_string(),
                    arguments: compact_json(&call.arguments),
                    parallel: false,
                    model_step: Some(model_steps),
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
                    drain_before_next_round = true;
                    let feedback = format!(
                        "update_goal(complete) was not accepted: delegated sub-agent(s) {} \
                             were still running. The runtime settles them before your next \
                             step, so their notices follow this result. A Goal is still \
                             resolved only through update_goal(complete|blocked).",
                        waiting.join(", ")
                    );
                    (self.observer)(AgentEvent::GoalIntercepted {
                        kind: "outstanding_children".to_string(),
                        detail: waiting.join(", "),
                    });
                    (self.observer)(AgentEvent::ToolResult {
                        exit_code: None,
                        stop: None,
                        execution_status: None,
                        id: call.id.as_str().to_string(),
                        name: UPDATE_GOAL_TOOL.to_string(),
                        is_error: true,
                        preview: goal_refusal_preview("outstanding_children", &feedback),
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
                        // Persisted like every other refusal: a rejected
                        // resolution is a fact resume/UI/eval must be able to
                        // count by reason, not only read from a tool result.
                        denied_calls_this_round += 1;
                        (self.observer)(AgentEvent::GoalIntercepted {
                            kind: "invalid_status".to_string(),
                            detail: other.unwrap_or("no status").to_string(),
                        });
                        (self.observer)(AgentEvent::ToolResult {
                            exit_code: None,
                            stop: None,
                            execution_status: None,
                            id: call.id.as_str().to_string(),
                            name: UPDATE_GOAL_TOOL.to_string(),
                            is_error: true,
                            preview: goal_refusal_preview("invalid_status", &feedback),
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
                // The user's pause is not the model's obstruction. A model
                // that reaches the authorized boundary truthfully reports
                // "waiting for the user", and `blocked` is the only terminal
                // that phrase resembles — but a block would hide the pause,
                // end the session's goal route and leave the goal resumable
                // only by a literal `继续`. State the fact and let the round
                // end normally; the closeout records the pause.
                if reason == StopReason::Blocked {
                    let (goal_paused, reached) =
                        user_pause_state(self.executor, &self.objective, &self.plan_state).await?;
                    if self.executor.goal_mode() && (goal_paused || reached) {
                        denied_calls_this_round += 1;
                        let message = "update_goal(blocked) is not the resolution for this \
                                       round: the user paused the goal at the boundary they \
                                       stated, and the model's own plan reached it. Nothing is \
                                       obstructed — the remaining work is withheld pending the \
                                       user's authorization. End the round normally; the user's \
                                       own continuation releases the pause, and \
                                       update_goal(complete) is still the terminal once every \
                                       stage is done."
                            .to_string();
                        (self.observer)(AgentEvent::GoalIntercepted {
                            kind: "user_pause_boundary".to_string(),
                            detail: message.clone(),
                        });
                        (self.observer)(AgentEvent::ToolResult {
                            exit_code: None,
                            stop: None,
                            execution_status: None,
                            id: call.id.as_str().to_string(),
                            name: UPDATE_GOAL_TOOL.to_string(),
                            is_error: true,
                            preview: goal_refusal_preview("user_pause_boundary", &message),
                            applied_diff: None,
                        });
                        results[index] = Some(ContentPart::ToolResult {
                            result: ToolResultContent {
                                call_id: call.id.clone(),
                                content: message,
                                is_error: true,
                            },
                        });
                        continue;
                    }
                }
                // A completion claim is not sole evidence. The model's own
                // verification field is checked against this drive's command
                // record; a failed test/build cannot be closed as complete,
                // and a changed workspace must name either a command that
                // actually passed after the change or an explicit
                // `not_applicable` reason. No semantic judgement is made here.
                if reason == StopReason::Completed {
                    let claim = match super::goal_verification::parse_claim(&call.arguments) {
                        Ok(claim) => claim,
                        Err(message) => {
                            denied_calls_this_round += 1;
                            (self.observer)(AgentEvent::GoalIntercepted {
                                kind: "invalid_verification".to_string(),
                                detail: message.clone(),
                            });
                            (self.observer)(AgentEvent::ToolResult {
                                exit_code: None,
                                stop: None,
                                execution_status: None,
                                id: call.id.as_str().to_string(),
                                name: UPDATE_GOAL_TOOL.to_string(),
                                is_error: true,
                                preview: goal_refusal_preview("invalid_verification", &message),
                                applied_diff: None,
                            });
                            results[index] = Some(ContentPart::ToolResult {
                                result: ToolResultContent {
                                    call_id: call.id,
                                    content: message,
                                    is_error: true,
                                },
                            });
                            continue;
                        }
                    };
                    let modified = !self.progress.cumulative_modified_paths.is_empty()
                        || !self.modified_files.is_empty();
                    match super::goal_verification::decide(
                        &claim,
                        &self.verification_runs,
                        &self.pending_commands,
                        self.ledger.total_mutation_ops,
                        modified,
                    ) {
                        super::goal_verification::VerificationDecision::Accept { warning } => {
                            if let Some(warning) = warning {
                                self.completion_warnings.push(warning);
                            }
                            self.repeated_gate_refusal = None;
                        }
                        super::goal_verification::VerificationDecision::Refuse(message) => {
                            verification_refusals_this_round += 1;
                            note_gate_refusal(&mut self.repeated_gate_refusal, &message);
                            (self.observer)(AgentEvent::GoalIntercepted {
                                kind: "verification_refused".to_string(),
                                detail: message.clone(),
                            });
                            (self.observer)(AgentEvent::ToolResult {
                                exit_code: None,
                                stop: None,
                                execution_status: None,
                                id: call.id.as_str().to_string(),
                                name: UPDATE_GOAL_TOOL.to_string(),
                                is_error: true,
                                preview: goal_refusal_preview("verification_refused", &message),
                                applied_diff: None,
                            });
                            results[index] = Some(ContentPart::ToolResult {
                                result: ToolResultContent {
                                    call_id: call.id,
                                    content: message,
                                    is_error: true,
                                },
                            });
                            continue;
                        }
                    }
                    // Acceptance scope. The user's explicitly enumerated
                    // deliverables are the authority; the durable goal row
                    // (objective plus user-authored amendments) is read HERE,
                    // so neither a resumed transcript nor a compaction fold
                    // can shrink the scope. Every requirement must be
                    // accounted for, and the claims the runtime can check
                    // mechanically are checked against the execution record.
                    let scope = match self.executor.goal_binder() {
                        Some(binder) => binder.scope().await?,
                        None => None,
                    };
                    if let Some(scope) = scope {
                        let requirements = goal_scope::extract(&scope.objective, &scope.amendments);
                        if !requirements.is_empty() {
                            // The accounting cap follows the requirement set: a long
                            // but legitimate task must be able to account for every
                            // requirement it has. The floor keeps a short goal from
                            // accepting an unbounded array.
                            let accounting = match goal_scope::parse_accounting(
                                &call.arguments,
                                requirements.len(),
                            ) {
                                Ok(accounting) => accounting,
                                Err(message) => {
                                    denied_calls_this_round += 1;
                                    (self.observer)(AgentEvent::GoalIntercepted {
                                        kind: "invalid_requirements".to_string(),
                                        detail: message.clone(),
                                    });
                                    (self.observer)(AgentEvent::ToolResult {
                                        exit_code: None,
                                        stop: None,
                                        execution_status: None,
                                        id: call.id.as_str().to_string(),
                                        name: UPDATE_GOAL_TOOL.to_string(),
                                        is_error: true,
                                        preview: goal_refusal_preview(
                                            "invalid_requirements",
                                            &message,
                                        ),
                                        applied_diff: None,
                                    });
                                    results[index] = Some(ContentPart::ToolResult {
                                        result: ToolResultContent {
                                            call_id: call.id,
                                            content: message,
                                            is_error: true,
                                        },
                                    });
                                    continue;
                                }
                            };
                            let workspace =
                                self.executor.tool_context.execution.workspace.as_deref();
                            let recorded =
                                epoch_modified_paths(&self.progress, &self.modified_files);
                            let artifact = |path: &str| artifact_state(workspace, path, &recorded);
                            match goal_scope::decide(&goal_scope::ScopeFacts {
                                requirements: &requirements,
                                accounting: accounting.as_deref(),
                                runs: &self.verification_runs,
                                mutation_ops: self.ledger.total_mutation_ops,
                                artifact: &artifact,
                            }) {
                                goal_scope::ScopeDecision::Accept { warnings } => {
                                    self.completion_warnings.extend(warnings);
                                    self.repeated_gate_refusal = None;
                                    self.requirements_refusal = None;
                                }
                                goal_scope::ScopeDecision::Refuse(message) => {
                                    denied_calls_this_round += 1;
                                    // A DIFFERENT verdict after an earlier one is
                                    // the model answering the feedback. The first
                                    // refusal of a drive has nothing to answer yet,
                                    // so it stays an ordinary refused round.
                                    let advanced = self
                                        .requirements_refusal
                                        .as_deref()
                                        .is_some_and(|previous| previous != message);
                                    self.requirements_refusal = Some(message.clone());
                                    note_gate_refusal(&mut self.repeated_gate_refusal, &message);
                                    gate_verdict_advanced_this_round =
                                        gate_verdict_advanced_this_round || advanced;
                                    (self.observer)(AgentEvent::GoalIntercepted {
                                        kind: "requirements_refused".to_string(),
                                        detail: message.clone(),
                                    });
                                    (self.observer)(AgentEvent::ToolResult {
                                        exit_code: None,
                                        stop: None,
                                        execution_status: None,
                                        id: call.id.as_str().to_string(),
                                        name: UPDATE_GOAL_TOOL.to_string(),
                                        is_error: true,
                                        preview: goal_refusal_preview(
                                            "requirements_refused",
                                            &message,
                                        ),
                                        applied_diff: None,
                                    });
                                    results[index] = Some(ContentPart::ToolResult {
                                        result: ToolResultContent {
                                            call_id: call.id,
                                            content: message,
                                            is_error: true,
                                        },
                                    });
                                    continue;
                                }
                            }
                        }
                    }
                }
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
                // An accepted completion is not "nothing to say": whatever the
                // runtime could not verify mechanically is a boundary of THIS
                // completion, and the model writing the delivery is the one
                // party that must not present it as verified.
                // An accepted completion is not "nothing to say": whatever the
                // runtime could not verify mechanically is a boundary of THIS
                // completion, and the model writing the delivery is the one
                // party that must not present it as verified.
                let receipt = if reason == StopReason::Blocked {
                    crate::injected_tools::completion_receipt("Goal blocked", &[])
                } else {
                    crate::injected_tools::completion_receipt(
                        "Goal completed",
                        &self.completion_warnings,
                    )
                };
                (self.observer)(AgentEvent::ToolResult {
                    exit_code: None,
                    stop: None,
                    execution_status: None,
                    id: call.id.as_str().to_string(),
                    name: UPDATE_GOAL_TOOL.to_string(),
                    is_error: false,
                    preview: receipt.to_string(),
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
                        content: receipt.to_string(),
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
                    model_step: Some(model_steps),
                });
                let answer = self.executor.handle_ask_user(&call, &cancellation).await?;
                (self.observer)(AgentEvent::ToolResult {
                    exit_code: None,
                    stop: None,
                    execution_status: None,
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
                    model_step: Some(model_steps),
                });
                (self.observer)(AgentEvent::ToolResult {
                    exit_code: None,
                    stop: None,
                    execution_status: None,
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
            // several spawns in one round run in parallel. The call itself is
            // announced FIRST: a delegation that only ever emitted its result
            // left no durable record of what was asked for, so a refused spawn
            // reached clients as a synthesized block with no arguments, and an
            // audit keyed on tool starts could see a child with no request
            // behind it. Same rule the ownership claim already follows.
            if call.name == SPAWN_AGENT_TOOL {
                (self.observer)(AgentEvent::ToolCall {
                    id: call.id.as_str().to_string(),
                    name: SPAWN_AGENT_TOOL.to_string(),
                    arguments: compact_json(&call.arguments),
                    parallel: false,
                    model_step: Some(model_steps),
                });
                spawn_jobs.push((index, call));
                continue;
            }

            // Late-bound ownership: a child claims exclusive write scope
            // against the shared registry. Atomic; a denial is an honest
            // coordination result (is_error=false) the child works around.
            if call.name == CLAIM_WRITE_SCOPE_TOOL {
                let _ownership_gate = self
                    .executor
                    .tool_context
                    .execution
                    .command_gate
                    .clone()
                    .lock_owned()
                    .await;
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
                    model_step: Some(model_steps),
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
                } else if !self
                    .executor
                    .background_write_conflicts(&paths)
                    .await
                    .is_empty()
                {
                    (
                        "not granted — a live background command still holds this write scope"
                            .to_string(),
                        false,
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
                    execution_status: None,
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
                if !self.executor.tool_context.policy.unrestricted_execution()
                    && self.progress.covers_denied_request(
                        requested.network,
                        requested.repository_git || requested.unrestricted_fs,
                    )
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
            // directly. Load it NOW (so this round's control context carries
            // it) and refuse only the calls whose own target paths need it;
            // an unrelated path in the same batch is not the punished one.
            if self.executor.registry.mutates_files(&call.name) {
                let mut call_paths = Vec::new();
                collect_scoped_paths_from_call(&call, &mut call_paths);
                let applicable = self.applicable_scoped_rules(&call_paths);
                let unread = applicable
                    .iter()
                    .filter(|rule| !self.has_scoped_rule(&rule.source))
                    .cloned()
                    .collect::<Vec<_>>();
                // Already injected earlier in this same round: the request
                // the model is answering left before that injection, so this
                // call has not read it either.
                let staged_here = applicable
                    .iter()
                    .any(|rule| self.staged_scoped_rules.contains(&rule.source));
                if !unread.is_empty() || staged_here {
                    for path in &call_paths {
                        push_unique_path(&mut self.scoped_paths, path);
                    }
                    for source in self.inject_scoped_rules(&unread) {
                        self.staged_scoped_rules.insert(source);
                    }
                    let sources = applicable
                        .iter()
                        .map(|rule| rule.source.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    let msg = format!(
                        "Edit paused before execution: project rules were discovered for \
                             this path ({sources}). They are part of the system rules from \
                             the next model step; review them, then retry the edit."
                    );
                    denied_calls_this_round += 1;
                    results[index] = Some(deny_call(&mut *self.observer, call, msg, model_steps));
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
                results[index] = Some(deny_call(&mut *self.observer, call, msg, model_steps));
                continue;
            }
            // Keep the claim/mutation race synchronized when no foreign claim
            // exists. Conflicts are approval reasons at the sole ToolHost
            // admission boundary, never a second refusal in the drive loop.
            let owner_key = self.executor.agent_id.as_deref().unwrap_or("parent");
            let _mutation_guard = if !self.executor.tool_context.policy.unrestricted_execution()
                && self.executor.depth == 0
                && self.executor.registry.mutates_files(&call.name)
            {
                self.executor
                    .ownership
                    .try_mutation_guard(owner_key, &crate::authorization::mutation_targets(&call))
                    .ok()
            } else {
                None
            };

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
                model_step: Some(model_steps),
            });
            collect_scoped_paths_from_call(&call, &mut self.scoped_paths);

            // A command may carry its own elevation for THIS call. Settling
            // it here — between the call event and the context — is what
            // makes approval and execution one round trip instead of two.
            let mut call_grants = TurnPermissionGrants::default();
            if !self.executor.tool_context.policy.unrestricted_execution()
                && is_escalatable_tool(&call.name)
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
            // The model-facing size of THIS call's result, from the room the
            // next request actually has rather than the static model ceiling.
            // Admission freezes the policy, and dispatch executes under it, so
            // the budget must be set here — before admission, not after.
            let ctx = ctx.with_tool_output_budget(tool_result_budget);

            // Admission: the ToolHost pipeline (side-effect barrier →
            // hooks → rules → policy → approval → barrier). Execution
            // requires the AdmittedCall it returns; `parallel` (computed
            // above) decides deferral to the batch — every other admitted
            // call runs here, in order.
            // A non-conflicting mutation guard remains held through dispatch;
            // ownership conflicts have already been resolved by host admission.
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
                    if self.executor.registry.runs_command(&admitted.call.name) {
                        self.commands_run += 1;
                    }
                    parallel_jobs.push(ParallelJob { index, admitted });
                    continue;
                }
                Ok(admitted) => {
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
                                let (tx, rx) = tokio::sync::mpsc::channel(64);
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
                        if result.8.stop.is_some()
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
                Err(AdmitError::Fatal(error)) => {
                    // A cancellation observed while this call waited for
                    // admission must not drop the calls that already ran: route
                    // it through the batch epilogue, which pairs the remaining
                    // results and flushes the completed command's spend before
                    // surfacing `Cancelled`.
                    if matches!(error, AgentError::Cancelled) {
                        cancelled_mid_batch = true;
                        break;
                    }
                    return Err(error);
                }
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
                        super::dispatch::CommandFacts::default(),
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
            // A test command that exits 0 while executing no tests is not
            // evidence of a passing suite. Record it as a failed verification
            // — the existing completion contract and a resumed turn already
            // understand that fact, so no new state is introduced.
            if self.executor.goal_mode()
                && !is_error
                && let Some(label) =
                    super::goal_verification::command_label(&call.name, &call.arguments)
                && super::goal_verification::is_verification_command(&label)
                && super::goal_verification::reports_no_tests(&content)
            {
                content.push_str(&format!(
                    "\n\n[verification] `{label}` exited 0 but executed no tests. \
                     A skipped or empty suite does not verify the objective; run the \
                     real tests, or call update_goal(status=\"blocked\") if they cannot \
                     run."
                ));
                is_error = true;
            }
            if let Some(steps) = plan
                && !is_error
            {
                match PlanState::from_model_explicit(steps) {
                    Ok(next) => {
                        // The latest declaration is the entire current plan.
                        // Earlier declarations already live in the event log;
                        // rewording or omission does not declare abandonment.
                        self.plan_state = next;
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

            // The start of a background task is a durable fact in its own
            // right: the registry that hosts it is process state and dies with
            // a crash, so this row is what lets a later snapshot say the task
            // existed, which call started it and what it ran. It says nothing
            // about the outcome — that is the task's own terminal record.
            if !is_error && let Some(start) = command_facts.started_background.as_ref() {
                (self.observer)(AgentEvent::BackgroundTaskStarted {
                    call_id: call.id.as_str().to_string(),
                    task_id: start.task_id.clone(),
                    program: start.program.clone(),
                    args: start.args.clone(),
                    lifetime: start.lifetime.clone(),
                });
            }

            (self.observer)(AgentEvent::ToolResult {
                exit_code: command_facts.exit_code,
                stop: command_facts.stop,
                execution_status: command_facts.execution_status,
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
                let touched = workspace_mutations(
                    self.executor.tool_context.execution.workspace.as_deref(),
                    touched,
                );
                if !touched.is_empty() {
                    note_tool_side_effects(
                        &mut self.ledger,
                        call.id.as_str(),
                        call.name.as_str(),
                        touched,
                        &self.plan_state,
                        &mut *self.observer,
                    );
                }
            }
            for path in &self.modified_files {
                push_unique_path(&mut self.scoped_paths, path);
            }

            // Mechanical verification fact. Recorded after the call's own
            // mutation note, so "passed after the last change" compares the
            // right sequence number.
            //
            // A command that only STARTED (a background task, or a foreground
            // run handed to the background) reported no execution status: its
            // result does not exist yet. Recording it as a pass would accept a
            // goal on a suite still running, and recording it as a failure
            // would block one that later passes. So a start is not a result and
            // is not evidence either way. A flagged failure stays a failed run:
            // the command did not pass, whatever stopped it before it ran.
            if let Some(label) =
                super::goal_verification::command_label(&call.name, &call.arguments)
            {
                if command_facts.execution_status.is_some() || is_error {
                    self.verification_runs
                        .push(super::goal_verification::VerificationRun {
                            label: label.clone(),
                            ok: !is_error,
                            model_step: model_steps,
                            mutation_ops: self.ledger.total_mutation_ops,
                        });
                    if self.verification_runs.len() > 64 {
                        self.verification_runs.remove(0);
                    }
                    // The command now HAS a result; it is no longer pending.
                    self.pending_commands
                        .retain(|pending| !pending.label.eq_ignore_ascii_case(&label));
                } else {
                    // A start is not evidence, but it is a fact: the completion
                    // gate must not call this command "never ran" while it is
                    // running, and must be able to name the task to wait on.
                    self.pending_commands
                        .push(super::goal_verification::PendingCommand {
                            label,
                            task_id: command_facts.started_task.clone(),
                        });
                    if self.pending_commands.len() > MAX_PENDING_COMMANDS {
                        self.pending_commands.remove(0);
                    }
                }
            }
            if let Some(task_id) = command_facts.terminal_task_id.as_ref() {
                observed_terminal_tasks.push(task_id.clone());
            }

            // The REAL result of a command whose call only reported a start:
            // `wait_task`/`get_task` observed the task's terminal state, and the
            // lifecycle tools attached it as a structured fact. Without this a
            // test handed to the background after 15 seconds could pass and
            // still leave `update_goal(complete)` with no evidence to cite.
            //
            // Observed twice is one result, not two runs: the same task's
            // repeated terminal reads collapse into the run it already added.
            if let Some(result) = command_facts.background_result.as_ref()
                && let Some(label) = super::goal_verification::background_command_label(
                    &result.program,
                    &result.args,
                )
            {
                // The task produced its result: it is no longer a pending
                // start, whatever label the process reports.
                self.pending_commands
                    .retain(|pending| pending.task_id.as_deref() != Some(result.task_id.as_str()));
                let duplicate = self
                    .verification_runs
                    .last()
                    .is_some_and(|last| last.label == label && last.ok == result.ok);
                if !duplicate {
                    self.verification_runs
                        .push(super::goal_verification::VerificationRun {
                            label,
                            ok: result.ok,
                            model_step: model_steps,
                            mutation_ops: self.ledger.total_mutation_ops,
                        });
                    if self.verification_runs.len() > 64 {
                        self.verification_runs.remove(0);
                    }
                }
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
                    let touched = workspace_mutations(
                        executor.tool_context.execution.workspace.as_deref(),
                        job_files,
                    );
                    if !touched.is_empty() {
                        note_tool_side_effects(
                            &mut self.ledger,
                            job.admitted.call.id.as_str(),
                            job.admitted.call.name.as_str(),
                            touched,
                            &self.plan_state,
                            &mut *self.observer,
                        );
                    }
                }
                if let Some(part) = extract_image(&metadata) {
                    pending_images.push(part);
                }
                let facts = super::dispatch::extract_command_facts(&metadata);
                if let Some(task_id) = facts.terminal_task_id.as_ref() {
                    observed_terminal_tasks.push(task_id.clone());
                }
                (self.observer)(AgentEvent::ToolResult {
                    exit_code: facts.exit_code,
                    stop: facts.stop,
                    execution_status: facts.execution_status,
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
                let _ownership_gate = self
                    .executor
                    .tool_context
                    .execution
                    .command_gate
                    .clone()
                    .lock_owned()
                    .await;
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
                } else if !self
                    .executor
                    .background_write_conflicts(&files)
                    .await
                    .is_empty()
                {
                    Some("Worker scope overlaps a live background command; settle that command before transferring ownership.".to_string())
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
                        execution_status: None,
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
                    denied_calls_this_round += 1;
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
                        self.executor.release_child_scope(&id).await?;
                        (self.observer)(AgentEvent::ToolResult {
                            exit_code: None,
                            stop: None,
                            execution_status: None,
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
                        denied_calls_this_round += 1;
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
                             background.{scope_line} The runtime reports when it settles.",
                        role.label()
                    );
                    // The immediate acknowledgment is model-visible, so it
                    // must be UI-visible too (P2 disclosure): pair the
                    // spawn call with its result like any other tool.
                    (self.observer)(AgentEvent::ToolResult {
                        exit_code: None,
                        stop: None,
                        execution_status: None,
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
                        self.executor.release_child_scope(&id).await?;
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
                        let content = leveler_tools::registry::cap_output_with(
                            &content,
                            tool_result_budget,
                        );
                        // The call that asked for this child settles NOW. A
                        // foreground spawn waited for exactly this result, so a
                        // row left `running` would keep a "waiting for a child"
                        // indicator alive after the child ended.
                        (self.observer)(AgentEvent::ToolResult {
                            exit_code: None,
                            stop: None,
                            execution_status: None,
                            id: call_id.as_str().to_string(),
                            name: SPAWN_AGENT_TOOL.to_string(),
                            is_error: !ok,
                            preview: preview(&content),
                            applied_diff: None,
                        });
                        results[index] = Some(ContentPart::ToolResult {
                            result: ToolResultContent {
                                // Sub-agent results bypass the registry, so the
                                // cap above is this call's own bound too.
                                call_id,
                                content,
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
                        model_steps,
                    ));
                }
            }
        }

        let results: Vec<ContentPart> = results
            .into_iter()
            .map(|r| r.expect("every tool call produced a result"))
            .collect();

        let tool_message = Message {
            origin: (!observed_terminal_tasks.is_empty()).then(|| {
                TranscriptOrigin::BackgroundTaskResults {
                    task_ids: observed_terminal_tasks.clone(),
                }
            }),
            role: Role::Tool,
            content: results,
            route: None,
        };
        messages.push(tool_message.clone());
        // Flush spend BEFORE transcript persistence: tools already ran, so
        // a sink I/O failure must not drop this batch's command/file totals.
        self.flush_epoch(rt);
        self.sink.append(&[assistant, tool_message]).await?;
        if let Some(delivered) = self.delivered_background_tasks.as_mut() {
            delivered.extend(observed_terminal_tasks);
        }
        // Batch was cancelled: the round is durable (results paired, spend
        // flushed above) — exit now instead of starting another model round.
        if cancelled_mid_batch {
            self.drain_background_children(rt, messages).await?;
            return Err(AgentError::Cancelled);
        }
        // A refused goal resolution asked for its children's settlements. They
        // are appended only now, after this round's tool results, so the
        // assistant message that carried the call keeps its paired result.
        if drain_before_next_round {
            self.drain_background_children(rt, messages).await?;
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
        // a safety boundary is not an experiment. It applies at every depth:
        // a delegated child re-issuing one refusal spends its whole step
        // ceiling and makes its parent wait for it.
        //
        // The classification is "did any call run", not "which refusal owns
        // the round". Counting a mixed round as progress reset both streaks,
        // so a model that put a citation refusal beside a denied call could
        // re-issue them until its budget ran out. A citation refusal is
        // feedback the model can act on, and it has its own bound, so a round
        // that carried one neither advances nor clears the denial streak; the
        // recovery count is what bounds it. A round whose refusals were all
        // pre-execution denials keeps the denial streak's stop.
        let all_refused = !call_snapshot.is_empty()
            && denied_calls_this_round.saturating_add(verification_refusals_this_round)
                == call_snapshot.len();
        let citation_refused = verification_refusals_this_round > 0;
        if all_refused {
            // Recoverable gate feedback: a refused citation, or a requirements
            // refusal whose verdict CHANGED (the model acted on the previous
            // one). Both are bounded here; neither is "no progress" while the
            // verdict keeps moving. An unchanged requirements verdict is a
            // repeat and stays on the denial streak.
            if citation_refused || gate_verdict_advanced_this_round {
                if gate_verdict_advanced_this_round {
                    // The gate's verdict moved because the model changed its
                    // answer: that clears the no-progress streak, exactly like
                    // an executed tool would. It is still bounded by the
                    // recovery budget below.
                    self.progress.note_progress(model_steps);
                    (self.observer)(AgentEvent::ProgressUpdated {
                        ledger: self.progress.clone(),
                    });
                }
                self.goal_gate_recovery_rounds = self.goal_gate_recovery_rounds.saturating_add(1);
                // A nested goal drive (a host-built sub-goal) has the same
                // bound; the counter and the stop stay on that drive.
                if self.goal_gate_recovery_rounds >= MAX_VERIFICATION_RECOVERY_ROUNDS {
                    let detail = format!(
                        "the goal gate refused {} times in a row without a corrected \
                         citation, a newly accounted requirement, or a new command",
                        self.goal_gate_recovery_rounds
                    );
                    return Ok(Flow::Stop(
                        self.stop_now(rt, messages, StopReason::Incomplete, &detail, &detail)
                            .await?,
                    ));
                }
            } else {
                self.progress.note_no_progress_round(model_steps);
                (self.observer)(AgentEvent::ProgressUpdated {
                    ledger: self.progress.clone(),
                });
                if self
                    .progress
                    .should_hard_stop_no_progress(self.progress_caps)
                {
                    return Ok(Flow::Stop(
                        self.stop_now(
                            rt,
                            messages,
                            StopReason::Incomplete,
                            "no-progress streak; model steps that only repeated refused calls short-circuited",
                            "Stopped: no progress (every attempted action was refused).",
                        )
                        .await?,
                    ));
                }
            }
        } else if !call_snapshot.is_empty() {
            self.progress.note_progress(model_steps);
            self.goal_gate_recovery_rounds = 0;
        }

        // A gate that returns the SAME verdict while the model keeps doing
        // other work is stuck, not slow. The all-refused bounds above own the
        // case where nothing else ran (they can name the correction); this one
        // owns the case that wasted thirty rounds in DF-06: real tool calls
        // between identical refusals, so no no-progress streak ever tripped.
        if all_refused {
            // Already classified by the bounded recovery above.
        } else if let Some(detail) = stuck_gate_detail(&self.repeated_gate_refusal) {
            return Ok(Flow::Stop(
                self.stop_now(rt, messages, StopReason::Incomplete, &detail, &detail)
                    .await?,
            ));
        }

        // Prose a work tool followed is progress, not the answer. Bookkeeping
        // (`update_goal`, `update_plan`) does not erase an answer the round
        // already wrote.
        let work_executed = call_snapshot
            .iter()
            .any(|call| !is_goal_bookkeeping(&call.name));
        if work_executed {
            self.terminal_answer = None;
        } else if round_spoke {
            self.terminal_answer = Some(round_prose.trim().to_string());
        }

        // Goal mode: an explicit update_goal resolved the goal, and its result
        // is committed. Resolving the goal is not delivering to the person: the
        // turn used to end right here, so a model whose last action was the
        // `update_goal` call (the common shape — the call IS the last action)
        // produced a completed goal with no answer at all. The drive therefore
        // keeps a delivery request that sees any tool results from this round.
        if let Some((reason, summary)) = goal_resolution {
            let already_settled = self.settled_goal.is_some();
            self.settled_goal = Some((reason, summary));
            // A blocked goal still owes the work itself, and its resolution is
            // a report, so its round ends exactly as before.
            if reason != StopReason::Completed {
                return self.finish_settled_goal(rt, messages, model_steps).await;
            }
            // The answer and the resolution landed together, and nothing acted
            // on that prose. That is the delivery.
            if self.terminal_answer.is_some() && !already_settled {
                return self.finish_settled_goal(rt, messages, model_steps).await;
            }
            // Resolving a goal this turn already resolved, without doing any
            // new work, is the model still not answering. It counts against
            // the wrap-up bound so it cannot spin.
            if already_settled && !work_executed {
                self.settled_rounds = self.settled_rounds.saturating_add(1);
                if self.settled_rounds >= MAX_SETTLED_DELIVERY_ROUNDS
                    || self.delivery_followup_issued
                {
                    return self.finish_settled_goal(rt, messages, model_steps).await;
                }
            }
            if !already_settled && self.terminal_answer.is_none() {
                let running: Vec<String> = match self.executor.background_tasks.as_ref() {
                    Some(tasks) => tasks
                        .active_snapshots_for_scope(self.executor.tool_context.session_scope())
                        .await
                        .into_iter()
                        .map(|task| task_line(&task))
                        .collect(),
                    None => Vec::new(),
                };
                self.inject_settled_delivery(
                    messages,
                    model_steps,
                    settled_delivery_notice(reason, &running),
                )
                .await?;
                return Ok(Flow::NextRound);
            }
        }

        // A tool round after the goal settled is wrap-up. Its results are not
        // an answer, so the turn does not end here: the model still has to see
        // them. Past the bound, exactly one follow-up request carries that
        // duty. Spending the follow-up on more tools ends the turn honestly.
        if self.settled_goal.is_some() && work_executed {
            self.settled_rounds = self.settled_rounds.saturating_add(1);
            if self.delivery_followup_issued {
                self.completion_warnings.push(
                    "the goal was resolved and the runtime spent its remaining delivery rounds \
                     waiting for the user-facing answer; the recorded goal summary is what the \
                     user received"
                        .to_string(),
                );
                return self.finish_settled_goal(rt, messages, model_steps).await;
            }
            if self.settled_rounds >= MAX_SETTLED_DELIVERY_ROUNDS {
                self.delivery_followup_issued = true;
                self.inject_settled_delivery(messages, model_steps, delivery_followup_notice())
                    .await?;
                return Ok(Flow::NextRound);
            }
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
            let image_message = Message::from_parts(
                Role::User,
                pending_images,
                Some(TranscriptOrigin::RuntimeNotice {
                    notice: RuntimeNoticeKind::LoadedImage,
                }),
            );
            messages.push(image_message.clone());
            self.sink.append(&[image_message]).await?;
        }

        // Auto-compaction (spec §53): when the context size exceeds the
        // budget, fold the in-memory transcript before the next request so a
        // long task never overflows the window. Prefer the provider's
        // reported token count, but fall back to a char/4 estimate — many
        // gateways don't report streaming usage, and without a fallback
        // compaction would silently never fire. The persisted transcript
        // (sink) is untouched — only what we resend shrinks.
        let context_tokens = self.projected_with_measured_gap(
            leveler_model::RequestProjection::project_with_control_context(
                messages,
                &self.tools,
                self.executor.policy.reasoning_replay,
                self.executor.policy.reasoning_retention,
                &self.request_context(rt),
                self.executor.route_identity().as_ref(),
            )
            .estimated_tokens(),
        );
        // Fold when the last request's estimate crossed the budget. One
        // threshold, one action: the runtime does not read the model's
        // re-reads as evidence that it "deserves" a bigger window.
        let context_policy = self.executor.policy.context_policy;
        let over_budget = context_policy.folding_enabled()
            && context_tokens > u64::from(context_policy.pressure_threshold);
        let requirement = fold_requirement(&context_policy, context_tokens);
        // A soft fold whose briefing just failed is not retried at the same
        // size: the uncompacted history is still legal to send, so the retry
        // would spend a model call to get the same answer. The next attempt is
        // due once the transcript has grown by what that fold would have
        // reclaimed. The hard bound ignores the schedule — there the fold is
        // mandatory.
        let soft_retry_due = requirement == FoldRequirement::HardRequired
            || self
                .soft_fold_retry_at
                .is_none_or(|retry_at| context_tokens >= retry_at);
        if has_next_model_step && over_budget && soft_retry_due {
            let before = messages.len();
            // TWO bounds, two meanings. `pressure_threshold` says recall is
            // expected to degrade; `hard_capacity` says the request can no
            // longer be sent. A fold below the hard bound is a quality choice
            // and may be abandoned; above it, a fold is required to send
            // anything at all, so its failure must be explicit.
            // Cap the retained working set at half the live budget so a
            // huge recent tool output can't keep the fold over the window;
            // the other half leaves room for the head, summary, and next
            // response. (current_budget > 0 is guaranteed by the decision.)
            // The retention budget is a POLICY field, not a fraction of the
            // threshold re-derived here: the two answer different questions.
            // The retention budget is sized from the SAME projected accounting
            // the threshold is measured with: the room left below the threshold
            // after the NON-foldable base (control context, tool schemas, the
            // task anchor, carried rules), halved for growth headroom, minus the
            // summary's own budget. A flat `threshold / 2` ignores that fixed
            // cost, so on a small `reliable_context` profile a fold lands back
            // on the threshold and re-fires every round or two.
            let pressure = u64::from(context_policy.pressure_threshold);
            let summary_budget = leveler_context::COMPACTION_SUMMARY_BUDGET_TOKENS;
            let keep_recent_messages = context_policy.retention.keep_recent_messages;
            let (tail_budget, non_foldable_projected_tokens) = context_policy
                .retention_budget_from_projection(messages, |base| {
                    leveler_model::RequestProjection::project_with_control_context(
                        base,
                        &self.tools,
                        self.executor.policy.reasoning_replay,
                        self.executor.policy.reasoning_retention,
                        &self.request_context(rt),
                        self.executor.route_identity().as_ref(),
                    )
                    .estimated_tokens()
                });
            let keep_recent_tokens = match tail_budget {
                Some(budget) => budget.max(1),
                None => {
                    // The non-foldable base already reaches the threshold, so
                    // this fold cannot reach low water — but it still removes
                    // foldable history, so it RUNS (down to a one-token tail)
                    // rather than skipping and overflowing. Reported so an
                    // operator can see the profile is misdeclared.
                    tracing::warn!(
                        operation = "compaction",
                        pressure_threshold = pressure,
                        non_foldable_tokens = non_foldable_projected_tokens,
                        "compaction has no low-water relief"
                    );
                    1
                }
            };
            tracing::info!(
                operation = "compaction",
                pressure_threshold = pressure,
                projected_before = context_tokens,
                non_foldable_tokens = non_foldable_projected_tokens,
                foldable_tokens = context_tokens.saturating_sub(non_foldable_projected_tokens),
                summary_budget,
                recent_tail_budget = keep_recent_tokens,
                "context fold sized from the projected request"
            );
            // Name this extra round trip so the UI shows "compacting…" instead
            // of a bare "waiting for model" during the summary call.
            (self.observer)(AgentEvent::AdvisoryStarted {
                kind: AdvisoryKind::ContextCompaction,
            });
            // Compaction discards detail by design; this is the only moment
            // a project can export a checkpoint before details are discarded.
            if self.executor.hook_runner.has_lifecycle() {
                self.executor
                    .hook_runner
                    .run_lifecycle(
                        leveler_execution::LifecycleEvent::PreCompact,
                        &format!(
                            r#"{{"context_tokens":{context_tokens},"budget":{}}}"#,
                            context_policy.pressure_threshold
                        ),
                        &cancellation,
                    )
                    .await;
            }
            let runtime = self.executor.runtime.clone();
            let request = leveler_context::summary_request(
                runtime.as_ref(),
                &self.executor.model,
                crate::ModelCallKind::Compaction.default_reasoning_effort(),
                messages,
                keep_recent_messages,
                keep_recent_tokens,
                self.executor.max_output_tokens,
            )
            .await;
            // A briefing is advisory. Whether its absence is fatal depends on
            // WHY the fold was attempted, not on the error: over the quality
            // boundary the uncompacted history is still legal to send, so the
            // task continues; over the hard capacity it is not.
            let mut summary: Option<String> = None;
            let mut summary_failure: Option<&'static str> = None;
            if let Some(request) = request {
                if !super::auxiliary_budget_available(
                    self.executor.step_limits,
                    &self.progress,
                    &request,
                    self.executor.pricing.as_ref(),
                ) {
                    summary_failure = Some("no_budget");
                } else {
                    let remaining = self.executor.step_limits.max_duration.map(|duration| {
                        duration.saturating_sub(
                            self.epoch_duration_at_start
                                .saturating_add(rt.run_started().elapsed()),
                        )
                    });
                    let reasoning_effort = request.reasoning_effort;
                    match super::run_auxiliary_round(
                        runtime.as_ref(),
                        request.clone(),
                        &cancellation,
                        &mut CompactionObserver {
                            drive: self,
                            rt,
                            reasoning_effort,
                            request: &request,
                        },
                        remaining,
                    )
                    .await
                    {
                        Ok(round) => {
                            summary =
                                leveler_context::accepted_summary(&leveler_model::ModelResponse {
                                    request_id: leveler_core::RequestId::new(round.request_id),
                                    message: round.message,
                                    usage: round.usage,
                                    finish_reason: round.finish_reason,
                                });
                            if summary.is_none() {
                                summary_failure = Some("rejected");
                            }
                        }
                        Err(AgentError::AuxiliaryBudgetUnavailable) => {
                            summary_failure = Some("no_budget");
                        }
                        // The harness cancelled the AUXILIARY child token when
                        // its deadline elapsed; the task's own token is still
                        // live. A real task cancellation is NOT this case and
                        // propagates below.
                        Err(AgentError::Cancelled) if !cancellation.is_cancelled() => {
                            summary_failure = Some("timeout");
                        }
                        Err(error) => {
                            if cancellation.is_cancelled() {
                                return Err(error);
                            }
                            summary_failure = Some(compaction_failure_class(&error));
                        }
                    }
                }
            } else {
                summary_failure = Some("no_request");
            }
            if let Some(failure) = summary_failure {
                log_compaction_summary_failure(
                    requirement,
                    failure,
                    context_tokens,
                    &context_policy,
                    requirement == FoldRequirement::Soft,
                );
                if requirement == FoldRequirement::Soft {
                    // A briefing was never requested: the transcript had no
                    // foldable middle yet (or the route cannot fit a summary),
                    // so no model call was spent. Re-check next round — the
                    // middle appears as the transcript grows — and only put a
                    // briefing that actually failed on the retry schedule.
                    if failure == "no_request" {
                        return Ok(Flow::Continue);
                    }
                    // The uncompacted history is still legal to send. A
                    // briefing nobody could produce must not cost the task —
                    // nor may it cost one model call per round for the rest of
                    // the turn. The next attempt is due once the transcript
                    // has grown by what this fold would have reclaimed without
                    // a briefing, so a retry only happens when there is a
                    // fold's worth of new history to justify it.
                    let retry_at = match tail_budget {
                        Some(_) => {
                            let reclaim = context_tokens.saturating_sub(
                                self.projected_with_measured_gap(
                                    leveler_model::RequestProjection::project_with_control_context(
                                        &compact_messages(
                                            messages,
                                            keep_recent_messages,
                                            keep_recent_tokens,
                                            None,
                                            Some(self.objective.text()),
                                        ),
                                        &self.tools,
                                        self.executor.policy.reasoning_replay,
                                        self.executor.policy.reasoning_retention,
                                        &self.request_context(rt),
                                        self.executor.route_identity().as_ref(),
                                    )
                                    .estimated_tokens(),
                                ),
                            );
                            context_tokens.saturating_add(reclaim.max(1))
                        }
                        // The fold cannot reach the quality boundary even after
                        // eliding everything it can: a soft retry at this size
                        // has nothing new to act on. Wait for the hard bound,
                        // where the fold is mandatory and retries regardless;
                        // with no declared capacity nothing can make it
                        // mandatory, so the briefing is not retried this turn.
                        None => context_policy
                            .hard_capacity()
                            .map_or(u64::MAX, |capacity| capacity.saturating_add(1)),
                    };
                    self.soft_fold_retry_at = Some(retry_at);
                    return Ok(Flow::Continue);
                }
                // Hard-required: fall through and fold MECHANICALLY (no model
                // briefing). The capacity check below proves the result is
                // sendable before the fold is committed.
            }
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
            // Build the fold as a CANDIDATE. The live transcript, the
            // divergence flag, and the fold record stay untouched until the
            // capacity gate below has proven the candidate sendable: a fold
            // that cannot fit must leave no state behind, so the failure path
            // is validate-before-commit, never mutate-then-fail. (This is the
            // same ordering `assemble_measured` uses for the engine entry.)
            let mut candidate = fold_permitted.then(|| {
                compact_messages(
                    messages,
                    keep_recent_messages,
                    keep_recent_tokens,
                    summary.as_deref(),
                    Some(self.objective.text()),
                )
            });
            // Record the fold as estimated tokens so the accounting can show
            // `before → after → reclaimed` without the TUI deriving it from a
            // second transcript. With no candidate this measures the unchanged
            // history, which is what the gate must judge when the checkpoint
            // port refused the fold.
            let mut after_tokens = self.projected_with_measured_gap(
                leveler_model::RequestProjection::project_with_control_context(
                    candidate.as_deref().unwrap_or(&messages[..]),
                    &self.tools,
                    self.executor.policy.reasoning_replay,
                    self.executor.policy.reasoning_retention,
                    &self.request_context(rt),
                    self.executor.route_identity().as_ref(),
                )
                .estimated_tokens(),
            );
            // Deterministic recovery. A short transcript can be over the hard
            // capacity while NOTHING is foldable: T8 had five messages, one of
            // them a single huge tool result, so `compaction_span` found no
            // middle and the mechanical fold was a no-op. An offloaded result
            // names the path to its full content, so the oldest previews can be
            // reduced to their references without losing anything recoverable.
            // Message count, roles and call ids stay untouched, so the tool
            // exchange stays paired. The gate below still decides whether the
            // reduced transcript is sendable.
            if requirement == FoldRequirement::HardRequired
                && let Some(capacity) = context_policy.hard_capacity()
                && after_tokens > capacity
                && let Some(base) = candidate.take()
            {
                let measured = |msgs: &[leveler_model::Message]| {
                    self.projected_with_measured_gap(
                        leveler_model::RequestProjection::project_with_control_context(
                            msgs,
                            &self.tools,
                            self.executor.policy.reasoning_replay,
                            self.executor.policy.reasoning_retention,
                            &self.request_context(rt),
                            self.executor.route_identity().as_ref(),
                        )
                        .estimated_tokens(),
                    )
                };
                let (shrunk, reduced) =
                    leveler_context::shrink_externalized_results(&base, capacity, measured);
                if reduced > 0 {
                    after_tokens = measured(&shrunk);
                    tracing::info!(
                        operation = "compaction",
                        reduced_results = reduced,
                        projected_after = after_tokens,
                        capacity,
                        "externalized tool results reduced to their references"
                    );
                    candidate = Some(shrunk);
                } else {
                    candidate = Some(base);
                }
            }
            // Hard-required: a legal context is mandatory. If neither the model
            // briefing nor the mechanical fold brought the request under the
            // model's hard capacity, fail explicitly BEFORE the fold is
            // committed. The same gate covers a checkpoint port that refused
            // the fold: then the candidate is the unchanged context and it is
            // still over capacity.
            if requirement == FoldRequirement::HardRequired
                && let Some(capacity) = context_policy.hard_capacity()
                && after_tokens > capacity
            {
                return Err(AgentError::ContextManagementFailure(format!(
                    "compaction could not fit the request into the model's hard context \
                     capacity: projected {after_tokens} tokens, capacity {capacity}"
                )));
            }
            // The candidate cleared the gate (or no gate applied): commit it.
            if let Some(folded) = candidate {
                *messages = folded;
                self.context_diverged = true;
                // The fold cycle restarts from the folded size.
                self.soft_fold_retry_at = None;
                if let Ok(mut slot) = self.compaction.lock() {
                    *slot = Some(CompactionRecord {
                        before_tokens: context_tokens,
                        after_tokens,
                    });
                }
                tracing::info!(
                    operation = "compaction",
                    pressure_threshold = pressure,
                    projected_before = context_tokens,
                    non_foldable_tokens = non_foldable_projected_tokens,
                    foldable_tokens = context_tokens.saturating_sub(non_foldable_projected_tokens),
                    summary_budget,
                    recent_tail_budget = keep_recent_tokens,
                    projected_after = after_tokens,
                    tokens_freed = context_tokens.saturating_sub(after_tokens),
                    "context fold committed"
                );
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
            // A reported fold means the message count really shrank (the notice
            // the user reads spells counts). A fold that elides an exchange but
            // keeps the count — the retained tail keeps the newest exchange
            // whole, and the pin plus breadcrumb can cost exactly what the
            // elided rounds did — is NOT reported here. Its durable record is
            // the folded `ContextSnapshot`, which is what resume and the RC
            // evidence read.
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
                through_ordinal: self.sink.watermark().await?,
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
        // The goal was already resolved this turn and only the delivery round is
        // missing. A mechanical limit firing now must not replace a resolved
        // goal with "limit reached": publish the settled terminal, which carries
        // the facts (and the warning that no answer was written) instead.
        if self.settled_goal.is_some() && !matches!(stop.reason, KernelStop::Cancelled) {
            match self
                .finish_settled_goal(rt, &mut messages, model_steps)
                .await?
            {
                Flow::Stop(outcome) => return Ok(outcome),
                Flow::Continue | Flow::NextRound => {
                    return Err(AgentError::ContextManagementFailure(
                        "settled goal terminal did not publish".to_string(),
                    ));
                }
            }
        }
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
pub(crate) fn residual_step_limits(
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

/// The capability-discovery fact for one request's session prefix.
///
/// Two shapes, because they answer different questions. With nothing on offer
/// it is the old sentence: a catalog exists and can be listed. With something
/// on offer it is an inventory — which packs, and how many tools each would
/// add — because a model cannot enumerate an unexposed pack's tools from the
/// tool table it was given, and "some capabilities exist" does not tell it that
/// the surface it was asked about is reachable through the harness at all.
///
/// Neither shape names a task, a repository or a server: the pack ids and the
/// counts are host facts.
fn capability_discovery_fact(inventory: &[(crate::capability::CapabilityId, usize)]) -> String {
    const NO_INVENTORY: &str = "Additional capabilities are available on demand. \
         Use capability(action=\"list\") to discover them, then \
         capability(action=\"enable\", id=...) when needed. Enabled tools appear on the \
         following request.";
    if inventory.is_empty() {
        return NO_INVENTORY.to_string();
    }
    let mut text = String::from(
        "Optional capability packs are on offer in this session. Enabling one adds its tools to \
         your surface on the following request:\n",
    );
    for (id, count) in inventory {
        let noun = if *count == 1 { "tool" } else { "tools" };
        text.push_str(&format!("- {} ({count} {noun})\n", id.as_str()));
    }
    text.push_str(
        "capability(action=\"list\") says what each pack is and why one is not on offer; \
         capability(action=\"enable\", id=<pack>) activates one when it is needed.",
    );
    text
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

/// Whether a user-named artifact exists in this agent's workspace.
///
/// Only workspace-relative paths are inspected. A bare name also resolves
/// through recorded changes: the user often names a file without its
/// directory, and the runtime already observed which paths the task changed.
/// An absolute path, a `..` escape, a missing workspace, or an unresolvable
/// path is `Unknown`; the completion gate then records an honest unverified
/// boundary rather than guessing.
/// Prefix that names the gate which refused an `update_goal`, in the result's
/// UI/JSONL preview.
///
/// The preview is the projection a client renders; the model reads `content`,
/// which keeps its own model-facing prose. A client localizes the refusal from
/// this tag, so the user reads the runtime's own reason instead of a guess,
/// and the model-facing English guidance never becomes the conversation body.
/// Same shape as the `commit_task` scope tag.
pub(crate) const GOAL_REFUSAL_TAG: &str = "[goal_refused:";

/// The preview for a refused `update_goal`: the refusing gate, then the
/// runtime's own account for audit.
fn goal_refusal_preview(kind: &str, message: &str) -> String {
    format!("{GOAL_REFUSAL_TAG}{kind}]\n{}", preview(message))
}

/// Record one goal-gate refusal against the previous one.
fn note_gate_refusal(slot: &mut Option<(String, u32)>, message: &str) {
    let count = match &*slot {
        Some((previous, count)) if previous == message => count.saturating_add(1),
        _ => 1,
    };
    *slot = Some((message.to_string(), count));
}

/// The stuck-gate detail once the same verdict has repeated past
/// [`MAX_IDENTICAL_GATE_REFUSALS`], else `None`.
fn stuck_gate_detail(slot: &Option<(String, u32)>) -> Option<String> {
    let (message, count) = slot.as_ref()?;
    (*count >= MAX_IDENTICAL_GATE_REFUSALS).then(|| {
        let reason = message.lines().nth(1).unwrap_or(message).trim();
        format!(
            "the goal gate returned the same refusal {count} times; the facts it reads did \
             not change, so retrying cannot satisfy it ({reason})"
        )
    })
}

/// The exact prompt text one scoped rule contributes.
///
/// This string IS the identity used to reconcile a block with the file: equal
/// bytes means the same rule is already delivered, different bytes means the
/// file changed and its block must be replaced. No separate digest table can
/// disagree with what the model was given.
fn scoped_rule_body(rule: &ProjectInstruction) -> String {
    format!(
        "Project rules:\n{}",
        render_instructions(std::slice::from_ref(rule))
    )
}

/// Keep mutation evidence about the task tree.
///
/// A model may keep scratch evidence under an absolute path outside the
/// workspace. That write is real, but it changes no file a task's tests or
/// build read, so counting it would make a fresh verification pass look
/// stale. Without a workspace there is nothing to compare against, so every
/// reported path is kept.
fn workspace_mutations(
    workspace: Option<&leveler_execution::Workspace>,
    paths: Vec<String>,
) -> Vec<String> {
    let Some(workspace) = workspace else {
        return paths;
    };
    let root = workspace.root();
    paths
        .into_iter()
        .filter(|path| path_is_within(root, path))
        .collect()
}

fn path_is_within(root: &std::path::Path, path: &str) -> bool {
    let raw = std::path::Path::new(path);
    let candidate = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        root.join(raw)
    };
    let candidate = lexical_clean(&candidate);
    if candidate.starts_with(root) {
        return true;
    }
    // The same directory can have more than one spelling (`/tmp` and
    // `/private/tmp` on macOS). Compare real paths when the file still
    // exists.
    match (candidate.canonicalize(), root.canonicalize()) {
        (Ok(candidate), Ok(root)) => candidate.starts_with(root),
        _ => false,
    }
}

fn lexical_clean(path: &std::path::Path) -> std::path::PathBuf {
    use std::path::Component;

    let mut cleaned = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                cleaned.pop();
            }
            other => cleaned.push(other.as_os_str()),
        }
    }
    cleaned
}

fn artifact_state(
    workspace: Option<&leveler_execution::Workspace>,
    path: &str,
    recorded_paths: &[String],
) -> goal_scope::ArtifactState {
    use goal_scope::ArtifactState;
    let path = path.trim().trim_start_matches("./");
    if path.is_empty()
        || path.starts_with('/')
        || path.starts_with('~')
        || path.starts_with('\\')
        || path.split(['/', '\\']).any(|part| part == "..")
    {
        return ArtifactState::Unknown;
    }
    let Some(workspace) = workspace else {
        return ArtifactState::Unknown;
    };
    match workspace.resolve_for_read(path) {
        Ok(resolved) if resolved.exists() => return ArtifactState::Present,
        Ok(_) => {}
        Err(_) => return ArtifactState::Unknown,
    }
    // A bare file name in the user's prose is shorthand for the file in its
    // real directory (`aggregate.go` for `internal/report/aggregate.go`). A
    // recorded change with that basename is evidence the named deliverable
    // exists; demanding the shorthand exist at the workspace root refuses
    // correct work.
    if !path.contains(['/', '\\']) {
        let suffix = format!("/{path}");
        if recorded_paths.iter().any(|recorded| {
            let recorded = recorded.trim().trim_start_matches("./");
            recorded == path || recorded.ends_with(&suffix)
        }) {
            return ArtifactState::Present;
        }
    }
    ArtifactState::Absent
}

#[cfg(test)]
mod artifact_scope_tests {
    use super::{artifact_state, goal_scope};

    /// A bare file name in the user's prose is shorthand for the file in its
    /// real directory (`aggregate.go` for `internal/report/aggregate.go`).
    /// A recorded change with that basename is evidence the named deliverable
    /// exists; demanding it at the workspace root rejects correct work.
    #[test]
    fn a_bare_name_is_present_when_a_recorded_change_has_that_basename() {
        let dir = tempfile::tempdir().expect("tempdir");
        let nested = dir.path().join("internal/report");
        std::fs::create_dir_all(&nested).expect("nested dir");
        std::fs::write(nested.join("aggregate.go"), "package report\n").expect("file");
        let workspace = leveler_execution::Workspace::new(dir.path()).expect("workspace");
        let recorded = vec!["internal/report/aggregate.go".to_string()];

        assert_eq!(
            artifact_state(Some(&workspace), "aggregate.go", &recorded),
            goal_scope::ArtifactState::Present
        );
    }

    #[test]
    fn an_unrecorded_bare_name_absent_from_the_root_stays_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = leveler_execution::Workspace::new(dir.path()).expect("workspace");

        assert_eq!(
            artifact_state(Some(&workspace), "done.txt", &[]),
            goal_scope::ArtifactState::Absent
        );
    }

    #[test]
    fn a_root_artifact_does_not_need_a_recorded_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("done.txt"), "RESUME_OK\n").expect("file");
        let workspace = leveler_execution::Workspace::new(dir.path()).expect("workspace");

        assert_eq!(
            artifact_state(Some(&workspace), "done.txt", &[]),
            goal_scope::ArtifactState::Present
        );
    }
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

#[cfg(test)]
mod fold_requirement_tests {
    use super::{FoldRequirement, compaction_failure_class, fold_requirement};
    use crate::AgentError;
    use crate::coding::policy::ResolvedContextPolicy as P;
    use leveler_model::{ModelError, ModelErrorKind};

    /// A model that declares no separate quality boundary: the threshold IS the
    /// capacity, so every fold is required and there is no soft zone to fall
    /// back into.
    fn no_quality_gap() -> P {
        P {
            context_window: 32_768,
            quality_boundary: 0,
            output_reservation: 1_024,
            headroom: 0,
            pressure_threshold: 32_768 - 1_024,
            soft_percent: 100,
            retention: P::default().retention,
        }
    }

    /// A model whose reliable context is well below its usable window: the gap
    /// between the two bounds is the room a failed fold may continue in.
    fn quality_gap() -> P {
        P {
            context_window: 131_072,
            quality_boundary: 65_536,
            output_reservation: 8_192,
            headroom: 0,
            pressure_threshold: 65_536,
            soft_percent: 100,
            retention: P::default().retention,
        }
    }

    #[test]
    fn below_the_hard_capacity_is_quality_pressure() {
        let policy = quality_gap();
        // Over the quality boundary, comfortably under the 122 880 capacity.
        assert_eq!(fold_requirement(&policy, 80_000), FoldRequirement::Soft);
    }

    #[test]
    fn beyond_the_hard_capacity_is_required() {
        let policy = quality_gap();
        assert_eq!(
            fold_requirement(&policy, 130_000),
            FoldRequirement::HardRequired
        );
    }

    /// When the threshold equals the capacity there is no soft zone: crossing
    /// the threshold already means the request cannot be sent.
    #[test]
    fn a_threshold_at_capacity_leaves_no_soft_zone() {
        let policy = no_quality_gap();
        let capacity = policy.hard_capacity().unwrap();
        assert_eq!(
            fold_requirement(&policy, capacity + 1),
            FoldRequirement::HardRequired
        );
    }

    /// With no declared window there is no hard limit to enforce: nothing may
    /// claim a capacity the model never stated.
    #[test]
    fn an_unknown_window_never_claims_a_hard_requirement() {
        let policy = P {
            context_window: 0,
            pressure_threshold: 1_000,
            retention: P::default().retention,
            ..P::default()
        };
        assert_eq!(policy.hard_capacity(), None);
        assert_eq!(fold_requirement(&policy, 10_000_000), FoldRequirement::Soft);
    }

    #[test]
    fn failure_classes_are_stable_and_never_leak_provider_text() {
        assert_eq!(
            compaction_failure_class(&AgentError::Model(ModelError::new(
                ModelErrorKind::Timeout,
                "read timed out at https://internal.example"
            ))),
            "timeout"
        );
        assert_eq!(
            compaction_failure_class(&AgentError::Model(ModelError::new(
                ModelErrorKind::ProviderUnavailable,
                "529 overloaded"
            ))),
            "provider"
        );
        assert_eq!(compaction_failure_class(&AgentError::Cancelled), "timeout");
        assert_eq!(
            compaction_failure_class(&AgentError::Persistence("db down".into())),
            "other"
        );
    }
}

#[cfg(test)]
mod gate_refusal_bound_tests {
    use super::{MAX_IDENTICAL_GATE_REFUSALS, note_gate_refusal, stuck_gate_detail};

    /// A gate that returns the same verdict again and again is not making
    /// progress; the bound ends the run with the gate's own words instead of
    /// spending thirty more rounds on it (DF-06: 30 rounds, 661k tokens).
    #[test]
    fn identical_gate_refusals_stop_the_run_at_the_bound() {
        let mut slot = None;
        let message = "update_goal(complete) was refused: the user's stated scope is not fully accounted for.\n\
             - `src/AGENTS.md` is accounted done, but it does not exist in the workspace";
        for attempt in 1..MAX_IDENTICAL_GATE_REFUSALS {
            note_gate_refusal(&mut slot, message);
            assert_eq!(
                stuck_gate_detail(&slot),
                None,
                "attempt {attempt} must still be recoverable"
            );
        }
        note_gate_refusal(&mut slot, message);
        let detail = stuck_gate_detail(&slot).expect("the bound stopped the run");
        assert!(detail.contains("src/AGENTS.md"), "{detail}");
        assert!(detail.contains("same refusal"), "{detail}");
    }

    /// Different verdicts are not the same stuck gate: a model fixing one
    /// requirement at a time must not be cut off.
    #[test]
    fn a_changing_verdict_restarts_the_count() {
        let mut slot = None;
        for _ in 0..(MAX_IDENTICAL_GATE_REFUSALS * 2) {
            note_gate_refusal(&mut slot, "first requirement is missing");
            note_gate_refusal(&mut slot, "second requirement is missing");
            note_gate_refusal(&mut slot, "third requirement is missing");
            assert_eq!(stuck_gate_detail(&slot), None);
        }
    }
}

#[cfg(test)]
mod workspace_mutation_tests {
    use super::path_is_within;

    /// Verification freshness is about the tree the command judged. A scratch
    /// file outside it, however absolute, is not a change to that tree.
    #[test]
    fn only_paths_inside_the_workspace_count_as_task_mutations() {
        let root = std::env::temp_dir().join("leveler-workspace-root");
        let root_text = root.to_string_lossy();

        assert!(path_is_within(&root, "src/lib.rs"));
        assert!(path_is_within(&root, &format!("{root_text}/src/lib.rs")));
        assert!(path_is_within(&root, "src/../src/lib.rs"));
        assert!(!path_is_within(
            &root,
            &format!("{root_text}/../outside.txt")
        ));
        assert!(!path_is_within(&root, "/tmp/captured-evidence.txt"));
    }
}
