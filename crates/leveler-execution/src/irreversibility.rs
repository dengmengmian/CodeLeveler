//! Directly identifiable irreversible effects for the effect-oriented Auto.
//!
//! The unit of analysis is the CURRENT call's resolved argv, never the text of
//! a script the call happens to name. `python script.py`, `npm run build` and
//! `make` are ordinary programs: their internal effects are the user's
//! accepted Auto risk and are deliberately not inspected.
//!
//! A recognized destruction primitive is the exception: when the program is
//! visibly `rm`/`git reset`/`dd`/etc. and its target cannot be read, the
//! uncertainty is about the ACTION ALREADY VISIBLE, so it asks. Unknown
//! programs never ask merely for being unknown.

use std::path::{Component, Path, PathBuf};

use leveler_core::EnvSnapshot;

use crate::approval::{
    CommandView, basename, nested_shell_c_body, shell_c_script, shell_tokens, split_shell_segments,
};
use crate::risk::RiskLevel;
use crate::{git_effects, shell_ast as ast};

/// Typed MCP annotations. Only `destructive_hint` can raise friction; a
/// server that omits or falsifies it cannot lower a boundary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct McpAnnotations {
    pub read_only_hint: Option<bool>,
    pub destructive_hint: Option<bool>,
    pub idempotent_hint: Option<bool>,
    pub open_world_hint: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrreversibleKind {
    FileDelete,
    FileDestroy,
    DiskDestroy,
    PersistentData,
    GitHistory,
    McpDestructive,
    HostShutdown,
}

impl IrreversibleKind {
    /// Stable reason code carried on the approval request and event.
    pub const fn code(self) -> &'static str {
        match self {
            Self::FileDelete => "irreversible.filesystem_delete",
            Self::FileDestroy => "irreversible.filesystem_destroy",
            Self::DiskDestroy => "irreversible.disk_destroy",
            Self::PersistentData => "irreversible.persistent_data",
            Self::GitHistory => "irreversible.git_history",
            Self::McpDestructive => "irreversible.mcp_destructive",
            Self::HostShutdown => "irreversible.host_shutdown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrreversibleFact {
    pub kind: IrreversibleKind,
    pub summary: String,
    /// Concrete deletion targets for the prompt and for `write_path_glob`
    /// rules. Empty when the target set is dynamic (`find -delete`) or remote
    /// (MCP).
    pub paths: Vec<PathBuf>,
}

impl IrreversibleFact {
    pub fn code(&self) -> &'static str {
        self.kind.code()
    }
}

/// The inputs the effect analysis may read. It deliberately has no tool
/// description and no model text.
pub struct EffectInput<'a> {
    pub tool: &'a str,
    pub risk: RiskLevel,
    pub command: Option<CommandView<'a>>,
    pub mcp_annotations: Option<McpAnnotations>,
    pub workspace_root: Option<&'a Path>,
    pub environment: &'a EnvSnapshot,
}

/// The single entry point. `None` means "no directly identifiable irreversible
/// effect", which is an AutoAllow for the effect-oriented policy.
pub fn analyze(input: &EffectInput<'_>) -> Option<IrreversibleFact> {
    if input
        .mcp_annotations
        .and_then(|annotations| annotations.destructive_hint)
        == Some(true)
    {
        return Some(IrreversibleFact {
            kind: IrreversibleKind::McpDestructive,
            summary: format!("MCP tool `{}` declares destructive behavior", input.tool),
            paths: Vec::new(),
        });
    }
    let command = input.command.as_ref()?;
    analyze_command(command, input)
}

