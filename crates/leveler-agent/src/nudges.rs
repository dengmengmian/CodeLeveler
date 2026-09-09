//! Goal-persistence nudge injected into the loop.

use leveler_model::{Message, Role};

pub(crate) fn first_user_text(messages: &[Message]) -> String {
    messages
        .iter()
        .find(|m| m.role == Role::User)
        .map(Message::text_content)
        .unwrap_or_default()
}

/// Goal mode: the model ended a round without calling `update_goal`.
///
/// This repairs the PROTOCOL and nothing else. The only fact behind it is
/// mechanical — a goal run resolves through `update_goal`, and this round did
/// not — so the text says exactly that and names the two calls that resolve
/// it. Whether the task was conversational or an implementation, whether the
/// workspace deserves an audit, whether tests should run now, whether every
/// requirement is proven: those are readings of the user's intent. They live
/// in the system prompt and in the model, not in a reminder the harness
/// injects mid-turn.
pub(crate) fn goal_resolve_nudge() -> String {
    "You ended this round without resolving the active goal.\n\n\
     If the goal is complete, call update_goal(status=\"complete\", summary=…).\n\
     If it cannot be completed as stated, call update_goal(status=\"blocked\", summary=…).\n\
     If more work is needed, keep working."
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// It states the protocol and stops. Every phrase this asserts against is
    /// one the runtime used to inject and has no standing to: a task-shape
    /// classification, a proof standard, an instruction about tests, or a
    /// restatement of the objective the model already has.
    #[test]
    fn the_goal_nudge_repairs_the_protocol_and_teaches_nothing() {
        let n = goal_resolve_nudge();
        assert!(n.contains("update_goal"), "{n}");
        assert!(n.contains("complete"), "{n}");
        assert!(n.contains("blocked"), "{n}");
        for banned in [
            "Conversational",
            "Implementation",
            "Follow-up",
            "PROVEN",
            "audit",
            "shrink",
            "<objective>",
        ] {
            assert!(!n.contains(banned), "must not coach (`{banned}`): {n}");
        }
    }
}
