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
