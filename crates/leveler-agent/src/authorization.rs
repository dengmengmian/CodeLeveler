//! Tool-call authorization helpers: command/path extraction, write
//! allowlists, approval signatures, tool classification.

use leveler_execution::{is_shell_wrapper_program, shell_c_script};
use leveler_model::ToolCall;
use sha2::{Digest, Sha256};

/// Whether a tool is a read-only search/lookup (subject to the per-step search
/// budget). These gather context but never change the workspace.
pub(crate) fn is_search_tool(name: &str) -> bool {
    matches!(
        name,
        "grep" | "repository_search" | "find_symbol" | "read_symbol" | "find_references"
    )
}

/// Whether a `run_command` call runs a verification-class program (build /
/// test / typecheck runner). Heuristic by program basename: the real gate is
/// the verifier — this only decides whether the executor nudges the model to
/// verify before accepting a completion.
pub(crate) fn is_verification_command(arguments: &serde_json::Value) -> bool {
    const VERIFICATION_PROGRAMS: &[&str] = &[
        "cargo", "rustc", "go", "npm", "pnpm", "yarn", "npx", "bun", "deno", "node", "tsc", "jest",
        "vitest", "mocha", "pytest", "python", "python3", "tox", "mypy", "ruff", "make", "just",
        "gradle", "gradlew", "mvn", "mvnw", "dotnet", "ctest", "cmake", "swift", "zig",
    ];
    let Some(program) = arguments.get("program").and_then(|v| v.as_str()) else {
        return false;
    };
    let base = std::path::Path::new(program)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(program);
    VERIFICATION_PROGRAMS.contains(&base)
}

pub(crate) fn collect_scoped_paths_from_call(call: &ToolCall, out: &mut Vec<String>) {
    if let Some(path) = call.arguments.get("path").and_then(|v| v.as_str()) {
        push_unique_path(out, path);
    }
    if call.name == "run_command"
        && let Some(cwd) = call.arguments.get("cwd").and_then(|v| v.as_str())
    {
        push_unique_path(out, cwd);
    }
    if call.name == "apply_patch"
        && let Some(patch) = call.arguments.get("patch").and_then(|v| v.as_str())
    {
        for path in patch_paths(patch) {
            push_unique_path(out, &path);
        }
    }
}

pub(crate) fn patch_paths(patch: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for line in patch.lines() {
        for prefix in [
            "*** Add File: ",
            "*** Update File: ",
            "*** Delete File: ",
            "*** Move to: ",
        ] {
            if let Some(path) = line.strip_prefix(prefix) {
                paths.push(path.trim().to_string());
            }
        }
    }
    paths
}

