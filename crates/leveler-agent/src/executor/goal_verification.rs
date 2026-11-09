//! Mechanical acceptance for `update_goal(complete)`.
//!
//! The runtime does not judge whether the work satisfies the user's objective
//! — that reading belongs to the model and the user. What it *can* check is
//! the model's own verification claim against its own execution record:
//!
//! - a command the model cites as proof must actually have run,
//! - its last run must have succeeded, and
//! - it must have run after the most recent file change.
//!
//! A Goal that changed files must therefore close in one of two honest ways:
//! cite verification that mechanically matches the record, or declare
//! `not_applicable` with a reason (the completion is then accepted with a
//! warning, never as a silently verified success). A failed test/build in the
//! task is hard evidence and cannot be overridden by either declaration.
//!
//! Everything here is a fact about commands and exit codes. No LLM call, no
//! keyword reading of the user's goal, no opinion about whether the change is
//! good.

/// One command execution the runtime observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationRun {
    /// The command as executed (`go test ./...`, `sh verify.sh`).
    pub label: String,
    /// The tool did not report an error (for a command, exit 0).
    pub ok: bool,
    /// Model step that ran it; ordering only.
    pub model_step: u32,
    /// `EvidenceLedger::total_mutation_ops` when it ran, so "after the last
    /// file change" is a comparison, not a timestamp guess.
    pub mutation_ops: u64,
    /// The tool call that produced this run. A citation of `call_id:<id>`
    /// names this execution, not whatever later run shares the command text.
    /// `None` for a record rebuilt before call ids were stored.
    pub call_id: Option<String>,
}

/// What the model said about verification in `update_goal`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VerificationClaim {
    /// Commands the model names as proof. Matched against [`VerificationRun`].
    Passed(Vec<String>),
    /// The objective has no executable check; the reason is shown to the user.
    NotApplicable(String),
    /// No verification field at all.
    Missing,
}

/// The gate's answer for one `update_goal(complete)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VerificationDecision {
    /// Resolve the goal. `warning` is persisted on the terminal.
    Accept { warning: Option<String> },
    /// Refuse this resolution and tell the model exactly which fact failed.
    Refuse(String),
}

/// A command this task STARTED and has not observed a result for.
///
/// It is not evidence in either direction: the process may still be running.
/// It is recorded so a citation of it is refused for the real reason — no
/// result yet — instead of "never ran in this task", which was untrue and sent
/// a real agent looking for a citation problem it did not have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingCommand {
    /// The same label a citation is matched against.
    pub label: String,
    /// The background task id, when the start reported one.
    pub task_id: Option<String>,
    /// The tool call that started it, so the result can be cited by that id.
    pub call_id: Option<String>,
}

/// Decide whether `update_goal(complete)` may resolve the goal.
pub(crate) fn decide(
    claim: &VerificationClaim,
    runs: &[VerificationRun],
    pending: &[PendingCommand],
    mutation_ops: u64,
    modified: bool,
) -> VerificationDecision {
    // A verification-class command that failed and has not been followed by a
    // successful one is hard evidence against completion. It outranks every
    // declaration, including `not_applicable`.
    if let Some(failed) = last_unrecovered_failure(runs) {
        return VerificationDecision::Refuse(format!(
            "the last verification command `{}` failed (step {}). The command failed. Fix the \
             failure and run it again, or call update_goal(status=\"blocked\") with the exact \
             obstruction. A failed verification cannot be closed as complete.",
            failed.label, failed.model_step
        ));
    }

    match claim {
        VerificationClaim::Passed(commands) if !commands.is_empty() => {
            for command in commands {
                let cited = command.trim();
                if cited.is_empty() {
                    continue;
                }
                let last = match cited_run(cited, runs) {
                    CitedRun::Refuse(message) => return VerificationDecision::Refuse(message),
                    CitedRun::Unmatched => {
                        return VerificationDecision::Refuse(unmatched_citation(
                            cited,
                            runs,
                            pending,
                            mutation_ops,
                        ));
                    }
                    CitedRun::Found(run) => run,
                };
                if !last.ok {
                    // A later successful compound line can contain this exact
                    // command as one segment (`cmd; next`). That is not this
                    // command's own record, but the earlier failure is no longer
                    // the current state. Reporting "last run failed (step N)"
                    // points the model at a stale step and invites a rerun that
                    // cannot fix it. Name the real record instead.
                    if let Some(later) = later_compound_pass(runs, last.model_step, cited) {
                        return VerificationDecision::Refuse(format!(
                            "`{cited}` last ran on its own at step {} and failed. It also appears \
                             inside the later command `{}` (step {}), which passed as a compound \
                             line; a subcommand is not its own execution record. Cite that command \
                             in full, or run `{cited}` on its own again.",
                            last.model_step, later.label, later.model_step
                        ));
                    }
                    return VerificationDecision::Refuse(format!(
                        "`{cited}` was cited as verification but the command failed: its last run \
                         failed (step {}).",
                        last.model_step
                    ));
                }
                // Freshness is a property of verification: a test or build
                // must judge the current tree. A command the user ordered as
                // an action can legitimately precede the change it produces;
                // demanding it run again would reverse the user's sequence.
                if is_verification_command(&last.label) && last.mutation_ops < mutation_ops {
                    return VerificationDecision::Refuse(format!(
                        "`{cited}` verification expired: it passed before the most recent file \
                         change. Re-run it against the current workspace before marking the goal \
                         complete."
                    ));
                }
            }
            VerificationDecision::Accept { warning: None }
        }
        VerificationClaim::NotApplicable(reason) => {
            let reason = reason.trim();
            let why = if reason.is_empty() {
                "no reason given"
            } else {
                reason
            };
            VerificationDecision::Accept {
                warning: Some(format!(
                    "goal completed without executable verification ({why}); insufficient evidence"
                )),
            }
        }
        // An empty `passed.commands` list is not evidence. Treat it like an
        // omission. The runtime does not force a command to exist (a Goal may
        // have no executable check), but it refuses to let a stale pass stand
        // in for a fresh one, and it records the absence on the terminal
        // instead of reporting a silently verified success.
        VerificationClaim::Passed(_) | VerificationClaim::Missing => {
            let fresh_pass = runs.iter().rev().any(|run| {
                is_verification_command(&run.label) && run.ok && run.mutation_ops >= mutation_ops
            });
            let stale_pass = runs
                .iter()
                .rev()
                .find(|run| is_verification_command(&run.label) && run.ok);
            if modified
                && !fresh_pass
                && let Some(stale) = stale_pass
            {
                return VerificationDecision::Refuse(format!(
                    "`{}` verification expired: it passed before the most recent file change \
                     (step {}). Re-run it against the current workspace, or declare why the \
                     change needs no executable check.",
                    stale.label, stale.model_step
                ));
            }
            if modified && !fresh_pass {
                VerificationDecision::Accept {
                    warning: Some(
                        "goal completed without executable verification (no verification \
                         command ran after the last file change); insufficient evidence"
                            .to_string(),
                    ),
                }
            } else {
                // A read-only goal has nothing to verify; its answer is the
                // deliverable.
                VerificationDecision::Accept { warning: None }
            }
        }
    }
}

