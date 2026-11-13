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

/// Goal mode: the goal is resolved and its answer was never written.
///
/// Nothing is unresolved here, so this is not a continuation: the model went
/// quiet after `update_goal` without telling the person anything. The harness
/// states that fact only — what the answer says is the model's.
pub(crate) fn goal_settled_delivery_nudge() -> String {
    "The goal is resolved and recorded, and the turn has not written the answer the person \
     is waiting for. Write it now: what you did, which checks ran and what they returned, what \
     is still true afterwards, and anything that did not work. End the turn after that answer."
        .to_string()
}

/// Chat: the turn went quiet while the model's own explicit plan still listed
/// steps it never declared completed.
///
/// The mechanical facts are: the plan this model published is still open, which
/// steps it names, and the three ways a model can honestly move on (do the work,
/// rewrite the plan because it no longer describes what is needed, or say what
/// blocks it). The harness does not choose the work and does not require the
/// plan to reach 100%: a step that is no longer necessary only has to stop
/// being declared open.
pub(crate) fn plan_unresolved_nudge(open_steps: &[String]) -> String {
    let mut text =
        String::from("Your declared plan is still open: these steps are not marked completed.\n");
    text.push_str(&listed_steps(open_steps));
    text.push_str(
        "The plan is your own declaration, so nothing else resolves this. Continue the work, \
         publish an updated plan if a step is no longer needed, or state what blocks the rest. \
         Ending the turn now leaves these steps open in the record.",
    );
    text
}

/// Goal mode: `update_goal(blocked)` was declared while the model's own latest
/// explicit plan still lists steps it never marked completed.
///
/// Both facts are the model's own declarations, and they describe different
/// states of the same work. Pending steps do not prove the remaining work is
/// still possible — that judgement is the model's — so this is one
/// reconciliation, not a gate: the block is accepted on a second declaration,
/// with the conflict kept in the record. The runtime does not choose the next
/// step, order the plan executed, or demand proof of the obstruction.
pub(crate) fn plan_block_conflict(open_steps: &[String]) -> String {
    let mut text = String::from(
        "update_goal(blocked) was not accepted: the latest plan this model published still \
         lists these steps as not completed.\n",
    );
    text.push_str(&listed_steps(open_steps));
    text.push_str(
        "The plan is your own declaration, so the block and the plan currently describe \
         different states of the same work. Continue the work, publish an updated plan with \
         update_plan when a step is no longer needed or is covered by what obstructs it, or \
         confirm the block with update_goal(blocked) again. A block confirmed while these \
         steps remain open is accepted, and the conflict stays recorded.",
    );
    text
}

/// One bounded rendering of a plan's open steps. The model owns the full list
/// in its own transcript; the feedback only has to name the conflict.
fn listed_steps(open_steps: &[String]) -> String {
    // Listing more than this pushes the plan into a prompt budget it does not
    // deserve.
    const MAX_LISTED: usize = 12;

    let mut text = String::new();
    for step in open_steps.iter().take(MAX_LISTED) {
        text.push_str("- ");
        text.push_str(step.trim());
        text.push('\n');
    }
    if open_steps.len() > MAX_LISTED {
        text.push_str(&format!("- (and {} more)\n", open_steps.len() - MAX_LISTED));
    }
    text
}

