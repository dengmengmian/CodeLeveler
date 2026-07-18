//! `run_command` — run a program with explicit arguments (no shell) (spec §18.3).

use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::{MutationBaseline, ProcessRequest, RiskLevel, WorkspaceSnapshot};

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

const MAX_OUTPUT: usize = 32 * 1024;

#[derive(Debug, Deserialize, JsonSchema)]
struct Input {
    /// The program to run, e.g. "cargo".
    program: String,
    /// Arguments passed as an array (never a shell string).
    #[serde(default)]
    args: Vec<String>,
    /// Working directory relative to the workspace root. Defaults to ".".
    #[serde(default)]
    cwd: Option<String>,
    /// Timeout in seconds. Defaults to 120.
    #[serde(default)]
    timeout_seconds: Option<u64>,
    /// When true, start the process in the background and return a task_id
    /// immediately. Use get_task / wait_task / kill_task to manage it.
    #[serde(default)]
    background: Option<bool>,
}

pub struct RunCommandTool;

#[async_trait]
impl Tool for RunCommandTool {
    fn name(&self) -> &'static str {
        "run_command"
    }

    fn description(&self) -> &'static str {
        "Run a program with an explicit argument array (no shell) in the \
         workspace. Returns exit code, stdout and stderr. Use for formatters, \
         builds, and tests. For npm/yarn/pnpm package scripts, call the package \
         manager script form such as npm run test -- args; do not use npx run \
         for package scripts. In a Node project, prefer the repo-local binary \
         at node_modules/.bin/<tool> (e.g. node_modules/.bin/vitest, \
         node_modules/.bin/tsc) over npx: npx and a fresh npm/pnpm/yarn install \
         fetch from the network and fail offline (and may rewrite lockfiles). \
         Do not run a dependency install unless the task requires it. \
         Set background=true for long-running processes; then use \
         get_task/wait_task/kill_task with the returned task_id."
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<Input>()
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::WorkspaceWrite
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: Input = super::parse_input(self.name(), input)?;
        let args = normalize_args(&input.program, input.args);
        if input.background.unwrap_or(false) {
            return execute_background(&input.program, args, input.cwd.as_deref(), context).await;
        }
        execute_program(
            &input.program,
            args,
            input.cwd.as_deref(),
            input.timeout_seconds,
            context,
            cancellation,
        )
        .await
    }
}

async fn execute_background(
    program: &str,
    args: Vec<String>,
    cwd_rel: Option<&str>,
    context: ToolContext,
) -> Result<ToolOutput, ToolError> {
    let Some(reg) = context.background_tasks.clone() else {
        return Ok(ToolOutput::error(
            "background tasks are not available in this session (no registry).",
        ));
    };
    let rel = cwd_rel.unwrap_or(".").to_string();
    let cwd = context.workspace.resolve(&rel)?;

    // Pre-spawn snapshot for wait-end mutation accounting (PR-3b). Restore is
    // only applied later when command_write_allowlist is set; default Goal
    // background (dev servers) keeps the baseline for accounting only.
    let root = context.workspace.root().to_path_buf();
    let mutation_baseline = if context.read_only {
        None
    } else {
        match WorkspaceSnapshot::capture(&root).await {
            Ok(Some(id)) => {
                if let Err(error) = WorkspaceSnapshot::persist_last(&root, &id).await {
                    tracing::warn!("could not persist pre-background snapshot: {error}");
                }
                Some(MutationBaseline {
                    snapshot: id,
                    workspace_root: root,
                })
            }
            Ok(None) => None,
            Err(error) => {
                tracing::warn!("pre-background snapshot failed: {error}");
                None
            }
        }
    };
    // Allowlist-constrained workers need a recoverable snapshot to restore on
    // wait. Without git we cannot enforce the constraint.
    if context.command_write_allowlist.is_some()
        && mutation_baseline.is_none()
        && !context.read_only
    {
        return Ok(ToolOutput::error(
            "Refused: command mutation constraints require a recoverable git workspace snapshot.\n",
        ));
    }

    let req = background_process_request(program, args.clone(), cwd, &context);
    match reg.spawn(req, mutation_baseline).await {
        Ok(task_id) => Ok(ToolOutput::ok(format!(
            "background task started\ntask_id: {task_id}\nprogram: {program}\nargs: {args:?}\n\
             status: running\nUse get_task/wait_task/kill_task with this task_id."
        ))),
        Err(e) => Ok(ToolOutput::error(format!("background spawn failed: {e}"))),
    }
}