fn analyze_command(command: &CommandView<'_>, input: &EffectInput<'_>) -> Option<IrreversibleFact> {
    let program = command.program;
    let args = command.args;

    // `cmd /C` is not a bash script. The bash AST would read it as text and
    // could both miss a real `del` and invent one inside a quoted argument.
    let cmd_wrapper = basename(program).eq_ignore_ascii_case("cmd")
        || basename(program).eq_ignore_ascii_case("cmd.exe");
    if cmd_wrapper {
        return shell_c_script(args).and_then(|script| analyze_script(script, true, input));
    }
    if crate::approval::is_shell_wrapper_program(program) {
        if let Some(script) = shell_c_script(args) {
            return analyze_script(script, false, input);
        }
        // `bash script.sh`: the body is not knowable here.
        return None;
    }

    let executed = ast::executed_commands(program, args);
    if let Some(fact) = git_fact(&executed, program, args) {
        return Some(fact);
    }
    for words in &executed.commands {
        if let Some(fact) = analyze_words(words, input) {
            return Some(fact);
        }
    }
    None
}

/// Analyze one executed argv, peeling a transparent wrapper (`sudo`, `env`,
/// `nohup`, …) when present. A wrapper is not the effect; what it runs is.
fn analyze_words(words: &[String], input: &EffectInput<'_>) -> Option<IrreversibleFact> {
    if let Some(inner) = transparent_wrapper_argv(words) {
        if let Some(fact) = analyze_words(&inner, input) {
            return Some(fact);
        }
        // The inner program may itself be a shell wrapper or a Git command,
        // which the argv classifier alone does not decode.
        if let Some((inner_program, inner_args)) = inner.split_first() {
            return analyze_command(
                &CommandView {
                    program: inner_program,
                    args: inner_args,
                },
                input,
            );
        }
        return None;
    }
    analyze_argv(words, input)
}

/// The command a transparent wrapper runs, or `None` when `words` is not such
/// a wrapper. Deterministic argv parsing: no shell text, no model output.
fn transparent_wrapper_argv(words: &[String]) -> Option<Vec<String>> {
    let program = basename(words.first()?);
    let lower = program.to_ascii_lowercase();
    // Options that consume the following word. A wrapper whose option syntax
    // this table does not know is still analyzed as its own program, never
    // guessed into a different command.
    let value_flags: &[&str] = match lower.as_str() {
        "sudo" => &[
            "-u",
            "-g",
            "-h",
            "-p",
            "-C",
            "-T",
            "-U",
            "-r",
            "-t",
            "-D",
            "-R",
            "--user",
            "--group",
            "--host",
            "--prompt",
            "--close-from",
            "--command-timeout",
            "--other-user",
            "--role",
            "--type",
            "--chdir",
            "--chroot",
        ],
        "doas" => &["-u", "-C"],
        "env" => &["-u", "-C", "-S", "--unset", "--chdir", "--split-string"],
        "command" => &["-p"],
        "time" => &["-o", "-f", "--output", "--format"],
        "nice" => &["-n", "--adjustment"],
        "nohup" => &[],
        _ => return None,
    };
    let mut index = 1;
    let mut consumes_value = false;
    while let Some(word) = words.get(index) {
        if consumes_value {
            consumes_value = false;
            index += 1;
            continue;
        }
        if word == "--" {
            index += 1;
            break;
        }
        if lower == "command" && matches!(word.as_str(), "-v" | "-V") {
            // A path query runs nothing.
            return None;
        }
        if lower == "env" && is_env_assignment(word) {
            index += 1;
            continue;
        }
        if word != "-" && word.starts_with('-') {
            let flag = word.split('=').next().unwrap_or(word);
            consumes_value = !word.contains('=') && value_flags.contains(&flag);
            index += 1;
            continue;
        }
        break;
    }
    (index < words.len()).then(|| words[index..].to_vec())
}

fn is_env_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn analyze_script(
    script: &str,
    cmd_syntax: bool,
    input: &EffectInput<'_>,
) -> Option<IrreversibleFact> {
    if !cmd_syntax {
        let executed = ast::executed_commands("sh", &["-c".to_string(), script.to_string()]);
        if let Some(fact) = git_fact_from_commands(&executed, true) {
            return Some(fact);
        }
        for words in &executed.commands {
            if let Some(fact) = analyze_words(words, input) {
                return Some(fact);
            }
        }
        if executed.complete {
            return None;
        }
    }
    analyze_fallback(script, cmd_syntax, input)
}

