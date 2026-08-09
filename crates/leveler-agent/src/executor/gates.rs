//! Product gates as pure decisions (convergence plan phase 5).
//!
//! These are the *judgements* the loop makes about a proposed tool call —
//! "this task needs a plan first", "you have searched enough", "you have
//! re-read the same thing three times". They are product policy, not safety:
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

/// Facts about the plan gate at the moment a call is proposed.
pub(crate) struct PlanGate {
    /// The task looks like it has several independently checkable steps AND
    /// the policy asks for an explicit plan.
    pub(crate) required: bool,
    /// A structured plan already exists.
    pub(crate) plan_started: bool,
    /// Whether `tool` is one of the read-only explore / navigation tools.
    pub(crate) tool_is_explore: bool,
    /// Whether `tool` is exempt outright (planning itself, and the two ways to
    /// ask the human something — a gate must never mute those).
    pub(crate) tool_is_exempt: bool,
}

/// Product gate for multi-step work that expects a structured plan.
///
/// C2.3A invariant: a missing plan may still block *mutations*, but it must
/// never deny Search / Read / Symbol / Reference (or other explore tools).
/// Plan is an execution aid, not a license to navigate code.
pub(crate) fn plan_gate(facts: &PlanGate) -> GateVerdict {
    if !facts.required || facts.plan_started || facts.tool_is_exempt {
        return GateVerdict::Allow;
    }
    // Navigation stays open without a plan. Soft plan nudges live in the drive
    // loop and never strip tools or force ToolChoice.
    if facts.tool_is_explore {
        return GateVerdict::Allow;
    }
    GateVerdict::Refuse(
        "This task has multiple independently verifiable steps. Call \
         update_plan first with one in_progress step and the remaining \
         steps pending; a prose checklist does not satisfy the plan gate. \
         Read-only navigation tools (read/grep/list/search/symbol/refs) \
         remain available while you gather evidence."
            .to_string(),
    )
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

    fn facts(explore: bool) -> PlanGate {
        PlanGate {
            required: true,
            plan_started: false,
            tool_is_explore: explore,
            tool_is_exempt: false,
        }
    }

    #[test]
    fn the_plan_gate_is_silent_when_not_required() {
        let mut f = facts(false);
        f.required = false;
        assert_eq!(plan_gate(&f), GateVerdict::Allow);
    }

    #[test]
    fn the_plan_gate_is_silent_once_a_plan_exists() {
        let mut f = facts(false);
        f.plan_started = true;
        assert_eq!(plan_gate(&f), GateVerdict::Allow);
    }

    #[test]
    fn a_write_before_the_plan_is_refused_with_a_next_step() {
        let verdict = plan_gate(&facts(false));
        match verdict {
            GateVerdict::Refuse(msg) => {
                assert!(
                    msg.contains("update_plan"),
                    "the refusal must say what to do: {msg}"
                );
                assert!(
                    msg.contains("navigation") || msg.contains("remain available"),
                    "refusal must leave navigation open in the message: {msg}"
                );
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// C2.3A: missing plan never blocks read/search/symbol-class tools, no
    /// matter how many explore rounds already ran.
    #[test]
    fn navigation_stays_open_without_a_plan() {
        assert_eq!(plan_gate(&facts(true)), GateVerdict::Allow);
    }

    /// A gate must never be able to silence the ways of asking the human, or a
    /// blocked run has no way out.
    #[test]
    fn asking_the_user_is_never_gated() {
        for tool in ["update_plan", "ask_user", "request_user_input"] {
            let mut f = facts(false);
            f.tool_is_exempt = true;
            assert_eq!(
                plan_gate(&f),
                GateVerdict::Allow,
                "{tool} must stay reachable"
            );
        }
    }

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
