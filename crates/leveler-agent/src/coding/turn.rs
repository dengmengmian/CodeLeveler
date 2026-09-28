//! Driving one engine turn with a coding executor.
//!
//! The engine opens the turn and hands over [`TurnPorts`]; everything here is
//! the harness's half — which executor to build, what to seed it with, how its
//! events map onto the engine's vocabulary, and which entry point a given
//! input calls.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use leveler_core::SessionId;
use leveler_engine::{
    EngineError, EngineEvent, LostChild, LostChildNote, LostChildVoice, TranscriptSink, TurnFacts,
    TurnFailure, TurnPorts,
};
use leveler_lifecycle::{EvidenceLedger, ObjectiveAnchor, PlanState, ProgressLedger};
use leveler_model::{ContentPart, Message, TranscriptOrigin};
use leveler_storage::EventStore;

use crate::coding::checkpoint::{CodingCheckpointContext, CodingCompactionCheckpoint};
use crate::coding::factory::{ExecutorFactory, TurnProfile};
use crate::sub_agent::SettledChildNotice;
use crate::{AgentError, AgentEvent, AgentOutcome, DriveAborted, Executor};

/// What the executor starts from this turn.
pub enum TurnInput {
    /// A fresh goal (seeds system + user messages). Optional `prior` is the
    /// bounded session history so multi-turn Goal can refer to earlier turns.
    Goal {
        goal: String,
        /// Original user content, preserved in both WAL and transcript.
        content: Vec<ContentPart>,
        /// Recorded on the transcript row. Not inferred from `Role::User`.
        origin: TranscriptOrigin,
        prior: Vec<Message>,
        goal_id: leveler_core::GoalId,
    },
    /// A resumed transcript (drive continues mid-conversation).
    ///
    /// `instruction` is the continuation message the user just sent (a bare
    /// `继续` or an amendment such as `继续，但是先不要跑测试`). It is already
    /// persisted by the caller; the driver appends it after the seeded prior
    /// so the model reads it in context, and the original objective stays the
    /// first user message rather than the continuation phrase.
    Resume {
        prior: Vec<Message>,
        instruction: Option<Message>,
        /// Host-resolved from the interrupted turn payload. Context assembly
        /// and the executor consume this same value.
        objective: ObjectiveAnchor,
        /// Root plus every persisted continuation turn in this lineage. State
        /// seeds must never escape this set into an older session epoch.
        lineage_turn_ids: Vec<leveler_core::TurnId>,
        root_turn_id: leveler_core::TurnId,
        goal_id: Option<leveler_core::GoalId>,
    },
    /// A conversational turn: prior transcript + new content parts.
    Content {
        prior: Vec<Message>,
        content: Vec<ContentPart>,
        /// Recorded on the transcript row. Not inferred from `Role::User`.
        origin: TranscriptOrigin,
    },
}

