//! Per-session ownership of active interactive turns.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

#[derive(Clone)]
pub(crate) struct TurnLease {
    session_id: SessionId,
    generation: u64,
    cancellation: CancellationToken,
    /// Set when the user asks to cancel the logical task rather than merely
    /// interrupt the running turn. The engine reads it at terminal time to
    /// distinguish `cancelled` from `interrupted`.
    task_cancel: Arc<AtomicBool>,
}

impl TurnLease {
    pub(crate) fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    /// The explicit-task-cancellation flag this turn runs under.
    pub(crate) fn task_cancel(&self) -> Arc<AtomicBool> {
        self.task_cancel.clone()
    }
}

struct ActiveTurn {
    generation: u64,
    cancellation: CancellationToken,
    task_cancel: Arc<AtomicBool>,
}

pub(crate) struct ActiveTurns {
    active: Mutex<HashMap<SessionId, ActiveTurn>>,
    next_generation: AtomicU64,
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
            next_generation: AtomicU64::new(0),
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

    pub(crate) fn admit(&self, session_id: &SessionId) -> Result<TurnLease, TurnAdmissionError> {
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
        let task_cancel = Arc::new(AtomicBool::new(false));
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed) + 1;
        active.insert(
            session_id.clone(),
            ActiveTurn {
                generation,
                cancellation: token.clone(),
                task_cancel: task_cancel.clone(),
            },
        );
        Ok(TurnLease {
            session_id: session_id.clone(),
            generation,
            cancellation: token,
            task_cancel,
        })
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
        if let Some(turn) = self.active.lock().unwrap().get(session_id) {
            turn.cancellation.cancel();
            true
        } else {
            false
        }
    }

    /// Cancel the LOGICAL task: mark the running turn as an explicit task
    /// cancellation (so its terminal is `cancelled`, not resumable
    /// `interrupted`) and stop it.
    ///
    /// Returns `false` when nothing is running — the caller then commits the
    /// cancellation against the persisted task instead.
    pub(crate) fn cancel_task(&self, session_id: &SessionId) -> bool {
        if let Some(turn) = self.active.lock().unwrap().get(session_id) {
            turn.task_cancel.store(true, Ordering::SeqCst);
            turn.cancellation.cancel();
            true
        } else {
            false
        }
    }

    /// Release only the epoch represented by `lease`. Repeating release is
    /// harmless, and a stale turn can never remove a newer turn admitted for
    /// the same session after terminal publication.
    pub(crate) fn finish(&self, lease: &TurnLease) -> bool {
        let mut active = self.active.lock().unwrap();
        let owns_slot = active
            .get(&lease.session_id)
            .is_some_and(|turn| turn.generation == lease.generation);
        if owns_slot {
            active.remove(&lease.session_id);
        }
        owns_slot
    }

    pub(crate) fn cancel_all(&self) {
        for (_, turn) in self.active.lock().unwrap().drain() {
            turn.cancellation.cancel();
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
        turns.finish(&admitted);
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
            !first.cancellation().is_cancelled(),
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
        assert!(token_a.cancellation().is_cancelled());
        assert!(!token_b.cancellation().is_cancelled());
        assert!(!turns.cancel(&SessionId::new("missing")));
    }

    /// The explicit-task-cancel flag is shared with the lease: the engine reads
    /// the same flag the runtime sets, so a cooperative stop becomes terminal.
    #[test]
    fn cancel_task_marks_the_lease_and_interrupt_does_not() {
        let turns = ActiveTurns::default();
        let session = SessionId::new("cancel-task-share");
        let lease = turns.admit(&session).unwrap();
        assert!(!lease.task_cancel().load(Ordering::SeqCst));

        assert!(turns.cancel_task(&session));
        assert!(lease.task_cancel().load(Ordering::SeqCst));
        assert!(lease.cancellation().is_cancelled());

        // A plain interrupt must not mark the task as cancelled.
        let other = SessionId::new("plain-interrupt");
        let plain = turns.admit(&other).unwrap();
        assert!(turns.cancel(&other));
        assert!(!plain.task_cancel().load(Ordering::SeqCst));
    }

    #[test]
    fn capacity_is_explicit_and_finishing_releases_it() {
        let turns = ActiveTurns {
            capacity: 2,
            ..Default::default()
        };
        let a = SessionId::new("a");
        let b = SessionId::new("b");
        let lease_a = turns.admit(&a).unwrap();
        turns.admit(&b).unwrap();
        assert!(matches!(
            turns.admit(&SessionId::new("c")),
            Err(TurnAdmissionError::Capacity(2))
        ));
        turns.finish(&lease_a);
        assert!(turns.admit(&SessionId::new("c")).is_ok());
    }

    #[test]
    fn a_stale_release_cannot_remove_a_newer_turn_for_the_same_session() {
        let turns = ActiveTurns::default();
        let session = SessionId::new("same-session");
        let old = turns.admit(&session).unwrap();
        assert!(turns.finish(&old));

        let current = turns.admit(&session).unwrap();
        assert!(
            !turns.finish(&old),
            "the old wrapper's late release must be an idempotent no-op"
        );
        assert!(turns.is_running(&session));
        assert!(matches!(
            turns.admit(&session),
            Err(TurnAdmissionError::Busy(id)) if id == session
        ));
        assert!(turns.cancel(&session));
        assert!(current.cancellation().is_cancelled());
    }
}
