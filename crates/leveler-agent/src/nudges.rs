//! Completion-gate and goal-persistence nudges injected into the loop.

use leveler_model::{Message, Role};

/// The transient progress-summary nudge injected by the step-summary control.
pub(crate) const STEP_SUMMARY_NUDGE: &str = "Briefly summarize what you have done so far and \
     what remains, then continue with the next concrete action.";

pub(crate) const COMPLETION_AUDIT_MARKER: &str = "Treat completion as unproven";

pub(crate) fn first_user_text(messages: &[Message]) -> String {
    messages
        .iter()
        .find(|m| m.role == Role::User)
        .map(Message::text_content)
        .unwrap_or_default()
}

/// The continuation nudge injected at a work-window boundary (goal persistence):
/// the task ran past a full round budget without finishing, so restate the full
/// objective and push the model to keep going rather than shrink or abandon it.
/// Goal mode: the model produced a final answer but did not call `update_goal`.
/// Going quiet does not finish the task — push it to resolve explicitly.
///
/// Conversational turns (greeting / Q&A / advice with no code delivery) must
/// close with **only** `update_goal` — no more tools, no second user-visible
/// "task complete" paragraph. Implementation tasks still require workspace
/// evidence before complete.
pub(crate) fn goal_resolve_nudge(objective: &str) -> String {
    let task = objective.trim();
    let restated = if task.is_empty() {
        String::new()
    } else {
        format!("\n\n<objective>\n{task}\n</objective>")
    };
    format!(
        "You stopped without calling update_goal. Going quiet does NOT finish the \
         task.{restated}\n\n\
         Choose ONE path:\n\
         A) **Conversational / already answered** — greeting, small talk, pure Q&A, \
         explanation, or advice where the user did not ask you to change the repo, \
         and your last assistant message already answers them fully:\n\
         → Immediately call update_goal(status=\"complete\", summary=one short clause). \
         Do NOT call any other tools. Do NOT send any further user-visible text \
         (no \"任务完成\", no \"已全面分析\", no restating the answer). The tool call \
         alone closes the turn.\n\
         B) **Implementation / delivery still open** — the user asked for code, \
         config, or other workspace changes, and requirements may still be unmet:\n\
         → Audit against the CURRENT workspace with tools (file contents, command \
         output, tests). If build/tests have not run since your last edit, run them. \
         Only when every requirement is PROVEN done, call update_goal(complete). \
         Otherwise keep working; do not shrink the objective. If genuinely stuck, \
         call update_goal(blocked).\n\
         C) **Follow-up in the same session** — use the conversation history; do \
         not claim you have \"no prior context\" and re-discover the project from \
         scratch unless the user started a truly new topic.\n\n\
         In every case: resolve by calling update_goal directly, and if you must \
         say anything user-visible first, output only NEW information that differs \
         from what you already said — do NOT repeat conclusions you have already \
         given."
    )
}

pub(crate) fn completion_audit_nudge(original_task: &str) -> String {
    let task = original_task.trim();
    let restated = if task.is_empty() {
        String::new()
    } else {
        format!("\n\nOriginal task:\n<task>\n{task}\n</task>")
    };
    format!(
        "{COMPLETION_AUDIT_MARKER}: audit it against the current state of the \
         workspace, not your memory of earlier steps.{restated}\n\n\
         - For every explicit requirement of the task, identify the evidence that \
         would prove it is done (file contents, command output, test results) and \
         check that evidence now with tools.\n\
         - If the build or tests have not run since your last change, run them \
         with run_command and see them pass; verification older than your last \
         edit does not count.\n\
         - Do not redefine the task into a smaller or easier one; the original \
         requirement is the bar.\n\n\
         If every requirement is verifiably satisfied, give your final answer. \
         If anything is missing or unverified, keep working now.\n\n\
         When you do answer, output only what is NEW since your last message — \
         do NOT repeat conclusions you have already stated."
    )
}

/// The repair prompt injected when the answer-completeness audit names
/// branches the answer skipped: continue from where the answer stopped with
/// the missing pieces only — never a second copy of what was already said.
pub(crate) fn answer_repair_nudge(missing: &[String]) -> String {
    let missing = if missing.is_empty() {
        "- The audit found the answer incomplete but did not name the missing branch. Re-check the original request and tool evidence.".to_string()
    } else {
        missing
            .iter()
            .map(|item| format!("- {item}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "A separate completeness audit found these omissions:\n{missing}\n\
         Continue the answer from where it stopped. Cover those omissions \
         precisely, do not repeat completed sections, and provide a clear \
         closing conclusion. Output only the NEW, incremental content that was \
         missing — do NOT restate conclusions already given above."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goal_resolve_nudge_has_conversational_fast_path() {
        let n = goal_resolve_nudge("你好");
        assert!(n.contains("Conversational"), "{n}");
        assert!(n.contains("Do NOT call any other tools"), "{n}");
        assert!(
            n.contains("Do NOT send any further user-visible text"),
            "{n}"
        );
        assert!(n.contains("Follow-up"), "{n}");
        // Must not force a full workspace audit for every quiet turn.
        assert!(
            !n.contains("For every explicit requirement, check the authoritative evidence now"),
            "old audit-only wording must not be the sole path: {n}"
        );
    }

    #[test]
    fn goal_resolve_nudge_still_has_delivery_audit_path() {
        let n = goal_resolve_nudge("add a cancel_order method");
        assert!(n.contains("Implementation / delivery"), "{n}");
        assert!(n.contains("PROVEN"), "{n}");
        assert!(n.contains("<objective>"), "{n}");
        assert!(n.contains("add a cancel_order method"), "{n}");
    }
}
