//! The runtime-owned `/goal` command.
//!
//! Slash commands are product semantics, not a per-client feature. A client
//! that recognizes `/goal` may send [`ClientCommand::RunGoal`], but a client
//! that does not — or a user who types it while a turn already runs — sends
//! the text as an ordinary submission or a steer. The runtime parses both
//! paths through this one function, so entering Goal mode cannot depend on
//! which UI happened to be open, and the durable axis is written by the
//! runtime rather than mirrored by each client.
//!
//! [`ClientCommand::RunGoal`]: crate::ClientCommand::RunGoal

/// What a `/goal` line asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalCommand {
    /// Start (or steer into) a goal with this objective. The objective is the
    /// user's text after the command word, verbatim.
    Run(String),
    /// Return the session to Chat.
    Clear,
    /// Report the current axis without starting work.
    Status,
    /// `/goal` with no argument: a usage error, never an empty goal.
    Usage,
}

/// Parse a leading `/goal` command from user text.
///
/// Returns `None` when the text is an ordinary message. Only a whole command
/// word counts: `/goalx` and `/goal-notes` are messages, not commands. The
/// objective may span lines — a pasted goal is still a goal.
pub fn parse_goal_command(text: &str) -> Option<GoalCommand> {
    let trimmed = text.trim_start();
    let rest = trimmed.strip_prefix('/')?;
    if !rest
        .get(..4)
        .is_some_and(|word| word.eq_ignore_ascii_case("goal"))
    {
        return None;
    }
    let after = &rest[4..];
    // The command word must end at a boundary; `/goalx` is a message.
    if !after.is_empty() && !after.chars().next().is_some_and(char::is_whitespace) {
        return None;
    }
    let body = after.trim();
    if body.is_empty() {
        return Some(GoalCommand::Usage);
    }
    let head = body.split_whitespace().next().unwrap_or("");
    match head.to_ascii_lowercase().as_str() {
        // Mirrors the terminal's contract: these words manage the axis; an
        // objective literally named "clear" would be unreachable, which is
        // the trade the existing command surface already makes.
        "clear" | "cancel" | "stop" => Some(GoalCommand::Clear),
        "status" => Some(GoalCommand::Status),
        _ => Some(GoalCommand::Run(body.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_goal_task_runs_with_its_text_verbatim() {
        assert_eq!(
            parse_goal_command("/goal 反正我的目标是最后做完。打通。"),
            Some(GoalCommand::Run("反正我的目标是最后做完。打通。".into()))
        );
    }

    #[test]
    fn a_pasted_multiline_goal_stays_whole() {
        assert_eq!(
            parse_goal_command("/goal 第一行\n第二行  "),
            Some(GoalCommand::Run("第一行\n第二行".into()))
        );
    }

    #[test]
    fn the_command_word_is_case_insensitive_and_may_be_indented() {
        assert_eq!(
            parse_goal_command("   /GoAl do it"),
            Some(GoalCommand::Run("do it".into()))
        );
    }

    #[test]
    fn axis_commands_are_not_objectives() {
        for text in ["/goal clear", "/goal cancel", "/goal stop"] {
            assert_eq!(parse_goal_command(text), Some(GoalCommand::Clear), "{text}");
        }
        assert_eq!(
            parse_goal_command("/goal status"),
            Some(GoalCommand::Status)
        );
    }

    #[test]
    fn a_bare_goal_is_a_usage_error_not_an_empty_goal() {
        assert_eq!(parse_goal_command("/goal"), Some(GoalCommand::Usage));
        assert_eq!(parse_goal_command("  /goal   "), Some(GoalCommand::Usage));
    }

    #[test]
    fn ordinary_text_is_not_a_command() {
        for text in [
            "goal: fix it",
            "please /goal fix it",
            "/goalx fix it",
            "/goal-notes fix it",
            "/go fix it",
            "",
        ] {
            assert_eq!(parse_goal_command(text), None, "{text:?}");
        }
    }
}