/// Why a citation is not an execution record, and what the model can do.
///
/// Matching stays exact. The text only reports commands the runtime recorded.
fn unmatched_citation(
    cited: &str,
    runs: &[VerificationRun],
    pending: &[PendingCommand],
    mutation_ops: u64,
) -> String {
    // A command this task started is not a command that never ran. Saying so
    // about a live process hid the real missing fact (its result) and pointed
    // the model at the wrong problem.
    if let Some(started) = pending.iter().rev().find(|p| cites(&p.label, cited)) {
        let task = match started.task_id.as_deref() {
            Some(id) => format!(" as background task {id}"),
            None => String::new(),
        };
        let wait = match started.task_id.as_deref() {
            Some(id) => format!("wait_task with task_id {id}"),
            None => "wait_task for that task".to_string(),
        };
        return format!(
            "`{cited}` was cited as verification but it has only STARTED{task} and reported no \
             result yet. A start is not a pass and not a failure: its outcome is unknown, so it \
             cannot close the goal. Wait for it ({wait}) and cite it again once it exits, or cite \
             a command that already finished. If it is a service that never exits, cite the check \
             that proved the service works instead."
        );
    }
    let partial = closest_recorded_command(cited, runs);
    let why = match &partial {
        Some(partial) if partial.glued => format!(
            "`{cited}` verification object does not match an execution record: it is only \
             part of a recorded command, not a command that ran on its own. It lines up with \
             `{label}` (step {step}) and that command continues (`{next}`). A subcommand of a \
             compound shell command is not its own execution record.",
            label = partial.run.label,
            step = partial.run.model_step,
            next = partial.next_token.as_deref().unwrap_or("")
        ),
        Some(partial) if partial.full_record_prefix => format!(
            "`{cited}` command does not match `{label}` (step {step}): the citation contains \
             extra text after the full recorded command, and that text is not a result note \
             (`→`, `->`, `=>`, or a parenthetical).",
            label = partial.run.label,
            step = partial.run.model_step
        ),
        Some(partial) if partial.matched >= 3 => format!(
            "`{cited}` command does not match an execution record. The closest recorded \
             command is `{label}` (step {step}); the citation diverges before the end of that \
             command.",
            label = partial.run.label,
            step = partial.run.model_step
        ),
        _ => format!(
            "`{cited}` citation does not exist: it was cited as verification but never ran in \
             this task."
        ),
    };
    let citable = citable_commands(
        runs,
        mutation_ops,
        partial.as_ref().map(|partial| partial.run),
    );
    if citable.is_empty() {
        format!(
            "{why} No eligible passing check matched this citation or can be suggested from \
             the recorded build/test checks. Cite a relevant execution record in full, or run \
             the check as its own command if the task has no suitable result."
        )
    } else {
        format!(
            "{why} Recorded commands that can be cited in full: {citable}. Reuse a relevant \
             command without rerunning it by citing `call_id:<id>` when one is shown, or by \
             copying the command exactly as shown. A trailing result note (`→ ...` or a \
             parenthetical) is not part of the command. If these records do not prove the \
             claim, run the required check as its own command."
        )
    }
}

struct RecordedPartial<'a> {
    run: &'a VerificationRun,
    matched: usize,
    glued: bool,
    full_record_prefix: bool,
    next_token: Option<String>,
}

/// Longest token alignment between `cited` and a recorded command.
///
/// A trailing shell separator glued to the last argument (`--summary;`) is
/// reported as glued. It is not a match: [`cites`] still requires equal tokens.
fn closest_recorded_command<'a>(
    cited: &str,
    runs: &'a [VerificationRun],
) -> Option<RecordedPartial<'a>> {
    let cited_tokens = command_tokens(strip_trailing_note(cited));
    if cited_tokens.is_empty() {
        return None;
    }
    let mut best: Option<RecordedPartial<'a>> = None;
    for run in runs.iter().rev() {
        let label_tokens = command_tokens(&run.label);
        for start in 0..label_tokens.len() {
            let mut matched = 0;
            while start + matched < label_tokens.len()
                && matched < cited_tokens.len()
                && label_tokens[start + matched] == cited_tokens[matched]
            {
                matched += 1;
            }
            let mut glued = false;
            let next_token = if matched < cited_tokens.len() && start + matched < label_tokens.len()
            {
                let label_token = &label_tokens[start + matched];
                let cited_token = &cited_tokens[matched];
                if label_token
                    .strip_prefix(cited_token.as_str())
                    .is_some_and(is_shell_separator)
                    && matched + 1 == cited_tokens.len()
                {
                    glued = true;
                    matched += 1;
                }
                Some(label_token.clone())
            } else {
                label_tokens.get(start + matched).cloned()
            };
            let full_record_prefix =
                start == 0 && matched == label_tokens.len() && matched < cited_tokens.len();
            let better = best.as_ref().is_none_or(|current| {
                matched > current.matched
                    || (matched == current.matched
                        && full_record_prefix
                        && !current.full_record_prefix)
            });
            if matched > 0 && better {
                best = Some(RecordedPartial {
                    run,
                    matched,
                    glued,
                    full_record_prefix,
                    next_token,
                });
            }
        }
    }
    best
}

fn is_shell_separator(rest: &str) -> bool {
    !rest.is_empty() && rest.chars().all(|c| matches!(c, ';' | '&' | '|'))
}

/// A later successful record in which the cited command is one segment of a
/// compound shell line.
///
/// The segment is deliberately not accepted as the cited command's own record:
/// `a; b` may still report `a`'s failure through `b`'s success. It is returned
/// only so a refusal can name the record that made the earlier failure stale.
fn later_compound_pass<'a>(
    runs: &'a [VerificationRun],
    after_step: u32,
    cited: &str,
) -> Option<&'a VerificationRun> {
    runs.iter().rev().find(|run| {
        run.ok
            && run.model_step > after_step
            && closest_recorded_command(cited, std::slice::from_ref(*run))
                .is_some_and(|partial| partial.glued)
    })
}