/// Build a [`ProcessRequest`] for background spawn with the same sandbox fields
/// as foreground `execute_program` (PR-3a). Non-FullAccess / non-turn-unrestricted
/// → write confinement; network follows `context.deny_network`.
fn background_process_request(
    program: &str,
    args: Vec<String>,
    cwd: std::path::PathBuf,
    context: &ToolContext,
) -> ProcessRequest {
    let confine_writes = context.mode.confines_workspace() && !context.turn_unrestricted_fs;
    let mut req = ProcessRequest::new(program, args, cwd);
    req.deny_network = context.deny_network;
    req.deny_env = context.deny_env.as_ref().clone();
    if confine_writes {
        let write_root = context.workspace.root().to_path_buf();
        let extra = context.workspace.readonly_roots().to_vec();
        req.write_root = Some(write_root.clone());
        req.extra_read_roots = extra.clone();
        req.filesystem_intent = Some(leveler_execution::FilesystemIntent::WorkspaceWrite {
            write_root,
            extra_read_roots: extra,
        });
    } else {
        req.filesystem_intent = Some(leveler_execution::FilesystemIntent::Unrestricted);
    }
    req
}

/// Shared runner used by `run_command` and `shell_command`.
pub(crate) async fn execute_program(
    program: &str,
    args: Vec<String>,
    cwd_rel: Option<&str>,
    timeout_seconds: Option<u64>,
    context: ToolContext,
    cancellation: CancellationToken,
) -> Result<ToolOutput, ToolError> {
    let rel = cwd_rel.unwrap_or(".").to_string();
    let cwd = context.workspace.resolve(&rel)?;

    // On macOS/Linux the OS sandbox allows broad *reads* and
    // confines *writes*; do not second-guess absolute path args there
    // (git/config paths, system tools). On Windows AppContainer uses host
    // intent; absolute-arg preflight remains a cheap fail-closed gate.
    let confine_writes = context.mode.confines_workspace() && !context.turn_unrestricted_fs;
    #[cfg(windows)]
    if confine_writes {
        let mut allowed = vec![context.workspace.root().to_path_buf()];
        allowed.extend(context.workspace.readonly_roots().iter().cloned());
        if let Some(bad) = leveler_execution::first_absolute_arg_outside_roots(&args, &allowed) {
            return Ok(ToolOutput::error(format!(
                "Refused: argument `{bad}` is outside the workspace root `{}` \
                     and outside readonly roots. Use `read_file` for workspace \
                     files, or pass `--readonly-root <dir>` (or config \
                     `readonly_roots`) for cross-repo reads.",
                context.workspace.root().display()
            )));
        }
    }
    let mut request = ProcessRequest::new(program.to_string(), args, cwd);
    request.timeout = Duration::from_secs(timeout_seconds.unwrap_or(120));
    request.deny_network = context.deny_network;
    request.deny_env = context.deny_env.as_ref().clone();
    // OS confinement when not full-access / turn-unrestricted:
    // - macOS/Linux: broad reads; writes limited to workspace + temp + toolchain
    // - Windows: AppContainer write-restricted (host-trusted FilesystemIntent)
    // - turn_unrestricted_fs: approved elevation for this turn only
    if confine_writes {
        let write_root = context.workspace.root().to_path_buf();
        let extra = context.workspace.readonly_roots().to_vec();
        request.write_root = Some(write_root.clone());
        request.extra_read_roots = extra.clone();
        request.filesystem_intent = Some(leveler_execution::FilesystemIntent::WorkspaceWrite {
            write_root,
            extra_read_roots: extra,
        });
    } else {
        request.filesystem_intent = Some(leveler_execution::FilesystemIntent::Unrestricted);
    }

    // Pre-command workspace snapshot (git only). Read-only overlays skip it.
    let root = context.workspace.root().to_path_buf();
    let snapshot = if context.read_only {
        None
    } else {
        match WorkspaceSnapshot::capture(&root).await {
            Ok(Some(id)) => {
                if let Err(error) = WorkspaceSnapshot::persist_last(&root, &id).await {
                    tracing::warn!("could not persist pre-command snapshot: {error}");
                }
                Some(id)
            }
            Ok(None) => None,
            Err(error) => {
                tracing::warn!("pre-command snapshot failed: {error}");
                None
            }
        }
    };

    let constrained = context.command_write_allowlist.is_some()
        || context.command_modified_files_remaining.is_some();
    if constrained && snapshot.is_none() && !context.read_only {
        return Ok(ToolOutput::error(
            "Refused: command mutation constraints require a recoverable git workspace snapshot.\n",
        ));
    }

    let sandboxed = request.write_root.is_some();
    let output = context.runner.run(request, cancellation).await?;

    // Detect what the command changed so scope checks and budgets see
    // command-driven mutations, not just tool edits.
    let mut command_modified: Vec<String> = Vec::new();
    let mut snapshot_note: Option<String> = None;
    match (&snapshot, context.read_only) {
        (Some(id), _) => match WorkspaceSnapshot::changed_since(&root, id).await {
            Ok(changed) => command_modified = changed,
            Err(error) => {
                snapshot_note = Some(format!(
                    "\n[note] could not diff the workspace after this command ({error}); \
                         its file changes were not tracked.\n"
                ));
            }
        },
        (None, true) => {}
        (None, false) => {
            snapshot_note = Some(
                "\n[note] this workspace is not a git repository; file changes made by \
                     this command cannot be rolled back.\n"
                    .to_string(),
            );
        }
    }

    let mut mutation_error = None;
    if let Some(id) = &snapshot {
        let outside: Vec<&str> = context
            .command_write_allowlist
            .as_deref()
            .map(|allowlist| {
                command_modified
                    .iter()
                    .map(String::as_str)
                    .filter(|path| !allowlist.iter().any(|allowed| path_allows(allowed, path)))
                    .collect()
            })
            .unwrap_or_default();
        let newly_modified = command_modified
            .iter()
            .filter(|path| !context.command_previously_modified.contains(path))
            .count();
        let budget_exceeded = context
            .command_modified_files_remaining
            .is_some_and(|remaining| newly_modified > remaining);

        let violation = if !outside.is_empty() {
            Some(format!(
                "command modified files outside allowed paths: {}",
                outside.join(", ")
            ))
        } else if budget_exceeded {
            Some(format!(
                "command exceeded the remaining file budget (modified {newly_modified})"
            ))
        } else {
            None
        };

        if let Some(violation) = violation {
            match WorkspaceSnapshot::restore(&root, id).await {
                Ok(()) => {
                    command_modified.clear();
                    mutation_error = Some(format!("{violation}; workspace restored"));
                }
                Err(error) => {
                    mutation_error = Some(format!(
                        "{violation}; automatic workspace restore failed: {error}"
                    ));
                }
            }
        }
    }

    let mut body = String::new();
    if output.timed_out {
        body.push_str("[timed out]\n");
    }
    body.push_str(&format!(
        "exit: {}\n",
        output
            .exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".to_string())
    ));
    let store = context.artifact_store.as_deref();
    if !output.stdout.trim().is_empty() {
        body.push_str("--- stdout ---\n");
        let stdout = leveler_core::sanitize_terminal_output(&output.stdout);
        body.push_str(&truncate_or_spill(&stdout, store));
    }
    if !output.stderr.trim().is_empty() {
        body.push_str("--- stderr ---\n");
        let stderr = leveler_core::sanitize_terminal_output(&output.stderr);
        body.push_str(&truncate_or_spill(&stderr, store));
    }
    if let Some(hint) = sandbox_denial_hint(sandboxed, output.success(), &body) {
        body.push_str(hint);
    }
    if let Some(note) = snapshot_note {
        body.push_str(&note);
    }
    if let Some(error) = &mutation_error {
        body.push_str("\n[mutation rejected] ");
        body.push_str(error);
        body.push('\n');
    }

    let out = ToolOutput {
        content: body,
        is_error: !output.success() || mutation_error.is_some(),
        metadata: serde_json::json!({
            "exit_code": output.exit_code,
            "timed_out": output.timed_out,
            "modified_files": command_modified,
            "workspace_snapshot": snapshot.as_ref().map(|id| id.0.clone()),
        }),
    };
    Ok(out)
}