/// The patch's target files that fall outside `allowlist` (worker ownership).
/// A target is allowed if it equals an entry or sits under an allowed directory
/// prefix. Paths are normalized (`./` stripped) before comparison.
/// The paths a write tool (`apply_patch`/`replace`) would touch that fall
/// outside the allowlist (files or directory prefixes).
pub(crate) fn write_targets_outside_allowlist(
    call: &ToolCall,
    allowlist: &[String],
) -> Vec<String> {
    let norm = |p: &str| p.trim().trim_start_matches("./").to_string();
    let allow: Vec<String> = allowlist.iter().map(|p| norm(p)).collect();
    let targets: Vec<String> = match call.name.as_str() {
        "apply_patch" => {
            let patch = call
                .arguments
                .get("patch")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            patch_paths(patch)
        }
        "replace" => call
            .arguments
            .get("path")
            .and_then(|v| v.as_str())
            .map(|p| vec![p.to_string()])
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    targets
        .into_iter()
        .map(|p| norm(&p))
        .filter(|target| {
            !allow
                .iter()
                .any(|a| target == a || target.starts_with(&format!("{a}/")))
        })
        .collect()
}

pub(crate) fn push_unique_path(out: &mut Vec<String>, path: &str) {
    let normalized = path.trim().trim_start_matches("./");
    if normalized.is_empty()
        || normalized.starts_with('/')
        || normalized
            .split('/')
            .any(|segment| segment == ".." || segment.is_empty())
    {
        return;
    }
    if !out.iter().any(|existing| existing == normalized) {
        out.push(normalized.to_string());
    }
}

/// Whether this call is a host opener (`open` / `xdg-open` / …) that must run
/// outside the workspace seatbelt after the user approves.
pub(crate) fn call_needs_host_escape(call: &ToolCall) -> bool {
    let (program, args) = extract_command(call);
    let Some(program) = program.as_deref() else {
        return false;
    };
    leveler_execution::command_needs_host_escape(&leveler_execution::CommandView {
        program,
        args: &args,
    })
}

/// Pull `(program, args)` out of a command tool call for classification.
///
/// - `run_command` → structured `(program, args)`
/// - `shell_command` → platform shell wrapper `(sh|cmd, ["-c"|/C, raw_cmd])`
///   so [`leveler_execution::classify_command`] can inspect the script body.
///   The original script is preserved as the final arg for grant identity.
pub(crate) fn extract_command(call: &ToolCall) -> (Option<String>, Vec<String>) {
    match call.name.as_str() {
        "run_command" => {
            let program = call
                .arguments
                .get("program")
                .and_then(|v| v.as_str())
                .map(String::from);
            let mut args = call
                .arguments
                .get("args")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            if let Some(program) = &program {
                drop_duplicate_program_arg(program, &mut args);
            }
            (program, args)
        }
        "shell_command" => {
            let cmd = call
                .arguments
                .get("cmd")
                .or_else(|| call.arguments.get("command"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let (program, args) = shell_invocation_for_classification(cmd);
            (Some(program), args)
        }
        _ => (None, Vec::new()),
    }
}

/// Platform shell wrapper used for classification (mirrors `shell_command` tool).
fn shell_invocation_for_classification(cmd: &str) -> (String, Vec<String>) {
    #[cfg(windows)]
    {
        ("cmd".into(), vec!["/C".into(), cmd.to_string()])
    }
    #[cfg(not(windows))]
    {
        ("sh".into(), vec!["-c".into(), cmd.to_string()])
    }
}

pub(crate) fn drop_duplicate_program_arg(program: &str, args: &mut Vec<String>) {
    let Some(first) = args.first() else {
        return;
    };
    let program_name = std::path::Path::new(program)
        .file_name()
        .and_then(|p| p.to_str())
        .unwrap_or(program);
    if first == program || first == program_name {
        args.remove(0);
    }
}

/// Command line for permission-rule matching and approval UI.
///
/// `shell_command` uses the raw `cmd` string (not `sh -c …`) so rules can match
/// prefixes like `cargo test` against the script body.
pub(crate) fn command_line_for_match(
    call: &ToolCall,
    program: Option<&str>,
    args: &[String],
) -> Option<String> {
    if call.name == "shell_command" {
        return call
            .arguments
            .get("cmd")
            .or_else(|| call.arguments.get("command"))
            .and_then(|v| v.as_str())
            .map(String::from);
    }
    program.map(|p| format!("{} {}", p, args.join(" ")).trim().to_string())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(bytes);
    format!("{:x}", digest.finalize())
}

/// A stable signature for "approve for the session".
///
/// Ordinary `run_command` uses `tool:program:first_arg` so approving `git push`
/// covers later `git push`s. Shell wrappers **must not** collapse to
/// `…:sh:-c` / `shell_command:sh:-c` — that would grant every subsequent shell
/// script. For `shell_command` and `run_command` of `sh -c` / `cmd /C <script>`,
/// identity is the SHA-256 of the trimmed script body.
///
/// Shell detection and `-c`/`/C` extraction are shared with
/// [`leveler_execution::classify_command`] via
/// [`leveler_execution::is_shell_wrapper_program`] /
/// [`leveler_execution::shell_c_script`].
pub(crate) fn approval_signature(tool: &str, program: Option<&str>, args: &[String]) -> String {
    if tool == "shell_command" {
        let script = shell_c_script(args).unwrap_or("").trim();
        return format!("shell_command:{}", sha256_hex(script.as_bytes()));
    }
    if tool == "run_command"
        && let Some(p) = program
        && is_shell_wrapper_program(p)
        && let Some(script) = shell_c_script(args)
    {
        return format!("run_command:{}", sha256_hex(script.trim().as_bytes()));
    }
    match program {
        Some(p) => format!(
            "{tool}:{p}:{}",
            args.first().map(String::as_str).unwrap_or("")
        ),
        None => tool.to_string(),
    }
}

/// Whether this tool call would count as completion verification evidence.
///
/// Double-gated: only `run_command` with a verification-class `program`
/// basename. Mirrors the executor gate so `shell_command` (even with a
/// spoofed `program` field) never counts.
pub(crate) fn counts_as_verification_evidence(tool: &str, arguments: &serde_json::Value) -> bool {
    tool == "run_command" && is_verification_command(arguments)
}

/// Stable, non-reversible identity of one exact proposed action. Used to bind
/// a pending user decision without retaining another copy of raw arguments.
pub(crate) fn action_fingerprint(call: &ToolCall) -> String {
    let mut digest = Sha256::new();
    digest.update(call.name.as_bytes());
    digest.update([0]);
    digest.update(serde_json::to_vec(&call.arguments).unwrap_or_default());
    format!("{:x}", digest.finalize())
}

/// Pure workspace observation (no mutation, no verification).
///
/// These calls are collapsed for the no-progress loop guard and refused after a
/// fully completed plan so the model cannot thrash on `git status` variants.
pub(crate) fn is_pure_observe_call(name: &str, arguments: &serde_json::Value) -> bool {
    observe_class(name, arguments).is_some()
}

/// Stable observe class for loop-guard fingerprinting.
///
/// `git status` via dedicated tool, `run_command`, or `shell_command` share one
/// class so swapping the wrapper does not reset the no-progress counter.
///
/// Returns an owned key so path/pattern can distinguish different observes
/// (same tool, different target is not thrash).
pub(crate) fn observe_class(name: &str, arguments: &serde_json::Value) -> Option<String> {
    match name {
        "list_files" => {
            let path = arguments
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or(".");
            Some(format!("observe:list_files:{path}"))
        }
        "grep" | "repository_search" => {
            let pattern = arguments
                .get("pattern")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            Some(format!("observe:search:{pattern}"))
        }
        "git_status" => Some("observe:git_status".into()),
        "run_command" => {
            let program = arguments
                .get("program")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let args = arguments
                .get("args")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            classify_observe_cmdline(program, &args).map(str::to_string)
        }
        "shell_command" => {
            let cmd = arguments
                .get("cmd")
                .or_else(|| arguments.get("command"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            classify_observe_shell(cmd).map(str::to_string)
        }
        _ => None,
    }
}

fn classify_observe_cmdline(program: &str, args: &str) -> Option<&'static str> {
    let base = std::path::Path::new(program)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(program)
        .to_ascii_lowercase();
    if base != "git" {
        return None;
    }
    classify_git_observe_args(args)
}

fn classify_observe_shell(cmd: &str) -> Option<&'static str> {
    let lower = cmd.to_ascii_lowercase();
    // Strip common prefixes so `cd … && git status` still classifies.
    let git_idx = lower.find("git ")?;
    let rest = lower[git_idx + 4..].trim();
    classify_git_observe_args(rest)
}

fn classify_git_observe_args(args: &str) -> Option<&'static str> {
    let a = args.trim();
    // First token after optional global flags is the subcommand.
    let sub = a
        .split_whitespace()
        .find(|t| !t.starts_with('-'))
        .unwrap_or("");
    match sub {
        "status" => Some("observe:git_status"),
        "diff" => Some("observe:git_diff"),
        "log" | "show" | "branch" | "remote" | "rev-parse" | "symbolic-ref" => {
            Some("observe:git_read")
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_core::ToolCallId;

    fn tool_call(name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: ToolCallId::new("t"),
            name: name.to_string(),
            arguments,
        }
    }

    #[test]
    fn git_status_variants_share_observe_class() {
        let a = observe_class(
            "run_command",
            &serde_json::json!({"program": "git", "args": ["status", "--porcelain"]}),
        );
        let b = observe_class(
            "shell_command",
            &serde_json::json!({"cmd": "git status -sb"}),
        );
        let c = observe_class("git_status", &serde_json::json!({}));
        assert_eq!(a.as_deref(), Some("observe:git_status"));
        assert_eq!(b.as_deref(), Some("observe:git_status"));
        assert_eq!(c.as_deref(), Some("observe:git_status"));
    }

    #[test]
    fn cargo_test_is_not_pure_observe() {
        assert!(
            observe_class(
                "run_command",
                &serde_json::json!({"program": "cargo", "args": ["test"]}),
            )
            .is_none()
        );
    }

    #[test]
    fn extract_command_shell_uses_platform_wrapper() {
        let call = tool_call("shell_command", serde_json::json!({"cmd": "rm -rf x"}));
        let (program, args) = extract_command(&call);
        #[cfg(windows)]
        {
            assert_eq!(program.as_deref(), Some("cmd"));
            assert_eq!(args, vec!["/C".to_string(), "rm -rf x".to_string()]);
        }
        #[cfg(not(windows))]
        {
            assert_eq!(program.as_deref(), Some("sh"));
            assert_eq!(args, vec!["-c".to_string(), "rm -rf x".to_string()]);
        }
    }

    #[test]
    fn open_index_html_needs_host_escape() {
        let run = tool_call(
            "run_command",
            serde_json::json!({"program": "open", "args": ["index.html"]}),
        );
        assert!(call_needs_host_escape(&run));
        let shell = tool_call(
            "shell_command",
            serde_json::json!({"cmd": "open index.html"}),
        );
        assert!(call_needs_host_escape(&shell));
        let safe = tool_call(
            "run_command",
            serde_json::json!({"program": "ls", "args": ["."]}),
        );
        assert!(!call_needs_host_escape(&safe));
    }

    #[test]
    fn shell_command_grant_is_script_hash_not_sh_c() {
        let call = tool_call("shell_command", serde_json::json!({"cmd": "echo hi"}));
        let (program, args) = extract_command(&call);
        let sig = approval_signature("shell_command", program.as_deref(), &args);
        assert!(
            !sig.contains(":-c") && !sig.ends_with(":/C") && !sig.contains(":sh:"),
            "must not collapse to shell wrapper flags: {sig}"
        );
        assert!(
            sig.starts_with("shell_command:"),
            "expected shell_command:{{hash}}, got {sig}"
        );
        let expected = format!("shell_command:{}", sha256_hex("echo hi".as_bytes()));
        assert_eq!(sig, expected);
    }

    #[test]
    fn session_grant_echo_does_not_cover_rm() {
        let echo = tool_call("shell_command", serde_json::json!({"cmd": "echo hi"}));
        let rm = tool_call("shell_command", serde_json::json!({"cmd": "rm -rf x"}));
        let (p1, a1) = extract_command(&echo);
        let (p2, a2) = extract_command(&rm);
        let sig_echo = approval_signature("shell_command", p1.as_deref(), &a1);
        let sig_rm = approval_signature("shell_command", p2.as_deref(), &a2);
        assert_ne!(
            sig_echo, sig_rm,
            "ApproveSession for echo must not auto-allow rm"
        );
        // Same script again shares the grant. Hash uses trim(script); raw cmd
        // padding must not change grant identity.
        let echo2 = tool_call("shell_command", serde_json::json!({"cmd": "  echo hi  "}));
        let (p3, a3) = extract_command(&echo2);
        let sig_echo_padded = approval_signature("shell_command", p3.as_deref(), &a3);
        assert_eq!(sig_echo, sig_echo_padded);

        // Mimic authorize(): ApproveSession inserts signature into session set;
        // a later call is auto-allowed only when its signature is present.
        let mut session_approved = std::collections::HashSet::new();
        session_approved.insert(sig_echo.clone());
        assert!(
            session_approved.contains(&sig_echo),
            "echo grant covers a second echo"
        );
        assert!(
            !session_approved.contains(&sig_rm),
            "echo grant must not cover rm (authorize would still NeedApproval)"
        );
    }

    #[test]
    fn run_command_shell_wrapper_uses_script_hash() {
        let args = vec!["-c".to_string(), "echo hi".to_string()];
        let sig = approval_signature("run_command", Some("sh"), &args);
        assert_eq!(
            sig,
            format!("run_command:{}", sha256_hex("echo hi".as_bytes()))
        );
        let sig_rm = approval_signature(
            "run_command",
            Some("bash"),
            &["-c".to_string(), "rm -rf x".to_string()],
        );
        assert_ne!(sig, sig_rm);
        // Windows cmd /C also hashes the script body (shared shell_c_script).
        let sig_cmd = approval_signature(
            "run_command",
            Some("cmd"),
            &["/C".to_string(), "echo hi".to_string()],
        );
        assert_eq!(
            sig_cmd,
            format!("run_command:{}", sha256_hex("echo hi".as_bytes()))
        );
        // Non-shell run_command keeps program:first_arg form.
        let git = approval_signature(
            "run_command",
            Some("git"),
            &["push".to_string(), "origin".to_string()],
        );
        assert_eq!(git, "run_command:git:push");
    }

    #[test]
    fn shell_command_is_not_verification_evidence() {
        // Evidence is double-gated: tool name == run_command AND program basename
        // is a verification runner. Helper alone is program-only; the name gate
        // is what seals shell_command (including spoofed program fields).
        let cargo_args = serde_json::json!({"program": "cargo", "args": ["test"]});
        let shell_cargo = serde_json::json!({"cmd": "cargo test"});
        let shell_spoofed =
            serde_json::json!({"cmd": "true", "program": "cargo", "args": ["test"]});

        assert!(counts_as_verification_evidence("run_command", &cargo_args));
        assert!(!counts_as_verification_evidence(
            "shell_command",
            &shell_cargo
        ));
        assert!(
            !counts_as_verification_evidence("shell_command", &shell_spoofed),
            "shell_command must not count even if arguments contain program=cargo"
        );
        assert!(!counts_as_verification_evidence(
            "shell_command",
            &serde_json::json!({"cmd": "true"})
        ));
        // Helper without name gate: no program → false; with program → true.
        assert!(!is_verification_command(&shell_cargo));
        assert!(is_verification_command(&cargo_args));
    }

    #[test]
    fn permission_match_line_uses_raw_shell_cmd() {
        let call = tool_call(
            "shell_command",
            serde_json::json!({"cmd": "cargo test --workspace"}),
        );
        let (program, args) = extract_command(&call);
        let line = command_line_for_match(&call, program.as_deref(), &args);
        assert_eq!(line.as_deref(), Some("cargo test --workspace"));
        // Must not be the wrapper form used for classification.
        assert!(!line.unwrap().starts_with("sh "));
    }
}
