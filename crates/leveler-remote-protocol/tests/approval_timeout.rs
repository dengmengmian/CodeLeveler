//! The dual-waiter rule for remote approval timeouts.
//!
//! A remote-only approval must not hang forever, so it auto-denies. But the
//! same timer aimed at a desktop user would be hostile: someone reading a diff
//! in their terminal would have the prompt yanked away because a phone happened
//! to be paired. So the timer tracks *who can still answer*, and the transition
//! that matters is when the last local waiter leaves — the countdown starts
//! then, not when the approval was first raised.
#![cfg(feature = "policy")]

use leveler_remote_protocol::policy::{ApprovalTimeoutState, TimerTransition, Waiters};

fn waiters(local: usize, remote: usize) -> Waiters {
    Waiters { local, remote }
}

/// Nobody is at the keyboard: without a timer this approval blocks the turn
/// until the phone comes back, which may be never.
#[test]
fn remote_only_arms_the_timer() {
    let mut state = ApprovalTimeoutState::new();
    assert_eq!(state.observe(waiters(0, 1)), TimerTransition::Arm);
    assert!(state.is_armed());
}

/// A desktop user owns their own prompt; nothing remote should expire it.
#[test]
fn local_only_never_arms() {
    let mut state = ApprovalTimeoutState::new();
    assert_eq!(state.observe(waiters(1, 0)), TimerTransition::Unchanged);
    assert!(!state.is_armed());
}

/// With both present the desktop user is still there, so the remote policy must
/// not interrupt them.
#[test]
fn both_present_does_not_arm() {
    let mut state = ApprovalTimeoutState::new();
    assert_eq!(state.observe(waiters(2, 1)), TimerTransition::Unchanged);
    assert!(!state.is_armed());
}

/// The re-arming case: the desktop user closed their terminal and only the
/// phone is left. The countdown begins now — an approval that sat unanswered
/// for an hour with someone watching must not auto-deny the instant they leave.
#[test]
fn losing_the_last_local_waiter_arms_from_that_moment() {
    let mut state = ApprovalTimeoutState::new();
    assert_eq!(state.observe(waiters(1, 1)), TimerTransition::Unchanged);
    assert!(!state.is_armed());

    assert_eq!(
        state.observe(waiters(0, 1)),
        TimerTransition::Arm,
        "the arm transition is what restarts the countdown"
    );
    assert!(state.is_armed());
}

/// A returning local waiter takes ownership back and cancels the countdown.
#[test]
fn a_local_waiter_returning_disarms() {
    let mut state = ApprovalTimeoutState::new();
    state.observe(waiters(0, 1));
    assert!(state.is_armed());

    assert_eq!(state.observe(waiters(1, 1)), TimerTransition::Disarm);
    assert!(!state.is_armed());
}

/// The phone disconnected: there is no remote client left to protect against,
/// and auto-denying would resolve an approval nobody is waiting on.
#[test]
fn losing_every_remote_stream_disarms() {
    let mut state = ApprovalTimeoutState::new();
    state.observe(waiters(0, 1));
    assert!(state.is_armed());

    assert_eq!(state.observe(waiters(0, 0)), TimerTransition::Disarm);
    assert!(!state.is_armed());
}

/// Re-observing the same population must not restart the countdown, or a
/// reconnecting sibling stream would keep pushing the deadline out forever.
#[test]
fn an_unchanged_population_does_not_rearm() {
    let mut state = ApprovalTimeoutState::new();
    assert_eq!(state.observe(waiters(0, 1)), TimerTransition::Arm);

    for _ in 0..5 {
        assert_eq!(
            state.observe(waiters(0, 1)),
            TimerTransition::Unchanged,
            "an already-armed timer keeps its original deadline"
        );
    }
    assert!(state.is_armed());

    // A second phone joining is still remote-only: same verdict, same deadline.
    assert_eq!(state.observe(waiters(0, 2)), TimerTransition::Unchanged);
    assert!(state.is_armed());
}

/// Answering resolves the approval, so the timer has nothing left to fire at.
#[test]
fn resolving_disarms_regardless_of_population() {
    let mut state = ApprovalTimeoutState::new();
    state.observe(waiters(0, 1));
    assert!(state.is_armed());

    assert_eq!(state.resolved(), TimerTransition::Disarm);
    assert!(!state.is_armed());

    // Already resolved: nothing to disarm twice.
    assert_eq!(state.resolved(), TimerTransition::Unchanged);
}

/// The whole matrix in one place, as the design states it.
#[test]
fn the_documented_matrix_holds() {
    let cases = [
        // (local, remote, should_be_armed)
        (0, 1, true),  // remote only
        (0, 3, true),  // several phones, still nobody local
        (1, 0, false), // local only
        (1, 1, false), // both
        (3, 2, false), // both, several each
        (0, 0, false), // nobody
    ];

    for (local, remote, expected) in cases {
        let mut state = ApprovalTimeoutState::new();
        state.observe(waiters(local, remote));
        assert_eq!(
            state.is_armed(),
            expected,
            "local={local} remote={remote} should be armed={expected}"
        );
    }
}