fn path_allows(allowed: &str, modified: &str) -> bool {
    let allowed = allowed.trim_end_matches('/');
    modified == allowed || modified.starts_with(&format!("{allowed}/"))
}

/// When a workspace-sandboxed command fails with an OS write denial, explain
/// that it is the sandbox — so the model reports the cause accurately instead of
/// guessing (e.g. calling it a "pre-existing, unrelated" failure). Writes
/// outside the workspace (temp/toolchain caches aside) are denied by design.
fn sandbox_denial_hint(sandboxed: bool, success: bool, body: &str) -> Option<&'static str> {
    let body = body.to_ascii_lowercase();
    let denied = body.contains("operation not permitted")
        || body.contains("permission denied")
        || body.contains("read-only file system");
    if sandboxed && !success && denied {
        Some(crate::recoverable::sandbox_write_denied())
    } else {
        None
    }
}

/// Legacy marker truncation, kept for unit tests of the no-store path shape.
#[cfg(test)]
mod grant_tests {
    use super::*;
    use crate::tool::ToolContext;
    use leveler_execution::{PermissionProfile, Workspace};
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn turn_unrestricted_fs_drops_write_root_confinement() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        let base = std::path::PathBuf::from(home)
            .join(format!(".leveler-grant-fs-{}", std::process::id()));
        let ws = base.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let workspace = Workspace::new(&ws).unwrap();
        let mut ctx = ToolContext::new(workspace, PermissionProfile::Assisted);
        ctx.turn_unrestricted_fs = true;
        // Write a file outside the workspace but under a sibling dir — only
        // possible when write_root is not applied.
        let outside = base.join("outside.txt");
        let _ = std::fs::remove_file(&outside);
        let out = execute_program(
            "sh",
            vec![
                "-c".into(),
                format!("echo elevated > {}", outside.display()),
            ],
            Some("."),
            Some(30),
            ctx,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(!out.is_error, "elevated write should succeed: {out:?}");
        assert!(
            outside.exists(),
            "file outside workspace must exist after unrestricted grant"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn confined_mode_still_blocks_outside_write() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        let base = std::path::PathBuf::from(home)
            .join(format!(".leveler-grant-confined-{}", std::process::id()));
        let ws = base.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let workspace = Workspace::new(&ws).unwrap();
        let ctx = ToolContext::new(workspace, PermissionProfile::Assisted);
        let outside = base.join("outside.txt");
        let _ = std::fs::remove_file(&outside);
        let out = execute_program(
            "sh",
            vec!["-c".into(), format!("echo no > {}", outside.display())],
            Some("."),
            Some(30),
            ctx,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(
            out.is_error || !outside.exists(),
            "confined write must not create outside file: {out:?}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// D4 canary: under assisted write_root, agent/shell cannot write into `.git`
    /// (so bare `git pull` fails until the turn gets filesystem elevation).
    #[tokio::test]
    async fn confined_mode_blocks_git_dir_write() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        let base = std::path::PathBuf::from(home)
            .join(format!(".leveler-grant-git-block-{}", std::process::id()));
        let ws = base.join("ws");
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        let workspace = Workspace::new(&ws).unwrap();
        let ctx = ToolContext::new(workspace, PermissionProfile::Assisted);
        let marker = ws.join(".git/canary-write");
        let _ = std::fs::remove_file(&marker);
        let out = execute_program(
            "sh",
            vec!["-c".into(), "echo blocked > .git/canary-write".into()],
            Some("."),
            Some(30),
            ctx,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(
            out.is_error || !marker.exists(),
            "assisted must block .git writes (A8): {out:?}"
        );
        if out.is_error {
            assert!(
                out.content.contains("request_permissions")
                    || out.content.contains("Operation not permitted")
                    || out.content.contains("operation not permitted"),
                "failure should surface sandbox/recoverable signal: {}",
                out.content
            );
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// D4 canary: after turn_unrestricted_fs, the same .git write succeeds
    /// (model path: request_permissions filesystem=unrestricted → retry git).
    #[tokio::test]
    async fn turn_unrestricted_fs_allows_git_dir_write() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        let base = std::path::PathBuf::from(home)
            .join(format!(".leveler-grant-git-ok-{}", std::process::id()));
        let ws = base.join("ws");
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        let workspace = Workspace::new(&ws).unwrap();
        let mut ctx = ToolContext::new(workspace, PermissionProfile::Assisted);
        ctx.turn_unrestricted_fs = true;
        let marker = ws.join(".git/canary-write");
        let _ = std::fs::remove_file(&marker);
        let out = execute_program(
            "sh",
            vec!["-c".into(), "echo elevated > .git/canary-write".into()],
            Some("."),
            Some(30),
            ctx,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(
            !out.is_error && marker.exists(),
            "unrestricted FS must allow .git write for git mutate: {out:?}"
        );
        assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "elevated");
        let _ = std::fs::remove_dir_all(&base);
    }
}

#[cfg(test)]
fn truncate(s: &str) -> String {
    if s.len() <= MAX_OUTPUT {
        return s.to_string();
    }
    let mut end = MAX_OUTPUT;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… [truncated]\n", &s[..end])
}

/// Show at most `MAX_OUTPUT` bytes. When the output is larger and an artifact
/// store is available, spill the FULL output to a content-addressed file and
/// reference it, so nothing is silently lost — the model (or user) can read
/// the full output back. Without a store, fall back to marker truncation.
fn truncate_or_spill(s: &str, store: Option<&leveler_execution::ArtifactStore>) -> String {
    if s.len() <= MAX_OUTPUT {
        return s.to_string();
    }
    let mut end = MAX_OUTPUT;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    match store.and_then(|store| store.write_text(s).ok()) {
        Some(art) => format!(
            "{}\n… [truncated to {} of {} bytes; full output: {}]\n",
            &s[..end],
            MAX_OUTPUT,
            art.size_bytes,
            art.path.display()
        ),
        None => format!("{}\n… [truncated]\n", &s[..end]),
    }
}

fn normalize_args(program: &str, mut args: Vec<String>) -> Vec<String> {
    let Some(first) = args.first() else {
        return args;
    };
    let program_name = std::path::Path::new(program)
        .file_name()
        .and_then(|p| p.to_str())
        .unwrap_or(program);
    if first == program || first == program_name {
        args.remove(0);
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_denial_gets_a_hint_only_when_relevant() {
        let denied = "exit: 1\n--- stderr ---\nmkdir /Users/x/.config: operation not permitted\n";
        // sandboxed + failed + OS write-denial → hint.
        let hint = sandbox_denial_hint(true, false, denied).expect("hint");
        assert!(hint.contains("request_permissions"));
        assert!(hint.contains("[recoverable]"));
        assert!(
            sandbox_denial_hint(true, false, "cannot create .git/x: Read-only file system")
                .is_some()
        );
        assert!(sandbox_denial_hint(true, false, "mkdir: Permission denied").is_some());
        // not sandboxed → no hint (real failure, no sandbox to blame).
        assert!(sandbox_denial_hint(false, false, denied).is_none());
        // succeeded → no hint.
        assert!(sandbox_denial_hint(true, true, denied).is_none());
        // failed for an unrelated reason → no hint.
        assert!(sandbox_denial_hint(true, false, "exit: 1\ncompile error").is_none());
    }

    #[test]
    fn description_warns_against_npx_run_for_package_scripts() {
        let description = RunCommandTool.description();

        assert!(description.contains("npm/yarn/pnpm package scripts"));
        assert!(description.contains("npm run test -- args"));
        assert!(description.contains("do not use npx run"));
    }

    #[test]
    fn description_steers_node_projects_to_the_local_binary() {
        let description = RunCommandTool.description();
        // The dogfood friction: the model reaches for npx / a fresh install,
        // which fails offline and rewrites lockfiles. Steer it to the local
        // binary and away from installs.
        assert!(description.contains("node_modules/.bin/"));
        assert!(description.contains("fail offline"));
        assert!(description.contains("Do not run a dependency install"));
    }

    #[test]
    fn truncate_or_spill_writes_full_output_and_references_it() {
        let big = "x".repeat(MAX_OUTPUT + 5000);
        let root = std::env::temp_dir().join(format!(
            "leveler-spill-{}-{}",
            std::process::id(),
            super::super::test_ordinal()
        ));
        let store = leveler_execution::ArtifactStore::new(&root);
        let shown = truncate_or_spill(&big, Some(&store));

        assert!(shown.len() < big.len(), "the shown output must be capped");
        assert!(
            shown.contains(&format!("full output: {}", root.display())),
            "must reference the artifact path: {}",
            &shown[shown.len().saturating_sub(120)..]
        );
        assert!(shown.contains(&format!("of {} bytes", big.len())));
        // The referenced file holds the FULL, untruncated output.
        let path_line = shown.lines().find(|l| l.contains("full output:")).unwrap();
        let path = path_line
            .split("full output: ")
            .nth(1)
            .unwrap()
            .trim_end_matches(']');
        assert_eq!(std::fs::read_to_string(path).unwrap(), big);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn truncate_or_spill_without_a_store_falls_back_to_marker() {
        let big = "y".repeat(MAX_OUTPUT + 100);
        let shown = truncate_or_spill(&big, None);
        assert!(shown.contains("… [truncated]"));
        assert!(!shown.contains("full output:"));
    }

    #[tokio::test]
    async fn runs_echo() {
        let dir =
            std::env::temp_dir().join(format!("leveler-run-{}", super::super::test_ordinal()));
        std::fs::create_dir_all(&dir).unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted);
        let out = RunCommandTool
            .execute(
                serde_json::json!({"program": "echo", "args": ["hi"]}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.content.contains("hi"));
        assert!(!out.is_error);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn drops_duplicate_program_from_first_arg() {
        let dir =
            std::env::temp_dir().join(format!("leveler-run-dupe-{}", super::super::test_ordinal()));
        std::fs::create_dir_all(&dir).unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted);
        let out = RunCommandTool
            .execute(
                serde_json::json!({"program": "echo", "args": ["echo", "hi"]}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.content.contains("hi"));
        assert!(
            !out.content.contains("echo hi"),
            "duplicate program should not be passed as an argument: {}",
            out.content
        );
        assert!(!out.is_error);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn truncate_leaves_short_output_untouched() {
        assert_eq!(truncate("hello"), "hello");
    }

    #[test]
    fn truncate_clips_and_marks_long_output() {
        let big = "x".repeat(MAX_OUTPUT + 10);
        let out = truncate(&big);
        assert!(out.ends_with("\n… [truncated]\n"));
        // The prefix is clamped to MAX_OUTPUT bytes, so the total stays bounded.
        assert!(out.len() <= MAX_OUTPUT + "\n… [truncated]\n".len());
    }

    #[tokio::test]
    async fn honors_custom_cwd() {
        let dir =
            std::env::temp_dir().join(format!("leveler-run-cwd-{}", super::super::test_ordinal()));
        std::fs::create_dir_all(dir.join("subdir")).unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        // FullAccess: seatbelt can refuse `pwd` on some macOS temp layouts under
        // WorkspaceWrite; this test only checks that cwd is honored.
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::FullAccess);
        let out = RunCommandTool
            .execute(
                serde_json::json!({"program": "pwd", "cwd": "subdir"}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            out.content.contains("subdir"),
            "expected subdir in pwd output: {}",
            out.content
        );
        assert!(!out.is_error);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn reports_nonzero_exit_as_error() {
        let dir =
            std::env::temp_dir().join(format!("leveler-run-err-{}", super::super::test_ordinal()));
        std::fs::create_dir_all(&dir).unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted);
        let out = RunCommandTool
            .execute(
                serde_json::json!({"program": "false"}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("exit: 1"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn background_request_fills_sandbox_fields_for_assisted() {
        let dir = std::env::temp_dir().join(format!(
            "leveler-bg-sandbox-{}",
            super::super::test_ordinal()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let root = ws.root().to_path_buf();
        let mut ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted);
        ctx.deny_network = true;
        let req = background_process_request("sleep", vec!["1".into()], root.clone(), &ctx);
        assert_eq!(req.write_root.as_deref(), Some(root.as_path()));
        assert!(req.deny_network);
        assert!(matches!(
            req.filesystem_intent,
            Some(leveler_execution::FilesystemIntent::WorkspaceWrite { .. })
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn background_request_unrestricted_under_full_access() {
        let dir =
            std::env::temp_dir().join(format!("leveler-bg-full-{}", super::super::test_ordinal()));
        std::fs::create_dir_all(&dir).unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let root = ws.root().to_path_buf();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::FullAccess);
        let req = background_process_request("sleep", vec!["1".into()], root, &ctx);
        assert!(req.write_root.is_none());
        assert!(matches!(
            req.filesystem_intent,
            Some(leveler_execution::FilesystemIntent::Unrestricted)
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn background_request_unrestricted_when_turn_fs_elevated() {
        let dir =
            std::env::temp_dir().join(format!("leveler-bg-elev-{}", super::super::test_ordinal()));
        std::fs::create_dir_all(&dir).unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let root = ws.root().to_path_buf();
        let mut ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted);
        ctx.turn_unrestricted_fs = true;
        let req = background_process_request("sleep", vec!["1".into()], root, &ctx);
        assert!(req.write_root.is_none());
        assert!(matches!(
            req.filesystem_intent,
            Some(leveler_execution::FilesystemIntent::Unrestricted)
        ));
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;
    use crate::tool::Tool;
    use leveler_execution::{PermissionProfile, Workspace};
    use tokio_util::sync::CancellationToken;

    async fn sh_in(dir: &std::path::Path, script: &str) {
        let out = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .current_dir(dir)
            .output()
            .await
            .unwrap();
        assert!(out.status.success(), "{script} failed");
    }

    fn ctx(dir: &std::path::Path) -> ToolContext {
        ToolContext::new(Workspace::new(dir).unwrap(), PermissionProfile::Assisted)
    }

    #[tokio::test]
    async fn command_mutations_are_reported_as_modified_files() {
        let dir = tempfile::tempdir().unwrap();
        sh_in(
            dir.path(),
            "git init -q && git config user.email t@t && git config user.name t \
             && echo hi > a.txt && git add -A && git commit -qm init",
        )
        .await;

        let out = RunCommandTool
            .execute(
                serde_json::json!({"program": "sh", "args": ["-c", "echo x > created.txt && rm a.txt"]}),
                ctx(dir.path()),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        let modified: Vec<String> = out
            .metadata
            .get("modified_files")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            modified.contains(&"created.txt".to_string())
                && modified.contains(&"a.txt".to_string()),
            "command mutations must surface as modified_files: {modified:?}"
        );
        assert!(
            dir.path()
                .join(".git/leveler/last-command-snapshot")
                .is_file(),
            "the pre-command snapshot must be persisted for crash recovery"
        );
        assert!(
            out.metadata
                .get("workspace_snapshot")
                .and_then(serde_json::Value::as_str)
                .is_some(),
            "tool metadata must identify the snapshot for turn/tool-call persistence"
        );
    }

    #[tokio::test]
    async fn non_git_workspace_notes_unrecoverable_changes() {
        let dir = tempfile::tempdir().unwrap();
        let out = RunCommandTool
            .execute(
                serde_json::json!({"program": "sh", "args": ["-c", "echo x > created.txt"]}),
                ctx(dir.path()),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            out.content.contains("cannot be rolled back"),
            "the non-git degradation must be explicit: {}",
            out.content
        );
    }

    #[tokio::test]
    async fn out_of_scope_command_mutations_are_rolled_back() {
        let dir = tempfile::tempdir().unwrap();
        sh_in(
            dir.path(),
            "git init -q && git config user.email t@t && git config user.name t \
             && mkdir src && echo original > src/lib.rs && git add -A && git commit -qm init",
        )
        .await;
        let constrained = ctx(dir.path()).with_command_write_constraints(
            Some(vec!["src".to_string()]),
            None,
            Vec::new(),
        );

        let out = RunCommandTool
            .execute(
                serde_json::json!({"program": "sh", "args": ["-c", "echo bad > outside.txt"]}),
                constrained,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert!(out.is_error, "scope violation must fail the tool call");
        assert!(
            out.content.contains("outside allowed paths"),
            "{}",
            out.content
        );
        assert!(
            !dir.path().join("outside.txt").exists(),
            "violation must be rolled back"
        );
    }

    #[tokio::test]
    async fn command_file_budget_violation_is_rolled_back() {
        let dir = tempfile::tempdir().unwrap();
        sh_in(
            dir.path(),
            "git init -q && git config user.email t@t && git config user.name t \
             && echo original > base.txt && git add -A && git commit -qm init",
        )
        .await;
        let constrained = ctx(dir.path()).with_command_write_constraints(None, Some(1), Vec::new());

        let out = RunCommandTool
            .execute(
                serde_json::json!({"program": "sh", "args": ["-c", "echo a > a.txt; echo b > b.txt"]}),
                constrained,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert!(out.is_error, "budget violation must fail the tool call");
        assert!(out.content.contains("file budget"), "{}", out.content);
        assert!(!dir.path().join("a.txt").exists());
        assert!(!dir.path().join("b.txt").exists());
    }
}
