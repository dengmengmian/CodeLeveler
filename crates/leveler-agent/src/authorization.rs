//! Tool-call authorization helpers: command/path extraction, write
//! allowlists, approval signatures, tool classification.

use leveler_model::ToolCall;
use sha2::{Digest, Sha256};

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
    // Shell targets are visible to path rules and approval prompts (R004 F3):
    // absolute-looking literal words of the command are scoped paths too.
    if call.name == "run_command"
        && let Some(args) = call.arguments.get("args").and_then(|v| v.as_array())
    {
        for arg in args.iter().filter_map(|a| a.as_str()) {
            if leveler_execution::looks_like_absolute_path_arg(arg) {
                push_unique_path(out, arg);
            }
        }
    }
    if call.name == "shell_command"
        && let Some(cmd) = call.arguments.get("cmd").and_then(|v| v.as_str())
    {
        for word in leveler_execution::literal_command_words(cmd).unwrap_or_default() {
            if leveler_execution::looks_like_absolute_path_arg(&word) {
                push_unique_path(out, &word);
            }
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
    let allow: Vec<String> = allowlist
        .iter()
        .map(|p| norm_scope_path(p))
        .filter(|p| !p.is_empty())
        .collect();
    write_targets(call)
        .into_iter()
        .map(|p| norm_scope_path(&p))
        .filter(|target| !allow.iter().any(|a| scope_covers(a, target)))
        .collect()
}

/// The paths a direct write tool would touch, normalized — for callers that
/// need the target set itself (ownership fences) rather than a containment
/// verdict against a fixed list.
pub(crate) fn mutation_targets(call: &ToolCall) -> Vec<String> {
    write_targets(call)
        .into_iter()
        .map(|p| norm_scope_path(&p))
        .filter(|p| !p.is_empty())
        .collect()
}

/// The paths a direct write tool would touch, unnormalized.
fn write_targets(call: &ToolCall) -> Vec<String> {
    match call.name.as_str() {
        "apply_patch" => {
            let patch = call
                .arguments
                .get("patch")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            patch_paths(patch)
        }
        "replace" | "write_file" => call
            .arguments
            .get("path")
            .and_then(|v| v.as_str())
            .map(|p| vec![p.to_string()])
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// Normalize a scope/target path: `./` prefix and trailing slashes stripped so
/// a directory grant written either way (`src/output` / `src/output/`, the
/// spawn_agent schema shows the latter) covers its subtree. A bare "/"
/// normalizes to "" and matches nothing.
pub(crate) fn norm_scope_path(p: &str) -> String {
    p.trim()
        .trim_start_matches("./")
        .trim_end_matches('/')
        .to_string()
}

/// Whether normalized scope entry `a` covers normalized `target` (equal path
/// or directory prefix).
fn scope_covers(a: &str, target: &str) -> bool {
    !a.is_empty() && (target == a || target.starts_with(&format!("{a}/")))
}

/// Whether a normalized write target cannot be proven in-workspace by string
/// comparison (absolute, or containing `..`). The ownership fence DENIES on a
/// match, so such a spelling must fail CLOSED — it is treated as inside every
/// claimed scope rather than silently escaping one.
pub(crate) fn target_is_unresolvable(target: &str) -> bool {
    target.starts_with('/') || target.split('/').any(|segment| segment == "..")
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

/// What a Git call's effects are, in the prompt.
///
/// Stable capability ids, not prose: the same string a log line and a bug
/// report can be grepped for. Deliberately not "Git requires permission" —
/// that sentence is what makes a permission problem undiagnosable later.
pub(crate) fn git_capability_note(effects: &leveler_execution::CallGitEffects) -> String {
    format!("Git 副作用: {}", effects.capabilities().join(", "))
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

/// Platform shell wrapper used for classification — the SAME single copy the
/// tool executes with, so the classified shape can never drift from the
/// executed shape.
fn shell_invocation_for_classification(cmd: &str) -> (String, Vec<String>) {
    leveler_execution::shell_invocation(cmd)
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
    program.map(|program| {
        std::iter::once(program)
            .chain(args.iter().map(String::as_str))
            .map(command_identity_word)
            .collect::<Vec<_>>()
            .join(" ")
    })
}

/// Canonical POSIX quoting preserves argv boundaries for display and rule
/// matching on every platform. Execution always consumes the original argv.
fn command_identity_word(word: &str) -> String {
    if !word.is_empty()
        && word
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-./:@%+=,".contains(&byte))
    {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

/// A stable signature for "approve for the session".
///
/// Bind the complete proposed call, including cwd, environment, script and
/// argv boundaries. Reuse the action fingerprint rather than maintaining a
/// second command identity. The proposal does not resolve repository config,
/// implicit cwd or executable contents, so this is not a resource grant.
pub(crate) fn approval_signature(call: &ToolCall) -> String {
    format!("{}:{}", call.name, action_fingerprint(call))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn approval_signature(tool: &str, program: Option<&str>, args: &[String]) -> String {
        let arguments = if tool == "shell_command" {
            serde_json::json!({"cmd": leveler_execution::shell_c_script(args).unwrap_or("")})
        } else {
            serde_json::json!({"program": program, "args": args})
        };
        super::approval_signature(&tool_call(tool, arguments))
    }

    use leveler_core::ToolCallId;

    fn tool_call(name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: ToolCallId::new("t"),
            name: name.to_string(),
            arguments,
        }
    }

    /// The spawn_agent schema's directory example uses a trailing slash
    /// (`src/output/`). The allowlist must accept that spelling: without
    /// trailing-slash normalization every write of a worker scoped per the
    /// schema's own example is refused (fail-closed, but a dead worker).
    #[test]
    fn a_trailing_slash_directory_allowlist_entry_admits_the_subtree() {
        let call = tool_call(
            "apply_patch",
            serde_json::json!({
                "patch": "*** Begin Patch\n*** Update File: src/output/mod.rs\n-a\n+b\n*** End Patch"
            }),
        );
        assert!(
            write_targets_outside_allowlist(&call, &["src/output/".to_string()]).is_empty(),
            "trailing-slash directory grant must cover its subtree"
        );
        assert_eq!(
            write_targets_outside_allowlist(&call, &["src/other/".to_string()]),
            vec!["src/output/mod.rs".to_string()],
            "normalization must not turn a trailing slash into allow-everything"
        );
    }

    /// The ownership fence and the per-worker write allowlist read a write
    /// tool's targets from here. A write tool this function does not know
    /// reports no target at all, so a sibling agent's file could be
    /// overwritten without the fence ever seeing it.
    #[test]
    fn every_direct_write_tool_declares_its_target() {
        let call = tool_call(
            "write_file",
            serde_json::json!({"path": "src/other/mod.rs", "content": "x\n"}),
        );
        assert_eq!(
            mutation_targets(&call),
            vec!["src/other/mod.rs".to_string()],
            "write_file must declare the file it writes"
        );
        assert_eq!(
            write_targets_outside_allowlist(&call, &["src/output/".to_string()]),
            vec!["src/other/mod.rs".to_string()],
            "a write_file outside the claimed scope must be caught"
        );
        assert!(
            write_targets_outside_allowlist(&call, &["src/other/".to_string()]).is_empty(),
            "a write_file inside the claimed scope must be allowed"
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
    fn shell_command_grant_is_exact_action_not_sh_c() {
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
        let expected = format!("shell_command:{}", action_fingerprint(&call));
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
        // Only the exact proposal shares the grant, without script normalization.
        let echo2 = tool_call("shell_command", serde_json::json!({"cmd": "  echo hi  "}));
        let (p3, a3) = extract_command(&echo2);
        let sig_echo_padded = approval_signature("shell_command", p3.as_deref(), &a3);
        assert_ne!(sig_echo, sig_echo_padded);

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
    fn session_grants_bind_explicit_working_directory() {
        let signature = |cwd: &str| {
            let call = tool_call(
                "run_command",
                serde_json::json!({
                    "program":"git", "args":["push", "origin", "main"], "cwd":cwd
                }),
            );
            super::approval_signature(&call)
        };
        assert_ne!(signature("repo-a"), signature("repo-b"));
    }

    #[test]
    fn wrapper_session_grants_bind_program_flags_and_positional_arguments() {
        let script = "git push \"$1\" \"$2\" main";
        let approved = vec![
            "-c".into(),
            script.into(),
            "owner".into(),
            "origin".into(),
            "--no-force".into(),
        ];
        let signature = approval_signature("run_command", Some("sh"), &approved);
        for (program, args) in [
            ("bash", approved.clone()),
            (
                "sh",
                vec![
                    "-lc".into(),
                    script.into(),
                    "owner".into(),
                    "origin".into(),
                    "--no-force".into(),
                ],
            ),
            (
                "sh",
                vec![
                    "-c".into(),
                    script.into(),
                    "owner".into(),
                    "other".into(),
                    "--no-force".into(),
                ],
            ),
            (
                "sh",
                vec![
                    "-c".into(),
                    script.into(),
                    "owner".into(),
                    "origin".into(),
                    "--force".into(),
                ],
            ),
        ] {
            assert_ne!(
                signature,
                approval_signature("run_command", Some(program), &args),
                "{program} {args:?}"
            );
        }
    }

    #[test]
    fn structured_session_grants_do_not_generalize_destructive_arguments() {
        for (program, approved, changed) in [
            ("rm", vec!["-rf", "approved"], vec!["-rf", "other"]),
            ("sudo", vec!["sh", "approved.sh"], vec!["sh", "other.sh"]),
        ] {
            let approved = approved.into_iter().map(String::from).collect::<Vec<_>>();
            let changed = changed.into_iter().map(String::from).collect::<Vec<_>>();
            assert_ne!(
                approval_signature("run_command", Some(program), &approved),
                approval_signature("run_command", Some(program), &changed)
            );
        }
    }

    #[test]
    fn opaque_shell_session_grants_bind_the_script() {
        for program in ["pwsh", "powershell", "powershell.exe", "fish"] {
            assert_ne!(
                approval_signature(
                    "run_command",
                    Some(program),
                    &["-c".into(), "echo approved".into()]
                ),
                approval_signature(
                    "run_command",
                    Some(program),
                    &["-c".into(), "rm victim".into()]
                ),
                "{program} must not grant all inline scripts"
            );
        }
    }

    #[test]
    fn durable_structured_git_grants_preserve_argument_boundaries() {
        let approved = tool_call(
            "run_command",
            serde_json::json!({
                "program":"git", "args":["push", "/tmp/remote --force", "main"]
            }),
        );
        let changed = tool_call(
            "run_command",
            serde_json::json!({
                "program":"git", "args":["push", "/tmp/remote", "--force", "main"]
            }),
        );
        let command = |call: &ToolCall| {
            let (program, args) = extract_command(call);
            command_line_for_match(call, program.as_deref(), &args).unwrap()
        };
        let approved = command(&approved);
        let changed = command(&changed);
        let rules = leveler_execution::always_rules_for("run_command", Some(&approved), &[]);
        let rules = leveler_execution::PermissionRuleSet::from_rules(rules);
        assert_eq!(
            rules.evaluate("run_command", Some(&approved), &[]),
            leveler_execution::RuleDecision::Allow
        );
        assert_eq!(
            rules.evaluate("run_command", Some(&changed), &[]),
            leveler_execution::RuleDecision::NoMatch
        );
    }

    #[test]
    fn run_command_shell_wrapper_uses_complete_argv_hash() {
        let args = vec!["-c".to_string(), "echo hi".to_string()];
        let sig = approval_signature("run_command", Some("sh"), &args);
        assert_eq!(
            sig,
            format!(
                "run_command:{}",
                action_fingerprint(&tool_call(
                    "run_command",
                    serde_json::json!({"program":"sh", "args":args})
                ))
            )
        );
        let sig_rm = approval_signature(
            "run_command",
            Some("bash"),
            &["-c".to_string(), "rm -rf x".to_string()],
        );
        assert_ne!(sig, sig_rm);
        // Windows cmd /C binds the wrapper and all its arguments as well.
        let sig_cmd = approval_signature(
            "run_command",
            Some("cmd"),
            &["/C".to_string(), "echo hi".to_string()],
        );
        assert_ne!(sig, sig_cmd);
        // Ordinary command policy stays the same; an approval is exact.
        assert_ne!(
            approval_signature(
                "run_command",
                Some("cargo"),
                &["test".into(), "-p".into(), "foo".into()]
            ),
            approval_signature(
                "run_command",
                Some("cargo"),
                &["test".into(), "-p".into(), "other".into()]
            )
        );
    }

    #[test]
    fn git_session_grants_bind_the_complete_argv() {
        for program in ["git", "/usr/bin/git"] {
            for (approved, variants) in [
                (
                    vec!["push", "origin", "main"],
                    vec![
                        vec!["push", "--force", "origin", "main"],
                        vec!["push", "other", "main"],
                    ],
                ),
                (
                    vec!["fetch", "origin"],
                    vec![
                        vec!["fetch", "https://unknown.example/repo"],
                        vec!["fetch", "origin", "--upload-pack=payload"],
                    ],
                ),
            ] {
                let args = approved.iter().map(|v| v.to_string()).collect::<Vec<_>>();
                let signature = approval_signature("run_command", Some(program), &args);
                assert_eq!(
                    signature,
                    approval_signature("run_command", Some(program), &args)
                );
                for variant in variants {
                    let variant = variant.iter().map(|v| v.to_string()).collect::<Vec<_>>();
                    assert_ne!(
                        signature,
                        approval_signature("run_command", Some(program), &variant),
                        "approval must bind every Git argument"
                    );
                }
            }
        }
        assert_ne!(
            approval_signature(
                "run_command",
                Some("git"),
                &["push".into(), "origin main".into()]
            ),
            approval_signature(
                "run_command",
                Some("git"),
                &["push".into(), "origin".into(), "main".into()]
            )
        );
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
