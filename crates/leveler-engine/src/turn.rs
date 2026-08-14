//! The turn runner: the ONE place the engine drives an `Executor`.
//!
//! Each turn gets a `turns` row, its messages are stamped with the turn id,
//! its events flow through the persist-before-forward [`EventLog`], and its
//! approvals/clarifications are recorded. The executor's observer is a sync
//! callback and its future is `!Send`, so events are pumped over an unbounded
//! channel and drained concurrently on the same task via `futures::join!` —
//! ordering into the log is exactly emission order.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use leveler_agent::{AgentError, AgentOutcome, Clarifier, Executor, TranscriptSink};
use leveler_core::{SessionId, TurnId};
use leveler_execution::Approver;
use leveler_model::Message;
use leveler_storage::{EngineStores, EventStore, MessageStore, ModelRequestStore};

use crate::factory::{ExecutorFactory, TurnProfile};
use crate::log::EventLog;
use crate::recorders::{EventEmitter, RecordingApprover, RecordingClarifier};
use crate::{EngineError, EngineEvent, TurnKind, TurnOutcome};

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
        content: Vec<leveler_model::ContentPart>,
    },
}

/// A finished turn.
pub struct TurnRecordedOutcome {
    pub turn_id: TurnId,
    pub outcome: AgentOutcome,
}

/// A transcript sink that stamps every persisted message with the turn.
struct TurnSink {
    messages: Arc<dyn MessageStore>,
    model_requests: Arc<dyn ModelRequestStore>,
    token: leveler_core::OwnershipToken,
    session_id: SessionId,
    turn_id: TurnId,
}

#[async_trait::async_trait]
impl TranscriptSink for TurnSink {
    async fn append(&mut self, messages: &[Message]) -> Result<(), AgentError> {
        let payloads: Vec<String> = messages
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<_, _>>()
            .map_err(|e| AgentError::Persistence(e.to_string()))?;
        self.messages
            .append_in_turn_owned(
                &self.token,
                &self.session_id,
                &self.turn_id,
                &payloads,
                leveler_core::now(),
            )
            .await
            .map_err(|e| AgentError::Persistence(e.to_string()))
    }

    async fn record_model_request(
        &mut self,
        record: &leveler_agent::ModelRequestRecord,
    ) -> Result<(), AgentError> {
        let finish_reason = serde_json::to_value(record.finish_reason)
            .ok()
            .and_then(|value| value.as_str().map(ToOwned::to_owned));
        self.model_requests
            .insert(&leveler_storage::ModelRequestRecord {
                id: record.id.clone(),
                session_id: self.session_id.clone(),
                provider: record.provider.clone(),
                model: record.model.clone(),
                input_tokens: record.usage.input_tokens,
                output_tokens: record.usage.output_tokens,
                finish_reason,
                error_kind: None,
                latency_ms: Some(record.latency_ms),
                retry_count: record.retry_count,
                created_at: leveler_core::now(),
            })
            .await
            .map_err(|error| AgentError::Persistence(error.to_string()))
    }
}

/// The engine's [`leveler_agent::ExecutionFence`]: before a tool dispatch,
/// re-read the task's current owner and compare it to this run's token.
/// Inherently check-then-dispatch (§ the fence guarantees a runtime already
/// known stale dispatches nothing new; it does not claim exactly-once).
struct OwnershipFence {
    ownership: Arc<dyn leveler_storage::OwnershipStore>,
    token: leveler_core::OwnershipToken,
}

#[async_trait::async_trait]
impl leveler_agent::ExecutionFence for OwnershipFence {
    async fn ensure_current(&self) -> Result<(), String> {
        match self.ownership.current(&self.token.task_id).await {
            Ok(Some(owner))
                if owner.runtime.as_ref() == Some(&self.token.runtime_id)
                    && owner.epoch == self.token.owner_epoch =>
            {
                Ok(())
            }
            Ok(current) => Err(format!(
                "token {} is stale (current owner: {:?})",
                self.token,
                current.map(|o| (o.runtime, o.epoch))
            )),
            // A fence that cannot be verified must fail closed.
            Err(error) => Err(format!("ownership check failed: {error}")),
        }
    }
}

/// Everything a strategy needs to run persisted turns. Persistence enters
/// through the same narrow ports as the engine — never a concrete database.
pub struct TurnRunner<'a> {
    pub stores: &'a EngineStores,
    /// Proof of current task ownership; every authoritative write this
    /// runner performs is fenced on it.
    pub token: leveler_core::OwnershipToken,
    pub session_id: SessionId,
    pub log: &'a EventLog<'a>,
    pub factory: &'a ExecutorFactory,
    pub approver: Arc<dyn Approver>,
    pub clarifier: Arc<dyn Clarifier>,
    /// C5-S3: the highest fold threshold any prior turn of this task expanded
    /// to (seeded from replayed `ContextExpanded` events on resume, updated
    /// live as this runner forwards them). Later turns start from it instead
    /// of shrinking back to the initial tier.
    pub expanded_context_budget: std::sync::Arc<std::sync::atomic::AtomicU32>,
}

