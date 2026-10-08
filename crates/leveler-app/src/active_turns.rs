//! Per-session ownership of active interactive turns.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

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
    /// When the turn was admitted. Turns the count into an age a handover
    /// wait can show instead of a bare `1`.
    started_at: Instant,
    /// Last observable activity signal for this turn. Advanced by
    /// [`ActiveTurns::touch`], never inferred from model identity, task
    /// complexity, or log growth — a turn that is genuinely working keeps
    /// touching this, and one that never reaches its terminal does not.
    last_activity_at: Instant,
}

/// The outcome of an atomic retirement decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetireAdmission {
    /// Admission is now closed and nothing was owed.
    Accepted,
    /// Work is still owed. NOTHING was changed: the runtime keeps admitting.
    Busy { active_turns: u32 },
    /// A previous request already closed admission.
    AlreadyRetiring,
}

/// A read-only view of one active turn, copied out without holding the map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ActiveTurnSnapshot {
    pub(crate) session_id: SessionId,
    pub(crate) elapsed_ms: u64,
    pub(crate) idle_ms: u64,
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
        // Take the admission lock BEFORE reading the retirement flag: retirement
        // is decided and committed inside this same lock (see
        // [`Self::retire_if_idle`]), so a turn can never be admitted after an
        // `Accepted` verdict, and a retirement can never be accepted while a
        // turn is mid-admission.
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.retiring.load(Ordering::SeqCst) {
            return Err(TurnAdmissionError::Retiring);
        }
        if active.contains_key(session_id) {
            return Err(TurnAdmissionError::Busy(session_id.clone()));
        }
        if active.len() >= self.capacity {
            return Err(TurnAdmissionError::Capacity(self.capacity));
        }
        let token = CancellationToken::new();
        let task_cancel = Arc::new(AtomicBool::new(false));
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed) + 1;
        let now = Instant::now();
        active.insert(
            session_id.clone(),
            ActiveTurn {
                generation,
                cancellation: token.clone(),
                task_cancel: task_cancel.clone(),
                started_at: now,
                last_activity_at: now,
            },
        );
        Ok(TurnLease {
            session_id: session_id.clone(),
            generation,
            cancellation: token,
            task_cancel,
        })
    }

    /// Decide whether this runtime may retire now, and commit the decision.
    ///
    /// ONE critical section decides and closes admission, which is the whole
    /// point: reading `active_turns == 0` outside the admission lock and
    /// setting the flag afterwards would let a turn slip in between the two,
    /// so the caller would be told "idle" about a runtime that is no longer
    /// idle. Sharing `admit`'s lock makes that interleaving impossible.
    ///
    /// Returns the turns still running rather than a bare verdict, so the
    /// caller can report what blocked it without a second, later read.
    pub(crate) fn retire_if_idle(&self) -> RetireAdmission {
        let active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.retiring.load(Ordering::SeqCst) {
            return RetireAdmission::AlreadyRetiring;
        }
        if !active.is_empty() {
            return RetireAdmission::Busy {
                active_turns: active.len() as u32,
            };
        }
        // Commit while still holding the lock: from this instant no new turn can
        // be admitted, and the emptiness this decision was based on cannot
        // change underneath it.
        self.retiring.store(true, Ordering::SeqCst);
        RetireAdmission::Accepted
    }

    /// Whether admission is closed for retirement.
    pub(crate) fn is_retiring(&self) -> bool {
        self.retiring.load(Ordering::SeqCst)
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

    /// Record observable progress for a running turn. A no-op when the session
    /// has no active turn, so an event that arrives after terminal publication
    /// can never resurrect a stale entry or advance a newer turn's clock.
    pub(crate) fn touch(&self, session_id: &SessionId) {
        if let Some(turn) = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(session_id)
        {
            turn.last_activity_at = Instant::now();
        }
    }

    /// Read-only snapshots of every active turn, oldest first — the facts a
    /// handover wait shows in place of a bare count. Copies out under one lock
    /// acquisition; callers never see the map itself.
    pub(crate) fn snapshots(&self) -> Vec<ActiveTurnSnapshot> {
        let now = Instant::now();
        let active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut out: Vec<ActiveTurnSnapshot> = active
            .iter()
            .map(|(session_id, turn)| ActiveTurnSnapshot {
                session_id: session_id.clone(),
                elapsed_ms: now.duration_since(turn.started_at).as_millis() as u64,
                idle_ms: now.duration_since(turn.last_activity_at).as_millis() as u64,
            })
            .collect();
        out.sort_by(|a, b| b.elapsed_ms.cmp(&a.elapsed_ms));
        out
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
        self.finish_with(lease, || {})
    }

    /// Publish the terminal and release its admission as one synchronized
    /// boundary. A consumer awakened by publication cannot observe the old
    /// turn as busy, and a new turn cannot overtake its terminal event.
    /// `publish` must be synchronous and must not call back into ActiveTurns.
    pub(crate) fn finish_with(&self, lease: &TurnLease, publish: impl FnOnce()) -> bool {
        let mut active = self.active.lock().unwrap();
        let owns_slot = active
            .get(&lease.session_id)
            .is_some_and(|turn| turn.generation == lease.generation);
        if owns_slot {
            publish();
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

    /// The retirement decision itself: `Busy` is a refusal that changes
    /// NOTHING. This is the difference between asking and telling — the old
    /// fire-and-forget shutdown stopped admitting work even when it was asked
    /// by a client that was wrong about the runtime being idle.
    #[test]
    fn a_busy_retirement_decision_leaves_admission_open() {
        let turns = ActiveTurns::default();
        let running = SessionId::new("running");
        let lease = turns.admit(&running).unwrap();

        assert_eq!(
            turns.retire_if_idle(),
            RetireAdmission::Busy { active_turns: 1 }
        );
        assert!(!turns.is_retiring(), "a refusal must not close admission");
        assert!(
            !lease.cancellation().is_cancelled(),
            "a refusal must not touch the running work"
        );
        assert!(
            turns.admit(&SessionId::new("other")).is_ok(),
            "other sessions must keep working after a deferred upgrade"
        );
    }

    /// An idle runtime commits: admission closes in the same critical section
    /// the emptiness was read in, so nothing can slip in behind the verdict.
    #[test]
    fn an_idle_retirement_decision_closes_admission_atomically() {
        let turns = ActiveTurns::default();
        assert_eq!(turns.retire_if_idle(), RetireAdmission::Accepted);
        assert!(turns.is_retiring());
        assert!(matches!(
            turns.admit(&SessionId::new("late")),
            Err(TurnAdmissionError::Retiring)
        ));
        assert_eq!(turns.retire_if_idle(), RetireAdmission::AlreadyRetiring);
    }

    /// The race the atomic decision exists for: a turn must never be admitted
    /// after an `Accepted` verdict, however the two interleave. Both sides take
    /// the same lock, so exactly one of the two orders is possible.
    #[test]
    fn a_turn_is_never_admitted_after_an_accepted_retirement() {
        for attempt in 0..200 {
            let turns = Arc::new(ActiveTurns::default());
            let admitted = Arc::new(AtomicBool::new(false));

            let retirer = {
                let turns = turns.clone();
                std::thread::spawn(move || turns.retire_if_idle())
            };
            let admitter = {
                let turns = turns.clone();
                let admitted = admitted.clone();
                std::thread::spawn(move || {
                    if turns.admit(&SessionId::new("racer")).is_ok() {
                        admitted.store(true, Ordering::SeqCst);
                    }
                })
            };
            let verdict = retirer.join().unwrap();
            admitter.join().unwrap();

            if verdict == RetireAdmission::Accepted {
                assert!(
                    !admitted.load(Ordering::SeqCst),
                    "attempt {attempt}: a turn was admitted after the runtime committed to retire"
                );
            } else {
                assert_eq!(
                    verdict,
                    RetireAdmission::Busy { active_turns: 1 },
                    "attempt {attempt}: only Busy or Accepted is possible"
                );
                assert!(
                    admitted.load(Ordering::SeqCst),
                    "attempt {attempt}: Busy must mean the turn is genuinely admitted"
                );
                assert!(
                    !turns.is_retiring(),
                    "attempt {attempt}: Busy changed state"
                );
            }
        }
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
        let mut published = false;
        assert!(!turns.finish_with(&old, || published = true));
        assert!(
            !published,
            "a stale turn must not publish a terminal for a newer epoch"
        );
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

    /// The snapshot is the fact a handover shows: an age, and how long since
    /// the turn last did anything observable. The age is real elapsed time —
    /// it grows with the wait — and both fields disappear with the turn.
    #[test]
    fn a_snapshot_reports_age_and_idle_and_lives_only_while_the_turn_does() {
        let turns = ActiveTurns::default();
        let session = SessionId::new("s1");
        let began = Instant::now();
        let lease = turns.admit(&session).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));

        let snapshot = turns.snapshots();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].session_id, session);
        // Bounded by the wait this test itself measured rather than by a fixed
        // millisecond budget: a busy runner can spend far longer than that
        // between `admit` and the read, and an age that tracks real time is
        // the property, not a small number.
        assert!(snapshot[0].elapsed_ms >= 15, "{:?}", snapshot[0]);
        assert!(
            snapshot[0].elapsed_ms <= began.elapsed().as_millis() as u64,
            "{:?}",
            snapshot[0]
        );
        // Idle begins at the same instant as the age and never leads it.
        assert!(snapshot[0].idle_ms >= 15, "{:?}", snapshot[0]);
        assert!(
            snapshot[0].idle_ms <= snapshot[0].elapsed_ms,
            "{:?}",
            snapshot[0]
        );

        turns.finish(&lease);
        assert!(turns.snapshots().is_empty());
    }

    /// `touch` is the only thing that advances the activity clock, and it can
    /// never resurrect a turn that has already finished.
    #[test]
    fn touch_advances_the_activity_clock_within_a_live_turn_only() {
        let turns = ActiveTurns::default();
        let session = SessionId::new("s1");
        let lease = turns.admit(&session).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(20));
        let idle_before = turns.snapshots()[0].idle_ms;
        assert!(idle_before >= 15, "{idle_before}");

        turns.touch(&session);
        let after = turns.snapshots();
        assert!(after[0].idle_ms <= 5, "{:?}", after[0]);
        assert!(
            after[0].elapsed_ms > after[0].idle_ms,
            "elapsed keeps running while idle resets: {:?}",
            after[0]
        );

        turns.finish(&lease);
        turns.touch(&session);
        assert!(
            turns.snapshots().is_empty(),
            "a late event must not resurrect a finished turn"
        );
    }
}
