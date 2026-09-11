//! `CommandExecution` — the one runtime behind both command primitives.
//!
//! `run_command` (argv) and `shell_command` (a shell line) are two different
//! model INTENTS and stay two tools, but there is one execution underneath:
//! process launch, cwd resolution, the write scope the OS sandbox enforces,
//! env scrubbing, the workspace-wide command gate, the pre-command snapshot
//! and the mutation attribution/rollback that settles against it, output
//! spilling, and the background task lifecycle.
//!
//! It used to live inside `run_command.rs`, so `shell_command` reached into
//! the internals of the other tool for its own runtime — which made one tool
//! look like the owner of the other. Neither owns it now; both are
//! constructed with it.

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use leveler_execution::{
    ArtifactStore, BackgroundTaskRegistry, MutationBaseline, ProcessRequest, WorkspaceSnapshot,
};

use crate::tool::{ToolContext, ToolError, ToolOutput};

/// Cap on the bytes of one output stream shown inline (the rest spills to the
/// artifact store when there is one).
pub(super) const MAX_OUTPUT: usize = 32 * 1024;

/// The command runtime both command tools are constructed with.
///
/// Holds the two long-lived services a command needs and nothing else: where
/// detached processes are registered, and where oversized output is spilled.
/// Everything else about a command — its authority, its cwd, its write scope —
/// arrives per call on the [`ToolContext`].
pub struct CommandExecution {
    background_tasks: Arc<BackgroundTaskRegistry>,
    artifact_store: Option<Arc<ArtifactStore>>,
}

impl CommandExecution {
    pub fn new(
        background_tasks: Arc<BackgroundTaskRegistry>,
        artifact_store: Option<Arc<ArtifactStore>>,
    ) -> Self {
        Self {
            background_tasks,
            artifact_store,
        }
    }

