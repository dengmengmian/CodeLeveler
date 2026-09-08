//! Product gates as pure decisions (convergence plan phase 5).
//!
//! These are the *judgements* the loop makes about a proposed tool call —
//! "you have searched enough", "you have re-read the same thing three times".
//! A missing plan is deliberately NOT one of them: the plan is the model's
//! cognitive aid, never a mutation license. They are product policy, not safety:
//! nothing here can permit a call that admission would refuse, and nothing
//! here is required for a correct or safe run (see [`TurnPolicy::minimal`]).
//!
//! Keeping them as pure functions over declared facts means the loop reads as
//! "gather facts → ask → act on the signal", each gate can be tested without a
//! model or a workspace, and the phase-5 migration to an external policy has a
//! seam to move rather than a rewrite to attempt.
//!
//! [`TurnPolicy::minimal`]: crate::TurnPolicy::minimal

use crate::executor::LOOP_GUARD_THRESHOLD;

/// A gate's verdict on one proposed call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GateVerdict {
    /// No product gate objects; admission decides from here.
    Allow,
    /// Refuse before execution and feed this reason back to the model.
    Refuse(String),
}

impl GateVerdict {
    #[cfg(test)]
    pub(crate) fn refused(&self) -> bool {
        matches!(self, GateVerdict::Refuse(_))
    }
}

/// Consecutive-search cap: act on what you already found (spec §17).
/// `consecutive` counts this call. A cap of 0 disables the gate.
pub(crate) fn search_budget_gate(
    tool_is_search: bool,
    consecutive: usize,
    cap: usize,
) -> GateVerdict {
    if cap == 0 || !tool_is_search || consecutive <= cap {
        return GateVerdict::Allow;
    }
    GateVerdict::Refuse(format!(
        "Search budget reached ({cap} consecutive searches). Use the results you \
         already have and take an action (read a specific file or edit) \
         instead of searching again."
    ))
}

/// Identical-result loop guard: the same observation class produced the same
/// output [`LOOP_GUARD_THRESHOLD`] times and moved nothing forward.
pub(crate) fn loop_guard(tool: &str, repeats: u32) -> GateVerdict {
    if repeats < LOOP_GUARD_THRESHOLD {
        return GateVerdict::Allow;
    }
    GateVerdict::Refuse(format!(
        "This exact `{tool}` call already ran {LOOP_GUARD_THRESHOLD} times with the same \
         result and made no progress. Do something different — change the arguments or take \
         another action."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_search_budget_refuses_only_past_the_cap() {
        assert_eq!(search_budget_gate(true, 2, 2), GateVerdict::Allow);
        assert!(search_budget_gate(true, 3, 2).refused());
        // A cap of zero disables the gate entirely (minimal policy).
        assert_eq!(search_budget_gate(true, 99, 0), GateVerdict::Allow);
        // Non-search calls are never counted against it.
        assert_eq!(search_budget_gate(false, 99, 2), GateVerdict::Allow);
    }

    #[test]
    fn the_loop_guard_fires_at_the_threshold_and_names_the_tool() {
        assert_eq!(loop_guard("list_files", 0), GateVerdict::Allow);
        assert_eq!(
            loop_guard("list_files", LOOP_GUARD_THRESHOLD - 1),
            GateVerdict::Allow
        );
        match loop_guard("list_files", LOOP_GUARD_THRESHOLD) {
            GateVerdict::Refuse(msg) => assert!(msg.contains("list_files"), "{msg}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }
}
