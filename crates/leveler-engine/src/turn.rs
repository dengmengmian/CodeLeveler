//! The turn runner: the ONE place a turn becomes durable.
//!
//! Each turn gets a `turns` row, its messages are stamped with the turn id,
//! its events flow through the persist-before-forward [`EventLog`], and its
//! approvals/clarifications are recorded. The engine owns all of that; what
//! actually runs inside the turn is a closure the harness supplies, and the
//! engine knows nothing about it beyond the facts it reports back.
//!
//! A harness observer is a sync callback and its future may be `!Send`, so
//! events are pumped over a bounded channel and drained concurrently on the
//! same task via `futures::join!` — ordering into the log is exactly
//! emission order.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use leveler_core::{SessionId, TurnId};
use leveler_execution::{Approver, Clarifier};
use leveler_lifecycle::StopReason;
use leveler_model::{Message, Role};
use leveler_storage::{EngineStores, MessageStore, ModelRequestStore};

use crate::log::EventLog;
use crate::ports::{
    EventBarrier, ExecutionFence, ModelCallKind, ModelRequestRecord, PortError, TranscriptSink,
};
use crate::recorders::{EventEmitter, RecordingApprover, RecordingClarifier};
use crate::{EngineError, EngineEvent, TurnKind, TurnOutcome};

/// How many times one child may be continued after its activation died. A
/// child whose activations keep dying with their window is settled as lost
/// rather than resumed forever.
pub const MAX_CHILD_RESUMES: u32 = 3;

/// The mechanical input that starts a turn.
#[derive(Debug, Clone)]
pub enum TurnStart {
    /// A resumed turn has no new initiating message.
    Resume,
    /// A fresh user turn carries the request accepted into the write-ahead log.
    Fresh(Message),
    /// A node or repair turn carries only its mechanical kind payload.
    Internal,
}

/// Versioned write-ahead record for the input that initiated a fresh user
/// turn. It lives in the same row that makes the turn `running`, so a crash
/// can leave the transcript projection behind but can never lose the accepted
/// request itself.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct TurnInitiationPayload {
    version: u8,
    initiating_message: Message,
}

impl TurnInitiationPayload {
    const VERSION: u8 = 1;

    fn encode(message: Message) -> Result<String, EngineError> {
        if message.role != Role::User {
            return Err(EngineError::Config(
                "a turn initiating message must have the user role".to_string(),
            ));
        }
        Ok(serde_json::to_string(&Self {
            version: Self::VERSION,
            initiating_message: message,
        })?)
    }

    pub(crate) fn decode(payload: &str) -> Result<Message, EngineError> {
        let decoded: Self = serde_json::from_str(payload)?;
        if decoded.version != Self::VERSION {
            return Err(EngineError::Corrupt(format!(
                "unsupported turn initiation payload version {}",
                decoded.version
            )));
        }
        if decoded.initiating_message.role != Role::User {
            return Err(EngineError::Corrupt(
                "turn initiation payload is not a user message".to_string(),
            ));
        }
        Ok(decoded.initiating_message)
    }
}

/// Everything the engine offers the harness for one turn.
///
/// This is the whole seam. The engine hands over durable ports and the state
/// it read back; what the harness builds on top of them — a model loop, a
/// prompt, a tool surface — the engine never sees.
pub struct TurnPorts {
    pub turn_id: TurnId,
    /// Where harness events enter the persist-before-forward log.
    pub emitter: EventEmitter,
    /// Persists the transcript and the model-call rows for this turn.
    pub sink: TurnSink,
    /// Durable child terminal facts observed after ghost reconciliation. Their
    /// domain meaning belongs to the harness.
    pub finished_children: Vec<crate::log::FinishedChildFact>,
    /// Interrupted children this turn continues, already recorded as resumed.
    /// The harness launches their new activations.
    pub resumed_children: Vec<crate::ResumedChild>,
    /// Interrupted children this turn settled as lost before the harness ran.
    pub lost_children: Vec<crate::LostChild>,
    pub barrier: Arc<dyn EventBarrier>,
    pub fence: Arc<dyn ExecutionFence>,
    /// The turn's approver/clarifier, wrapped so every request and decision
    /// is recorded before it is answered.
    pub approver: Arc<dyn Approver>,
    pub clarifier: Arc<dyn Clarifier>,
}

