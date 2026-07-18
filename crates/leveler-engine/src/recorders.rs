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
#[derive(Clone)]
pub(crate) struct EventEmitter {
    tx: Sender<EngineEvent>,
    state: EventPumpState,
    cancel: CancellationToken,
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
    ) -> (Self, Receiver<EngineEvent>, EventPumpState) {
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
        match self.tx.try_send(event) {
            Ok(()) => {}
            Err(TrySendError::Full(event) | TrySendError::Closed(event))
                if event.is_transient() => {}
            Err(TrySendError::Full(event) | TrySendError::Closed(event)) => {
                if !self.state.overloaded.swap(true, Ordering::AcqRel) {
                    *self.state.overflow.lock().unwrap() = Some(event);
                    self.cancel.cancel();
                }
            }
        }
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

pub struct RecordingApprover {
    pub inner: Arc<dyn Approver>,
    pub(crate) events: EventEmitter,
    pub(crate) turn_id: TurnId,
}

#[async_trait]
impl Approver for RecordingApprover {
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

        assert!(
            matches!(rx.recv().await, Some(EngineEvent::AssistantDelta { text }) if text == "first")
        );
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
            Some(EngineEvent::AssistantDelta { .. })
        ));
        assert!(rx.recv().await.is_none());
        assert!(matches!(
            state.take_overflow(),
            Some(EngineEvent::VerificationStarted)
        ));
    }
}
