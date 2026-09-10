//! Driving one engine turn with a coding executor.
//!
//! The engine opens the turn and hands over [`TurnPorts`]; everything here is
//! the harness's half — which executor to build, what to seed it with, how its
//! events map onto the engine's vocabulary, and which entry point a given
//! input calls.

use tokio_util::sync::CancellationToken;

use leveler_engine::{EngineEvent, TurnFacts, TurnFailure, TurnPorts};
use leveler_model::{ContentPart, Message};

use crate::coding::factory::{ExecutorFactory, TurnProfile};
use crate::sub_agent::SettledChildNotice;
use crate::{AgentError, AgentEvent, AgentOutcome, Executor};

/// What the executor starts from this turn.
pub enum TurnInput {
    /// A fresh goal (seeds system + user messages). Optional `prior` is the
    /// bounded session history so multi-turn Goal can refer to earlier turns.
    Goal { goal: String, prior: Vec<Message> },
    /// A resumed transcript (drive continues mid-conversation).
    Resume(Vec<Message>),
    /// A conversational turn: prior transcript + new content parts.
    Content {
        prior: Vec<Message>,
        content: Vec<ContentPart>,
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

/// Build the turn's executor, run it, and report the facts the engine records.
///
/// Every durable side of the turn arrives through `ports`: the executor never
/// touches storage itself, and the engine never sees the executor.
pub async fn drive_turn(
    factory: &ExecutorFactory,
    profile: TurnProfile,
    input: TurnInput,
    ports: TurnPorts,
    cancellation: CancellationToken,
) -> Result<TurnFacts<AgentOutcome>, TurnFailure> {
    let TurnPorts {
        turn_id,
        emitter,
        mut sink,
        seeds,
        barrier,
        fence,
        checkpoint,
        approver,
        clarifier,
    } = ports;
    let _ = turn_id;
    let is_goal_profile = matches!(profile, TurnProfile::Goal { .. });
    // The raw request text, when the caller has one. Resume turns carry no
    // new request, so they stay unclassified.
    let task_text: Option<String> = match &input {
        TurnInput::Goal { goal, .. } => Some(goal.clone()),
        TurnInput::Content { content, .. } => {
            let text = content_text(content);
            (!text.is_empty()).then_some(text)
        }
        TurnInput::Resume(_) => None,
    };

    let mut executor: Executor = factory
        .build(profile, task_text.as_deref())
        .await
        .map_err(|error| TurnFailure {
            cancelled: false,
            detail: error.to_string(),
            stale_ownership: false,
            model: None,
        })?
        .with_approver(approver)
        .with_clarifier(clarifier)
        // Side-effect barrier: tool dispatch waits until the announcing
        // canonical events are durable in this turn's event log.
        .with_event_barrier(barrier)
        // Long-goal P3: a context fold first cuts a durable goal checkpoint;
        // the fold's summary becomes persisted truth, and a failed checkpoint
        // keeps the context uncompacted (fail closed in the loop).
        .with_compaction_checkpoint(checkpoint)
        // Ownership fence: after the barriers, before dispatch, the host
        // re-proves this runtime still owns the task. Inherited by delegated
        // child executors.
        .with_execution_fence(fence);

    // Resume / same unfinished task: seed Plan/Ledger/Progress so Delivery and
    // closeout stay consistent. The engine decided whether this turn may
    // inherit them; `None` means it may not.
    if let Some(seeds) = seeds {
        if let Some(plan) = seeds.plan {
            executor = executor.with_seeded_plan(plan);
        }
        if let Some(ledger) = seeds.ledger {
            executor = executor.with_seeded_ledger(ledger);
        }
        if let Some(progress) = seeds.progress {
            executor = executor.with_seeded_progress(progress);
        }
        if !seeds.settled_children.is_empty() {
            executor = executor.with_restart_settled_children(
                seeds
                    .settled_children
                    .into_iter()
                    .map(|child| SettledChildNotice {
                        id: child.id,
                        nickname: child.nickname,
                        role: child.role,
                        ok: child.ok,
                        summary: child.summary,
                    })
                    .collect(),
            );
        }
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
        TurnInput::Goal { goal, prior } => {
            let objective = leveler_lifecycle::ObjectiveAnchor::from_session_goal(goal.as_str());
            if prior.is_empty() {
                executor
                    .with_objective(objective)
                    .run(&goal, &mut forward, &mut sink, cancellation.clone())
                    .await
            } else {
                // Multi-turn Goal: carry bounded history so deictic follow-ups
                // ("刚才那个") resolve against prior work.
                executor
                    .with_objective(objective)
                    .run_conversation(
                        prior,
                        vec![ContentPart::Text { text: goal }],
                        &mut forward,
                        &mut sink,
                        cancellation.clone(),
                    )
                    .await
            }
        }
        TurnInput::Resume(prior) => {
            executor
                .resume(prior, &mut forward, &mut sink, cancellation.clone())
                .await
        }
        TurnInput::Content { prior, content } => {
            let text = content_text(&content);
            let objective = if is_goal_profile {
                leveler_lifecycle::ObjectiveAnchor::from_session_goal(text)
            } else {
                leveler_lifecycle::ObjectiveAnchor::from_user_message(text)
            };
            executor
                .with_objective(objective)
                .run_conversation(
                    prior,
                    content,
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
            rounds: outcome.rounds,
            modified_files: outcome.modified_files.clone(),
            outcome,
        }),
        Err(error) => Err(failure(error)),
    }
}

/// Report an executor error to the engine. Cancellation is called out so the
/// turn is recorded as `interrupted` rather than `failed`: the run was
/// stopped, it did not break.
fn failure(error: AgentError) -> TurnFailure {
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
    }
}
