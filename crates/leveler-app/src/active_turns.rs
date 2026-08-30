//! Per-session ownership of active interactive turns.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use leveler_core::SessionId;
use tokio_util::sync::CancellationToken;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum TurnAdmissionError {
    #[error("session {0} already has an active turn")]
    Busy(SessionId),
    #[error("interactive runtime is at its {0}-turn capacity")]
    Capacity(usize),
    /// The runtime is retiring — it agreed to hand over to a newer build and
    /// is waiting for its current work to finish. Taking new work here would
    /// postpone the handover indefinitely, which is exactly how a stale
    /// runtime survives forever.
    #[error("interactive runtime is retiring and is not taking new work")]
    Retiring,
}

pub(crate) struct ActiveTurns {
    active: Mutex<HashMap<SessionId, CancellationToken>>,
    capacity: usize,
    /// Shared with the runtime's shutdown flag: admission is where retiring
    /// has to bite, because reporting `accepting_work: false` while still
    /// accepting work is just a label.
    retiring: Arc<AtomicBool>,
}

impl Default for ActiveTurns {
    fn default() -> Self {
        Self {
            active: Mutex::new(HashMap::new()),
            capacity: 4,
            retiring: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl ActiveTurns {
    /// Share the runtime's retirement flag, so a retiring runtime actually
    /// refuses work rather than merely saying it would.
    pub(crate) fn with_retiring(retiring: Arc<AtomicBool>) -> Self {
        Self {
            retiring,
            ..Self::default()
        }
    }

    /// `(active main turns, admission capacity)` — the real limit `admit`
    /// enforces, surfaced for runtime health.
    pub(crate) fn load(&self) -> (usize, usize) {
        let active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        (active, self.capacity)
    }

    pub(crate) fn admit(
        &self,
        session_id: &SessionId,
    ) -> Result<CancellationToken, TurnAdmissionError> {
        if self.retiring.load(Ordering::SeqCst) {
            return Err(TurnAdmissionError::Retiring);
        }
        let mut active = self.active.lock().unwrap();
        if active.contains_key(session_id) {
            return Err(TurnAdmissionError::Busy(session_id.clone()));
        }
        if active.len() >= self.capacity {
            return Err(TurnAdmissionError::Capacity(self.capacity));
        }
        let token = CancellationToken::new();
        active.insert(session_id.clone(), token.clone());
        Ok(token)
    }

    /// Whether a main turn is currently running for this session.
    ///
    /// Used to decide whether mid-turn input can be steered into the running
    /// loop or should be rejected so the caller submits it normally.
    pub(crate) fn is_running(&self, session_id: &SessionId) -> bool {
        self.active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(session_id)
    }

    pub(crate) fn cancel(&self, session_id: &SessionId) -> bool {
        if let Some(token) = self.active.lock().unwrap().get(session_id) {
            token.cancel();
            true
        } else {
            false
        }
    }

    pub(crate) fn finish(&self, session_id: &SessionId) {
        self.active.lock().unwrap().remove(session_id);
    }

    pub(crate) fn cancel_all(&self) {
        for (_, token) in self.active.lock().unwrap().drain() {
            token.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The defect this test exists for: `accepting_work: false` was reported
    /// while admission still said yes. A runtime that keeps taking work never
    /// reaches the idle it promised to retire at, so the handover never
    /// happens — the stale runtime simply outlives everyone.
    #[test]
    fn a_retiring_runtime_refuses_new_turns() {
        let retiring = Arc::new(AtomicBool::new(false));
        let turns = ActiveTurns::with_retiring(retiring.clone());
        let session = SessionId::new("s1");

        let admitted = turns.admit(&session).expect("a live runtime takes work");
        turns.finish(&session);
        drop(admitted);

        retiring.store(true, Ordering::SeqCst);
        assert!(
            matches!(turns.admit(&session), Err(TurnAdmissionError::Retiring)),
            "a retiring runtime must refuse work, not merely report that it would"
        );
    }

    #[test]
    fn same_session_has_exactly_one_active_turn() {
        let turns = ActiveTurns::default();
        let session = SessionId::new("a");
        let first = turns.admit(&session).unwrap();
        assert!(matches!(
            turns.admit(&session),
            Err(TurnAdmissionError::Busy(id)) if id == session
        ));
        assert!(
            !first.is_cancelled(),
            "rejected admission must not replace it"
        );
    }

    #[test]
    fn cancel_is_scoped_to_the_target_session() {
        let turns = ActiveTurns::default();
        let a = SessionId::new("a");
        let b = SessionId::new("b");
        let token_a = turns.admit(&a).unwrap();
        let token_b = turns.admit(&b).unwrap();

        assert!(turns.cancel(&a));
        assert!(token_a.is_cancelled());
        assert!(!token_b.is_cancelled());
        assert!(!turns.cancel(&SessionId::new("missing")));
    }

    #[test]
    fn capacity_is_explicit_and_finishing_releases_it() {
        let turns = ActiveTurns {
            capacity: 2,
            ..Default::default()
        };
        let a = SessionId::new("a");
        let b = SessionId::new("b");
        turns.admit(&a).unwrap();
        turns.admit(&b).unwrap();
        assert!(matches!(
            turns.admit(&SessionId::new("c")),
            Err(TurnAdmissionError::Capacity(2))
        ));
        turns.finish(&a);
        assert!(turns.admit(&SessionId::new("c")).is_ok());
    }
}