    /// Start a detached process and hand back its task id. The lifecycle tools
    /// (`get_task` / `wait_task` / `kill_task`) manage it from the same registry.
    pub async fn start_background(
        &self,
        program: &str,
        args: Vec<String>,
        cwd_rel: Option<&str>,
        context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let reg = &self.background_tasks;
        let rel = cwd_rel.unwrap_or(".").to_string();
        let cwd = context
            .execution
            .workspace
            .resolve_command_cwd(&rel, &context.write_scope())?;

        // Pre-spawn snapshot the runtime settles against when the process exits.
        // Restore only applies when a write allowlist is set; default Goal
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
                        // The authority this task runs under is the one it is
                        // spawned with: it outlives this round, so there is no
                        // later scope for the runtime to consult when it settles.
                        write_allowlist: context
                            .policy
                            .command_write_allowlist
                            .as_deref()
                            .map(|allow| allow.to_vec()),
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

        let req = Self::background_process_request(program, args.clone(), cwd, &context);
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
    /// as [`Self::run_foreground`] (PR-3a). Non-FullAccess / non-turn-unrestricted
    /// → write confinement; network follows `context.policy.network_denied()`.
    pub(super) fn background_process_request(
        program: &str,
        args: Vec<String>,
        cwd: std::path::PathBuf,
        context: &ToolContext,
    ) -> ProcessRequest {
        let mut req = ProcessRequest::new(program, args, cwd);
        req.deny_network = context.policy.network_denied();
        req.deny_env = context.policy.deny_env.as_ref().clone();
        req.write_scope = context.write_scope();
        req
    }

    /// Run a program to completion in the foreground: the shared path both
    /// command tools take once they have decoded their own arguments.
    pub async fn run_foreground(
        &self,
        program: &str,
        args: Vec<String>,
        cwd_rel: Option<&str>,
        timeout_seconds: Option<u64>,
        context: ToolContext,
        cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let rel = cwd_rel.unwrap_or(".").to_string();
        let cwd = context
            .execution
            .workspace
            .resolve_command_cwd(&rel, &context.write_scope())?;
        let executed_commands = leveler_execution::proven_executed_commands(program, &args);

        let scope = context.write_scope();
        let mut request = ProcessRequest::new(program.to_string(), args, cwd);
        let timeout = resolve_timeout(timeout_seconds);
        request.timeout = timeout;
        request.deny_network = context.policy.network_denied();
        request.deny_env = context.policy.deny_env.as_ref().clone();
        // OS confinement from the one write boundary (`WriteScope`):
        // - `Workspace`: broad reads on every host, writes limited to
        //   workspace + temp + toolchain caches.
        // - `None` (pre-claim child): the workspace is mounted read-only, so
        //   observation works while every mutation — rmdir, redirection, sed -i,
        //   a Python script — fails in the kernel. Enforcing the EFFECT beats
        //   guessing which commands are read-only (the PB_B hole). On Windows the
        //   ReadOnly intent fails closed when the backend cannot enforce it.
        // - `Unrestricted`: 完全访问, or an approved elevation.
        request.write_scope = scope;

        // Pre-command workspace snapshot (git only). Read-only overlays skip it,
        // and so does a caller with no write authority at all: its workspace is
        // mounted read-only for this command, so it CANNOT have changed anything.
        // Anything the diff would report then belongs to a concurrent sibling, and
        // rolling back to this snapshot would destroy that sibling's authorized
        // work — a `ls` reverting another agent's committed file.
        let root = context.execution.workspace.root().to_path_buf();
        let snapshot = if context.policy.read_only || context.policy.has_zero_write_authority() {
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

        // A caller with no write authority has nothing to roll back: its workspace
        // is read-only for this command, so the snapshot is deliberately absent
        // (see above) rather than unavailable. Demanding one here would refuse
        // pre-claim exploration outright — the capability the read-only workspace
        // exists to preserve.
        let constrained = (context.policy.command_write_allowlist.is_some()
            || context.policy.command_modified_files_remaining.is_some())
            && !context.policy.has_zero_write_authority();
        if constrained && snapshot.is_none() && !context.policy.read_only {
            return Ok(ToolOutput::error(
                "Refused: command mutation constraints require a recoverable git workspace snapshot.\n",
            ));
        }

        // Where HEAD sits before the command. A command that moves it (`pull`,
        // `switch`, `reset`) rewrites files this run did not author, and the tree
        // diff below cannot tell the two apart on its own.
        let head_before = if snapshot.is_some() {
            WorkspaceSnapshot::head_commit(&root).await
        } else {
            None
        };

        let sandboxed = request.write_scope.confines();
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
            // The diff covers the WHOLE workspace, so in a shared tree it also
            // reports what a concurrent sibling wrote inside its own exclusive
            // scope. This command cannot have written those — the ownership fence
            // and the write allowlist refuse them — and charging them here rolls
            // the sibling's authorized work back along with everything else.
            command_modified.retain(|path| {
                !context
                    .policy
                    .command_foreign_paths
                    .iter()
                    .any(|owned| path_allows(owned, path))
            });
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

        // Two different things change the working tree: writing files, and moving
        // HEAD. Only the first is authored by this run, and only the first carries
        // a source-change verification obligation — a plain `git switch` that
        // reported every file differing between the branches used to hang the
        // project's whole `cargo test` gate on a task that wrote nothing.
        //
        // Deliberately AFTER the write-allowlist, file-budget and rollback checks
        // above: those still see every path the command touched, however it
        // touched it, so a repository operation can never walk past a write scope.
        if !command_modified.is_empty()
        && let Some(before) = &head_before
        && let Some(after) = WorkspaceSnapshot::head_commit(&root).await
        && &after != before
        // A command that wrote a file and then committed it also moves HEAD and
        // also leaves the tree matching it. That content IS this run's, so
        // nothing is attributed away from a command that made a commit.
        && !WorkspaceSnapshot::head_moves_since_include_a_commit(&root, before).await
        {
            match WorkspaceSnapshot::paths_explained_by_head_move(&root, before, &after).await {
                Ok(explained) => command_modified.retain(|path| !explained.contains(path)),
                // Unattributable: keep every path. Over-reporting costs a
                // verification run; under-reporting would skip one that was owed.
                Err(error) => {
                    tracing::warn!("could not attribute a HEAD move to the repository: {error}")
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
        let store = self.artifact_store.as_deref();
        let mut locators: Vec<String> = Vec::new();
        if !output.stdout.trim().is_empty() {
            body.push_str("--- stdout ---\n");
            let stdout = leveler_core::sanitize_terminal_output(&output.stdout);
            let (shown, locator) = truncate_or_spill("stdout", &stdout, store);
            body.push_str(&shown);
            locators.extend(locator);
        }
        if !output.stderr.trim().is_empty() {
            body.push_str("--- stderr ---\n");
            let stderr = leveler_core::sanitize_terminal_output(&output.stderr);
            let (shown, locator) = truncate_or_spill("stderr", &stderr, store);
            body.push_str(&shown);
            locators.extend(locator);
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
        // HCH-FIX-3: recovery locators go LAST, after every note, so the
        // registry cap's retained tail always carries them. And when the body
        // plus locators would exceed this model's result budget, shrink the
        // PREVIEWS first — the registry's later head/tail cap must never be the
        // thing that decides whether a locator survives, because at small
        // budgets its retained tail is smaller than two locator lines.
        if !locators.is_empty() {
            let budget = context.policy.tool_output_budget;
            let locators_len: usize = locators.iter().map(|l| l.len() + 1).sum();
            if body.len() + locators_len > budget {
                let preview_budget = budget
                    .saturating_sub(locators_len)
                    .max(crate::registry::MIN_TOOL_OUTPUT / 2);
                body = crate::registry::cap_output_with(&body, preview_budget);
            }
        }
        for locator in &locators {
            body.push('\n');
            body.push_str(locator);
        }

        let out = ToolOutput {
            content: body,
            is_error: !output.success() || mutation_error.is_some(),
            metadata: serde_json::json!({
                "exit_code": output.exit_code,
                "timed_out": output.timed_out,
                "modified_files": command_modified,
                "workspace_snapshot": snapshot.as_ref().map(|id| id.0.clone()),
                // What this execution proves ran, stated by the layer that ran it.
                // Every command tool funnels through here, so completion evidence
                // reads one execution fact instead of guessing from tool names
                // (HC-002 F1: the same `go build ./...` counted through
                // `run_command` and vanished through `shell_command`).
                "executed_commands": executed_commands,
            }),
        };
        Ok(out)
    }
}

/// Empty claimed scope: refuse a BACKGROUND command before spawn. Only the
/// detached path — it outlives the round, so a scope claimed later cannot bound
/// it, and git cannot audit empty-dir removals after the fact. Foreground
/// commands are NOT refused: they run under a read-only workspace
/// (`WriteScope::None`), so exploration still works before a claim.
pub(super) fn refuse_zero_write_authority(context: &ToolContext) -> Option<ToolOutput> {
    context.policy.has_zero_write_authority().then(|| {
        ToolOutput::error(
            "Refused: no write scope is currently owned, so this command may not run \
             (it could modify the workspace). Read the relevant code, then use \
             claim_write_scope(paths) to take the bounded scope you need.\n",
        )
    })
}

pub(super) fn path_allows(allowed: &str, modified: &str) -> bool {
    let allowed = allowed.trim_end_matches('/');
    modified == allowed || modified.starts_with(&format!("{allowed}/"))
}

/// When a workspace-sandboxed command fails with an OS write denial, explain
/// that it is the sandbox — so the model reports the cause accurately instead of
/// guessing (e.g. calling it a "pre-existing, unrelated" failure). Writes
/// outside the workspace (temp/toolchain caches aside) are denied by design.
pub(super) fn sandbox_denial_hint(
    sandboxed: bool,
    success: bool,
    body: &str,
) -> Option<&'static str> {
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

/// Show at most `MAX_OUTPUT` bytes, keeping head AND tail (build/test errors
/// land at the end) with an elision marker between, mirroring
/// [`crate::registry::cap_output`]. When the output is larger and an artifact
/// store is available, spill the FULL output to a content-addressed file and
/// return its recovery locator SEPARATELY, so nothing is silently lost — the
/// model (or user) can read the full output back.
///
/// The locator is returned rather than embedded (HCH-FIX-3): the registry
/// applies a second, model-specific head/tail cap after this tool returns,
/// and a locator inside a stream's own block can land in that cap's elided
/// middle when BOTH streams spill under a small model budget. The caller
/// appends every locator at the absolute end of the result, inside the
/// cap's retained tail — previews are disposable, locators are not.
pub(super) fn truncate_or_spill(
    label: &str,
    s: &str,
    store: Option<&leveler_execution::ArtifactStore>,
) -> (String, Option<String>) {
    if s.len() <= MAX_OUTPUT {
        return (s.to_string(), None);
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
    let shown = format!("{}\n{}\n{}", &s[..head], marker, &s[tail..]);
    let locator =
        artifact.map(|artifact| format!("[{label} full output: {}]\n", artifact.path.display()));
    (shown, locator)
}

/// Default command timeout, and the ceiling we clamp any request to.
pub(super) const DEFAULT_TIMEOUT_SECS: u64 = 120;
pub(super) const MAX_TIMEOUT_SECS: u64 = 3600;

/// Resolve the effective timeout. A missing or zero value uses the default
/// (zero would otherwise mean "expire immediately"); anything above the ceiling
/// is clamped so a stray huge value can't wedge the agent forever — use
/// `background=true` for genuinely long-lived processes.
pub(super) fn resolve_timeout(timeout_seconds: Option<u64>) -> Duration {
    let secs = timeout_seconds
        .filter(|&s| s > 0)
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        .min(MAX_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

#[cfg(test)]
mod grant_tests {
    use super::*;

    /// A command runtime with no artifact store: these tests are about the
    /// write boundary, not about output spilling.
    fn commands() -> CommandExecution {
        CommandExecution::new(
            std::sync::Arc::new(BackgroundTaskRegistry::with_environment(
                std::sync::Arc::new(leveler_core::environment().clone()),
            )),
            None,
        )
    }
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
        let out = commands()
            .run_foreground(
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
        let out = commands()
            .run_foreground(
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
        let out = commands()
            .run_foreground(
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
        let out = commands()
            .run_foreground(
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