/// String fallback for an AST that could not resolve every word, and the only
/// path for `cmd /C`. It recognizes a destruction primitive only when it is
/// the first word of a segment; quoted text stays text.
fn analyze_fallback(
    script: &str,
    cmd_syntax: bool,
    input: &EffectInput<'_>,
) -> Option<IrreversibleFact> {
    for segment in split_shell_segments(script) {
        let tokens = shell_tokens(&segment);
        if let Some(inner) = nested_shell_c_body(&tokens) {
            if let Some(fact) = analyze_script(&inner, cmd_syntax, input) {
                return Some(fact);
            }
            continue;
        }
        let Some((program, _rest)) = tokens.split_first() else {
            continue;
        };
        let lower = program.to_ascii_lowercase();
        if lower == "git" {
            if let Some(fact) = git_fact_from_words(&tokens, cmd_syntax) {
                return Some(fact);
            }
            continue;
        }
        if let Some(fact) = analyze_words(&tokens, input) {
            return Some(fact);
        }
    }
    None
}

fn git_fact(
    executed: &ast::ExecutedCommands,
    program: &str,
    args: &[String],
) -> Option<IrreversibleFact> {
    if let Some(fact) = git_fact_from_commands(executed, executed.complete) {
        return Some(fact);
    }
    if basename(program) == "git" && git_has_opaque_destructive_arg(program, args) {
        return Some(IrreversibleFact {
            kind: IrreversibleKind::GitHistory,
            summary: "Git history loss is possible: a destructive Git flag is not readable"
                .to_string(),
            paths: Vec::new(),
        });
    }
    None
}

fn git_fact_from_commands(
    executed: &ast::ExecutedCommands,
    complete: bool,
) -> Option<IrreversibleFact> {
    let effects = executed.git_effects();
    if effects.effects.irreversible {
        return Some(IrreversibleFact {
            kind: IrreversibleKind::GitHistory,
            summary: format!(
                "Git history or working-tree changes would be lost ({})",
                effects.effects.capabilities().join(", ")
            ),
            paths: Vec::new(),
        });
    }
    let _ = complete;
    None
}

fn git_fact_from_words(words: &[String], cmd_syntax: bool) -> Option<IrreversibleFact> {
    let complete = !words
        .iter()
        .any(|word| word.contains('$') || word.contains('`') || (cmd_syntax && word.contains('%')));
    let effects = git_effects::call_git_effects(&[words.to_vec()], complete);
    if effects.effects.irreversible {
        return Some(IrreversibleFact {
            kind: IrreversibleKind::GitHistory,
            summary: format!(
                "Git history or working-tree changes would be lost ({})",
                effects.effects.capabilities().join(", ")
            ),
            paths: Vec::new(),
        });
    }
    if !complete && git_words_are_destructive_capable(words) {
        return Some(IrreversibleFact {
            kind: IrreversibleKind::GitHistory,
            summary: "Git history loss is possible: a destructive Git flag is not readable"
                .to_string(),
            paths: Vec::new(),
        });
    }
    None
}

fn git_has_opaque_destructive_arg(program: &str, args: &[String]) -> bool {
    if basename(program) != "git" {
        return false;
    }
    let words: Vec<String> = std::iter::once(program.to_string())
        .chain(args.iter().cloned())
        .collect();
    let complete = !args
        .iter()
        .any(|word| word.contains('$') || word.contains('`'));
    !complete && git_words_are_destructive_capable(&words)
}

/// Only subcommands whose destructive form is flag-driven. `checkout $branch`
/// and `switch $branch` are ordinary branch movement and must not ask merely
/// because the name is a variable.
fn git_words_are_destructive_capable(words: &[String]) -> bool {
    let Some(subcommand) = words
        .iter()
        .skip(1)
        .find(|word| !word.starts_with('-'))
        .map(|word| word.as_str())
    else {
        return false;
    };
    matches!(
        subcommand,
        "reset" | "clean" | "push" | "branch" | "tag" | "stash" | "rm" | "restore" | "prune"
    )
}

