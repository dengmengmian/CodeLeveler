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
    /// The program to run, e.g. "cargo". Required in practice, but kept
    /// schema-optional so a missing/blank value reaches the tool and returns
    /// actionable guidance (steer to `shell_command`) instead of a bare
    /// "program is a required field" schema rejection from the registry.
    #[serde(default)]
    program: Option<String>,
    /// Arguments passed as an array (never a shell string).
    #[serde(default)]
    args: Vec<String>,
    /// Working directory relative to the workspace root. Defaults to ".".
    #[serde(default)]
    cwd: Option<String>,
    /// Timeout in seconds. Defaults to 120. Do not raise this to "wait forever"
    /// for dev servers — use `background=true` instead.
    #[serde(default)]
    timeout_seconds: Option<u64>,
    /// When true, start the process in the background and return a task_id
    /// immediately. Use get_task / wait_task / kill_task to manage it.
    /// Required for long-lived processes (HTTP servers, watchers): foreground
    /// runs block the agent until exit or timeout.
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

    fn runs_command(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: Input = super::parse_input(self.name(), input)?;
        // The frequent mixup: the model hands run_command a whole shell string
        // (or shell_command's `cmd` field) instead of program+args. Steer it to
        // the right tool rather than surfacing a bare "program is a required
        // field" schema note. `program` is schema-optional (see `Input`) so a
        // missing/blank value lands here instead of being rejected upstream.
        let Some(program) = input
            .program
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
        else {
            return Ok(ToolOutput::error(
                "run_command needs a `program` (the executable) and an optional \
                 `args` array — it does not take a shell string. To run a whole \
                 command line (e.g. `./admin-server`, or one with pipes / $() / \
                 redirection / &&), use shell_command with its `cmd` field instead.",
            ));
        };
        let args = normalize_args(program, input.args);
        // Same product semantics as the workspace layer: read_file(".env") is
        // Denied, so `cat .env` via argv must not be the workaround. Applies to
        // background commands too.
        if let Some(reason) = super::shell_guard::refuse_sensitive_args(&args) {
            return Ok(ToolOutput::error(reason));
        }
        if input.background.unwrap_or(false) {
            return execute_background(program, args, input.cwd.as_deref(), context).await;
        }
        // Close the `sh -c 'python app.py & …'` bypass of shell_command guards.
        if let Some(reason) = super::shell_guard::refuse_run_command_shell_bypass(program, &args) {
            return Ok(ToolOutput::error(reason));
        }
        execute_program(
            program,
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
    // Same R004 F3 read-preflight as the foreground path (before any
    // environment checks: a refused path is a refusal, not a config gap).
    #[cfg(not(windows))]
    {
        let confine_writes =
            context.policy.mode.confines_workspace() && !context.policy.unrestricted_fs();
        if confine_writes {
            let mut allowed = vec![context.execution.workspace.root().to_path_buf()];
            allowed.extend(context.execution.workspace.readonly_roots().iter().cloned());
            if let Some(output) = refuse_home_escape(program, &args, &allowed, &context) {
                return Ok(output);
            }
        }
    }
    let Some(reg) = context.services.background_tasks.clone() else {
        return Ok(ToolOutput::error(
            "background tasks are not available in this session (no registry).",
        ));
    };
    let rel = cwd_rel.unwrap_or(".").to_string();
    let cwd = context.execution.workspace.resolve(&rel)?;

    // Pre-spawn snapshot for wait-end mutation accounting (PR-3b). Restore is
    // only applied later when command_write_allowlist is set; default Goal
    // background (dev servers) keeps the baseline for accounting only.
    let root = context.execution.workspace.root().to_path_buf();
    let mutation_baseline = if context.policy.read_only {
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
    // A background process outlives the round that started it, so a scope
    // claimed later cannot bound it: a child with no write authority may not
    // detach one at all. Foreground commands instead run with a read-only
    // workspace (see `execute_program`).
    if let Some(output) = refuse_zero_write_authority(&context) {
        return Ok(output);
    }
    if context.policy.command_write_allowlist.is_some()
        && mutation_baseline.is_none()
        && !context.policy.read_only
    {
        return Ok(ToolOutput::error(
            "Refused: command mutation constraints require a recoverable git workspace snapshot.\n",
        ));
    }

    let req = background_process_request(program, args.clone(), cwd, &context);
    // Session-owned: reaped when this session's goal reaches terminal state
    // or the daemon shuts down (R004 F7). Daemon-scoped spawning is reserved
    // for runtime-internal services, not agent tool calls.
    match reg
        .spawn_owned(req, mutation_baseline, Some(context.session_scope()))
        .await
    {
        Ok(task_id) => Ok(ToolOutput::ok(format!(
            "background task started\ntask_id: {task_id}\nprogram: {program}\nargs: {args:?}\n\
             status: running\nUse get_task/wait_task/kill_task with this task_id."
        ))),
        Err(e) => Ok(ToolOutput::error(format!("background spawn failed: {e}"))),
    }
}

/// Build a [`ProcessRequest`] for background spawn with the same sandbox fields
/// as foreground `execute_program` (PR-3a). Non-FullAccess / non-turn-unrestricted
/// → write confinement; network follows `context.policy.network_denied()`.
fn background_process_request(
    program: &str,
    args: Vec<String>,
    cwd: std::path::PathBuf,
    context: &ToolContext,
) -> ProcessRequest {
    let confine_writes =
        context.policy.mode.confines_workspace() && !context.policy.unrestricted_fs();
    let mut req = ProcessRequest::new(program, args, cwd);
    req.deny_network = context.policy.network_denied();
    req.deny_env = context.policy.deny_env.as_ref().clone();
    if confine_writes {
        let write_root = context.execution.workspace.root().to_path_buf();
        let extra = context.execution.workspace.readonly_roots().to_vec();
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

/// R004 F3 read-preflight (Unix): refuse absolute args that resolve into the
/// user's HOME tree outside workspace + readonly roots + the runtime tool
/// cache. `sh -c` scripts are checked by their parsed literal words; a script
/// that cannot be parsed falls through to the danger classifier / approval.
#[cfg(not(windows))]
fn refuse_home_escape(
    program: &str,
    args: &[String],
    allowed: &[std::path::PathBuf],
    context: &ToolContext,
) -> Option<ToolOutput> {
    let home = std::path::PathBuf::from(std::env::var_os("HOME")?);
    let mut allowed = allowed.to_vec();
    // The runtime-managed tool cache feeds toolchains (go, node); it holds no
    // foreign user content and must stay readable (C2.3C-S §11).
    allowed.push(home.join(".leveler").join("cache"));
    let program_base = program.rsplit(['/', '\\']).next().unwrap_or(program);
    let script_words: Vec<String>;
    let words: Vec<&str> = if leveler_execution::is_shell_wrapper_program(program_base)
        && let Some(script) = leveler_execution::shell_c_script(args)
    {
        script_words = leveler_execution::literal_command_words(script).unwrap_or_default();
        script_words.iter().map(String::as_str).collect()
    } else {
        args.iter().map(String::as_str).collect()
    };
    let bad = leveler_execution::first_home_path_outside_roots(words, &allowed, &home)?;
    Some(ToolOutput::error(format!(
        "Refused: `{bad}` is outside the workspace root `{}` and outside          readonly roots — shell commands may not read other user directories.          Use workspace paths, or grant access with `--readonly-root <dir>` /          config `readonly_roots`.",
        context.execution.workspace.root().display()
    )))
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
    let cwd = context.execution.workspace.resolve(&rel)?;

    // Read-preflight (R004 F3). On macOS/Linux the OS sandbox confines
    // *writes* and leaves reads broad so toolchains keep working — but that
    // left `cat/ls/find` of foreign USER trees wide open. The preflight
    // refuses absolute args that resolve into $HOME outside the workspace,
    // readonly roots, and the runtime tool cache; system paths (/etc, /tmp,
    // toolchains) stay readable. For `sh -c` scripts the literal words of the
    // parsed script are checked, so `cat /Users/x/other` inside a shell string
    // is seen. On Windows AppContainer the stricter any-absolute-arg gate
    // remains the primary defense.
    let confine_writes =
        context.policy.mode.confines_workspace() && !context.policy.unrestricted_fs();
    if confine_writes {
        let mut allowed = vec![context.execution.workspace.root().to_path_buf()];
        allowed.extend(context.execution.workspace.readonly_roots().iter().cloned());
        #[cfg(windows)]
        if let Some(bad) = leveler_execution::first_absolute_arg_outside_roots(&args, &allowed) {
            return Ok(ToolOutput::error(format!(
                "Refused: argument `{bad}` is outside the workspace root `{}` \
                     and outside readonly roots. Use `read_file` for workspace \
                     files, or pass `--readonly-root <dir>` (or config \
                     `readonly_roots`) for cross-repo reads.",
                context.execution.workspace.root().display()
            )));
        }
        #[cfg(not(windows))]
        if let Some(output) = refuse_home_escape(program, &args, &allowed, &context) {
            return Ok(output);
        }
    }
    let mut request = ProcessRequest::new(program.to_string(), args, cwd);
    let timeout = resolve_timeout(timeout_seconds);
    request.timeout = timeout;
    // Pre-claim child: run the process, but make the workspace read-only at
    // the OS boundary. Observation is exactly what a child must do before it
    // can know which scope to claim, while every workspace mutation — rmdir,
    // redirection, sed -i, a Python script — fails in the kernel. Enforcing
    // the EFFECT beats guessing which commands are read-only, and it closes
    // the PB_B hole that post-hoc git diffing could not see.
    request.read_only_workspace = context.policy.has_zero_write_authority();
    request.deny_network = context.policy.network_denied();
    request.deny_env = context.policy.deny_env.as_ref().clone();
    // OS confinement when not full-access / turn-unrestricted:
    // - macOS/Linux: broad reads; writes limited to workspace + temp + toolchain
    // - Windows: AppContainer write-restricted (host-trusted FilesystemIntent)
    // - turn_unrestricted_fs: approved elevation for this turn only
    if confine_writes {
        let write_root = context.execution.workspace.root().to_path_buf();
        let extra = context.execution.workspace.readonly_roots().to_vec();
        request.write_root = Some(write_root.clone());
        request.extra_read_roots = extra.clone();
        // A pre-claim child declares a READ-ONLY intent, so the Windows
        // backend gate fails closed when it cannot enforce one (§18) instead
        // of quietly spawning a writable process.
        request.filesystem_intent = Some(if request.read_only_workspace {
            leveler_execution::FilesystemIntent::ReadOnly {
                read_roots: {
                    let mut roots = vec![write_root.clone()];
                    roots.extend(extra.iter().cloned());
                    roots
                },
            }
        } else {
            leveler_execution::FilesystemIntent::WorkspaceWrite {
                write_root,
                extra_read_roots: extra,
            }
        });
    } else {
        request.filesystem_intent = Some(leveler_execution::FilesystemIntent::Unrestricted);
    }

    // Pre-command workspace snapshot (git only). Read-only overlays skip it.
    let root = context.execution.workspace.root().to_path_buf();
    let snapshot = if context.policy.read_only {
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

    let constrained = context.policy.command_write_allowlist.is_some()
        || context.policy.command_modified_files_remaining.is_some();
    if constrained && snapshot.is_none() && !context.policy.read_only {
        return Ok(ToolOutput::error(
            "Refused: command mutation constraints require a recoverable git workspace snapshot.\n",
        ));
    }

    let sandboxed = request.write_root.is_some();
    // Hold the workspace-wide gate for the command AND the mutation detection
    // that follows: concurrent sub-agents share one working tree, so a command
    // that observes the tree mid-edit produces an authoritative-looking wrong
    // answer. Background commands never reach here (they return earlier), so a
    // long-lived server cannot hold the gate.
    let _gate = context.execution.command_gate.clone().lock_owned().await;
    let output = context.execution.runner.run(request, cancellation).await?;

    // Detect what the command changed so scope checks and budgets see
    // command-driven mutations, not just tool edits.
    let mut command_modified: Vec<String> = Vec::new();
    let mut snapshot_note: Option<String> = None;
    match (&snapshot, context.policy.read_only) {
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
            .policy
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
            .filter(|path| !context.policy.command_previously_modified.contains(path))
            .count();
        let budget_exceeded = context
            .policy
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
        // Name the limit that fired: the model can't tell a too-tight timeout
        // from a genuinely hung command without it.
        body.push_str(&format!("[timed out after {}s]\n", timeout.as_secs()));
    }
    body.push_str(&format!(
        "exit: {}\n",
        output
            .exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".to_string())
    ));
    let store = context.services.artifact_store.as_deref();
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

/// Empty claimed scope: refuse a BACKGROUND command before spawn. Only the
/// detached path — it outlives the round, so a scope claimed later cannot bound
/// it, and git cannot audit empty-dir removals after the fact. Foreground
/// commands are NOT refused: they run under a read-only workspace
/// (`read_only_workspace`), so exploration still works before a claim.
fn refuse_zero_write_authority(context: &ToolContext) -> Option<ToolOutput> {
    context.policy.has_zero_write_authority().then(|| {
        ToolOutput::error(
            "Refused: no write scope is currently owned, so this command may not run \
             (it could modify the workspace). Read the relevant code, then use \
             claim_write_scope(paths) to take the bounded scope you need.\n",
        )
    })
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

#[cfg(test)]
mod hang_guard_tests {
    use super::*;
    use crate::tool::{Tool, ToolContext};
    use crate::tools::shell_guard::HANG_ANTI_PATTERN;
    use leveler_execution::{PermissionProfile, Workspace};
    use std::time::{Duration, Instant};
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn sh_c_anti_pattern_refused_instantly() {
        let dir =
            std::env::temp_dir().join(format!("leveler-run-hang-{}", super::super::test_ordinal()));
        std::fs::create_dir_all(&dir).unwrap();
        let ws = Workspace::new(&dir).unwrap();
        let ctx = ToolContext::new(ws, PermissionProfile::Assisted);
        let start = Instant::now();
        let out = RunCommandTool
            .execute(
                serde_json::json!({
                    "program": "sh",
                    "args": ["-c", HANG_ANTI_PATTERN],
                }),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let elapsed = start.elapsed();
        assert!(out.is_error, "must refuse bypass: {out:?}");
        // The job-control (`&`) guard is Unix-only; on Windows the same
        // anti-pattern is refused by the `#`-comment guard instead. Either way
        // the shell bypass is caught before spawn.
        #[cfg(not(windows))]
        assert!(out.content.contains("background=true"), "{out:?}");
        #[cfg(windows)]
        assert!(out.content.contains("comment"), "{out:?}");
        assert!(
            elapsed < Duration::from_millis(100),
            "must not hang, took {elapsed:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod grant_tests {
    use super::*;
    use crate::tool::ToolContext;
    use leveler_execution::{PermissionProfile, Workspace};
    use tokio_util::sync::CancellationToken;

    // Unix write-root confinement semantics (seatbelt/bubblewrap); Windows FS
    // boundaries are covered by the dedicated `windows_` canary tests.
    #[cfg(unix)]
    #[tokio::test]
    async fn turn_unrestricted_fs_drops_write_root_confinement() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        let base = std::path::PathBuf::from(home)
            .join(format!(".leveler-grant-fs-{}", std::process::id()));
        let ws = base.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let workspace = Workspace::new(&ws).unwrap();
        let mut ctx = ToolContext::new(workspace, PermissionProfile::Assisted);
        ctx.policy.grant_unrestricted_fs();
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
    #[cfg(unix)]
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
    #[cfg(unix)]
    #[tokio::test]
    async fn turn_unrestricted_fs_allows_git_dir_write() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        let base = std::path::PathBuf::from(home)
            .join(format!(".leveler-grant-git-ok-{}", std::process::id()));
        let ws = base.join("ws");
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        let workspace = Workspace::new(&ws).unwrap();
        let mut ctx = ToolContext::new(workspace, PermissionProfile::Assisted);
        ctx.policy.grant_unrestricted_fs();
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

/// Show at most `MAX_OUTPUT` bytes, keeping head AND tail (build/test errors
/// land at the end) with an elision marker between, mirroring
/// [`crate::registry::cap_output`]. When the output is larger and an artifact
/// store is available, spill the FULL output to a content-addressed file and
/// reference it in the marker, so nothing is silently lost — the model (or
/// user) can read the full output back.
fn truncate_or_spill(s: &str, store: Option<&leveler_execution::ArtifactStore>) -> String {
    if s.len() <= MAX_OUTPUT {
        return s.to_string();
    }
    let head = leveler_core::floor_char_boundary(s, MAX_OUTPUT / 2);
    let tail = leveler_core::ceil_char_boundary(s, s.len() - MAX_OUTPUT / 4);
    let elided_tokens = crate::registry::approx_tokens(tail - head);
    let artifact = store.and_then(|store| store.write_text(s).ok());
    let marker = format!(
        "… [{} of {} bytes (~{elided_tokens} tokens) elided] …",
        tail - head,
        s.len()
    );
    let mut shown = format!("{}\n{}\n{}", &s[..head], marker, &s[tail..]);
    if let Some(artifact) = artifact {
        // Keep the recovery reference at the very end. The registry applies a
        // second, model-specific head/tail cap after this tool returns; a link
        // embedded in the middle marker would be elided and make the stored
        // full output unreachable.
        shown.push_str(&format!("\n[full output: {}]\n", artifact.path.display()));
    }
    shown
}

/// Default command timeout, and the ceiling we clamp any request to.
const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_TIMEOUT_SECS: u64 = 3600;

/// Resolve the effective timeout. A missing or zero value uses the default
/// (zero would otherwise mean "expire immediately"); anything above the ceiling
/// is clamped so a stray huge value can't wedge the agent forever — use
/// `background=true` for genuinely long-lived processes.
fn resolve_timeout(timeout_seconds: Option<u64>) -> Duration {
    let secs = timeout_seconds
        .filter(|&s| s > 0)
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        .min(MAX_TIMEOUT_SECS);
    Duration::from_secs(secs)
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

    /// Late-bound ownership depends on this: a child that has claimed NOTHING
    /// yet gets `Some(vec![])`, and an empty allowlist must deny every command
    /// write (the violation path restores the snapshot), never read as "no
    /// constraint". `None` alone means unconstrained.
    #[test]
    fn an_empty_command_allowlist_allows_no_path() {
        let allow: Vec<String> = Vec::new();
        for modified in ["src/main.rs", "a.txt", "nested/deep/file.rs"] {
            assert!(
                !allow.iter().any(|a| path_allows(a, modified)),
                "{modified} must fall outside an empty allowlist"
            );
        }
        // And a non-empty one still covers its own subtree.
        let allowed = "src/output";
        assert!(path_allows(allowed, "src/output/json.rs"));
        assert!(!path_allows(allowed, "src/input.rs"));
    }

    // ── R004 F3: workspace read boundary for shell/argv (T4) ────────────────

    #[cfg(not(windows))]
    struct HomeSecret {
        dir: std::path::PathBuf,
        file: std::path::PathBuf,
    }

    #[cfg(not(windows))]
    impl HomeSecret {
        fn create(tag: &str) -> Self {
            let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
            let dir = home.join(format!(
                ".leveler-r004-t4-{tag}-{}",
                u64::from(std::process::id()) * 31 + super::super::test_ordinal()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let file = dir.join("secret.txt");
            std::fs::write(&file, "HIDDEN_CONTROL_CONTENT").unwrap();
            Self { dir, file }
        }
    }

    #[cfg(not(windows))]
    impl Drop for HomeSecret {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).ok();
        }
    }

    #[cfg(not(windows))]
    fn t4_workspace(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "leveler-t4-{tag}-{}",
            u64::from(std::process::id()) * 37 + super::super::test_ordinal()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn shell_cat_of_foreign_home_tree_is_refused() {
        use crate::tool::{Tool, ToolContext};
        let secret = HomeSecret::create("cat");
        let dir = t4_workspace("cat");
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted);
        let out = super::super::shell_command::ShellCommandTool
            .execute(
                serde_json::json!({"cmd": format!("cat {}", secret.file.display())}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.is_error, "{out:?}");
        assert!(out.content.contains("Refused"), "{out:?}");
        assert!(!out.content.contains("HIDDEN_CONTROL_CONTENT"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn run_command_argv_foreign_home_path_is_refused_fg_and_bg() {
        use crate::tool::{Tool, ToolContext};
        let secret = HomeSecret::create("argv");
        let dir = t4_workspace("argv");
        for extra in [
            serde_json::json!({}),
            serde_json::json!({"background": true}),
        ] {
            let ws = leveler_execution::Workspace::new(&dir).unwrap();
            let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted);
            let mut input = serde_json::json!({
                "program": "cat",
                "args": [secret.file.to_string_lossy()],
            });
            input
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            let out = RunCommandTool
                .execute(input, ctx, CancellationToken::new())
                .await
                .unwrap();
            assert!(out.is_error, "{out:?}");
            assert!(out.content.contains("Refused"), "{out:?}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn workspace_readonly_roots_and_system_paths_stay_readable() {
        use crate::tool::{Tool, ToolContext};
        let secret = HomeSecret::create("ro");
        let dir = t4_workspace("ro");
        std::fs::write(dir.join("inside.txt"), "WS_OK").unwrap();

        // workspace absolute path → allowed
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted);
        let out = super::super::shell_command::ShellCommandTool
            .execute(
                serde_json::json!({"cmd": format!("cat {}", dir.join("inside.txt").display())}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{out:?}");
        assert!(out.content.contains("WS_OK"), "{out:?}");

        // declared readonly root under HOME → allowed
        let ws = leveler_execution::Workspace::new(&dir)
            .unwrap()
            .with_readonly_roots(vec![secret.dir.clone()]);
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted);
        let out = super::super::shell_command::ShellCommandTool
            .execute(
                serde_json::json!({"cmd": format!("cat {}", secret.file.display())}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "readonly root must stay readable: {out:?}");

        // system path → allowed (not a home tree)
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted);
        let out = super::super::shell_command::ShellCommandTool
            .execute(
                serde_json::json!({"cmd": "head -1 /etc/hosts"}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "system reads must keep working: {out:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn symlink_into_home_tree_is_refused() {
        use crate::tool::{Tool, ToolContext};
        let secret = HomeSecret::create("sym");
        let dir = t4_workspace("sym");
        let link = dir.join("link");
        std::os::unix::fs::symlink(&secret.dir, &link).unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted);
        let out = super::super::shell_command::ShellCommandTool
            .execute(
                serde_json::json!({"cmd": format!("cat {}/secret.txt", link.display())}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.is_error, "symlink escape must be refused: {out:?}");
        assert!(!out.content.contains("HIDDEN_CONTROL_CONTENT"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn nested_sh_c_body_words_are_checked() {
        use crate::tool::{Tool, ToolContext};
        let secret = HomeSecret::create("nested");
        let dir = t4_workspace("nested");
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted);
        let out = super::super::shell_command::ShellCommandTool
            .execute(
                serde_json::json!({"cmd": format!("sh -c 'cat {}'", secret.file.display())}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.is_error, "nested -c body must be checked: {out:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_timeout_defaults_zero_and_clamps_huge() {
        assert_eq!(
            resolve_timeout(None),
            Duration::from_secs(DEFAULT_TIMEOUT_SECS)
        );
        // Zero would expire instantly — fall back to the default instead.
        assert_eq!(
            resolve_timeout(Some(0)),
            Duration::from_secs(DEFAULT_TIMEOUT_SECS)
        );
        assert_eq!(resolve_timeout(Some(45)), Duration::from_secs(45));
        assert_eq!(
            resolve_timeout(Some(u64::MAX)),
            Duration::from_secs(MAX_TIMEOUT_SECS)
        );
    }

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
    fn truncate_or_spill_keeps_head_and_tail() {
        // Errors land at the end of build/test output; truncation must keep the
        // tail, not just the head.
        let big = format!("HEAD{}TAIL", "z".repeat(MAX_OUTPUT));
        let shown = truncate_or_spill(&big, None);
        assert!(shown.len() < big.len(), "must shrink");
        assert!(shown.starts_with("HEAD"), "keeps the head");
        assert!(shown.trim_end().ends_with("TAIL"), "keeps the tail");
        assert!(shown.contains("elided"), "marks the elision");
    }

    #[test]
    fn truncate_or_spill_with_store_keeps_tail_and_references_artifact() {
        let big = format!("HEAD{}TAIL", "z".repeat(MAX_OUTPUT));
        let root = std::env::temp_dir().join(format!(
            "leveler-spill-tail-{}-{}",
            std::process::id(),
            super::super::test_ordinal()
        ));
        let store = leveler_execution::ArtifactStore::new(&root);
        let shown = truncate_or_spill(&big, Some(&store));
        assert!(shown.starts_with("HEAD"), "keeps the head");
        assert!(shown.contains("TAIL"), "keeps the tail");
        assert!(
            shown.contains(&format!("full output: {}", root.display())),
            "must reference the artifact path: {shown}"
        );
        std::fs::remove_dir_all(&root).ok();
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
            "must reference the artifact path: {shown}"
        );
        assert!(shown.contains(&format!("of {} bytes", big.len())));
        // The referenced file holds the FULL, untruncated output.
        let path_line = shown.lines().find(|l| l.contains("full output:")).unwrap();
        let path = path_line
            .split("full output: ")
            .nth(1)
            .unwrap()
            .split(']')
            .next()
            .unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), big);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn central_output_cap_does_not_erase_the_artifact_recovery_link() {
        let big = format!("HEAD{}TAIL", "x".repeat(MAX_OUTPUT + 5000));
        let root = std::env::temp_dir().join(format!(
            "leveler-double-cap-{}-{}",
            std::process::id(),
            super::super::test_ordinal()
        ));
        let store = leveler_execution::ArtifactStore::new(&root);
        let shown = truncate_or_spill(&big, Some(&store));
        let capped = crate::registry::cap_output_with(&shown, 4 * 1024);

        assert!(
            capped.contains(&format!("full output: {}", root.display())),
            "the central cap must preserve the only way to recover full output: {capped}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn truncate_or_spill_without_a_store_falls_back_to_marker() {
        let big = "y".repeat(MAX_OUTPUT + 100);
        let shown = truncate_or_spill(&big, None);
        assert!(shown.contains("elided"));
        assert!(!shown.contains("full output:"));
    }

    #[tokio::test]
    async fn missing_program_guides_to_shell_command() {
        // The common mixup: the model passes a whole command line (or uses the
        // shell_command `cmd` field) to run_command, which needs program+args.
        // The error must steer it to shell_command instead of a raw serde note.
        let dir = std::env::temp_dir().join(format!(
            "leveler-run-noprog-{}",
            super::super::test_ordinal()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted);
        let out = RunCommandTool
            .execute(
                serde_json::json!({"cmd": "./admin-server"}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.is_error, "missing program must be an error");
        assert!(
            out.content.contains("shell_command"),
            "must steer to shell_command: {}",
            out.content
        );
        assert!(
            out.content.contains("program"),
            "must name the missing field: {}",
            out.content
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // Invokes bare Unix coreutils (`echo`) as a program; Windows has no such
    // executable (it is a shell builtin). Windows exec is covered by
    // `shell_command_runs_echo` and the `windows_` canaries.
    #[cfg(unix)]
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

    #[cfg(unix)]
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

    // Invokes bare Unix `pwd`; Git's pwd.exe fails to initialize under the
    // Windows sandbox (STATUS_DLL_INIT_FAILED). cwd handling on Windows is
    // covered by the windows_ canaries.
    #[cfg(unix)]
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

    // Invokes bare Unix `false`; no such executable on Windows.
    #[cfg(unix)]
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
        ctx = ctx.with_sandbox(true);
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
        ctx.policy.grant_unrestricted_fs();
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
    use leveler_execution::PermissionProfile;
    #[cfg(unix)]
    use leveler_test_support::git::{run, scratch_repo};
    use tokio_util::sync::CancellationToken;

    fn ctx(dir: &std::path::Path) -> ToolContext {
        super::super::test_ctx_in(dir, PermissionProfile::Assisted)
    }

    // The git/coreutils rollback assertions below exercise Unix-shell-driven
    // mutations; Windows rollback is not driven through `sh -c` here.
    #[cfg(unix)]
    #[tokio::test]
    async fn command_mutations_are_reported_as_modified_files() {
        let dir = scratch_repo();
        std::fs::write(dir.path().join("a.txt"), "hi\n").unwrap();
        run(dir.path(), &["add", "-A"]);
        run(dir.path(), &["commit", "-qm", "init"]);

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

    /// A pre-claim child must still be able to OBSERVE: exploring the code is
    /// exactly what it has to do before it can know which scope to claim.
    /// Blanket-refusing every command made that impossible (P3 evidence: a
    /// `head … && grep …` pipeline was refused). The boundary belongs at the
    /// filesystem — the process runs with the workspace read-only.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_zero_scope_child_can_still_observe_the_workspace() {
        let dir = scratch_repo();
        std::fs::write(dir.path().join("keep.txt"), "original\n").unwrap();
        run(dir.path(), &["add", "-A"]);
        run(dir.path(), &["commit", "-qm", "init"]);

        let context =
            ctx(dir.path()).with_command_write_constraints(Some(Vec::new()), None, Vec::new());
        let out = RunCommandTool
            .execute(
                serde_json::json!({"program": "sh", "args": ["-c", "ls && cat keep.txt"]}),
                context,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            out.content.contains("original"),
            "a pre-claim child must be able to read the workspace: {}",
            out.content
        );
    }

    /// RO5/RO7: a scripting language cannot escape the boundary (the effect is
    /// enforced, not the program name), and once a scope IS claimed the very
    /// same write succeeds inside it.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_read_only_boundary_holds_for_scripts_and_lifts_after_a_claim() {
        let dir = scratch_repo();
        std::fs::create_dir_all(dir.path().join("allowed")).unwrap();
        std::fs::write(dir.path().join("allowed/x.txt"), "before\n").unwrap();
        run(dir.path(), &["add", "-A"]);
        run(dir.path(), &["commit", "-qm", "init"]);

        // Zero scope: a shell script write is denied by the kernel.
        let zero =
            ctx(dir.path()).with_command_write_constraints(Some(Vec::new()), None, Vec::new());
        let _ = RunCommandTool
            .execute(
                serde_json::json!({
                    "program": "sh",
                    "args": ["-c", "printf tampered > allowed/x.txt"]
                }),
                zero,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("allowed/x.txt")).unwrap(),
            "before\n",
            "a zero-scope script must not rewrite a workspace file"
        );

        // With the scope claimed, the same write goes through.
        let claimed = ctx(dir.path()).with_command_write_constraints(
            Some(vec!["allowed".to_string()]),
            None,
            Vec::new(),
        );
        let out = RunCommandTool
            .execute(
                serde_json::json!({
                    "program": "sh",
                    "args": ["-c", "printf after > allowed/x.txt"]
                }),
                claimed,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("allowed/x.txt")).unwrap(),
            "after",
            "a claimed scope must allow the write: {}",
            out.content
        );
    }

    /// PB_B_ORCH_1 root cause. A child that has claimed NOTHING gets an EMPTY
    /// write allowlist — it holds no write authority at all. Enforcing that by
    /// diffing a git snapshot AFTER the command is not enough: git does not
    /// track empty directories, so `rmdir` mutated the workspace invisibly and
    /// no violation fired (production evidence: an unclaimed child removed
    /// test/cases/verb-fieldlen/0003 with exit 0). With zero claimed paths a
    /// mutation-capable command must be refused BEFORE it runs.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_zero_scope_command_is_refused_before_it_can_mutate() {
        let dir = scratch_repo();
        std::fs::create_dir_all(dir.path().join("victim")).unwrap();
        std::fs::write(dir.path().join("keep.txt"), "hi\n").unwrap();
        run(dir.path(), &["add", "-A"]);
        run(dir.path(), &["commit", "-qm", "init"]);

        // An unclaimed child: constrained, with an EMPTY allowlist.
        let context =
            ctx(dir.path()).with_command_write_constraints(Some(Vec::new()), None, Vec::new());
        let out = RunCommandTool
            .execute(
                serde_json::json!({"program": "sh", "args": ["-c", "rmdir victim"]}),
                context,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            dir.path().join("victim").is_dir(),
            "a zero-scope child must not be able to remove a directory: {}",
            out.content
        );
        // The denial now comes from the OS (the workspace is read-only for a
        // zero-scope child), not from a tool-layer refusal string: the effect
        // is enforced, whatever program attempts it.
        assert!(
            out.content.contains("not permitted") || out.content.contains("denied"),
            "the mutation must fail at the filesystem boundary: {}",
            out.content
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

    #[cfg(unix)]
    #[tokio::test]
    async fn out_of_scope_command_mutations_are_rolled_back() {
        let dir = scratch_repo();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "original\n").unwrap();
        run(dir.path(), &["add", "-A"]);
        run(dir.path(), &["commit", "-qm", "init"]);
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

    #[cfg(unix)]
    #[tokio::test]
    async fn command_file_budget_violation_is_rolled_back() {
        let dir = scratch_repo();
        std::fs::write(dir.path().join("base.txt"), "original\n").unwrap();
        run(dir.path(), &["add", "-A"]);
        run(dir.path(), &["commit", "-qm", "init"]);
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

#[cfg(test)]
mod command_gate_tests {
    use super::*;
    use crate::tool::ToolContext;
    use leveler_execution::PermissionProfile;
    use std::time::Instant;

    fn ctx(dir: &std::path::Path) -> ToolContext {
        super::super::test_ctx_in(dir, PermissionProfile::FullAccess)
    }

    /// Parallel workers own disjoint files but share one working tree and one
    /// build directory. Two commands running at once means one agent's test can
    /// observe another's half-finished edit — a result that looks authoritative
    /// and is not. The gate serializes them; it is shared through the cloned
    /// `ToolContext`, which is exactly how a sub-agent inherits it.
    #[tokio::test]
    async fn concurrent_commands_are_serialized_across_cloned_contexts() {
        let dir = std::env::temp_dir().join(format!("leveler-gate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let parent = ctx(&dir);
        // A sub-agent gets a clone — the gate must still be the same gate.
        let a = parent.clone();
        let b = parent.clone();

        let call = |c: ToolContext| async move {
            RunCommandTool
                .execute(
                    serde_json::json!({"program": "sh", "args": ["-c", "sleep 0.4"]}),
                    c,
                    CancellationToken::new(),
                )
                .await
        };

        let started = Instant::now();
        let (ra, rb) = tokio::join!(call(a), call(b));
        let elapsed = started.elapsed();
        ra.unwrap();
        rb.unwrap();

        assert!(
            elapsed.as_millis() >= 800,
            "two 0.4s commands ran concurrently ({}ms) — the gate did not hold",
            elapsed.as_millis()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A single agent must not pay for the gate.
    #[tokio::test]
    async fn one_command_is_not_slowed_by_the_gate() {
        let dir = std::env::temp_dir().join(format!("leveler-gate-solo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let started = Instant::now();
        RunCommandTool
            .execute(
                serde_json::json!({"program": "sh", "args": ["-c", "sleep 0.2"]}),
                ctx(&dir),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            started.elapsed().as_millis() < 900,
            "uncontended gate must be ~free"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
