//! Approver/clarifier wrappers that record every request/decision into the
//! event stream — approvals were never persisted before the engine.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::sync::mpsc::{Receiver, Sender, error::TrySendError};
use tokio_util::sync::CancellationToken;

use leveler_agent::{ClarificationRequest, Clarifier};
use leveler_core::TurnId;
use leveler_execution::{ApprovalDecision, ApprovalRequest, Approver};

use crate::EngineEvent;

/// Bounded bridge from the executor's synchronous observer to the async event
/// log pump. Transient deltas may be dropped under pressure. The first
/// canonical event that cannot enter the queue is retained separately and
/// turns the run into an explicit overload failure.
///
/// The channel carries both events and flush markers, so a [`Self::flush`]
/// resolves exactly when the pump has durably appended everything emitted
/// before it — the side-effect barrier rides the same ordered queue and can
/// never race the events it waits for.
#[derive(Clone)]
pub(crate) struct EventEmitter {
    tx: Sender<PumpItem>,
    state: EventPumpState,
    cancel: CancellationToken,
}

/// One unit of pump work: an event to persist-then-forward, or a flush marker
/// to acknowledge once every item before it has been handled.
pub(crate) enum PumpItem {
    Event(EngineEvent),
    Flush(tokio::sync::oneshot::Sender<Result<(), String>>),
}

#[derive(Clone)]
pub(crate) struct EventPumpState {
    overloaded: Arc<AtomicBool>,
    overflow: Arc<Mutex<Option<EngineEvent>>>,
}

impl EventEmitter {
    pub(crate) fn channel(
        capacity: usize,
        cancel: CancellationToken,
    ) -> (Self, Receiver<PumpItem>, EventPumpState) {
        let (tx, rx) = tokio::sync::mpsc::channel(capacity);
        let state = EventPumpState {
            overloaded: Arc::new(AtomicBool::new(false)),
            overflow: Arc::new(Mutex::new(None)),
        };
        (
            Self {
                tx,
                state: state.clone(),
                cancel,
            },
            rx,
            state,
        )
    }

    pub(crate) fn emit(&self, event: EngineEvent) {
        if self.state.is_overloaded() {
            return;
        }
        match self.tx.try_send(PumpItem::Event(event)) {
            Ok(()) => {}
            Err(
                TrySendError::Full(PumpItem::Event(event))
                | TrySendError::Closed(PumpItem::Event(event)),
            ) if event.is_transient() => {}
            Err(
                TrySendError::Full(PumpItem::Event(event))
                | TrySendError::Closed(PumpItem::Event(event)),
            ) => {
                if !self.state.overloaded.swap(true, Ordering::AcqRel) {
                    *self.state.overflow.lock().unwrap() = Some(event);
                    self.cancel.cancel();
                }
            }
            // try_send returns what we sent; we only send Event here.
            Err(
                TrySendError::Full(PumpItem::Flush(_)) | TrySendError::Closed(PumpItem::Flush(_)),
            ) => {
                unreachable!("emit only sends PumpItem::Event")
            }
        }
    }

    /// Resolve once the pump has durably appended every event emitted before
    /// this call, or report why it cannot. Uses an awaiting send (never drops
    /// the marker): the barrier must not silently degrade under load.
    pub(crate) async fn flush(&self) -> Result<(), String> {
        if self.state.is_overloaded() {
            return Err("event pump overloaded; canonical event was dropped".into());
        }
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(PumpItem::Flush(ack_tx))
            .await
            .map_err(|_| "event pump closed before flush".to_string())?;
        ack_rx
            .await
            .map_err(|_| "event pump dropped the flush ack".to_string())?
    }
}

impl EventPumpState {
    pub(crate) fn is_overloaded(&self) -> bool {
        self.overloaded.load(Ordering::Acquire)
    }

    pub(crate) fn take_overflow(&self) -> Option<EngineEvent> {
        self.overflow.lock().unwrap().take()
    }
}

/// The engine's side-effect barrier: flushing the turn's event pump makes
/// every canonical event emitted so far durable (see `EventLog`). Handed to
/// the executor via [`leveler_agent::Executor::with_event_barrier`].
pub(crate) struct PumpBarrier {
    pub(crate) events: EventEmitter,
}

#[async_trait]
impl leveler_agent::EventBarrier for PumpBarrier {
    async fn flush(&self) -> Result<(), leveler_agent::AgentError> {
        self.events
            .flush()
            .await
            .map_err(leveler_agent::AgentError::Persistence)
    }

    /// Enqueue a delegated agent's tool event on the SAME queue `flush`
    /// drains. That ordering is the whole point: a separate channel would let
    /// the flush marker overtake the event, and the barrier would report a
    /// side effect durable that is not recorded yet.
    fn record_child_tool_event(&self, event: leveler_agent::ChildToolEvent) {
        self.events.emit(match event {
            leveler_agent::ChildToolEvent::Started {
                agent_id,
                call_id,
                name,
                arguments,
            } => EngineEvent::ToolCallStarted {
                call_id,
                name,
                arguments,
                parallel: false,
                // Stamped by the pump, which owns the registry.
                risk: None,
                agent_id: Some(agent_id),
            },
            leveler_agent::ChildToolEvent::Finished {
                agent_id,
                call_id,
                name,
                is_error,
                preview,
            } => EngineEvent::ToolCallFinished {
                call_id,
                name,
                is_error,
                preview,
                agent_id: Some(agent_id),
            },
        });
    }
}