fn analyze_argv(words: &[String], input: &EffectInput<'_>) -> Option<IrreversibleFact> {
    let (program, args) = words.split_first()?;
    let program = basename(program);
    let lower = program.to_ascii_lowercase();
    let stem = lower.strip_suffix(".exe").unwrap_or(&lower);

    if matches!(stem, "shutdown" | "reboot" | "halt" | "poweroff")
        || matches!(stem, "stop-computer" | "restart-computer")
    {
        return Some(IrreversibleFact {
            kind: IrreversibleKind::HostShutdown,
            summary: format!("`{program}` stops or restarts the host"),
            paths: Vec::new(),
        });
    }

    if matches!(
        stem,
        "rm" | "rmdir" | "unlink" | "shred" | "del" | "erase" | "rd"
    ) {
        return classify_deletion(program, args, input);
    }
    if stem == "truncate" {
        return classify_truncate(program, args, input);
    }
    if stem == "find" {
        return classify_find(args);
    }
    if stem == "xargs" {
        return classify_xargs(args);
    }
    if stem == "rsync" {
        return args
            .iter()
            .any(|arg| arg == "--delete" || arg.starts_with("--delete-"))
            .then(|| IrreversibleFact {
                kind: IrreversibleKind::FileDelete,
                summary: "`rsync --delete` removes files in the destination".to_string(),
                paths: Vec::new(),
            });
    }
    if stem == "dd" {
        return classify_dd(args);
    }
    if stem.starts_with("mkfs")
        || matches!(
            stem,
            "mke2fs" | "wipefs" | "sfdisk" | "fdisk" | "parted" | "diskutil" | "format"
        )
    {
        return classify_disk(program, args);
    }
    if matches!(
        stem,
        "psql" | "mysql" | "mariadb" | "sqlite3" | "clickhouse-client" | "duckdb"
    ) {
        return classify_sql(program, args);
    }
    if stem == "redis-cli" {
        return classify_redis(program, args);
    }
    if stem == "docker" {
        return classify_docker(args);
    }
    if matches!(stem, "kubectl" | "terraform" | "pulumi") {
        return classify_remote_delete(program, args);
    }
    None
}