/// Passing commands the model may copy. The closest record is included even
/// when it is not classified as verification, because that is the command
/// the citation almost named. Suggest only records the gate could accept.
fn citable_commands(
    runs: &[VerificationRun],
    mutation_ops: u64,
    closest: Option<&VerificationRun>,
) -> String {
    let mut listed: Vec<String> = Vec::new();
    let push = |listed: &mut Vec<String>, run: &VerificationRun| {
        if !run.ok
            || run.label.contains('\n')
            || (is_verification_command(&run.label) && run.mutation_ops < mutation_ops)
            || !runs
                .iter()
                .rev()
                .find(|latest| cites(&latest.label, &run.label))
                .is_some_and(|latest| std::ptr::eq(latest, run))
        {
            return;
        }
        let line = match run.call_id.as_deref() {
            Some(id) => format!("`call_id:{id}` = `{}` (step {})", run.label, run.model_step),
            None => format!("`{}` (step {})", run.label, run.model_step),
        };
        if !listed.contains(&line) {
            listed.push(line);
        }
    };
    if let Some(run) = closest {
        push(&mut listed, run);
    }
    for run in runs.iter().rev().filter(|run| {
        run.ok && is_verification_command(&run.label) && run.mutation_ops >= mutation_ops
    }) {
        push(&mut listed, run);
        if listed.len() >= 3 {
            break;
        }
    }
    listed.join("; ")
}

/// The most recent verification-class failure with no later verification
/// success. `runs` is chronological.
fn last_unrecovered_failure(runs: &[VerificationRun]) -> Option<&VerificationRun> {
    let mut failure: Option<&VerificationRun> = None;
    for run in runs
        .iter()
        .filter(|run| is_verification_command(&run.label))
    {
        if run.ok {
            failure = None;
        } else {
            failure = Some(run);
        }
    }
    failure
}

/// Parse the optional `verification` object from `update_goal` arguments.
///
/// A malformed declaration is an error the model can correct, never an
/// implicit `not_applicable`.
pub(crate) fn parse_claim(arguments: &serde_json::Value) -> Result<VerificationClaim, String> {
    let Some(verification) = arguments.get("verification") else {
        return Ok(VerificationClaim::Missing);
    };
    let Some(status) = verification.get("status").and_then(|s| s.as_str()) else {
        return Err(
            "`verification` requires `status` set to \"passed\" or \"not_applicable\"".to_string(),
        );
    };
    let detail = verification
        .get("detail")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    match status {
        "passed" => {
            let commands = detail
                .lines()
                .map(str::trim)
                .filter(|command| !command.is_empty() && !is_observation(command))
                .map(str::to_string)
                .collect::<Vec<_>>();
            Ok(VerificationClaim::Passed(commands))
        }
        "not_applicable" => Ok(VerificationClaim::NotApplicable(detail.to_string())),
        other => Err(format!(
            "`verification.status` must be \"passed\" or \"not_applicable\" (got \"{other}\")"
        )),
    }
}

/// Whether a line is an observation rather than a command.
///
/// The model may put a result note on its own line under the command it
/// annotates (`go test ./...` then `(exit 0; every package ok)`). A line that
/// is nothing but a parenthetical records what the command produced; no tool
/// run is recorded under it, so reading it as a citation refuses a completion
/// that did cite the command.
fn is_observation(line: &str) -> bool {
    line.starts_with('(') && line.ends_with(')')
}

enum CitedRun<'a> {
    Found(&'a VerificationRun),
    /// The citation itself is not a usable reference.
    Refuse(String),
    /// Text that is not a `call_id:` and does not name a recorded run.
    Unmatched,
}

/// Resolve one citation to the execution it names.
///
/// `call_id:<id>` selects that tool call. Otherwise the existing command text
/// matches, including a full command with a trailing result note. A bare id,
/// an abbreviation, or a different command stays unmatched.
fn cited_run<'a>(cited: &str, runs: &'a [VerificationRun]) -> CitedRun<'a> {
    if let Some(rest) = cited.strip_prefix("call_id:") {
        let id = rest.trim();
        if id.is_empty() || id.chars().any(char::is_whitespace) {
            return CitedRun::Refuse(
                "citation format is invalid: `call_id:` must name one tool-call id and nothing \
                 else. Cite `call_id:<id>` of the tool call that ran the check, or cite the \
                 exact command."
                    .to_string(),
            );
        }
        return match runs
            .iter()
            .rev()
            .find(|run| run.call_id.as_deref() == Some(id))
        {
            Some(run) => CitedRun::Found(run),
            None => CitedRun::Refuse(format!(
                "citation does not exist: no execution record in this task has call_id `{id}`."
            )),
        };
    }
    if let Some(run) = runs.iter().rev().find(|run| cites(&run.label, cited)) {
        return CitedRun::Found(run);
    }
    if let Some(run) = runs
        .iter()
        .rev()
        .find(|run| cites_recorded_command_with_result_note(&run.label, cited))
    {
        return CitedRun::Found(run);
    }
    CitedRun::Unmatched
}

/// Resolve one evidence line to the execution it names.
///
/// Structured requirement accounts cite a run the same way `verification`
/// does, so both readings share one identity rule: a `call_id:<id>`, or the
/// command the runtime actually recorded.
pub(crate) fn cited_run_in<'a>(
    line: &str,
    runs: &'a [VerificationRun],
) -> Option<&'a VerificationRun> {
    match cited_run(line, runs) {
        CitedRun::Found(run) => Some(run),
        CitedRun::Refuse(_) | CitedRun::Unmatched => None,
    }
}

/// Whether a recorded command is the command a requirement names.
pub(crate) fn run_matches_command(label: &str, command: &str) -> bool {
    cites(label, command)
}

fn result_note_marker(token: &str) -> bool {
    matches!(token, "→" | "->" | "=>" | "⇒")
}

/// A full recorded command plus a trailing result note.
///
/// The note starts at an unquoted `→`, `->`, `=>`, or `⇒`. An arrow inside a
/// quoted argument stays part of that argument, so it cannot open a note.
fn cites_recorded_command_with_result_note(label: &str, cited: &str) -> bool {
    let cited_tokens = command_tokens(strip_trailing_note(cited));
    let label_tokens = command_tokens(label);
    if label_tokens.is_empty() || cited_tokens.len() <= label_tokens.len() {
        return false;
    }
    cited_tokens[..label_tokens.len()] == label_tokens[..]
        && result_note_marker(&cited_tokens[label_tokens.len()])
}

