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
    EngineEvent, LostChild, LostChildNote, LostChildVoice, TurnFacts, TurnFailure, TurnPorts,
};
use leveler_model::{ContentPart, Message};
use leveler_storage::EventStore;

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
        if let Some(mut progress) = seeds.progress {
            // F9.2: the `outstanding_children` encoding is THIS crate's, so the
            // reconciliation against the engine's durable terminal facts
            // happens here. The engine carries the facts; it does not decode
            // them.
            let settled = reconcile_outstanding_children(
                &mut progress.outstanding_children,
                &seeds.finished_children,
            );
            executor = executor.with_seeded_progress(progress);
            if !settled.is_empty() {
                executor = executor.with_restart_settled_children(settled);
            }
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
        let ledger =
            match leveler_engine::last_persisted_ledger(self.events.as_ref(), &self.session_id)
                .await
            {
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
            .filter_map(|child| {
                let projection = leveler_lifecycle::ChildResultProjection::from_findings(
                    &child.id,
                    &child.role,
                    &ledger.findings,
                );
                let preserved = projection.findings_total;
                // Nothing to add for a child that reported nothing: the
                // engine's own sentence is already the whole truth.
                (preserved > 0).then(|| {
                    (
                        child.id.clone(),
                        LostChildNote {
                            detail: Some(format!(
                                "{preserved} finding(s) on its ledger record remain adopted"
                            )),
                            contribution: Some(projection),
                        },
                    )
                })
            })
            .collect()
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
