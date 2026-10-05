//! Protocol repair injected when a goal turn goes quiet.

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
/// The mechanical facts are: this Goal is unresolved, it is still active, and
/// the two calls that resolve it are `update_goal(complete)` and
/// `update_goal(blocked)`. The harness states those facts only — it does not
/// choose the next file, test, or plan item.
pub(crate) fn goal_continuation_nudge() -> String {
    "Goal remains active. Continue working toward the original goal, and resolve it \
     with update_goal(complete|blocked) when the work is finished or cannot proceed."
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_goal_nudge_states_the_lifecycle_and_does_not_choose_the_work() {
        let n = goal_continuation_nudge();
        assert!(n.contains("remains active"), "{n}");
        assert!(n.contains("update_goal(complete|blocked)"), "{n}");
        for banned in [
            "read_file",
            "apply_patch",
            "run_command",
            "plan item",
            "第一步",
            "下一步应该",
        ] {
            assert!(!n.contains(banned), "must not coach (`{banned}`): {n}");
        }
    }
}