fn classify_deletion(
    program: &str,
    args: &[String],
    input: &EffectInput<'_>,
) -> Option<IrreversibleFact> {
    let windows = matches!(
        program.to_ascii_lowercase().as_str(),
        "del" | "erase" | "rd"
    );
    let mut operands: Vec<&str> = Vec::new();
    let mut options_ended = false;
    for arg in args {
        if !options_ended && arg == "--" {
            options_ended = true;
            continue;
        }
        if !options_ended && is_delete_option(arg, windows) {
            continue;
        }
        operands.push(arg);
    }
    if operands.is_empty() {
        // `rm -rf` with no target fails usage; it deletes nothing.
        return None;
    }
    let mut paths = Vec::new();
    let mut unreadable = false;
    for operand in operands {
        match literal_target(operand, input) {
            Some(path) => {
                if !is_disposable(&path, input) {
                    paths.push(path);
                }
            }
            None => unreadable = true,
        }
    }
    if unreadable {
        return Some(IrreversibleFact {
            kind: IrreversibleKind::FileDelete,
            summary: format!("`{program}` deletes a target that cannot be resolved before running"),
            paths,
        });
    }
    if paths.is_empty() {
        return None;
    }
    Some(IrreversibleFact {
        kind: IrreversibleKind::FileDelete,
        summary: format!(
            "`{program}` deletes real data: {}",
            paths
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        paths,
    })
}

fn is_delete_option(arg: &str, windows: bool) -> bool {
    if arg != "-" && arg.starts_with('-') {
        return true;
    }
    windows && arg.len() > 1 && arg.starts_with('/') && !arg.starts_with("//")
}

fn classify_truncate(
    program: &str,
    args: &[String],
    input: &EffectInput<'_>,
) -> Option<IrreversibleFact> {
    let zero = args
        .iter()
        .any(|arg| arg == "-s0" || arg == "--size=0" || arg == "-s 0")
        || args
            .windows(2)
            .any(|pair| pair[0] == "-s" && pair[1] == "0");
    if !zero {
        return None;
    }
    let operands: Vec<&str> = args
        .iter()
        .filter(|arg| !arg.starts_with('-') && *arg != "0")
        .map(String::as_str)
        .collect();
    let paths: Vec<PathBuf> = operands
        .into_iter()
        .filter_map(|operand| literal_target(operand, input))
        .filter(|path| !is_disposable(path, input))
        .collect();
    (!paths.is_empty()).then(|| IrreversibleFact {
        kind: IrreversibleKind::FileDestroy,
        summary: format!(
            "`{program}` truncates real data: {}",
            paths
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        paths,
    })
}

fn classify_find(args: &[String]) -> Option<IrreversibleFact> {
    let mut deletes = args.iter().any(|arg| arg == "-delete");
    if !deletes {
        for pair in args.windows(2) {
            if matches!(pair[0].as_str(), "-exec" | "-execdir" | "-ok" | "-okdir")
                && matches!(
                    basename(&pair[1]).to_ascii_lowercase().as_str(),
                    "rm" | "rmdir" | "unlink" | "shred"
                )
            {
                deletes = true;
                break;
            }
        }
    }
    deletes.then(|| IrreversibleFact {
        kind: IrreversibleKind::FileDelete,
        summary: "`find` deletes a target set that is only known at run time".to_string(),
        paths: Vec::new(),
    })
}

fn classify_xargs(args: &[String]) -> Option<IrreversibleFact> {
    let command = args
        .iter()
        .find(|arg| !arg.starts_with('-'))
        .map(|arg| basename(arg).to_ascii_lowercase())?;
    matches!(command.as_str(), "rm" | "rmdir" | "unlink" | "shred").then(|| IrreversibleFact {
        kind: IrreversibleKind::FileDelete,
        summary: format!("`xargs {command}` deletes a target set read from its input"),
        paths: Vec::new(),
    })
}

fn classify_dd(args: &[String]) -> Option<IrreversibleFact> {
    let target = args.iter().find_map(|arg| arg.strip_prefix("of="))?;
    let device = target.starts_with("/dev/")
        || target.starts_with("\\\\.\\")
        || target.to_ascii_lowercase().starts_with("\\\\.\\");
    device.then(|| IrreversibleFact {
        kind: IrreversibleKind::DiskDestroy,
        summary: format!("`dd` writes raw data to device `{target}`"),
        paths: Vec::new(),
    })
}

fn classify_disk(program: &str, args: &[String]) -> Option<IrreversibleFact> {
    let lower = program.to_ascii_lowercase();
    let destructive = if lower == "fdisk" {
        args.iter().any(|arg| arg == "-w" || arg == "--script")
    } else if lower == "parted" {
        args.iter()
            .any(|arg| matches!(arg.as_str(), "mkpart" | "rm" | "mklabel" | "mkfs"))
    } else if lower == "diskutil" {
        args.iter().any(|arg| {
            let arg = arg.to_ascii_lowercase();
            arg.starts_with("erase") || arg == "partitiondisk" || arg == "secureerase"
        })
    } else {
        true
    };
    destructive.then(|| IrreversibleFact {
        kind: IrreversibleKind::DiskDestroy,
        summary: format!("`{program}` destroys a disk, partition or filesystem"),
        paths: Vec::new(),
    })
}

fn classify_sql(program: &str, args: &[String]) -> Option<IrreversibleFact> {
    let sql = inline_sql(args)?;
    let first = first_keyword(sql)?;
    let destructive = matches!(
        first,
        "DROP" | "TRUNCATE" | "DELETE" | "FLUSHALL" | "FLUSHDB" | "SHUTDOWN"
    ) || (first == "ALTER" && sql.to_ascii_uppercase().contains(" DROP "));
    destructive.then(|| IrreversibleFact {
        kind: IrreversibleKind::PersistentData,
        summary: format!("`{program}` runs a destructive inline statement ({first})"),
        paths: Vec::new(),
    })
}

fn inline_sql(args: &[String]) -> Option<&str> {
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        if matches!(arg.as_str(), "-c" | "-e" | "--command" | "-q" | "--execute")
            && let Some(value) = iter.next()
        {
            return Some(value);
        }
        if !arg.starts_with('-') {
            return Some(arg);
        }
    }
    None
}

fn first_keyword(sql: &str) -> Option<&'static str> {
    let mut rest = sql.trim_start();
    loop {
        if let Some(after) = rest.strip_prefix("--") {
            rest = after.split_once('\n').map(|(_, tail)| tail).unwrap_or("");
            continue;
        }
        if let Some(after) = rest.strip_prefix("/*") {
            rest = after.split_once("*/").map(|(_, tail)| tail).unwrap_or("");
            continue;
        }
        break;
    }
    let word: String = rest
        .trim_start()
        .chars()
        .take_while(|ch| ch.is_ascii_alphabetic())
        .collect();
    match word.to_ascii_uppercase().as_str() {
        "DROP" => Some("DROP"),
        "TRUNCATE" => Some("TRUNCATE"),
        "DELETE" => Some("DELETE"),
        "FLUSHALL" => Some("FLUSHALL"),
        "FLUSHDB" => Some("FLUSHDB"),
        "SHUTDOWN" => Some("SHUTDOWN"),
        "ALTER" => Some("ALTER"),
        _ => None,
    }
}

