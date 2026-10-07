//! Which calls act on the turn's answer.
//!
//! The `FinalAnswer` commitment is decided by event order: the assistant
//! message the turn ends on is the answer, and a *work* call after a message
//! proves that message was interim narration instead. Deciding that inside each
//! renderer made the classification a second truth source — the TUI owned it
//! and Web, Desktop and the mobile App each copied it.
//!
//! This module is the single owner. The runtime stamps the outcome onto every
//! `ToolCallStarted` it projects to a client (`leveler_tools::acts_on_answer`
//! is called once, in the client projection), and every surface reads the
//! stamped value. A surface must never re-derive it from the tool name.
//!
//! The outcome is frozen by `docs/EXECUTION_PRESENTATION_CONTRACT.md` §I9 and
//! covered by the shared fixture corpus (C4 / C5 / C9 / C14).
//!
//! `update_plan` is the sharp edge of the question: it is an `Important`
//! presentation row — the reader should see the plan change — yet it is pure
//! bookkeeping, so an answer written in front of it is still the answer.
//! `update_goal(blocked)` is the opposite: it is why the run stopped, and that
//! reason is never the turn's answer.

/// Tool names whose calls record state or observe, and never do work.
///
/// Deliberately an allowlist with a conservative default. A name that is not
/// here — including any tool that does not exist yet — acts on the answer, so
/// an unrecognized call demotes a committed answer instead of silently
/// promoting stale prose to `FinalAnswer`. Never a presentation list: how
/// loudly a row is shown is the surface's own question.
pub const BOOKKEEPING_TOOLS: &[&str] = &[
    "consolidate_memory",
    "create_checkpoint",
    "expand_tools",
    "get_task",
    "git_status",
    "list_files",
    "memory",
    "spawn_agent",
    "update_goal",
    "update_plan",
    "wait_task",
];

/// Whether a started call acts on the answer: its presence proves the prose
/// before it was interim narration rather than the turn's answer.
///
/// The one authority for the question. Unknown tools answer `true` on purpose —
/// see [`BOOKKEEPING_TOOLS`].
pub fn acts_on_answer(name: &str, arguments: &str) -> bool {
    if is_silent_shell_probe(name, arguments) {
        return false;
    }
    // The goal tool is one name over two jobs: completing is bookkeeping,
    // blocking is the reason the run stopped.
    if name == "update_goal" {
        return update_goal_is_blocked(arguments);
    }
    !BOOKKEEPING_TOOLS.contains(&name)
}

