//! The one mechanical guard the loop applies to a proposed tool call.
//!
//! What is left here is deliberately narrow: the SAME call, with the same
//! arguments, returning the same result, repeated past a threshold. That is a
//! fact the runtime can establish from its own record, and bounding it is what
//! stops a runaway loop.
//!
//! What used to live here and does not any more is the judgement half — "you
//! have searched enough, act on what you found". The runtime cannot establish
//! that: in a large repository a long grep/glob/read chain is often exactly
//! right, and when to stop looking and start writing is the model's strategy,
//! not a safety boundary. Nothing here can permit a call admission would
//! refuse, and nothing here is required for a correct run (see
//! [`TurnPolicy::minimal`]).
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