fn classify_redis(program: &str, args: &[String]) -> Option<IrreversibleFact> {
    let destructive = args.iter().any(|arg| {
        matches!(
            arg.to_ascii_uppercase().as_str(),
            "FLUSHALL" | "FLUSHDB" | "SHUTDOWN"
        )
    });
    destructive.then(|| IrreversibleFact {
        kind: IrreversibleKind::PersistentData,
        summary: format!("`{program}` flushes or stops a persistent data store"),
        paths: Vec::new(),
    })
}

fn classify_docker(args: &[String]) -> Option<IrreversibleFact> {
    let destructive = match args.first().map(String::as_str) {
        Some("volume") => matches!(args.get(1).map(String::as_str), Some("rm" | "prune")),
        Some("compose") => {
            args.iter().any(|arg| arg == "down")
                && args.iter().any(|arg| arg == "-v" || arg == "--volumes")
        }
        Some("system") => {
            args.iter().any(|arg| arg == "prune") && args.iter().any(|arg| arg == "--volumes")
        }
        _ => false,
    };
    destructive.then(|| IrreversibleFact {
        kind: IrreversibleKind::PersistentData,
        summary: "Docker removes a persistent volume".to_string(),
        paths: Vec::new(),
    })
}

fn classify_remote_delete(program: &str, args: &[String]) -> Option<IrreversibleFact> {
    let destructive = match program.to_ascii_lowercase().as_str() {
        "kubectl" => args.iter().any(|arg| arg == "delete"),
        "terraform" | "pulumi" => args.iter().any(|arg| arg == "destroy"),
        _ => false,
    };
    destructive.then(|| IrreversibleFact {
        kind: IrreversibleKind::PersistentData,
        summary: format!("`{program}` deletes a remote resource"),
        paths: Vec::new(),
    })
}