/// Chat: the model declared the user's request as a task to carry to
/// completion, and `commit_task` refused the declaration because the acceptance
/// list did not cover the user's own deliverables.
///
/// The refusal is about the CONTRACT the model wrote, not about the request:
/// the user's work is still outstanding, and the model may finish it without
/// ever registering. The harness states those facts and the exits — it does
/// not say the task failed, does not open a goal on the model's behalf, and
/// does not ask for a narrower scope.
pub(crate) fn task_unregistered_nudge() -> String {
    "Your commit_task declaration was refused; its tool result names the exact reason.\n\
     The refusal is about the declaration you wrote, not about the user's request: the work \
     is still outstanding and nothing about it has been delivered.\n\
     Continue the work, correct the declaration, or state what blocks it. Do not narrow the \
     user's scope to fit a declaration the runtime will accept."
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

    /// The plan nudge quotes the model's own declaration and offers the three
    /// honest resolutions. It must not name a tool, order a step executed, or
    /// demand an all-completed plan.
    #[test]
    fn the_plan_nudge_states_the_open_steps_and_offers_every_exit() {
        let n = plan_unresolved_nudge(&["step seven".to_string(), "step nine".to_string()]);
        assert!(n.contains("- step seven"), "{n}");
        assert!(n.contains("- step nine"), "{n}");
        assert!(n.contains("publish an updated plan"), "{n}");
        assert!(n.contains("blocks"), "{n}");
        assert!(n.contains("your own declaration"), "{n}");
        for banned in [
            "read_file",
            "apply_patch",
            "run_command",
            "update_goal",
            "you must complete",
            "下一步应该",
        ] {
            assert!(!n.contains(banned), "must not coach (`{banned}`): {n}");
        }
    }

    /// With no refused declaration there is nothing to say about registration:
    /// a clean plan nudge must not invent a task contract.
    #[test]
    fn a_clean_plan_nudge_does_not_mention_registration() {
        let n = plan_unresolved_nudge(&["step seven".to_string()]);
        assert!(!n.contains("commit_task"), "{n}");
        assert!(!n.contains("unregistered"), "{n}");
    }

    /// The registration nudge states a contract fact and every honest exit. It
    /// must not declare the task failed, and must not pressure the model into
    /// shrinking the user's scope to make the gate pass.
    #[test]
    fn the_unregistered_task_nudge_states_the_contract_fact() {
        let n = task_unregistered_nudge();
        assert!(n.contains("refused"), "{n}");
        assert!(n.contains("not about the user's request"), "{n}");
        assert!(n.contains("still outstanding"), "{n}");
        assert!(n.contains("Do not narrow the user's scope"), "{n}");
        for banned in [
            "read_file",
            "apply_patch",
            "run_command",
            "update_goal",
            "task failed",
            "下一步应该",
        ] {
            assert!(!n.contains(banned), "must not coach (`{banned}`): {n}");
        }
    }

    #[test]
    fn the_plan_nudge_caps_a_long_plan() {
        let steps: Vec<String> = (0..20).map(|i| format!("s{i}")).collect();
        let n = plan_unresolved_nudge(&steps);
        assert!(n.contains("- s11"), "{n}");
        assert!(!n.contains("- s12"), "{n}");
        assert!(n.contains("(and 8 more)"), "{n}");
    }

    /// The block reconciliation states both of the model's own declarations,
    /// every honest exit, and that a confirmed block is accepted. It must not
    /// choose work, require the plan to complete, or claim pending steps prove
    /// the remaining work is possible.
    #[test]
    fn the_plan_block_conflict_states_the_conflict_and_every_exit() {
        let n = plan_block_conflict(&["step seven".to_string(), "step nine".to_string()]);
        assert!(n.contains("- step seven"), "{n}");
        assert!(n.contains("- step nine"), "{n}");
        assert!(n.contains("not accepted"), "{n}");
        assert!(n.contains("update_plan"), "{n}");
        assert!(n.contains("update_goal(blocked) again"), "{n}");
        assert!(n.contains("is accepted"), "{n}");
        for banned in [
            "read_file",
            "apply_patch",
            "run_command",
            "you must complete",
            "下一步应该",
            "cannot be done",
        ] {
            assert!(!n.contains(banned), "must not coach (`{banned}`): {n}");
        }
    }

    #[test]
    fn the_plan_block_conflict_caps_a_long_plan() {
        let steps: Vec<String> = (0..20).map(|i| format!("s{i}")).collect();
        let n = plan_block_conflict(&steps);
        assert!(n.contains("- s11"), "{n}");
        assert!(!n.contains("- s12"), "{n}");
        assert!(n.contains("(and 8 more)"), "{n}");
    }
}