/// What the harness reports about a turn that ran to its own end.
///
/// `outcome` is the harness's own result type, carried through untouched —
/// the engine records the mechanical facts beside it and reads nothing from
/// it.
pub struct TurnFacts<T> {
    pub stop: StopReason,
    pub rounds: u32,
    pub modified_files: Vec<String>,
    pub outcome: T,
}

/// Why the harness stopped without reporting facts.
#[derive(Debug)]
pub struct TurnFailure {
    /// A cancelled turn is `interrupted`, not `failed`: the run was stopped,
    /// it did not break.
    pub cancelled: bool,
    pub detail: String,
    /// This runtime lost the task mid-turn. The terminal write is skipped:
    /// the current owner decides the task's future.
    pub stale_ownership: bool,
    /// The provider failure behind this stop, when there was one.
    pub model: Option<leveler_model::ModelError>,
}

impl From<TurnFailure> for EngineError {
    fn from(failure: TurnFailure) -> Self {
        if failure.cancelled {
            EngineError::Cancelled
        } else if failure.stale_ownership {
            EngineError::StaleOwnership(failure.detail)
        } else {
            EngineError::Execution {
                detail: failure.detail,
                model: failure.model,
            }
        }
    }
}

/// A finished turn, paired with whatever the harness returned for it.
pub struct TurnRecordedOutcome<T> {
    pub turn_id: TurnId,
    pub outcome: T,
}

/// A transcript sink that stamps every persisted message with the turn.
pub struct TurnSink {
    messages: Arc<dyn MessageStore>,
    model_requests: Arc<dyn ModelRequestStore>,
    token: leveler_core::OwnershipToken,
    session_id: SessionId,
    turn_id: TurnId,
}

#[async_trait::async_trait]
impl TranscriptSink for TurnSink {
    async fn append(&mut self, messages: &[Message]) -> Result<(), PortError> {
        let payloads: Vec<String> = messages
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<_, _>>()
            .map_err(|e| PortError::Persistence(e.to_string()))?;
        self.messages
            .append_in_turn_owned(
                &self.token,
                &self.session_id,
                &self.turn_id,
                &payloads,
                leveler_core::now(),
            )
            .await
            .map_err(|e| PortError::Persistence(e.to_string()))
    }

    async fn record_model_request(&mut self, record: &ModelRequestRecord) -> Result<(), PortError> {
        self.model_requests
            .insert(&storage_model_request(record, &self.session_id))
            .await
            .map_err(|error| PortError::Persistence(error.to_string()))
    }
}

/// The durable row for one model call, whoever made it: the root turn, a
/// delegated child, or the harness-launched closure reviewer. The record
/// already carries the caller's `agent_id`; the engine owns the row's
/// identity and the session it belongs to.
pub fn storage_model_request(
    record: &ModelRequestRecord,
    session_id: &SessionId,
) -> leveler_storage::ModelRequestRecord {
    let finish_reason = serde_json::to_value(record.finish_reason)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned));
    leveler_storage::ModelRequestRecord {
        // The engine owns the row's identity; the provider's id rides
        // along as a diagnostic. Two calls that report the same id are
        // two rows, not a persistence failure that ends the turn.
        id: leveler_core::EventId::generate().into_inner(),
        provider_request_id: record.provider_request_id.clone(),
        session_id: session_id.clone(),
        provider: record.provider.clone(),
        model: record.model.clone(),
        input_tokens: record.usage.input_tokens,
        output_tokens: record.usage.output_tokens,
        // Recorded, not inferred: `Some(0)` is a provider that
        // reported no cache hit, and the `None` this never writes is
        // reserved for rows from before the column existed.
        cached_input_tokens: Some(record.usage.cached_input_tokens),
        cost_usd_micros: record.cost_usd_micros,
        agent_id: record.agent_id.clone(),
        finish_reason,
        error_kind: None,
        latency_ms: Some(record.latency_ms),
        retry_count: record.retry_count,
        kind: match record.kind {
            ModelCallKind::Round => leveler_storage::ModelCallKind::Round,
            ModelCallKind::Compaction => leveler_storage::ModelCallKind::Compaction,
            ModelCallKind::Advisory => leveler_storage::ModelCallKind::Advisory,
        },
        created_at: leveler_core::now(),
    }
}