pub struct RecordingApprover {
    pub inner: Arc<dyn Approver>,
    pub(crate) events: EventEmitter,
    pub(crate) turn_id: TurnId,
}

#[async_trait]
impl Approver for RecordingApprover {
    /// Delegate: this only records: wrapping a headless approver must not make
    /// the run look attended, or its denials get reported as user refusals.
    fn has_human(&self) -> bool {
        self.inner.has_human()
    }

    async fn decide(&self, request: &ApprovalRequest) -> ApprovalDecision {
        let mut request = request.clone();
        request.turn_id = Some(self.turn_id.clone());
        self.events.emit(EngineEvent::ApprovalRequested {
            id: request.id.clone(),
            tool: request.tool.clone(),
            summary: request.description.clone(),
            command: request.command.clone(),
            risk: format!("{:?}", request.risk),
        });
        let decision = self.inner.decide(&request).await;
        self.events.emit(EngineEvent::ApprovalResolved {
            id: request.id.clone(),
            decision: match decision {
                ApprovalDecision::ApproveOnce => "approve_once".to_string(),
                ApprovalDecision::ApproveSession => "approve_session".to_string(),
                ApprovalDecision::ApproveAlways => "approve_always".to_string(),
                ApprovalDecision::Deny => "deny".to_string(),
            },
        });
        decision
    }
}

pub struct RecordingClarifier {
    pub inner: Arc<dyn Clarifier>,
    pub(crate) events: EventEmitter,
    pub(crate) turn_id: TurnId,
}

#[async_trait]
impl Clarifier for RecordingClarifier {
    async fn clarify(&self, request: &ClarificationRequest) -> String {
        let mut request = request.clone();
        request.turn_id = Some(self.turn_id.clone());
        self.events.emit(EngineEvent::ClarificationRequested {
            id: request.id.clone(),
            question: request.question.clone(),
            options: request.options.clone(),
        });
        let answer = self.inner.clarify(&request).await;
        self.events.emit(EngineEvent::ClarificationAnswered {
            id: request.id.clone(),
            answer: answer.clone(),
        });
        answer
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn transient_overflow_is_lossy_without_failing_the_turn() {
        let (emitter, mut rx, state) =
            EventEmitter::channel(1, tokio_util::sync::CancellationToken::new());
        emitter.emit(EngineEvent::AssistantDelta {
            text: "first".into(),
        });
        emitter.emit(EngineEvent::AssistantDelta {
            text: "dropped".into(),
        });
        drop(emitter);

        assert!(matches!(
            rx.recv().await,
            Some(PumpItem::Event(EngineEvent::AssistantDelta { text })) if text == "first"
        ));
        assert!(rx.recv().await.is_none());
        assert!(!state.is_overloaded());
        assert!(state.take_overflow().is_none());
    }

    #[tokio::test]
    async fn canonical_overflow_is_retained_and_reported() {
        let cancel = tokio_util::sync::CancellationToken::new();
        let (emitter, mut rx, state) = EventEmitter::channel(1, cancel.clone());
        emitter.emit(EngineEvent::AssistantDelta {
            text: "first".into(),
        });
        emitter.emit(EngineEvent::VerificationStarted);
        drop(emitter);

        assert!(cancel.is_cancelled());
        assert!(state.is_overloaded());
        assert!(matches!(
            rx.recv().await,
            Some(PumpItem::Event(EngineEvent::AssistantDelta { .. }))
        ));
        assert!(rx.recv().await.is_none());
        assert!(matches!(
            state.take_overflow(),
            Some(EngineEvent::VerificationStarted)
        ));
    }

    /// A flush marker is acknowledged only after the pump has consumed every
    /// item queued before it, and an overloaded pump refuses the flush — the
    /// barrier never resolves optimistically.
    #[tokio::test]
    async fn flush_after_overload_reports_the_failure() {
        let cancel = tokio_util::sync::CancellationToken::new();
        let (emitter, _rx, state) = EventEmitter::channel(1, cancel.clone());
        emitter.emit(EngineEvent::AssistantDelta { text: "a".into() });
        emitter.emit(EngineEvent::VerificationStarted); // overflows: canonical
        assert!(state.is_overloaded());
        let err = emitter
            .flush()
            .await
            .expect_err("overloaded flush must fail");
        assert!(err.contains("overloaded"), "unexpected error: {err}");
    }
}

#[cfg(test)]
mod approver_delegation_tests {
    use super::*;
    use leveler_execution::AutoApprove;

    /// A decorator that answers `has_human()` for itself re-labels a headless
    /// run as an attended one, and every denial downstream then reads as "the
    /// user refused" — for something no user ever saw.
    #[tokio::test]
    async fn recording_approver_reports_the_inner_approver_s_humanity() {
        let (events, _rx, _state) =
            EventEmitter::channel(4, tokio_util::sync::CancellationToken::new());
        let recorder = RecordingApprover {
            inner: Arc::new(AutoApprove),
            events,
            turn_id: TurnId::new("t1"),
        };
        assert!(
            !recorder.has_human(),
            "wrapping AutoApprove must not conjure a human"
        );
    }
}