impl TurnRunner<'_> {
    /// Run one fully-persisted turn. On success the turn row is terminal
    /// (`completed`); an executor error marks it `failed` (or `interrupted`
    /// on cancellation) before the error propagates.
    pub async fn run_turn(
        &self,
        kind: TurnKind,
        profile: TurnProfile,
        input: TurnInput,
        observer: &mut dyn FnMut(EngineEvent),
        cancellation: CancellationToken,
    ) -> Result<TurnRecordedOutcome, EngineError> {
        let is_repair_turn = matches!(kind, TurnKind::Repair { .. });
        let payload = match &kind {
            TurnKind::Node { node_id } => Some(format!(r#"{{"node_id":"{node_id}"}}"#)),
            TurnKind::Repair { attempt } => Some(format!(r#"{{"attempt":{attempt}}}"#)),
            _ => None,
        };
        // Reap zombies left by kill -9 / unclean TUI exit so a new turn never
        // coexists with a permanent `running` sibling on the same session.
        let reaped_events = crate::reap_running_turns_owned(
            self.stores.turns.as_ref(),
            self.stores.terminal.as_ref(),
            &self.token,
            Some(&self.session_id),
        )
        .await?;
        let reaped = reaped_events.len();
        for event in reaped_events {
            observer(event);
        }
        if reaped > 0 {
            tracing::warn!(
                session_id = %self.session_id.as_str(),
                reaped,
                "reaped zombie running turns before starting a new turn"
            );
        }
        let turn = self
            .stores
            .turns
            .start_owned(
                &self.token,
                &self.session_id,
                kind.as_str(),
                payload.as_deref(),
                leveler_core::now(),
            )
            .await?;
        let turn_id = TurnId::new(turn.id.clone());
        self.log
            .append(
                Some(&turn_id),
                EngineEvent::TurnStarted {
                    turn_id: turn_id.clone(),
                    kind,
                },
                observer,
            )
            .await?;

        const EVENT_BUFFER_CAPACITY: usize = 256;
        let (events, mut rx, pump_state) =
            EventEmitter::channel(EVENT_BUFFER_CAPACITY, cancellation.clone());
        let mut sink = TurnSink {
            messages: self.stores.messages.clone(),
            model_requests: self.stores.model_requests.clone(),
            token: self.token.clone(),
            session_id: self.session_id.clone(),
            turn_id: turn_id.clone(),
        };

        // The executor block OWNS the emitter (observer closure + recorders); when it
        // ends, every sender is dropped and the pump drains to close.
        let is_goal_profile = matches!(profile, TurnProfile::Goal { .. });
        // P3: the raw request text feeds task-class gate grading in the
        // factory. Resume turns carry no new request, so they stay
        // unclassified and keep the default (fully gated) assembly.
        let task_text: Option<String> = match &input {
            TurnInput::Goal { goal, .. } => Some(goal.clone()),
            TurnInput::Content { content, .. } => {
                let text = content_text(content);
                (!text.is_empty()).then_some(text)
            }
            TurnInput::Resume(_) => None,
        };
        let exec = async {
            let restored = {
                let value = self
                    .expanded_context_budget
                    .load(std::sync::atomic::Ordering::Relaxed);
                (value > 0).then_some(value)
            };
            let mut executor: Executor = self
                .factory
                .build(profile, task_text.as_deref())
                .await?
                .with_repair_expansion_evidence(is_repair_turn)
                .with_restored_context_budget(restored)
                .with_approver(Arc::new(RecordingApprover {
                    inner: self.approver.clone(),
                    events: events.clone(),
                    turn_id: turn_id.clone(),
                }))
                .with_clarifier(Arc::new(RecordingClarifier {
                    inner: self.clarifier.clone(),
                    events: events.clone(),
                    turn_id: turn_id.clone(),
                }))
                // Side-effect barrier: tool dispatch waits until the announcing
                // canonical events are durable in this turn's event log.
                .with_event_barrier(Arc::new(crate::recorders::PumpBarrier {
                    events: events.clone(),
                }))
                // Ownership fence: after the barriers, before dispatch, the
                // host re-proves this runtime still owns the task. Inherited
                // by delegated child executors.
                .with_execution_fence(Arc::new(OwnershipFence {
                    ownership: self.stores.ownership.clone(),
                    token: self.token.clone(),
                }));
            // Resume / same unfinished task: seed Plan/Ledger/Progress so
            // Delivery and closeout stay consistent. Fresh Content turns must
            // NOT inherit terminal Closing/Completed state (new task epoch).
            // Load the last persisted plan/progress once and reuse them for both
            // the seed decision and the seeding itself. Each `last_persisted_*`
            // call scans the full event log, so loading plan/progress twice
            // (as this did) doubled that cost every Content/Goal turn.
            let progress =
                last_persisted_progress(self.stores.events.as_ref(), &self.session_id).await?;
            let plan = last_persisted_plan(self.stores.events.as_ref(), &self.session_id).await?;
            let seed_state = match &input {
                TurnInput::Resume(_) => true,
                TurnInput::Content { .. } | TurnInput::Goal { .. } => {
                    should_seed_task_state(plan.as_ref(), progress.as_ref())
                }
            };
            if seed_state {
                if let Some(plan) = plan {
                    executor = executor.with_seeded_plan(plan);
                }
                if let Some(ledger) =
                    last_persisted_ledger(self.stores.events.as_ref(), &self.session_id).await?
                {
                    executor = executor.with_seeded_ledger(ledger);
                }
                if let Some(progress) = progress {
                    executor = executor.with_seeded_progress(progress);
                }
            }
            let expanded_budget = self.expanded_context_budget.clone();
            let mut forward = |event: leveler_agent::AgentEvent| {
                if let leveler_agent::AgentEvent::ContextExpanded { to, .. } = &event {
                    expanded_budget.fetch_max(*to, std::sync::atomic::Ordering::Relaxed);
                }
                events.emit(EngineEvent::from(event));
            };
            let result = match input {
                TurnInput::Goal { goal, prior } => {
                    let objective =
                        leveler_lifecycle::ObjectiveAnchor::from_session_goal(goal.as_str());
                    if prior.is_empty() {
                        executor
                            .with_objective(objective)
                            .run(&goal, &mut forward, &mut sink, cancellation.clone())
                            .await
                    } else {
                        // Multi-turn Goal: carry bounded history so deictic
                        // follow-ups ("刚才那个") resolve against prior work.
                        executor
                            .with_objective(objective)
                            .run_conversation(
                                prior,
                                vec![leveler_model::ContentPart::Text { text: goal }],
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
            drop(events);
            result.map_err(EngineError::from)
        };

        // Persist-then-forward each pumped event, in emission order. A
        // persistence failure stops persisting but keeps draining so the
        // executor never blocks; the error aborts the turn afterwards. Flush
        // markers (the side-effect barrier) are acknowledged with the current
        // persistence state: after a failed append the barrier reports the
        // failure, so the executor refuses to run the tool it was announcing.
        let pump = async {
            let mut result: Result<(), EngineError> = Ok(());
            while let Some(item) = rx.recv().await {
                let mut event = match item {
                    crate::recorders::PumpItem::Event(event) => event,
                    crate::recorders::PumpItem::Flush(ack) => {
                        let _ = ack.send(match &result {
                            Ok(()) => Ok(()),
                            Err(error) => Err(error.to_string()),
                        });
                        continue;
                    }
                };
                if let EngineEvent::ToolCallStarted { name, risk, .. } = &mut event {
                    *risk = self.factory.registry.get(name).map(|tool| tool.risk());
                }
                if result.is_ok() {
                    result = self.log.append(Some(&turn_id), event, observer).await;
                }
            }
            if let Some(mut event) = pump_state.take_overflow() {
                if let EngineEvent::ToolCallStarted { name, risk, .. } = &mut event {
                    *risk = self.factory.registry.get(name).map(|tool| tool.risk());
                }
                if result.is_ok() {
                    result = self.log.append(Some(&turn_id), event, observer).await;
                }
            }
            if result.is_ok() && pump_state.is_overloaded() {
                result = Err(EngineError::EventBufferOverloaded);
            }
            result
        };

        let (exec_result, pump_result) = futures::join!(exec, pump);
        let run_result = match pump_result {
            Ok(()) => exec_result,
            Err(error) => Err(error),
        };
        // The terminal event and query projection commit atomically. Forwarding
        // happens only after commit, so observers never see an uncommitted fact.
        let (terminal, stop_reason, stop, rounds, modified_files) = match &run_result {
            Ok(outcome) => (
                TurnOutcome::Completed,
                format!("{:?}", outcome.stop_reason),
                Some(outcome.stop_reason),
                outcome.rounds,
                outcome.modified_files.clone(),
            ),
            Err(EngineError::Agent(AgentError::Cancelled)) => (
                TurnOutcome::Interrupted,
                "cancelled".to_string(),
                None,
                0,
                Vec::new(),
            ),
            Err(error) => (TurnOutcome::Failed, error.to_string(), None, 0, Vec::new()),
        };
        let event = EngineEvent::TurnFinished {
            turn_id: turn_id.clone(),
            outcome: terminal,
            stop_reason,
            stop,
            rounds,
            modified_files,
        };
        let (event_type, payload) = event.to_row()?;
        self.stores
            .terminal
            .finish_turn_owned(
                &self.token,
                &self.session_id,
                &turn_id,
                &event_type,
                &payload,
                terminal,
                leveler_core::now(),
            )
            .await?;
        observer(event);

        let outcome = run_result?;

        Ok(TurnRecordedOutcome { turn_id, outcome })
    }
}

/// Join the text parts of a multimodal user message (objective anchors and
/// P3 task classification both read the request through this one view).
fn content_text(content: &[leveler_model::ContentPart]) -> String {
    content
        .iter()
        .filter_map(|p| match p {
            leveler_model::ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether a fresh Content/Goal turn should inherit Plan/Ledger/Progress.
///
/// Resume always seeds (caller uses `TurnInput::Resume`). For Content/Goal we
/// seed only when the prior task is still open — never Closing/Terminal or a
/// fully completed plan (that would be a finished epoch).
pub(crate) fn should_seed_task_state(
    plan: Option<&leveler_agent::PlanState>,
    progress: Option<&leveler_lifecycle::ProgressLedger>,
) -> bool {
    if let Some(p) = progress
        && p.is_terminal_for_inheritance()
    {
        return false;
    }
    if let Some(plan) = plan
        && plan.is_fully_completed()
    {
        return false;
    }
    // Seed when there is unfinished work to carry, or no terminal signal.
    true
}

/// Seed loaders: indexed single-row lookups (never full-log scans), and a
/// selected row that fails to parse is a hard error — same fail-loud policy as
/// `EventLog::replay`, never a silently missing seed.
async fn last_event_of_type(
    events: &dyn EventStore,
    session_id: &SessionId,
    event_type: &str,
) -> Result<Option<EngineEvent>, EngineError> {
    match events
        .load_last_by_type(session_id, event_type, None)
        .await?
    {
        Some(row) => Ok(Some(EngineEvent::from_payload(&row.payload)?)),
        None => Ok(None),
    }
}

/// Last full-list plan from the event log (SoT for resume PlanState).
pub(crate) async fn last_persisted_plan(
    events: &dyn EventStore,
    session_id: &SessionId,
) -> Result<Option<leveler_agent::PlanState>, EngineError> {
    Ok(
        match last_event_of_type(events, session_id, "plan_updated").await? {
            Some(EngineEvent::PlanUpdated { steps }) => Some(leveler_agent::PlanState { steps }),
            _ => None,
        },
    )
}

/// Last EvidenceLedger snapshot from the event log (SoT for Delivery resume).
async fn last_persisted_ledger(
    events: &dyn EventStore,
    session_id: &SessionId,
) -> Result<Option<leveler_lifecycle::EvidenceLedger>, EngineError> {
    Ok(
        match last_event_of_type(events, session_id, "evidence_ledger_updated").await? {
            Some(EngineEvent::EvidenceLedgerUpdated { ledger }) => Some(ledger),
            _ => None,
        },
    )
}

/// Last ProgressLedger snapshot (closeout / no-progress streak for continue).
async fn last_persisted_progress(
    events: &dyn EventStore,
    session_id: &SessionId,
) -> Result<Option<leveler_lifecycle::ProgressLedger>, EngineError> {
    Ok(
        match last_event_of_type(events, session_id, "progress_updated").await? {
            Some(EngineEvent::ProgressUpdated { ledger }) => Some(ledger),
            _ => None,
        },
    )
}

#[cfg(test)]
mod seed_gate_tests {
    use super::*;
    use leveler_lifecycle::{PlanOrigin, PlanState, PlanStep, ProgressLedger, TurnPhase};

    fn completed_plan() -> PlanState {
        PlanState {
            steps: vec![PlanStep {
                step: "done".into(),
                status: "completed".into(),
                id: Some("1".into()),
                origin: PlanOrigin::ModelExplicit,
            }],
        }
    }

    #[test]
    fn fresh_content_does_not_seed_closing_progress() {
        let mut progress = ProgressLedger::default();
        progress.enter_closing();
        assert!(!should_seed_task_state(None, Some(&progress)));
    }

    #[test]
    fn fresh_content_does_not_seed_fully_completed_plan() {
        assert!(!should_seed_task_state(Some(&completed_plan()), None));
    }

    #[test]
    fn open_progress_still_seeds() {
        let progress = ProgressLedger {
            phase: TurnPhase::Active,
            closing: false,
            ..Default::default()
        };
        assert!(should_seed_task_state(None, Some(&progress)));
    }

    #[test]
    fn empty_prior_state_seeds_harmlessly() {
        assert!(should_seed_task_state(None, None));
    }
}