/// The engine's [`ExecutionFence`]: before a tool dispatch,
/// re-read the task's current owner and compare it to this run's token.
/// Inherently check-then-dispatch (§ the fence guarantees a runtime already
/// known stale dispatches nothing new; it does not claim exactly-once).
pub(crate) struct OwnershipFence {
    ownership: Arc<dyn leveler_storage::OwnershipStore>,
    token: leveler_core::OwnershipToken,
}

#[async_trait::async_trait]
impl ExecutionFence for OwnershipFence {
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
    pub approver: Arc<dyn Approver>,
    pub clarifier: Arc<dyn Clarifier>,
    /// Who speaks for a child this runtime lost. The engine detects the loss
    /// and writes the terminal either way; a harness supplies one so the
    /// terminal can also say what the child contributed (§F9.3). `None` is a
    /// harness with no child semantics at all — the engine's own tests, or
    /// another product on this kernel.
    pub lost_child_voice: Option<Arc<dyn crate::LostChildVoice>>,
}

impl TurnRunner<'_> {
    /// The ownership fence for this runner's task, for a harness that runs an
    /// execution outside [`Self::run_turn`] — a closure-boundary child, say.
    /// Fenced the same way: a stale runtime dispatches nothing new.
    pub fn ownership_fence(&self) -> Arc<dyn ExecutionFence> {
        Arc::new(OwnershipFence {
            ownership: self.stores.ownership.clone(),
            token: self.token.clone(),
        })
    }

    /// Run one fully-persisted turn.
    ///
    /// The engine opens the turn, hands the harness its [`TurnPorts`], pumps
    /// every event it emits into the log, and stamps the terminal row. What
    /// runs inside `execute` is the harness's business: the engine sees only
    /// the [`TurnFacts`] it reports, or the [`TurnFailure`] it ended on.
    ///
    /// On success the turn row is terminal (`completed`); a reported failure
    /// marks it `failed` (or `interrupted` when the harness says it was
    /// cancelled) before the error propagates.
    pub async fn run_turn<T, F, Fut>(
        &self,
        kind: TurnKind,
        start: TurnStart,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
        cancellation: CancellationToken,
        execute: F,
    ) -> Result<TurnRecordedOutcome<T>, EngineError>
    where
        F: FnOnce(TurnPorts) -> Fut,
        Fut: std::future::Future<Output = Result<TurnFacts<T>, TurnFailure>>,
    {
        let payload = match (&kind, start) {
            (TurnKind::User | TurnKind::Chat, TurnStart::Fresh(message)) => {
                Some(TurnInitiationPayload::encode(message)?)
            }
            (TurnKind::User | TurnKind::Chat, TurnStart::Resume) => None,
            (TurnKind::User | TurnKind::Chat, TurnStart::Internal) => {
                return Err(EngineError::Config(
                    "a user turn requires a fresh message or resume start".to_string(),
                ));
            }
            (TurnKind::Node { node_id }, TurnStart::Internal) => {
                Some(format!(r#"{{"node_id":"{node_id}"}}"#))
            }
            (TurnKind::Repair { attempt }, TurnStart::Internal) => {
                Some(format!(r#"{{"attempt":{attempt}}}"#))
            }
            (TurnKind::Node { .. } | TurnKind::Repair { .. }, _) => {
                return Err(EngineError::Config(
                    "internal turns require an internal start".to_string(),
                ));
            }
        };
        // Reap zombies left by kill -9 / unclean TUI exit so a new turn never
        // coexists with a permanent `running` sibling on the same session.
        let reaped_events = crate::reap_running_turns_owned(
            self.stores.turns.as_ref(),
            self.stores.messages.as_ref(),
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

        // MA-RT-2: durable ghosts — children a dead window STARTED and never
        // finished — are reconciled into truthful terminal debt BEFORE this
        // turn seeds its state, so both completion gates see the debt instead
        // of an empty outstanding list. Hard error on purpose: running a turn
        // past an unreconciled ghost Worker is exactly the false-Verified path
        // this exists to close. The finished facts feed the settlement
        // re-delivery during seeding below.
        //
        // At a turn start no activation of this session can be live, so every
        // open child is a dead activation: it is marked interrupted, then the
        // harness says which it continues. Those are recorded as resumed and
        // handed over; the rest settle as lost here, before anything runs.
        let (open, finished_children) = self.log.interrupt_open_children(observer).await?;
        let continued = match &self.lost_child_voice {
            Some(voice) if !open.is_empty() => voice.continues(&lost_view(&open)).await,
            _ => Vec::new(),
        };
        let (wanted, lost): (Vec<_>, Vec<_>) = open
            .into_iter()
            .partition(|child| continued.contains(&child.id));
        let (resumable, exhausted): (Vec<_>, Vec<_>) = wanted
            .into_iter()
            .partition(|child| child.resumes < MAX_CHILD_RESUMES);
        let lost_children = lost_view(&lost)
            .into_iter()
            .chain(lost_view(&exhausted))
            .collect();
        self.settle_ghost_children(
            lost,
            "was lost when its previous runtime window ended before it reported",
            leveler_lifecycle::ChildStop::Lost,
            &turn_id,
            observer,
        )
        .await?;
        self.settle_ghost_children(
            exhausted,
            &format!(
                "was interrupted again after {MAX_CHILD_RESUMES} resumes and is not continued"
            ),
            leveler_lifecycle::ChildStop::Lost,
            &turn_id,
            observer,
        )
        .await?;
        let mut resumed_children = Vec::with_capacity(resumable.len());
        for child in resumable {
            let attempt = child.resumes + 1;
            self.log
                .append(
                    Some(&turn_id),
                    EngineEvent::SubAgentResumed {
                        id: child.id.clone(),
                        attempt,
                    },
                    observer,
                )
                .await?;
            resumed_children.push(crate::ResumedChild {
                id: child.id,
                nickname: child.nickname,
                role: child.role,
                attempt,
            });
        }

        // Margin, not the fix. The fix is the batching pump below: raising this
        // alone only moves the cliff, and a wider fan-out would walk straight
        // back off it. Sized so a burst from several agents has somewhere to sit
        // while a batch commits.
        const EVENT_BUFFER_CAPACITY: usize = 4096;
        let (events, mut rx, pump_state) =
            EventEmitter::channel(EVENT_BUFFER_CAPACITY, cancellation.clone());
        let sink = TurnSink {
            messages: self.stores.messages.clone(),
            model_requests: self.stores.model_requests.clone(),
            token: self.token.clone(),
            session_id: self.session_id.clone(),
            turn_id: turn_id.clone(),
        };

        // The harness block OWNS the emitter (its ports and observer closure);
        // when it ends, every sender is dropped and the pump drains to close.
        let exec = async {
            let ports = TurnPorts {
                turn_id: turn_id.clone(),
                emitter: events.clone(),
                sink,
                finished_children,
                resumed_children,
                lost_children,
                barrier: Arc::new(crate::recorders::PumpBarrier {
                    events: events.clone(),
                }),
                fence: Arc::new(OwnershipFence {
                    ownership: self.stores.ownership.clone(),
                    token: self.token.clone(),
                }),
                approver: Arc::new(RecordingApprover {
                    inner: self.approver.clone(),
                    events: events.clone(),
                    turn_id: turn_id.clone(),
                }),
                clarifier: Arc::new(RecordingClarifier {
                    inner: self.clarifier.clone(),
                    events: events.clone(),
                    turn_id: turn_id.clone(),
                }),
            };
            let result = execute(ports).await;
            let settling_started = result.as_ref().ok().map(|_| {
                events.emit(EngineEvent::FinalizationPhaseStarted {
                    phase: "settling_turn".to_string(),
                    at: leveler_core::now(),
                });
                std::time::Instant::now()
            });
            drop(events);
            (result, settling_started)
        };

        // Persist-then-forward each pumped event, in emission order. A
        // persistence failure stops persisting but keeps draining so the
        // harness never blocks; the error aborts the turn afterwards. Flush
        // markers (the side-effect barrier) are acknowledged with the current
        // persistence state: after a failed append the barrier reports the
        // failure, so the harness refuses to run the tool it was announcing.
        // Drain in batches. One `append` per event costs two database
        // round-trips, and a Multi-Agent turn out-produces that: parent plus
        // children plus background tasks all emit into this one channel, and a
        // canonical event arriving at a full channel cancels the run. Batching
        // raises the drain rate; the buffer's own size is only margin on top.
        //
        // A flush marker ends the batch it lands in. The side-effect barrier
        // must resolve only after everything emitted BEFORE it is durable, so
        // the marker cannot be acknowledged while its predecessors are still
        // sitting in the batch.
        const MAX_DRAIN_BATCH: usize = 64;
        let pump = async {
            let mut result: Result<(), EngineError> = Ok(());
            let mut batch: Vec<EngineEvent> = Vec::with_capacity(MAX_DRAIN_BATCH);
            while let Some(item) = rx.recv().await {
                let mut pending_ack = None;
                match item {
                    crate::recorders::PumpItem::Event(event) => batch.push(event),
                    crate::recorders::PumpItem::Flush(ack) => pending_ack = Some(ack),
                }
                // Opportunistically take whatever else is already queued, up to
                // the batch bound, stopping at a flush marker.
                while pending_ack.is_none() && batch.len() < MAX_DRAIN_BATCH {
                    match rx.try_recv() {
                        Ok(crate::recorders::PumpItem::Event(event)) => batch.push(event),
                        Ok(crate::recorders::PumpItem::Flush(ack)) => pending_ack = Some(ack),
                        Err(_) => break,
                    }
                }
                if !batch.is_empty() {
                    let drained = std::mem::take(&mut batch);
                    if result.is_ok() {
                        result = self
                            .log
                            .append_batch(Some(&turn_id), drained, observer)
                            .await;
                    }
                }
                if let Some(ack) = pending_ack {
                    let _ = ack.send(match &result {
                        Ok(()) => Ok(()),
                        Err(error) => Err(error.to_string()),
                    });
                }
            }
            // The one canonical event that could not be queued is still
            // persisted — it is why the run is failing, so losing it would be
            // the worst possible moment to lose an event. Its identity is also
            // what the diagnostic needs, so read that off before it moves.
            let mut overflow_diagnostic = None;
            if let Some(event) = pump_state.take_overflow() {
                overflow_diagnostic = Some((
                    event
                        .to_row()
                        .map(|(tag, _)| tag)
                        .unwrap_or_else(|_| "unknown".to_string()),
                    event.producer().to_string(),
                ));
                if result.is_ok() {
                    result = self.log.append(Some(&turn_id), event, observer).await;
                }
            }
            if result.is_ok() && pump_state.is_overloaded() {
                let (event_type, producer) = overflow_diagnostic
                    .unwrap_or_else(|| ("unknown".to_string(), "unknown".to_string()));
                result = Err(EngineError::EventBufferOverloaded {
                    event_type,
                    producer,
                    capacity: EVENT_BUFFER_CAPACITY,
                    turn_id: turn_id.as_str().to_string(),
                });
            }
            result
        };

        let ((exec_result, settling_started), pump_result) = futures::join!(exec, pump);
        // A pump failure outranks whatever the harness reported: losing
        // canonical history is the more serious fact, and the harness's
        // outcome was computed against a log that is now incomplete.
        let run_result: Result<TurnFacts<T>, EngineError> = match pump_result {
            Ok(()) => exec_result.map_err(EngineError::from),
            Err(error) => Err(error),
        };
        // Every activation announced by this turn must have a durable ending
        // before the turn itself closes. This is lifecycle settlement, not
        // advisory cleanup: leaving an open child behind would make the next
        // turn infer a crash that did not happen.
        let child_stop = if matches!(run_result, Err(EngineError::Cancelled)) {
            leveler_lifecycle::ChildStop::Cancelled
        } else {
            leveler_lifecycle::ChildStop::Lost
        };
        self.reconcile_terminal_children(child_stop, observer)
            .await
            .map_err(|error| {
                EngineError::UnclosedTerminalBoundary(format!(
                    "could not settle terminal-dependent children: {error}"
                ))
            })?;
        // The terminal event and query projection commit atomically. Forwarding
        // happens only after commit, so observers never see an uncommitted fact.
        let (terminal, stop_reason, stop, rounds, modified_files) = match &run_result {
            Ok(facts) => (
                TurnOutcome::Completed,
                format!("{:?}", facts.stop),
                Some(facts.stop),
                facts.rounds,
                facts.modified_files.clone(),
            ),
            Err(EngineError::Cancelled) => (
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
        let (event_type, payload) = event.to_row().map_err(|error| {
            EngineError::UnclosedTerminalBoundary(format!(
                "could not serialize the turn terminal: {error}"
            ))
        })?;
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
            .await
            .map_err(|error| {
                EngineError::UnclosedTerminalBoundary(format!(
                    "could not commit the turn terminal: {error}"
                ))
            })?;
        observer(event);
        if let Some(settling_started) = settling_started {
            tracing::debug!(
                phase = "settling_turn",
                elapsed_ms = settling_started.elapsed().as_millis(),
                "finalization phase finished"
            );
        }

        let facts = run_result?;

        Ok(TurnRecordedOutcome {
            turn_id,
            outcome: facts.outcome,
        })
    }

    /// Close child activations that did not report before their owning turn
    /// ended. This runs before `TurnFinished`; callers outside `run_turn` must
    /// likewise invoke it only while they still own the turn boundary.
    pub async fn reconcile_terminal_children(
        &self,
        stop: leveler_lifecycle::ChildStop,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
    ) -> Result<(), EngineError> {
        let open = self.log.unfinished_children().await?;
        for child in open {
            let event = EngineEvent::SubAgentFinished {
                id: child.id.clone(),
                nickname: child.nickname.clone(),
                ok: false,
                contribution: None,
                summary: format!(
                    "[sub-agent {}] did not report before the task reached its terminal",
                    child.nickname
                ),
                outcome: None,
                stop: Some(stop),
                limit: None,
            };
            let origin = child.turn_id.as_deref().map(TurnId::new);
            self.log.append(origin.as_ref(), event, observer).await?;
        }
        Ok(())
    }
}

/// The harness-facing view of children read off the log.
fn lost_view(children: &[crate::log::UnfinishedChild]) -> Vec<crate::LostChild> {
    children
        .iter()
        .map(|child| crate::LostChild {
            id: child.id.clone(),
            nickname: child.nickname.clone(),
            role: child.role.clone(),
        })
        .collect()
}

impl TurnRunner<'_> {
    /// Settle durable ghosts — children with a persisted `SubAgentStarted` and
    /// no terminal — into truthful terminal facts under CURRENT lost-child
    /// semantics: the activation is gone, the work is not done, the child is
    /// NOT resumed.
    ///
    /// The engine owns every mechanical part of that and delegates none of it:
    ///
    /// - the terminal says `ok: false` — the mechanical fact that this child
    ///   started and never reported. It used to also write a host-authored
    ///   BLOCKING finding so a completion could be refused over it; a lost
    ///   child is a fact to report, not a gate to hold;
    /// - the terminal is attributed to the turn the child STARTED in;
    /// - it is appended through the same persist-before-forward log, fenced on
    ///   this runtime's ownership like every other authoritative write.
    ///
    /// What the child CONTRIBUTED is the one thing the engine cannot know: it
    /// would have to read the harness's role vocabulary and its evidence
    /// record. That answer comes from [`LostChildVoice`], and a runner without
    /// one still settles every ghost — its terminals just say less.
    ///
    /// `desc` finishes the sentence "[sub-agent {nickname}] …" on the terminal
    /// event; a harness note extends it after "; ".
    async fn settle_ghost_children(
        &self,
        open: Vec<crate::log::UnfinishedChild>,
        desc: &str,
        stop: leveler_lifecycle::ChildStop,
        current_turn: &TurnId,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
    ) -> Result<(), EngineError> {
        if open.is_empty() {
            return Ok(());
        }
        let notes = match &self.lost_child_voice {
            Some(voice) => {
                let lost: Vec<crate::LostChild> = open
                    .iter()
                    .map(|child| crate::LostChild {
                        id: child.id.clone(),
                        nickname: child.nickname.clone(),
                        role: child.role.clone(),
                    })
                    .collect();
                voice.speak_for(&lost).await
            }
            None => Vec::new(),
        };
        for child in open {
            let note = notes.iter().find(|(id, _)| *id == child.id).map(|(_, n)| n);
            let summary = match note.and_then(|note| note.detail.as_deref()) {
                Some(detail) => format!("[sub-agent {}] {desc}; {detail}", child.nickname),
                None => format!("[sub-agent {}] {desc}", child.nickname),
            };
            let event = EngineEvent::SubAgentFinished {
                id: child.id.clone(),
                nickname: child.nickname.clone(),
                // Never negotiable: a child that started and never reported did
                // not succeed, whatever a harness says about it.
                ok: false,
                contribution: note.and_then(|note| note.contribution.clone()),
                summary,
                outcome: note.and_then(|note| note.outcome),
                stop: Some(stop),
                limit: None,
            };
            let origin = child.turn_id.clone().map(TurnId::new);
            let attribute_to = origin.as_ref().unwrap_or(current_turn);
            self.log.append(Some(attribute_to), event, observer).await?;
        }
        Ok(())
    }
}
