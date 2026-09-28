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
/// The only fact is mechanical: this Goal is unresolved, and the two calls
/// that resolve it are `update_goal(complete)` and `update_goal(blocked)`.
pub(crate) fn goal_resolve_nudge() -> String {
    "This Goal has not been resolved. Resolve it with update_goal(complete|blocked).".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_goal_nudge_states_the_protocol_and_does_not_choose_the_work() {
        let n = goal_resolve_nudge();
        assert!(n.contains("has not been resolved"), "{n}");
        assert!(n.contains("update_goal(complete|blocked)"), "{n}");
        for banned in [
            "keep working",
            "continue",
            "investigate",
            "Conversational",
            "Implementation",
            "Greeting",
            "start coding",
        ] {
            assert!(!n.contains(banned), "must not coach (`{banned}`): {n}");
        }
    }
}