/// Whether a call is a read-only shell probe (`ls`, `find`, `pwd`, an
/// existence test) rather than a command that did work.
///
/// Also read by the TUI's presentation: a probe that succeeds is not a row.
/// It is a property of the call, not of the surface that shows it, so it lives
/// with the tool vocabulary instead of being restated per renderer.
pub fn is_silent_shell_probe(name: &str, arguments: &str) -> bool {
    if name != "run_command" && name != "shell_command" {
        return false;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return is_silent_program_line(arguments);
    };
    // shell_command uses a single `cmd` string.
    if let Some(cmd) = v
        .get("cmd")
        .or_else(|| v.get("command"))
        .and_then(|c| c.as_str())
    {
        return is_silent_program_line(cmd);
    }
    let program = v
        .get("program")
        .and_then(|p| p.as_str())
        .unwrap_or("")
        .trim();
    let args: Vec<&str> = v
        .get("args")
        .and_then(|a| a.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
        .unwrap_or_default();
    is_silent_program(program, &args)
}

/// Whether an `update_goal` call reports the run as blocked.
pub fn update_goal_is_blocked(arguments: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .and_then(|v| {
            v.get("status")
                .and_then(|s| s.as_str())
                .map(|s| s == "blocked")
        })
        .unwrap_or(false)
}

fn is_silent_program_line(raw: &str) -> bool {
    let mut parts = raw.split_whitespace();
    let Some(program) = parts.next() else {
        return false;
    };
    let args: Vec<&str> = parts.collect();
    is_silent_program(program, &args)
}

fn is_silent_program(program: &str, args: &[&str]) -> bool {
    let base = std::path::Path::new(program)
        .file_name()
        .and_then(|p| p.to_str())
        .unwrap_or(program);
    match base {
        "ls" | "tree" | "find" | "pwd" | "stat" | "dirname" | "basename" | "realpath"
        | "readlink" => true,
        "test" | "[" => true, // file existence / type probes
        "which" | "command" | "type" => {
            // `which cargo` style lookups are internal harness noise.
            true
        }
        "bash" | "sh" | "zsh" | "dash" => {
            // Only demote trivial one-shot probes: `bash -c 'ls'`, `sh -c pwd`.
            shell_c_probe(args)
        }
        _ => false,
    }
}

fn shell_c_probe(args: &[&str]) -> bool {
    let Some(idx) = args.iter().position(|a| *a == "-c") else {
        return false;
    };
    let Some(script) = args.get(idx + 1).copied() else {
        return false;
    };
    let first = script
        .split(|c: char| c.is_whitespace() || c == ';' || c == '|' || c == '&')
        .find(|s| !s.is_empty())
        .unwrap_or("");
    matches!(
        first,
        "ls" | "tree" | "find" | "pwd" | "stat" | "test" | "which" | "[" | "dirname" | "basename"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_after_a_message_makes_it_narration() {
        assert!(acts_on_answer("read_file", r#"{"path":"a"}"#));
        assert!(acts_on_answer("grep", r#"{"pattern":"x"}"#));
        assert!(acts_on_answer("apply_patch", r#"{"patch":"p"}"#));
        assert!(acts_on_answer("run_command", r#"{"program":"cargo"}"#));
        assert!(acts_on_answer("shell_command", r#"{"cmd":"cargo test"}"#));
        assert!(acts_on_answer("write_file", r#"{"path":"a"}"#));
    }

    #[test]
    fn bookkeeping_after_the_answer_keeps_it() {
        // `update_plan` is presented loudly and is still bookkeeping.
        assert!(!acts_on_answer("update_plan", r#"{"steps":[]}"#));
        assert!(!acts_on_answer(
            "update_goal",
            r#"{"status":"complete","summary":"done"}"#
        ));
        for name in [
            "list_files",
            "get_task",
            "wait_task",
            "git_status",
            "create_checkpoint",
            "expand_tools",
            "memory",
            "consolidate_memory",
            "spawn_agent",
        ] {
            assert!(!acts_on_answer(name, "{}"), "{name} is bookkeeping");
        }
    }

    #[test]
    fn a_blocked_goal_is_a_boundary_not_the_answer() {
        assert!(acts_on_answer(
            "update_goal",
            r#"{"status":"blocked","summary":"缺密钥"}"#
        ));
        // Anything else the goal tool reports — complete, or an unreadable
        // payload a rejected call would carry — is bookkeeping. That is the
        // frozen v1 outcome (the TUI's `activity_visibility` read it the same
        // way), and it is the safe direction: a rejected plan/goal call did no
        // work, so it never demotes an answer.
        assert!(!acts_on_answer("update_goal", "not json"));
        assert!(!acts_on_answer("update_goal", r#"{"status":"complete"}"#));
    }

    #[test]
    fn observation_probes_are_not_work() {
        for arguments in [
            r#"{"program":"ls","args":["-la"]}"#,
            r#"{"program":"/bin/find","args":["."]}"#,
            r#"{"program":"pwd"}"#,
            r#"{"program":"test","args":["-f","x"]}"#,
            r#"{"cmd":"ls -la"}"#,
            r#"{"cmd":"bash -c ls"}"#,
        ] {
            assert!(
                !acts_on_answer("run_command", arguments),
                "{arguments} is a probe"
            );
            assert!(
                !acts_on_answer("shell_command", arguments),
                "{arguments} is a probe"
            );
        }
        // A one-shot probe is not a whole script, and the probe predicate reads
        // the command line as written: `bash -c 'ls'` is a quoted argument, not
        // a bare lookup, so it stays work. Both are the frozen v1 outcome.
        assert!(acts_on_answer(
            "shell_command",
            r#"{"cmd":"bash -c 'cargo test'"}"#
        ));
        assert!(acts_on_answer("shell_command", r#"{"cmd":"bash -c 'ls'"}"#));
    }

    /// The conservative default the contract's `I9` depends on: an
    /// unclassified call — a tool that does not exist yet, an MCP tool — is
    /// work, so it demotes a committed answer rather than promoting stale prose
    /// into `FinalAnswer`.
    #[test]
    fn an_unknown_tool_acts_on_the_answer() {
        assert!(acts_on_answer("brand_new_tool", "{}"));
        assert!(acts_on_answer("mcp__server__tool", r#"{"x":1}"#));
    }
}