/// Whether a citation names a command the runtime actually ran.
///
/// The model writes the citation as prose: it may quote one argument for
/// readability (`-run 'A|B'`) and may append an observation in parentheses.
/// The execution record stores the command as the tool received it. Compare
/// token sequences after removing only that presentation, never words:
/// `grep 'a b' file` and `grep a b file` are different commands.
fn cites(label: &str, cited: &str) -> bool {
    let cited = command_tokens(strip_trailing_note(cited));
    if cited.is_empty() {
        return false;
    }
    let label = command_tokens(label);
    label
        .windows(cited.len())
        .any(|window| window == cited.as_slice())
}

/// Split a command into argument-shaped tokens. Quotes group and are dropped;
/// whitespace outside quotes separates. No word is discarded, so a quoted
/// argument cannot stand in for several unquoted ones.
fn command_tokens(command: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for c in command.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => current.push(c),
            None if c == '\'' || c == '"' => quote = Some(c),
            None if c.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            None => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Drop a trailing parenthetical observation such as
/// `gofmt -l . (only pre-existing files flagged)`. Only a note that closes
/// the citation and starts outside a quoted argument is removed; a
/// parenthesis that belongs to the command itself stays.
///
/// A fullwidth note is the same observation in Chinese
/// (`make up（退出码 0，五单元探活 200）`) and needs no separator before it,
/// because `（` is never part of a shell command. Requiring the ASCII shape
/// refused evidence that had really run.
fn strip_trailing_note(command: &str) -> &str {
    let command = command.trim_end();
    let open = if command.ends_with('）') {
        command.rfind('（')
    } else if command.ends_with(')') {
        command.rfind(" (")
    } else {
        None
    };
    let Some(open) = open else {
        return command;
    };
    if command[open..].contains('\n') {
        return command;
    }
    let mut quote: Option<char> = None;
    for (index, c) in command.char_indices() {
        if index == open {
            if quote.is_some() {
                return command;
            }
            break;
        }
        match quote {
            Some(q) if c == q => quote = None,
            None if c == '\'' || c == '"' => quote = Some(c),
            _ => {}
        }
    }
    command[..open].trim_end()
}

/// The command a tool call actually asked to run, in one display string.
///
/// Only the two command tools have one. Every other tool is not executable
/// verification, so a call returns `None`.
pub(crate) fn command_label(name: &str, arguments: &serde_json::Value) -> Option<String> {
    match name {
        "run_command" => {
            let program = arguments.get("program")?.as_str()?.trim();
            if program.is_empty() {
                return None;
            }
            let args: Vec<&str> = arguments
                .get("args")
                .and_then(|value| value.as_array())
                .map(|args| args.iter().filter_map(|arg| arg.as_str()).collect())
                .unwrap_or_default();
            Some(if args.is_empty() {
                program.to_string()
            } else {
                format!("{program} {}", args.join(" "))
            })
        }
        "shell_command" => {
            let command = arguments
                .get("cmd")
                .or_else(|| arguments.get("command"))?
                .as_str()?
                .trim();
            (!command.is_empty()).then(|| command.to_string())
        }
        _ => None,
    }
}

/// The command label of a process the runtime actually ran, when the result
/// arrives from the process itself rather than from the call that started it.
///
/// Uses the same rendering as a `run_command` call — `program arg arg` — so a
/// cited command and its background result are compared as one command. A
/// `shell_command` caller's argv is a shell invocation whose tokens contain the
/// user's command, which is what [`cites`] matches on.
pub(crate) fn background_command_label(program: &str, args: &[String]) -> Option<String> {
    let program = program.trim();
    if program.is_empty() {
        return None;
    }
    let arguments = serde_json::json!({ "program": program, "args": args });
    command_label("run_command", &arguments)
}

/// Whether a command is a test/build/check-class command.
///
/// Deliberately narrow and mechanical: it only decides which FAILURES are hard
/// evidence. A command that is not recognised here is still usable as cited
/// verification (`README` inspection, a custom CI script the model names).
pub(crate) fn is_verification_command(label: &str) -> bool {
    const VERB_TARGETS: &[&str] = &[
        "test",
        "tests",
        "build",
        "check",
        "clippy",
        "vet",
        "lint",
        "typecheck",
        "type-check",
        "verify",
        "acceptance",
        "e2e",
        "ci",
    ];
    // `;`, `&&`, `||` and pipes run several programs; a verification in any
    // segment counts, because a failure in that segment fails the command.
    for segment in label.split([';', '&', '|', '\n']) {
        let words: Vec<&str> = segment
            .split_whitespace()
            .map(|word| word.trim_matches(|c| c == '"' || c == '\''))
            .filter(|word| !word.is_empty())
            .collect();
        let Some(first) = words.first().copied() else {
            continue;
        };
        let second = words.get(1).copied().unwrap_or("");
        let basename = first.rsplit('/').next().unwrap_or(first);
        let matches = match basename {
            "go" => matches!(second, "test" | "build" | "vet"),
            "cargo" => matches!(second, "test" | "build" | "check" | "clippy"),
            "npm" | "pnpm" | "yarn" | "bun" | "deno" => {
                second == "test"
                    || (second == "run" && words.get(2).is_some_and(|w| VERB_TARGETS.contains(w)))
            }
            "make" | "just" | "bazel" => {
                words.iter().skip(1).any(|word| VERB_TARGETS.contains(word))
            }
            "mvn" | "gradle" | "dotnet" | "swift" | "flutter" | "dart" | "mix" | "ctest" => {
                words.iter().skip(1).any(|word| VERB_TARGETS.contains(word))
            }
            "pytest" | "phpunit" => true,
            "python" | "python3" | "py" => {
                let module = words.windows(2).any(|pair| {
                    pair[0] == "-m" && matches!(pair[1], "unittest" | "pytest" | "nose2")
                });
                module || first_positional(&words).is_some_and(python_test_script)
            }
            "node" => words.iter().skip(1).any(|word| *word == "--test"),
            // A shell entry point named for verification: `sh verify.sh`,
            // `bash ./scripts/check.sh`, `./accept.sh`.
            "sh" | "bash" | "zsh" | "pwsh" | "powershell" | "cmd" => {
                words.iter().skip(1).any(|word| {
                    let name = word.rsplit('/').next().unwrap_or(word);
                    VERB_TARGETS.iter().any(|target| name.contains(target))
                })
            }
            _ => {
                // `./verify.sh`, `./acceptance.sh`, `scripts/test.sh` run
                // directly.
                VERB_TARGETS.iter().any(|target| basename.contains(target))
                    && (basename.ends_with(".sh")
                        || basename.ends_with(".py")
                        || basename.ends_with(".ps1")
                        || basename.ends_with(".bat"))
            }
        };
        if matches {
            return true;
        }
    }
    false
}

/// The first non-option word after the interpreter: the script it runs.
fn first_positional<'a>(words: &'a [&'a str]) -> Option<&'a str> {
    words
        .iter()
        .skip(1)
        .find(|word| !word.starts_with('-'))
        .copied()
}

/// A Python file whose own name says it is a test suite.
///
/// `python3 test_app.py` is the file analogue of `sh run-tests.sh`. The
/// extension check keeps `python3 -m pip install pytest` and inline `-c` code
/// out: they name a suite without running one.
fn python_test_script(word: &str) -> bool {
    let name = word
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(word)
        .to_ascii_lowercase();
    name.ends_with(".py") && TEST_SUITE_WORDS.iter().any(|target| name.contains(target))
}

const TEST_SUITE_WORDS: &[&str] = &[
    "test", "tests", "pytest", "unittest", "jest", "vitest", "mocha", "rspec", "ctest",
];

/// Whether a command's output says the test runner executed no tests.
///
/// A suite that only skipped, or a runner that found no tests, cannot prove
/// that the objective's tests pass. This is a mechanical fact about the
/// output, not a judgment about the project; the markers are deliberately
/// narrow so a mixed suite that really passed is never mistaken for an empty
/// one.
pub(crate) fn reports_no_tests(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    // Whole-invocation markers. Per-package messages such as Go's
    // `[no test files]` are deliberately NOT used: another package in the
    // same command may have run real tests.
    if lower.contains("ran 0 tests") || lower.contains("no tests ran") {
        return true;
    }
    // unittest: `Ran 3 tests ... OK (skipped=3)` means every discovered test
    // was skipped; `Ran 5 tests ... skipped=1` executed four real tests.
    if let Some(ran) = number_after(&lower, "ran ")
        && let Some(skipped) = number_after(&lower, "skipped=")
    {
        return ran > 0 && ran <= skipped;
    }
    // cargo: one `test result:` summary per test binary. Only the sum of
    // passed tests decides emptiness, so a crate with no tests cannot turn a
    // workspace run that passed elsewhere into a false failure.
    if lower.contains("test result:") {
        let mut any_passed = false;
        for (index, _) in lower.match_indices("test result:") {
            let rest = &lower[index + "test result:".len()..];
            if number_after(rest, "ok. ").is_some_and(|passed| passed > 0) {
                any_passed = true;
                break;
            }
        }
        return !any_passed;
    }
    // pytest: `3 skipped in 0.01s` with nothing passed, failed or errored.
    if lower.contains(" skipped in ")
        && !lower.contains(" passed")
        && !lower.contains(" failed")
        && !lower.contains(" error")
    {
        return true;
    }
    false
}

fn number_after(text: &str, needle: &str) -> Option<usize> {
    let rest = &text[text.find(needle)? + needle.len()..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

#[cfg(test)]
mod background_label_tests {
    use super::*;

    /// The result arrives from the PROCESS (argv), while the model cites the
    /// command it asked for. Both must compare as one command.
    #[test]
    fn a_background_result_is_labelled_like_the_command_that_was_cited() {
        let label =
            background_command_label("cargo", &["test".into(), "--lib".into()]).expect("label");
        assert_eq!(label, "cargo test --lib");
        assert!(cites(&label, "cargo test --lib"));
        // A shell invocation keeps the user's command inside its own tokens, so
        // citing the user's command still matches the shell's argv result.
        let shell = background_command_label("sh", &["-c".into(), "make up && make e2e".into()])
            .expect("label");
        assert!(cites(&shell, "make up && make e2e"), "{shell}");
        assert!(cites(&shell, "make e2e"), "{shell}");
        assert!(!cites(&shell, "make migrate"), "{shell}");
    }

    #[test]
    fn an_empty_program_has_no_label() {
        assert!(background_command_label("  ", &[]).is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(label: &str, ok: bool, step: u32, mutation_ops: u64) -> VerificationRun {
        VerificationRun {
            label: label.to_string(),
            ok,
            model_step: step,
            mutation_ops,
            call_id: None,
        }
    }

    fn run_id(
        label: &str,
        ok: bool,
        step: u32,
        mutation_ops: u64,
        call_id: &str,
    ) -> VerificationRun {
        VerificationRun {
            call_id: Some(call_id.to_string()),
            ..run(label, ok, step, mutation_ops)
        }
    }

    #[test]
    fn empty_and_all_skipped_test_runs_are_not_evidence() {
        for output in [
            "Ran 0 tests in 0.000s",
            "test result: ok. 0 passed; 0 failed; 3 ignored",
            "Ran 1 test in 0.000s\n\nOK (skipped=1)",
            "3 skipped in 0.01s",
            "no tests ran in 0.01s",
        ] {
            assert!(
                reports_no_tests(output),
                "must be treated as empty: {output:?}"
            );
        }
        for output in [
            "Ran 5 tests in 0.12s\n\nOK (skipped=1)",
            "test result: ok. 5 passed; 0 failed; 1 ignored",
            // A workspace run may include an empty test binary. The real
            // passes elsewhere must keep the command as evidence.
            "test result: ok. 0 passed; 0 failed; 0 ignored\n\n\
             test result: ok. 12 passed; 0 failed; 0 ignored",
            "3 passed, 1 skipped in 0.2s",
            "5 tests completed",
        ] {
            assert!(
                !reports_no_tests(output),
                "a real mixed run must stay evidence: {output:?}"
            );
        }
    }

    #[test]
    fn a_citation_shadowed_by_a_later_compound_pass_names_the_real_record() {
        // T10: `gofmt -l cmd pkg/yqlib` failed on its own at step 8, then ran
        // inside a compound line that passed at step 54. "last run failed"
        // pointed the model back at step 8 and invited a rerun that could not
        // change the outcome.
        let runs = vec![
            run("gofmt -l cmd pkg/yqlib", false, 8, 1),
            run(
                "gofmt -l cmd pkg/yqlib; echo gofmt-done vet-ok",
                true,
                54,
                2,
            ),
        ];
        match decide(
            &VerificationClaim::Passed(vec!["gofmt -l cmd pkg/yqlib".into()]),
            &runs,
            &[],
            2,
            true,
        ) {
            VerificationDecision::Refuse(message) => {
                assert!(
                    message.contains("appears inside the later command"),
                    "{message}"
                );
                assert!(message.contains("(step 54)"), "{message}");
                assert!(!message.contains("its last run failed"), "{message}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_failed_later_compound_line_does_not_hide_the_failure() {
        // The same citation, but the later compound line failed too. No stale
        // step exists, so the plain failure is the honest answer.
        let runs = vec![
            run("gofmt -l cmd pkg/yqlib", false, 8, 1),
            run(
                "gofmt -l cmd pkg/yqlib; echo gofmt-done vet-ok",
                false,
                54,
                2,
            ),
        ];
        match decide(
            &VerificationClaim::Passed(vec!["gofmt -l cmd pkg/yqlib".into()]),
            &runs,
            &[],
            2,
            true,
        ) {
            VerificationDecision::Refuse(message) => {
                assert!(
                    !message.contains("appears inside the later command"),
                    "{message}"
                )
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_cited_command_that_ran_and_passed_after_the_change_is_accepted() {
        let runs = vec![run("go test ./...", true, 4, 2)];
        assert_eq!(
            decide(
                &VerificationClaim::Passed(vec!["go test ./...".into()]),
                &runs,
                &[],
                2,
                true
            ),
            VerificationDecision::Accept { warning: None }
        );
    }

    #[test]
    fn a_citation_that_never_ran_is_refused() {
        let runs = vec![run("go build ./...", true, 4, 2)];
        let decision = decide(
            &VerificationClaim::Passed(vec!["go test ./...".into()]),
            &runs,
            &[],
            2,
            true,
        );
        match decision {
            VerificationDecision::Refuse(message) => {
                assert!(message.contains("never ran"), "{message}")
            }
            other => panic!("expected refusal, got {other:?}"),
        }
    }

    /// A refusal that names no recorded command sends the model back through
    /// the whole verification sequence to discover an exact string. Name the
    /// commands the runtime actually recorded, so the model can cite one and
    /// keep the evidence check exact.
    #[test]
    fn a_missed_citation_refusal_names_a_command_the_runtime_actually_ran() {
        let runs = vec![
            run("go build ./...", true, 4, 2),
            run("go test -count=1 ./...", true, 6, 2),
        ];
        let decision = decide(
            &VerificationClaim::Passed(vec!["go test ./...".into()]),
            &runs,
            &[],
            2,
            true,
        );
        match decision {
            VerificationDecision::Refuse(message) => {
                assert!(message.contains("never ran"), "{message}");
                assert!(
                    message.contains("go test -count=1 ./..."),
                    "the refusal must name a recorded command: {message}"
                );
                assert!(
                    message.contains("in full") && message.contains("own command"),
                    "the refusal must say how to correct the citation: {message}"
                );
            }
            other => panic!("expected refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_full_record_with_a_trailing_result_note_is_that_execution() {
        let command = "ssh -o BatchMode=yes fixture 'psql -tAc SELECT'";
        let runs = [run_id(command, true, 4, 2, "probe")];
        for cited in [
            format!("{command} → 0|0|15|1"),
            format!("{command} -> 0|0|15|1"),
            format!("{command} (0|0|15|1)"),
            "call_id:probe".to_string(),
        ] {
            assert_eq!(
                decide(
                    &VerificationClaim::Passed(vec![cited.clone()]),
                    &runs,
                    &[],
                    2,
                    true,
                ),
                VerificationDecision::Accept { warning: None },
                "{cited}"
            );
        }
    }

    #[test]
    fn an_abbreviated_command_is_still_refused() {
        let command = "ssh -o BatchMode=yes fixture 'psql -tAc SELECT'";
        let runs = [run(command, true, 4, 2)];
        let decision = decide(
            &VerificationClaim::Passed(vec!["ssh fixture 'psql'".into()]),
            &runs,
            &[],
            2,
            true,
        );
        let VerificationDecision::Refuse(message) = decision else {
            panic!("an abbreviation is not the recorded command");
        };
        assert!(
            message.contains("never ran") || message.contains("command does not match"),
            "{message}"
        );
        assert!(message.contains("without rerunning"), "{message}");
    }

    #[test]
    fn extra_text_that_is_not_a_result_note_does_not_match() {
        let command = "git status";
        let runs = [run(command, true, 4, 0)];
        let decision = decide(
            &VerificationClaim::Passed(vec!["git status --short".into()]),
            &runs,
            &[],
            0,
            false,
        );
        let VerificationDecision::Refuse(message) = decision else {
            panic!("a different command must not match");
        };
        assert!(message.contains("command does not match"), "{message}");
    }

    #[test]
    fn a_call_id_names_one_execution_of_a_repeated_command() {
        let runs = [
            run_id("cargo test --lib", false, 2, 1, "old"),
            run_id("cargo test --lib", true, 5, 3, "new"),
        ];
        assert_eq!(
            decide(
                &VerificationClaim::Passed(vec!["call_id:new".into()]),
                &runs,
                &[],
                3,
                true,
            ),
            VerificationDecision::Accept { warning: None }
        );
        let failed = decide(
            &VerificationClaim::Passed(vec!["call_id:old".into()]),
            &runs,
            &[],
            3,
            true,
        );
        let VerificationDecision::Refuse(message) = failed else {
            panic!("the failed instance is not redeemed by a later one");
        };
        assert!(message.contains("command failed"), "{message}");
        assert_eq!(
            decide(
                &VerificationClaim::Passed(vec!["cargo test --lib".into()]),
                &runs,
                &[],
                3,
                true,
            ),
            VerificationDecision::Accept { warning: None },
            "text citation still names the latest matching run"
        );
    }

    #[test]
    fn a_call_id_from_another_task_does_not_exist_here() {
        let runs = [run_id("cargo test --lib", true, 2, 1, "ours")];
        let decision = decide(
            &VerificationClaim::Passed(vec!["call_id:theirs".into()]),
            &runs,
            &[],
            1,
            true,
        );
        let VerificationDecision::Refuse(message) = decision else {
            panic!("another task's call id is not this task's evidence");
        };
        assert!(message.contains("citation does not exist"), "{message}");
        assert!(!message.contains("never ran"), "{message}");
    }

    #[test]
    fn a_malformed_call_id_is_a_format_error() {
        let runs = [run_id("cargo test --lib", true, 2, 1, "ours")];
        for cited in ["call_id:", "call_id:ours extra"] {
            let decision = decide(
                &VerificationClaim::Passed(vec![cited.into()]),
                &runs,
                &[],
                1,
                false,
            );
            let VerificationDecision::Refuse(message) = decision else {
                panic!("{cited} must be a format error");
            };
            assert!(message.contains("citation format is invalid"), "{message}");
        }
    }

    #[test]
    fn a_stale_call_id_is_expired_even_though_the_run_exists() {
        let runs = [run_id("cargo test --lib", true, 2, 1, "probe")];
        let decision = decide(
            &VerificationClaim::Passed(vec!["call_id:probe".into()]),
            &runs,
            &[],
            4,
            true,
        );
        let VerificationDecision::Refuse(message) = decision else {
            panic!("a pass from before the file change is expired");
        };
        assert!(message.contains("verification expired"), "{message}");
    }

    #[test]
    fn a_failed_command_citation_names_the_failure() {
        let runs = [run("cargo test --lib", false, 2, 1)];
        let decision = decide(
            &VerificationClaim::Passed(vec!["cargo test --lib".into()]),
            &runs,
            &[],
            1,
            true,
        );
        let VerificationDecision::Refuse(message) = decision else {
            panic!("a failed command is not a pass");
        };
        assert!(message.contains("command failed"), "{message}");
    }

    #[test]
    fn an_arrow_argument_stays_part_of_the_executed_command() {
        let command = "printf %s 'alpha → beta'";
        let runs = [run(command, true, 4, 0)];
        assert_eq!(
            decide(
                &VerificationClaim::Passed(vec![command.into()]),
                &runs,
                &[],
                0,
                false,
            ),
            VerificationDecision::Accept { warning: None }
        );
        assert!(matches!(
            decide(
                &VerificationClaim::Passed(vec!["printf %s 'alpha → gamma'".into()]),
                &runs,
                &[],
                0,
                false,
            ),
            VerificationDecision::Refuse(_)
        ));
    }

    /// The T6 shape: the model cites one segment of a compound shell command.
    /// The runtime recorded the whole script, and the separator is glued to
    /// the last argument, so the segment is not an execution record. The
    /// refusal must name that full command and the correction, and must not
    /// pretend the script never ran.
    /// The real DevOrder refusal said "it never ran in this task" about
    /// `make up`, which HAD started as background task `bg-1` and was still
    /// running. The missing fact was its result, not its existence; naming that
    /// (and the task to wait on) is what makes the refusal actionable.
    #[test]
    fn a_citation_of_a_started_command_names_the_task_to_wait_for() {
        let pending = vec![PendingCommand {
            label: "make up CONFIG=probe.yaml".to_string(),
            task_id: Some("bg-1".to_string()),
            call_id: None,
        }];
        let decision = decide(
            &VerificationClaim::Passed(vec!["make up CONFIG=probe.yaml".into()]),
            &[],
            &pending,
            0,
            false,
        );
        let VerificationDecision::Refuse(message) = decision else {
            panic!("a start is not a pass");
        };
        assert!(message.contains("only STARTED"), "{message}");
        assert!(message.contains("bg-1"), "{message}");
        assert!(
            !message.contains("never ran"),
            "the command did run; its result is what is missing: {message}"
        );
    }

    #[test]
    fn a_compound_shell_subcommand_is_refused_and_names_the_full_command() {
        let recorded = "/bin/sh ./verify.sh; echo marker";
        let runs = vec![run(recorded, true, 9, 2)];
        let decision = decide(
            &VerificationClaim::Passed(vec!["/bin/sh ./verify.sh".into()]),
            &runs,
            &[],
            2,
            true,
        );
        match decision {
            VerificationDecision::Refuse(message) => {
                assert!(
                    message.contains(recorded),
                    "the refusal must name the recorded command: {message}"
                );
                assert!(
                    message.contains("continues") || message.contains("subcommand"),
                    "the refusal must explain the mismatch: {message}"
                );
                assert!(
                    message.contains("in full") && message.contains("own command"),
                    "the refusal must say how to correct it: {message}"
                );
                assert!(
                    !message.contains("never ran"),
                    "the compound command did run: {message}"
                );
            }
            other => panic!("expected refusal, got {other:?}"),
        }
    }

    /// Citing the recorded script itself is the correction. The gate stays
    /// exact: only the full command matches.
    #[test]
    fn citing_the_full_compound_command_is_accepted() {
        let recorded = "/bin/sh ./verify.sh; echo marker";
        let runs = vec![run(recorded, true, 9, 2)];
        assert_eq!(
            decide(
                &VerificationClaim::Passed(vec![recorded.into()]),
                &runs,
                &[],
                2,
                true
            ),
            VerificationDecision::Accept { warning: None }
        );
    }

    #[test]
    fn a_cited_command_may_quote_its_argument() {
        let runs = vec![run(
            "go test -v -run TestNamesOnly|TestRenderNamesIncludesInvalid ./cmd/... ./internal/report/...",
            true,
            4,
            2,
        )];
        assert_eq!(
            decide(
                &VerificationClaim::Passed(vec![
                    "go test -v -run 'TestNamesOnly|TestRenderNamesIncludesInvalid' ./cmd/... ./internal/report/..."
                        .into()
                ]),
                &runs, &[],
                2,
                true
            ),
            VerificationDecision::Accept { warning: None }
        );
    }

    /// The T8 shape: the model writes the command on one line and its
    /// observation on the next. The observation is presentation, not a second
    /// command — an `(exit 0; …)` line cannot be executed as one.
    #[test]
    fn an_observation_line_is_not_a_cited_command() {
        let runs = vec![run("go test -count=1 ./...", true, 4, 2)];
        let claim = parse_claim(&serde_json::json!({
            "verification": {
                "status": "passed",
                "detail": "go test -count=1 ./...\n(exit 0; every package ok, no FAIL)"
            }
        }))
        .expect("valid claim");
        assert_eq!(
            decide(&claim, &runs, &[], 2, true),
            VerificationDecision::Accept { warning: None }
        );
    }

    #[test]
    fn a_cited_command_may_carry_a_trailing_annotation() {
        let runs = vec![run("gofmt -l .", true, 4, 2)];
        assert_eq!(
            decide(
                &VerificationClaim::Passed(vec![
                    "gofmt -l . (only pre-existing internal/config/config.go flagged; touched files clean)"
                        .into()
                ]),
                &runs, &[],
                2,
                true
            ),
            VerificationDecision::Accept { warning: None }
        );
    }

    #[test]
    fn a_cited_command_may_carry_a_cjk_annotation() {
        // The citation the DevOrder session wrote for a command it had run
        // (`bash -lc make up > tmp/run/up.log 2>&1; echo "make up 退出码: $?"`).
        let runs = vec![run(
            "bash -lc make up > tmp/run/up.log 2>&1; echo \"make up 退出码: $?\"",
            true,
            6,
            0,
        )];
        assert_eq!(
            decide(
                &VerificationClaim::Passed(vec![
                    "make up（退出码 0，五单元启动并探活 200）".into()
                ]),
                &runs,
                &[],
                0,
                false
            ),
            VerificationDecision::Accept { warning: None }
        );
    }

    #[test]
    fn a_quoted_argument_does_not_match_a_different_unquoted_command() {
        let runs = vec![run("grep a b file", true, 4, 2)];
        assert!(matches!(
            decide(
                &VerificationClaim::Passed(vec!["grep 'a b' file".into()]),
                &runs, &[],
                2,
                true
            ),
            VerificationDecision::Refuse(message) if message.contains("never ran")
        ));
    }

    #[test]
    fn a_citation_whose_last_run_failed_is_refused() {
        let runs = vec![run("go test ./...", false, 4, 2)];
        assert!(matches!(
            decide(&VerificationClaim::Passed(vec!["go test".into()]), &runs, &[], 2, true),
            VerificationDecision::Refuse(message) if message.contains("failed")
        ));
    }

    #[test]
    fn a_citation_that_predates_the_last_change_is_refused() {
        let runs = vec![run("go test ./...", true, 4, 1)];
        assert!(matches!(
            decide(&VerificationClaim::Passed(vec!["go test".into()]), &runs, &[], 3, true),
            VerificationDecision::Refuse(message) if message.contains("before the most recent")
        ));
    }

    /// The T9 shape: a task orders a side-effect-free command BEFORE the file
    /// it creates ("run sleep 60, only then create done.txt"). A cited
    /// command that is not verification-class must have run and passed, but
    /// it cannot be made "stale" by the later change it was ordered to
    /// precede; demanding a re-run reverses the user's own sequence.
    #[test]
    fn a_cited_non_verification_command_need_not_follow_the_last_change() {
        let runs = vec![
            run("sleep 60", true, 3, 0),
            run("od -c done.txt", true, 5, 1),
        ];
        assert_eq!(
            decide(
                &VerificationClaim::Passed(vec!["sleep 60".into()]),
                &runs,
                &[],
                1,
                true
            ),
            VerificationDecision::Accept { warning: None }
        );
    }

    #[test]
    fn a_failed_verification_blocks_completion_even_without_a_claim() {
        let runs = vec![run("cargo test", false, 5, 2)];
        assert!(matches!(
            decide(&VerificationClaim::Missing, &runs, &[], 2, true),
            VerificationDecision::Refuse(message) if message.contains("failed")
        ));
        assert!(matches!(
            decide(
                &VerificationClaim::NotApplicable("config only".into()),
                &runs, &[],
                2,
                true
            ),
            VerificationDecision::Refuse(message) if message.contains("failed")
        ));
    }

    #[test]
    fn a_later_success_recovers_a_failed_verification() {
        let runs = vec![
            run("cargo test", false, 5, 2),
            run("cargo test", true, 7, 2),
        ];
        assert_eq!(
            decide(&VerificationClaim::Missing, &runs, &[], 2, true),
            VerificationDecision::Accept { warning: None }
        );
    }

    #[test]
    fn changed_files_without_evidence_complete_with_a_warning_and_read_only_goals_do_not() {
        assert!(matches!(
            decide(&VerificationClaim::Missing, &[], &[], 0, true),
            VerificationDecision::Accept { warning: Some(warning) }
                if warning.contains("without executable verification")
        ));
        assert_eq!(
            decide(&VerificationClaim::Missing, &[], &[], 0, false),
            VerificationDecision::Accept { warning: None }
        );
    }

    #[test]
    fn a_pass_that_predates_the_last_change_cannot_stand_in_for_a_fresh_one() {
        let runs = vec![run("go test ./...", true, 4, 1)];
        assert!(matches!(
            decide(&VerificationClaim::Missing, &runs, &[], 3, true),
            VerificationDecision::Refuse(message) if message.contains("before the most recent")
        ));
    }

    #[test]
    fn an_explicit_not_applicable_is_accepted_with_a_warning() {
        match decide(
            &VerificationClaim::NotApplicable("  documentation only  ".into()),
            &[],
            &[],
            0,
            true,
        ) {
            VerificationDecision::Accept {
                warning: Some(warning),
            } => {
                assert!(warning.contains("documentation only"), "{warning}")
            }
            other => panic!("expected an accepted warning, got {other:?}"),
        }
    }

    #[test]
    fn claims_parse_only_their_two_explicit_shapes() {
        assert_eq!(
            parse_claim(&serde_json::json!({})),
            Ok(VerificationClaim::Missing)
        );
        assert_eq!(
            parse_claim(&serde_json::json!({
                "verification": {"status": "passed", "detail": "go test ./...\ngo vet ./..."}
            })),
            Ok(VerificationClaim::Passed(vec![
                "go test ./...".into(),
                "go vet ./...".into()
            ]))
        );
        assert_eq!(
            parse_claim(&serde_json::json!({
                "verification": {"status": "not_applicable", "detail": "docs"}
            })),
            Ok(VerificationClaim::NotApplicable("docs".into()))
        );
        assert!(
            parse_claim(&serde_json::json!({
                "verification": {"status": "maybe"}
            }))
            .is_err()
        );
        assert!(parse_claim(&serde_json::json!({"verification": {}})).is_err());
    }

    #[test]
    fn command_labels_are_the_commands_the_tools_ran() {
        assert_eq!(
            command_label(
                "run_command",
                &serde_json::json!({"program": "go", "args": ["test", "./..."]})
            ),
            Some("go test ./...".to_string())
        );
        assert_eq!(
            command_label("shell_command", &serde_json::json!({"cmd": "make ci"})),
            Some("make ci".to_string())
        );
        assert_eq!(command_label("read_file", &serde_json::json!({})), None);
        assert_eq!(
            command_label("run_command", &serde_json::json!({"program": "  "})),
            None
        );
    }

    #[test]
    fn verification_classification_is_narrow_enough_not_to_read_prose_as_a_test() {
        for command in [
            "go test ./...",
            "go build ./...",
            "cargo clippy --all-targets",
            "npm run build",
            "pnpm test",
            "make ci",
            "sh verify.sh",
            "bash ./scripts/check.sh",
            "./acceptance.sh",
            "pytest -q",
            "python3 -m unittest",
            "python3 test_app.py",
            "python -m pytest -q",
        ] {
            assert!(is_verification_command(command), "{command}");
        }
        for command in [
            "grep -rn validate src",
            "cat testdata/verify.md",
            "go version",
            "git status",
            "ls scripts",
            "python3 script.py",
        ] {
            assert!(!is_verification_command(command), "{command}");
        }
    }
}