/// Join the text parts of a multimodal user message (objective anchors and
/// task-text both read the request through this one view).
pub(crate) fn content_text(content: &[ContentPart]) -> String {
    content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Durable objective text for multimodal user input. Images are represented
/// without persisting base64 bytes or remote URLs into prompt-facing text.
pub(crate) fn content_objective_text(content: &[ContentPart]) -> String {
    let text = content_text(content);
    let images = content
        .iter()
        .filter(|part| matches!(part, ContentPart::Image { .. }))
        .count();
    match (text.trim().is_empty(), images) {
        (false, 0) => text,
        (false, count) => format!("{text}\n[{count} image attachment(s)]"),
        (true, count) if count > 0 => format!("[{count} image attachment(s)]"),
        _ => "[non-text user request]".to_string(),
    }
}

/// Build the turn's executor, run it, and report the facts the engine records.
///
/// Every durable side of the turn arrives through `ports`: the executor never
/// touches storage itself, and the engine never sees the executor.
pub async fn drive_turn(
    factory: &ExecutorFactory,
    profile: TurnProfile,
    input: TurnInput,
    seed_task_state: bool,
    session_id: SessionId,
    events: Arc<dyn EventStore>,
    checkpoint_context: Option<CodingCheckpointContext>,
    ports: TurnPorts,
    cancellation: CancellationToken,
) -> Result<TurnFacts<AgentOutcome>, TurnFailure> {
    // Only an explicit resume continues the prior task epoch's active Plan.
    // Fresh Goal/Content turns still seed runtime truth such as ledger,
    // progress and running children, but a terminal historical Plan must not
    // become active merely because its final declaration retained open rows.
    let resumes_task_epoch = matches!(&input, TurnInput::Resume { .. });
    let seed_scope = match &input {
        TurnInput::Resume {
            lineage_turn_ids, ..
        } => Some(lineage_turn_ids.as_slice()),
        _ => None,
    };
    let TurnPorts {
        turn_id,
        emitter,
        mut sink,
        model_requests,
        budget_scope,
        finished_children,
        resumed_children,
        lost_children,
        barrier,
        fence,
        approver,
        clarifier,
    } = ports;
    let checkpoint_scope = match &input {
        TurnInput::Goal { goal_id, .. } => {
            Some(crate::coding::checkpoint::GoalCheckpointScope::new(
                goal_id.clone(),
                turn_id.clone(),
                vec![turn_id.clone()],
            ))
        }
        TurnInput::Resume {
            root_turn_id,
            lineage_turn_ids,
            goal_id: Some(goal_id),
            ..
        } => {
            let mut turn_ids = lineage_turn_ids.clone();
            turn_ids.push(turn_id.clone());
            Some(crate::coding::checkpoint::GoalCheckpointScope::new(
                goal_id.clone(),
                root_turn_id.clone(),
                turn_ids,
            ))
        }
        _ => None,
    };
    let seeds = if seed_task_state {
        Some(
            load_coding_seeds(events.as_ref(), &session_id, seed_scope)
                .await
                .map_err(seed_failure)?,
        )
    } else {
        None
    };
    let is_goal_profile = matches!(profile, TurnProfile::Goal { .. });
    // The raw request text, when the caller has one. Resume turns carry no
    // new request, so they stay unclassified.
    let task_text: Option<String> = match &input {
        TurnInput::Goal { goal, .. } => Some(goal.clone()),
        TurnInput::Content { content, .. } => {
            let text = content_text(content);
            (!text.is_empty()).then_some(text)
        }
        TurnInput::Resume { .. } => None,
    };

    let mut executor: Executor = factory
        .build(profile, task_text.as_deref())
        .await
        .map_err(|error| TurnFailure {
            cancelled: false,
            detail: error.to_string(),
            stale_ownership: false,
            model: None,
            model_steps: 0,
            modified_files: Vec::new(),
        })?
        .with_approver(approver)
        .with_clarifier(clarifier)
        // Side-effect barrier: tool dispatch waits until the announcing
        // canonical events are durable in this turn's event log.
        .with_event_barrier(barrier)
        .with_model_request_store(model_requests.clone(), session_id.clone())
        .with_budget_scope(budget_scope.clone())
        // Ownership fence: after the barriers, before dispatch, the host
        // re-proves this runtime still owns the task. Inherited by delegated
        // child executors.
        .with_execution_fence(fence);

    if let (Some(context), Some(scope)) = (checkpoint_context, checkpoint_scope) {
        executor = executor.with_compaction_checkpoint(Arc::new(CodingCompactionCheckpoint::new(
            context.engine,
            session_id.clone(),
            scope,
            context.workspace,
            emitter.clone(),
        )));
    }

    // Open task state keeps ledger/progress (including running-child truth)
    // across boundaries. Plan is narrower: only explicit Resume makes the
    // prior declaration active again; fresh inputs retain it as history.
    if let Some(seeds) = seeds {
        if resumes_task_epoch && let Some(plan) = seeds.plan {
            executor = executor.with_seeded_plan(plan);
        }
        if let Some(ledger) = seeds.ledger {
            executor = executor.with_seeded_ledger(ledger);
        }
        if let Some(mut progress) = seeds.progress {
            // F9.2: the `outstanding_children` encoding is THIS crate's, so the
            // reconciliation against the engine's durable terminal facts
            // happens here. The engine carries the facts; it does not decode
            // them.
            let settled = reconcile_outstanding_children(
                &mut progress.outstanding_children,
                &finished_children,
            );
            // A resumed child is relaunched (and re-listed) by the drive; a
            // lost one must be named even when its outstanding entry never
            // became durable before the window died.
            progress.outstanding_children.retain(|entry| {
                let id = entry.split('|').next().unwrap_or("");
                !resumed_children.iter().any(|child| child.id == id)
            });
            for child in &lost_children {
                if !progress
                    .outstanding_children
                    .iter()
                    .any(|entry| entry.split('|').next() == Some(child.id.as_str()))
                {
                    progress
                        .outstanding_children
                        .push(format!("{}|{}|{}|", child.id, child.nickname, child.role));
                }
            }
            executor = executor.with_seeded_progress(progress);
            if !settled.is_empty() {
                executor = executor.with_restart_settled_children(settled);
            }
        }
    }

    let inherited = executor.seeded_progress_for_budget();
    let scoped_progress = persisted_budget_progress(events.as_ref(), &session_id, &budget_scope)
        .await
        .map_err(seed_failure)?;
    if resumes_task_epoch
        && scoped_progress.is_none()
        && inherited.budget_scope.is_none()
        && (inherited.cumulative_model_tokens > 0 || inherited.cumulative_cost_usd_micros > 0)
        && (executor.budget_limits().max_model_tokens.is_some()
            || executor.budget_limits().max_cost_usd_micros.is_some())
    {
        return Err(seed_failure(EngineError::Config("cannot reconstruct capped legacy task budget: request scope attribution is unavailable".into())));
    }
    let progress = reconcile_model_spend(
        scoped_progress.unwrap_or_else(|| inherited.clone()),
        model_requests.as_ref(),
        &session_id,
        &budget_scope,
    )
    .await
    .map_err(seed_failure)?;
    executor = executor.with_seeded_progress(progress);

    let resumable = crate::coding::child_session::load_resumable_children(
        events.as_ref(),
        &session_id,
        &resumed_children,
    )
    .await
    .map_err(seed_failure)?;
    if !resumable.is_empty() {
        executor = executor.with_resumed_children(resumable);
    }

    let registry = factory.registry.clone();
    let mut forward = |event: AgentEvent| {
        let mut event = EngineEvent::from(event);
        // The harness owns the registry, so it stamps the tool's declared
        // risk before the fact leaves for the log. Crash recovery reads this
        // to decide whether replaying the call unattended is safe, so a
        // `ToolCallStarted` that reaches the log without it is not recoverable.
        if let EngineEvent::ToolCallStarted { name, risk, .. } = &mut event {
            *risk = registry.get(name).map(|tool| tool.risk());
        }
        emitter.emit(event);
    };
    let result = match input {
        TurnInput::Goal {
            goal,
            content,
            origin,
            prior,
            goal_id: _,
        } => {
            let objective = leveler_lifecycle::ObjectiveAnchor::from_session_goal(goal.as_str());
            executor
                .with_objective(objective)
                .run_conversation_tracked(
                    prior,
                    content,
                    origin,
                    &mut forward,
                    &mut sink,
                    cancellation.clone(),
                )
                .await
        }
        TurnInput::Resume {
            mut prior,
            instruction,
            objective,
            lineage_turn_ids: _,
            root_turn_id: _,
            goal_id: _,
        } => {
            // Persist the continuation message before running: it is this
            // turn's user input, and the transcript must match what the user
            // sent (the engine's write-ahead payload already names it for
            // crash recovery).
            let continuation_request = instruction.as_ref().map(Message::text_content);
            if let Some(message) = instruction {
                if let Err(error) =
                    TranscriptSink::append(&mut sink, std::slice::from_ref(&message)).await
                {
                    return Err(unstarted_failure(AgentError::from(error)));
                }
                prior.push(message);
                // Resume rebuilds its control contract separately. The user's
                // continuation remains the only new conversation message.
            }
            executor
                .with_objective(objective)
                .resume_tracked_with_instruction(
                    prior,
                    continuation_request.as_deref(),
                    &mut forward,
                    &mut sink,
                    cancellation.clone(),
                )
                .await
        }
        TurnInput::Content {
            prior,
            content,
            origin,
        } => {
            let text = content_objective_text(&content);
            let objective = if is_goal_profile {
                leveler_lifecycle::ObjectiveAnchor::from_session_goal(text)
            } else {
                leveler_lifecycle::ObjectiveAnchor::from_user_message(text)
            };
            executor
                .with_objective(objective)
                .run_conversation_tracked(
                    prior,
                    content,
                    origin,
                    &mut forward,
                    &mut sink,
                    cancellation.clone(),
                )
                .await
        }
    };
    // Every sender this turn handed out goes with it: the engine's pump
    // drains to close only once the last one is gone.
    drop(emitter);
    match result {
        Ok(outcome) => Ok(TurnFacts {
            stop: outcome.stop_reason,
            model_steps: outcome.model_steps,
            modified_files: outcome.modified_files.clone(),
            outcome,
        }),
        Err(error) => Err(failure(error)),
    }
}

struct CodingTurnSeeds {
    plan: Option<PlanState>,
    ledger: Option<EvidenceLedger>,
    progress: Option<ProgressLedger>,
}

/// Rebuild model spend from durable invocation facts after a crash. Other
/// mechanical counters remain owned by their existing progress events.
pub(crate) async fn reconcile_model_spend(
    mut progress: ProgressLedger,
    store: &dyn leveler_storage::ModelRequestStore,
    session_id: &SessionId,
    scope: &str,
) -> Result<ProgressLedger, EngineError> {
    if progress.budget_scope.as_deref() != Some(scope) {
        progress = ProgressLedger::default();
    }
    progress.budget_scope = Some(scope.to_string());
    let records = store.load_for_budget_scope(session_id, scope).await?;
    if records.is_empty() {
        return Ok(progress);
    }
    progress.cumulative_model_tokens = 0;
    progress.cumulative_estimated_model_tokens = 0;
    progress.cumulative_cost_usd_micros = 0;
    progress.has_unpriced_model_attempt = false;
    for record in records {
        progress.absorb_request_spend(
            record.input_tokens.saturating_add(record.output_tokens),
            record.estimated_tokens.unwrap_or(0),
            record.cost_usd_micros.unwrap_or(0),
        );
        progress.has_unpriced_model_attempt |= record.cost_usd_micros.is_none();
    }
    Ok(progress)
}

async fn load_coding_seeds(
    events: &dyn EventStore,
    session_id: &SessionId,
    turn_ids: Option<&[leveler_core::TurnId]>,
) -> Result<CodingTurnSeeds, EngineError> {
    Ok(CodingTurnSeeds {
        plan: persisted_plan(events, session_id, turn_ids).await?,
        ledger: persisted_ledger(events, session_id, turn_ids).await?,
        progress: persisted_progress(events, session_id, turn_ids).await?,
    })
}

pub(crate) fn seed_failure(error: EngineError) -> TurnFailure {
    TurnFailure {
        cancelled: false,
        stale_ownership: matches!(
            error,
            EngineError::StaleOwnership(_) | EngineError::Ownership(_)
        ),
        detail: error.to_string(),
        model: None,
        model_steps: 0,
        modified_files: Vec::new(),
    }
}

async fn last_event_of_type(
    events: &dyn EventStore,
    session_id: &SessionId,
    event_type: &str,
    turn_ids: Option<&[leveler_core::TurnId]>,
) -> Result<Option<EngineEvent>, EngineError> {
    let row = if let Some(turn_ids) = turn_ids {
        let mut newest = None;
        for turn_id in turn_ids {
            if let Some(candidate) = events
                .load_last_by_type(session_id, event_type, Some(turn_id))
                .await?
                && newest
                    .as_ref()
                    .is_none_or(|row: &leveler_storage::EventRecord| {
                        candidate.sequence > row.sequence
                    })
            {
                newest = Some(candidate);
            }
        }
        newest
    } else {
        events
            .load_last_by_type(session_id, event_type, None)
            .await?
    };
    match row {
        Some(row) => Ok(Some(EngineEvent::from_payload(&row.payload)?)),
        None => Ok(None),
    }
}

pub(crate) async fn last_persisted_plan(
    events: &dyn EventStore,
    session_id: &SessionId,
) -> Result<Option<PlanState>, EngineError> {
    Ok(
        match last_event_of_type(events, session_id, "plan_updated", None).await? {
            Some(EngineEvent::PlanUpdated { steps }) => Some(PlanState { steps }),
            _ => None,
        },
    )
}

async fn persisted_plan(
    events: &dyn EventStore,
    session_id: &SessionId,
    turn_ids: Option<&[leveler_core::TurnId]>,
) -> Result<Option<PlanState>, EngineError> {
    Ok(
        match last_event_of_type(events, session_id, "plan_updated", turn_ids).await? {
            Some(EngineEvent::PlanUpdated { steps }) => Some(PlanState { steps }),
            _ => None,
        },
    )
}

pub(crate) async fn last_persisted_ledger(
    events: &dyn EventStore,
    session_id: &SessionId,
) -> Result<Option<EvidenceLedger>, EngineError> {
    Ok(
        match last_event_of_type(events, session_id, "evidence_ledger_updated", None).await? {
            Some(EngineEvent::EvidenceLedgerUpdated { ledger }) => Some(ledger),
            _ => None,
        },
    )
}

async fn persisted_ledger(
    events: &dyn EventStore,
    session_id: &SessionId,
    turn_ids: Option<&[leveler_core::TurnId]>,
) -> Result<Option<EvidenceLedger>, EngineError> {
    Ok(
        match last_event_of_type(events, session_id, "evidence_ledger_updated", turn_ids).await? {
            Some(EngineEvent::EvidenceLedgerUpdated { ledger }) => Some(ledger),
            _ => None,
        },
    )
}

/// Rebuild the most recently scoped task budget for a host auxiliary call.
/// Context-epoch resets carry no scope and do not erase task resource spend.
pub async fn load_auxiliary_budget_progress(
    turns: &dyn leveler_storage::TurnStore,
    events: &dyn EventStore,
    requests: &dyn leveler_storage::ModelRequestStore,
    session: &SessionId,
) -> Result<ProgressLedger, EngineError> {
    let turns = turns.list_for_session(session).await?;
    let Some(turn) = turns.last() else {
        return Ok(ProgressLedger::default());
    };
    let continuation = turn
        .payload
        .as_deref()
        .map(leveler_engine::decode_turn_continuation)
        .transpose()?;
    let scope = continuation
        .as_ref()
        .and_then(|c| c.goal_id.as_ref().map(ToString::to_string))
        .or_else(|| {
            continuation
                .as_ref()
                .and_then(|c| c.root_turn_id.as_ref().map(ToString::to_string))
        })
        .unwrap_or_else(|| turn.id.to_string());
    let progress = persisted_budget_progress(events, session, &scope).await?;
    if progress.is_none() {
        // A new scoped row cannot replace known legacy spend from this same
        // lineage. Unrelated older chats must not donate their debt, either.
        let lineage: Vec<_> = turns
            .iter()
            .filter(|candidate| {
                candidate.id == turn.id
                    || candidate.id == scope
                    || candidate
                        .payload
                        .as_deref()
                        .and_then(|payload| leveler_engine::decode_turn_continuation(payload).ok())
                        .is_some_and(|c| {
                            c.goal_id.as_ref().is_some_and(|id| id.as_str() == scope)
                                || c.root_turn_id
                                    .as_ref()
                                    .is_some_and(|id| id.as_str() == scope)
                        })
            })
            .map(|turn| turn.id.as_str())
            .collect();
        for row in events.load(session).await? {
            if row.event_type == "progress_updated"
                && row
                    .turn_id
                    .as_deref()
                    .is_some_and(|id| lineage.contains(&id))
                && let EngineEvent::ProgressUpdated { ledger } =
                    EngineEvent::from_payload(&row.payload)?
                && ledger.budget_scope.is_none()
                && (ledger.cumulative_model_tokens > 0
                    || ledger.cumulative_cost_usd_micros > 0
                    || ledger.cumulative_estimated_model_tokens > 0
                    || ledger.has_unpriced_model_attempt
                    || ledger.cumulative_duration_ms > 0)
            {
                return Err(EngineError::Config("cannot reconstruct legacy task budget: request scope attribution is unavailable".into()));
            }
        }
    }
    let progress = progress.unwrap_or_default();
    reconcile_model_spend(progress, requests, session, &scope).await
}

pub(crate) async fn persisted_budget_progress(
    events: &dyn EventStore,
    session_id: &SessionId,
    scope: &str,
) -> Result<Option<ProgressLedger>, EngineError> {
    for row in events.load(session_id).await?.into_iter().rev() {
        if row.event_type == "progress_updated"
            && let EngineEvent::ProgressUpdated { ledger } =
                EngineEvent::from_payload(&row.payload)?
            && ledger.budget_scope.as_deref() == Some(scope)
        {
            return Ok(Some(ledger));
        }
    }
    Ok(None)
}

pub(crate) async fn last_persisted_progress(
    events: &dyn EventStore,
    session_id: &SessionId,
) -> Result<Option<ProgressLedger>, EngineError> {
    Ok(
        match last_event_of_type(events, session_id, "progress_updated", None).await? {
            Some(EngineEvent::ProgressUpdated { ledger }) => Some(ledger),
            _ => None,
        },
    )
}

async fn persisted_progress(
    events: &dyn EventStore,
    session_id: &SessionId,
    turn_ids: Option<&[leveler_core::TurnId]>,
) -> Result<Option<ProgressLedger>, EngineError> {
    Ok(
        match last_event_of_type(events, session_id, "progress_updated", turn_ids).await? {
            Some(EngineEvent::ProgressUpdated { ledger }) => Some(ledger),
            _ => None,
        },
    )
}

/// Report an executor error to the engine. Cancellation is called out so the
/// turn is recorded as `interrupted` rather than `failed`: the run was
/// stopped, it did not break.
fn failure(aborted: DriveAborted) -> TurnFailure {
    let error = aborted.error;
    TurnFailure {
        cancelled: matches!(error, AgentError::Cancelled),
        stale_ownership: matches!(error, AgentError::StaleOwnership(_)),
        detail: error.to_string(),
        // Carried, not re-derived: eval classifies a provider fault off the
        // typed error, and flattening it to text here would lose that.
        model: match error {
            AgentError::Model(error) => Some(error),
            _ => None,
        },
        // The loop's proven work before it aborted: a failed turn that ran
        // model steps and changed files must not record none of that.
        model_steps: aborted.facts.model_steps,
        modified_files: aborted.facts.modified_files,
    }
}

/// Refuse before the loop starts: no round ran, no file changed.
fn unstarted_failure(error: AgentError) -> TurnFailure {
    TurnFailure {
        cancelled: matches!(error, AgentError::Cancelled),
        stale_ownership: matches!(error, AgentError::StaleOwnership(_)),
        detail: error.to_string(),
        model: match error {
            AgentError::Model(error) => Some(error),
            _ => None,
        },
        model_steps: 0,
        modified_files: Vec::new(),
    }
}

/// Reconcile the harness's own outstanding-child record against the engine's
/// durable terminal facts: a child that durably FINISHED is not lost — the
/// settlement raced the window's end.
///
/// The entry encoding (`id|nickname|role|files`, written where the child is
/// spawned) belongs to THIS crate; the engine carries only the terminal fact,
/// so both the decode and the prune live here.
///
/// Returns the settlements to re-deliver. A child with no terminal fact is left
/// listed untouched: it is a ghost, and calling it settled would be false.
pub(crate) fn reconcile_outstanding_children(
    outstanding: &mut Vec<String>,
    finished: &[leveler_engine::FinishedChildFact],
) -> Vec<SettledChildNotice> {
    let mut settled = Vec::new();
    outstanding.retain(|entry| {
        let id = entry.split('|').next().unwrap_or("");
        let role = entry.split('|').nth(2).unwrap_or("?");
        match finished.iter().find(|fact| fact.id == id) {
            Some(fact) => {
                settled.push(SettledChildNotice {
                    id: fact.id.clone(),
                    nickname: fact.nickname.clone(),
                    role: role.to_string(),
                    ok: fact.ok,
                    summary: fact.summary.clone(),
                });
                false
            }
            None => true,
        }
    });
    settled
}

/// The Coding harness's answer to "what did this lost child contribute?".
///
/// The engine finds the ghost, orders the terminal, attributes it to the turn
/// the child started in and stamps `ok: false`. It cannot say what the child
/// contributed: that means reading a Coding role label against the Coding
/// evidence ledger, which is this crate's vocabulary and this crate's record
/// (§F9.3). So it asks here.
///
/// Findings a child durably reported before it was lost stay adopted, and the
/// synthetic terminal carries a projection over them — the terminal must not
/// contradict durable evidence (C9).
pub(crate) struct CodingLostChildVoice {
    pub events: Arc<dyn EventStore>,
    pub session_id: SessionId,
}

#[async_trait::async_trait]
impl LostChildVoice for CodingLostChildVoice {
    async fn speak_for(&self, lost: &[LostChild]) -> Vec<(String, LostChildNote)> {
        // One ledger read for the whole batch. On failure the harness says
        // nothing and the engine still settles every ghost truthfully — a
        // ghost left running is the failure this whole path exists to prevent.
        let ledger = match last_persisted_ledger(self.events.as_ref(), &self.session_id).await {
            Ok(ledger) => ledger.unwrap_or_default(),
            Err(error) => {
                tracing::warn!(
                    session_id = %self.session_id.as_str(),
                    %error,
                    "could not read the evidence ledger to speak for a lost child; \
                     its terminal will carry the lifecycle fact only"
                );
                return Vec::new();
            }
        };
        lost.iter()
            .map(|child| {
                let projection = leveler_lifecycle::ChildResultProjection::from_findings(
                    &child.id,
                    &child.role,
                    &ledger.findings,
                );
                let preserved = projection.findings_total;
                // Nothing to add to the sentence for a child that reported
                // nothing — the engine's own words are the whole truth — but
                // the reading is still this crate's to give.
                let note = if preserved > 0 {
                    LostChildNote {
                        detail: Some(format!(
                            "{preserved} finding(s) on its ledger record remain adopted"
                        )),
                        contribution: Some(projection),
                        outcome: Some(leveler_lifecycle::ChildStatus::IncompletePartial),
                    }
                } else {
                    LostChildNote {
                        detail: None,
                        contribution: None,
                        outcome: Some(leveler_lifecycle::ChildStatus::IncompleteNoResult),
                    }
                };
                (child.id.clone(), note)
            })
            .collect()
    }

    async fn continues(&self, interrupted: &[LostChild]) -> Vec<String> {
        // Infallible like `speak_for`: a harness that cannot read its own
        // record continues nothing, and the engine settles the child as lost —
        // a truthful terminal, never a child left running.
        match crate::coding::child_session::continuable_children(
            self.events.as_ref(),
            &self.session_id,
            interrupted,
        )
        .await
        {
            Ok(ids) => ids,
            Err(error) => {
                tracing::warn!(
                    session_id = %self.session_id.as_str(),
                    %error,
                    "could not read the child sessions to continue them; they settle as lost"
                );
                Vec::new()
            }
        }
    }
}

#[cfg(test)]
mod outstanding_child_tests {
    use super::*;

    fn fact(id: &str, ok: bool) -> leveler_engine::FinishedChildFact {
        leveler_engine::FinishedChildFact {
            id: id.to_string(),
            nickname: format!("nick-{id}"),
            ok,
            summary: format!("summary-{id}"),
        }
    }

    #[test]
    fn a_finished_child_is_pruned_and_re_delivered() {
        let mut outstanding = vec![
            "c1|Explorer|explorer|src/a.rs".to_string(),
            "c2|Worker|worker|src/b.rs".to_string(),
        ];
        let settled = reconcile_outstanding_children(&mut outstanding, &[fact("c1", true)]);
        assert_eq!(outstanding, vec!["c2|Worker|worker|src/b.rs".to_string()]);
        assert_eq!(settled.len(), 1);
        assert_eq!(settled[0].id, "c1");
        assert_eq!(settled[0].role, "explorer");
        assert!(settled[0].ok);
        assert_eq!(settled[0].summary, "summary-c1");
    }

    #[test]
    fn an_open_child_with_no_terminal_fact_is_never_touched() {
        let mut outstanding = vec!["c1|Explorer|explorer|".to_string()];
        let settled = reconcile_outstanding_children(&mut outstanding, &[]);
        assert!(settled.is_empty());
        assert_eq!(
            outstanding,
            vec!["c1|Explorer|explorer|".to_string()],
            "a ghost is not a settlement"
        );
    }

    #[test]
    fn a_terminal_fact_for_another_child_settles_nothing() {
        let mut outstanding = vec!["c1|Worker|worker|".to_string()];
        let settled = reconcile_outstanding_children(&mut outstanding, &[fact("c9", true)]);
        assert!(settled.is_empty());
        assert_eq!(outstanding, vec!["c1|Worker|worker|".to_string()]);
    }
}