/// A literal path the shell will not expand. `$TMPDIR/...` is the one trusted
/// host variable the scratch-root rule needs; everything else opaque stays
/// opaque.
fn literal_target(raw: &str, input: &EffectInput<'_>) -> Option<PathBuf> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let resolved = if let Some(rest) = raw
        .strip_prefix("$TMPDIR/")
        .or_else(|| raw.strip_prefix("${TMPDIR}/"))
    {
        if rest.contains(['$', '`', '*', '?', '[', '{']) {
            return None;
        }
        input.environment.temp_dir().join(rest)
    } else {
        if raw.contains(['$', '`', '*', '?', '[', '{', '~']) || (raw.contains('%') && cfg!(windows))
        {
            return None;
        }
        let path = Path::new(raw);
        if path.components().any(|c| c == Component::ParentDir) {
            return None;
        }
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            input.workspace_root?.join(path)
        }
    };
    Some(resolved)
}

/// A target whose removal cannot lose user data. Conservative by design: a
/// path that is not provably a rebuildable artifact is NOT disposable.
fn is_disposable(path: &Path, input: &EffectInput<'_>) -> bool {
    if !path.exists() {
        return true;
    }
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    // A deletion outside the workspace is ordinary work in V2 Auto, but it is
    // still real data unless the path is a provably rebuildable artifact. The
    // class cannot be inferred from a parent directory (`/tmp`, `/var/tmp`)
    // alone: an untracked real file there is not disposable.
    let Some(workspace) = input.workspace_root else {
        return false;
    };
    let workspace = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    if !canonical.starts_with(&workspace) {
        return false;
    }
    if canonical.components().any(|component| {
        matches!(
            component,
            Component::Normal(name)
                if matches!(
                    name.to_string_lossy().as_ref(),
                    "__pycache__" | ".pytest_cache" | ".mypy_cache" | ".ruff_cache"
                )
        )
    }) {
        return true;
    }
    // Ecosystem-anchored build outputs. The anchor is what separates a real
    // build directory from a project that happens to be named `target`.
    let relative = match canonical.strip_prefix(&workspace) {
        Ok(relative) => relative,
        Err(_) => return false,
    };
    let mut ancestor = workspace.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return false;
        };
        let name = name.to_string_lossy();
        if name == "target" && ancestor.join("Cargo.toml").is_file() {
            return true;
        }
        if name == "node_modules" && ancestor.join("package.json").is_file() {
            return true;
        }
        if name == ".next"
            && std::fs::read_dir(&ancestor).is_ok_and(|entries| {
                entries.filter_map(Result::ok).any(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with("next.config.")
                })
            })
        {
            return true;
        }
        if matches!(name.as_ref(), "dist" | "build" | "out")
            && (ancestor.join("package.json").is_file()
                || ancestor.join("pyproject.toml").is_file()
                || ancestor.join("setup.py").is_file())
        {
            return true;
        }
        ancestor.push(name.as_ref());
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        env: EnvSnapshot,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let env = EnvSnapshot::new(
            Vec::<(std::ffi::OsString, std::ffi::OsString)>::new(),
            root.clone(),
            tmp.path().to_path_buf(),
        );
        Fixture {
            _tmp: tmp,
            root,
            env,
        }
    }

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_string()).collect()
    }

    fn analyze_shell(f: &Fixture, script: &str) -> Option<IrreversibleFact> {
        let args = args(&["-c", script]);
        analyze(&EffectInput {
            tool: "shell_command",
            risk: RiskLevel::WorkspaceWrite,
            command: Some(CommandView {
                program: "sh",
                args: &args,
            }),
            mcp_annotations: None,
            workspace_root: Some(&f.root),
            environment: &f.env,
        })
    }

    #[test]
    fn ordinary_scripts_are_not_inspected() {
        let f = fixture();
        std::fs::write(f.root.join("script.py"), "import os\n").unwrap();
        assert!(analyze_shell(&f, "python script.py").is_none());
        assert!(analyze_shell(&f, "npm run build").is_none());
        assert!(analyze_shell(&f, "cargo test").is_none());
    }

    #[test]
    fn real_deletion_asks_and_build_output_does_not() {
        let f = fixture();
        std::fs::write(f.root.join("Cargo.toml"), "[package]\n").unwrap();
        std::fs::create_dir_all(f.root.join("src")).unwrap();
        std::fs::create_dir_all(f.root.join("target/debug")).unwrap();
        let real = analyze_shell(&f, "rm -rf src").expect("src is real data");
        assert_eq!(real.kind, IrreversibleKind::FileDelete);
        assert!(analyze_shell(&f, "rm -rf target").is_none());
        // A real file outside the workspace is still real data. A parent
        // directory's name (`/tmp`, the OS temp dir) cannot make it
        // disposable; only an absent target is free.
        let victim = f._tmp.path().join("victim.txt");
        std::fs::write(&victim, "x").unwrap();
        let outside = analyze_shell(&f, &format!("rm -rf {}", victim.display()))
            .expect("a real file outside the workspace is not disposable");
        assert_eq!(outside.kind, IrreversibleKind::FileDelete);
        let missing = f._tmp.path().join("never-created");
        assert!(analyze_shell(&f, &format!("rm -rf {}", missing.display())).is_none());
    }

    #[test]
    fn an_unreadable_deletion_target_asks() {
        let f = fixture();
        assert!(analyze_shell(&f, "rm -rf \"$dir\"").is_some());
    }

    #[test]
    fn git_history_loss_is_detected() {
        let f = fixture();
        assert!(analyze_shell(&f, "git reset --hard").is_some());
        assert!(analyze_shell(&f, "git clean -fd").is_some());
        assert!(analyze_shell(&f, "git push --force origin main").is_some());
        assert!(analyze_shell(&f, "git status").is_none());
        assert!(analyze_shell(&f, "git push origin main").is_none());
    }

    #[test]
    fn mcp_destructive_hint_asks_and_ordinary_mcp_does_not() {
        let f = fixture();
        let base = |hint| EffectInput {
            tool: "mcp__db__drop",
            risk: RiskLevel::Network,
            command: None,
            mcp_annotations: Some(McpAnnotations {
                destructive_hint: hint,
                ..McpAnnotations::default()
            }),
            workspace_root: Some(&f.root),
            environment: &f.env,
        };
        assert!(analyze(&base(Some(true))).is_some());
        assert!(analyze(&base(Some(false))).is_none());
        assert!(analyze(&base(None)).is_none());
    }

    #[test]
    fn inline_database_destruction_is_detected() {
        let f = fixture();
        assert!(analyze_shell(&f, "psql -c 'DROP TABLE users'").is_some());
        assert!(analyze_shell(&f, "psql -c 'SELECT 1'").is_none());
        assert!(analyze_shell(&f, "psql -f migration.sql").is_none());
    }

    #[test]
    fn device_destruction_is_detected_but_a_file_overwrite_is_not() {
        let f = fixture();
        assert!(analyze_shell(&f, "dd if=/dev/zero of=/dev/disk2").is_some());
        assert!(analyze_shell(&f, "dd if=in.iso of=out.img").is_none());
    }

    #[test]
    fn sudo_is_not_an_irreversible_effect() {
        let f = fixture();
        assert!(analyze_shell(&f, "sudo -n true").is_none());
    }

    #[test]
    fn transparent_wrappers_do_not_hide_a_direct_deletion() {
        let f = fixture();
        let victim = f._tmp.path().join("wrapped.txt");
        std::fs::write(&victim, "x").unwrap();
        let target = victim.display().to_string();
        for script in [
            format!("sudo rm -rf {target}"),
            format!("sudo -u root rm -rf {target}"),
            format!("env LEVELER_TEST=1 rm -rf {target}"),
            format!("nohup rm -rf {target}"),
            format!("env FOO=1 sudo -n rm -rf {target}"),
        ] {
            let fact = analyze_shell(&f, &script)
                .unwrap_or_else(|| panic!("{script} must name the deletion"));
            assert_eq!(fact.kind, IrreversibleKind::FileDelete, "{script}");
        }
        // The same wrappers around ordinary work stay ordinary.
        for script in ["sudo -n true", "env FOO=1 true", "nohup true"] {
            assert!(analyze_shell(&f, script).is_none(), "{script}");
        }
    }
}
