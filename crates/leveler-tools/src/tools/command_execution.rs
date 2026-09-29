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
    ArtifactStore, BackgroundTaskLifetime, BackgroundTaskRegistry, MutationBaseline,
    ProcessRequest, WorkspaceSnapshot,
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
        lifetime: BackgroundTaskLifetime,
        context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let _gate = if context.command_lease.is_none() {
            Some(context.execution.command_gate.clone().lock_owned().await)
        } else {
            None
        };
        let reg = &self.background_tasks;
        let rel = cwd_rel.unwrap_or(".").to_string();
        let cwd = context
            .execution
            .workspace
            .resolve_command_cwd(&rel, &context.write_scope())?;

        // Pre-spawn baseline for namespace accounting after process-group exit.
        let root = context.execution.workspace.root().to_path_buf();
        let mutation_baseline = if context.policy.read_only {
            None
        } else {
            match WorkspaceSnapshot::capture_for_scope(&root, &context.write_scope()).await {
                Ok(Some(id)) => {
                    if !matches!(
                        context.write_scope(),
                        leveler_execution::WriteScope::ScopedWorkspace { .. }
                    ) && let Err(error) = WorkspaceSnapshot::persist_last(&root, &id).await
                    {
                        tracing::warn!("could not persist pre-background snapshot: {error}");
                    }
                    Some(MutationBaseline {
                        snapshot: id,
                        workspace_root: root,
                        // The authority this task runs under is the one it is
                        // spawned with: it outlives this round, so there is no
                        // later scope for the runtime to consult when it settles.
                        write_scope: context.write_scope(),
                    })
                }
                Ok(None) => None,
                Err(error) => {
                    tracing::warn!("pre-background snapshot failed: {error}");
                    None
                }
            }
        };
        // Write-capable scoped tasks require an accounting baseline. Read-only
        // tasks can run without one because the sandbox denies their writes.
        if context.policy.command_write_allowlist.is_some()
            && !context.policy.has_zero_write_authority()
            && mutation_baseline.is_none()
            && !context.policy.read_only
        {
            return Ok(ToolOutput::error(
                "Refused: command mutation constraints require a git workspace mutation baseline.\n",
            ));
        }

        let req = Self::background_process_request(program, args.clone(), cwd, &context);
        // Always session-owned so the creator can observe and stop it. Lifetime
        // decides whether goal terminal cleanup includes it; daemon-scoped
        // spawning remains reserved for runtime-internal services.
        match reg
            .spawn_for_writer(
                req,
                mutation_baseline,
                Some(context.session_scope()),
                context.writer_scope(),
                lifetime,
            )
            .await
        {
            Ok(task_id) => Ok(ToolOutput::ok(format!(
                "background task started\ntask_id: {task_id}\nprogram: {program}\nargs: {args:?}\n\
             status: running\nlifetime: {}\nUse get_task/wait_task/kill_task with this task_id.",
                match lifetime {
                    BackgroundTaskLifetime::Goal => "goal",
                    BackgroundTaskLifetime::Runtime => "runtime",
                }
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
        let _gate = if context.command_lease.is_none() {
            Some(context.execution.command_gate.clone().lock_owned().await)
        } else {
            None
        };
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
        let network_denied = context.policy.network_denied();
        request.deny_network = network_denied;
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
            match WorkspaceSnapshot::capture_for_scope(&root, &context.write_scope()).await {
                Ok(Some(id)) => {
                    if !matches!(
                        context.write_scope(),
                        leveler_execution::WriteScope::ScopedWorkspace { .. }
                    ) && let Err(error) = WorkspaceSnapshot::persist_last(&root, &id).await
                    {
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
                "Refused: command mutation constraints require a git workspace mutation baseline.\n",
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
        let output = match context.output.clone() {
            Some(chunks) => {
                context
                    .execution
                    .runner
                    .run_streaming(request, cancellation, chunks)
                    .await?
            }
            None => context.execution.runner.run(request, cancellation).await?,
        };

        // Detect what the command changed so scope checks and budgets see
        // command-driven mutations, not just tool edits.
        let mut command_modified: Vec<String> = Vec::new();
        let mut snapshot_note: Option<String> = None;
        match (&snapshot, context.policy.read_only) {
            (Some(id), _) => {
                match WorkspaceSnapshot::changed_since_for_scope(&root, id, &context.write_scope())
                    .await
                {
                    Ok(changed) => command_modified = changed,
                    Err(error) => {
                        snapshot_note = Some(format!(
                            "\n[note] could not diff the workspace after this command ({error}); \
                         its file changes were not tracked.\n"
                        ));
                    }
                }
            }
            (None, true) => {}
            (None, false) => {
                snapshot_note = Some(
                    "\n[note] a workspace mutation baseline was unavailable; file changes made by \
                     this command were not tracked and cannot be rolled back.\n"
                        .to_string(),
                );
            }
        }

        let mut mutation_error = None;
        if snapshot.is_some() {
            let newly_modified = command_modified
                .iter()
                .filter(|path| !context.policy.command_previously_modified.contains(path))
                .count();
            let budget_exceeded = context
                .policy
                .command_modified_files_remaining
                .is_some_and(|remaining| newly_modified > remaining);

            if budget_exceeded {
                // The command really wrote these owned paths. Preserve them
                // and report the budget overrun; never roll a shared tree back.
                mutation_error = Some(format!(
                    "command exceeded the remaining file budget (modified {newly_modified})"
                ));
            }
        }

        // Two different things change the working tree: writing files, and moving
        // HEAD. Only the first is authored by this run, and only the first carries
        // a source-change verification obligation — a plain `git switch` that
        // reported every file differing between the branches used to hang the
        // project's whole `cargo test` gate on a task that wrote nothing.
        //
        // The OS write boundary prevents repository commands from crossing scope;
        // file budgets still count all observed changes before attribution.
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
        if network_denied {
            body.insert_str(0, crate::recoverable::network_permission_required());
        }
        if sandboxed {
            body.push_str(crate::recoverable::sandbox_write_denied());
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
                "network_denied": network_denied,
                "filesystem_confined": sandboxed,
                "timed_out": output.timed_out,
                // Whether the RUNNER completed, independent of the exit code: a
                // command that ran and failed a test reports `completed` with a
                // non-zero `exit_code`. Consumers that count tool failures must
                // read this, not `is_error` (which also carries a non-zero exit
                // and a rejected mutation).
                "execution_status": output.execution_status(),
                "modified_files": command_modified,
                "workspace_snapshot": snapshot.as_ref().filter(|_| !matches!(context.write_scope(), leveler_execution::WriteScope::ScopedWorkspace { .. })).map(|id| id.0.clone()),
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

#[cfg(test)]
mod exit_taxonomy_tests {
    //! C2 regression: execution outcome and command outcome are two axes.
    //!
    //! A command that ran to completion and exited non-zero must stay an
    //! execution SUCCESS carrying its non-zero exit code and its output. Only
    //! a process the runtime could not run (spawn failure, timeout,
    //! cancellation) is an execution failure. `is_error` keeps its old
    //! model-visible meaning; the machine-readable status is `metadata`.
    use super::*;
    use crate::tool::ToolContext;
    use leveler_execution::{PermissionProfile, ToolExecutionStatus, Workspace};

    fn ws() -> (std::path::PathBuf, ToolContext) {
        ws_with(PermissionProfile::Assisted)
    }

    fn ws_with(profile: PermissionProfile) -> (std::path::PathBuf, ToolContext) {
        let dir = std::env::temp_dir().join(format!(
            "leveler-exit-taxonomy-{}",
            crate::tools::test_ordinal()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let ws = Workspace::new(&dir).unwrap();
        (dir, ToolContext::new(ws, profile))
    }

    fn commands() -> CommandExecution {
        CommandExecution::new(
            std::sync::Arc::new(BackgroundTaskRegistry::with_environment(
                std::sync::Arc::new(leveler_core::environment().clone()),
            )),
            None,
        )
    }

    fn status_of(out: &ToolOutput) -> ToolExecutionStatus {
        serde_json::from_value(out.metadata["execution_status"].clone())
            .unwrap_or_else(|e| panic!("execution_status missing/invalid: {e}; {out:?}"))
    }

    #[tokio::test]
    async fn exit_zero_is_a_completed_execution() {
        let (dir, ctx) = ws();
        let out = commands()
            .run_foreground(
                "sh",
                vec!["-c".into(), "echo ok; exit 0".into()],
                Some("."),
                Some(30),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{out:?}");
        assert_eq!(status_of(&out), ToolExecutionStatus::Completed);
        assert_eq!(out.metadata["exit_code"], 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The exact case the benchmark mis-counted: a test/check command that ran
    /// normally and reported failure.
    #[tokio::test]
    async fn nonzero_exit_is_completed_with_a_command_failure_visible() {
        let (dir, ctx) = ws();
        let out = commands()
            .run_foreground(
                "sh",
                vec![
                    "-c".into(),
                    "printf 'test failed: 1\n' >&2; exit 101".into(),
                ],
                Some("."),
                Some(30),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            status_of(&out),
            ToolExecutionStatus::Completed,
            "a command that ran and failed is not a tool failure: {out:?}"
        );
        assert_eq!(out.metadata["exit_code"], 101);
        // The model-visible contract is unchanged: it still sees the exit code
        // and the failing output, and the result is still flagged as an error.
        assert!(out.is_error, "model-visible failure signal must survive");
        assert!(out.content.contains("exit: 101"), "{out:?}");
        assert!(out.content.contains("test failed: 1"), "{out:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A probe that uses `exit 1` to mean "false" (grep with no match, a
    /// diff that found a difference) is a completed execution, not a failure.
    #[tokio::test]
    async fn probe_exit_one_is_a_completed_execution() {
        let (dir, ctx) = ws();
        let out = commands()
            .run_foreground(
                "sh",
                vec!["-c".into(), "exit 1".into()],
                Some("."),
                Some(30),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(status_of(&out), ToolExecutionStatus::Completed);
        assert_eq!(out.metadata["exit_code"], 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// With no OS sandbox in front of it (full access), a program that does not
    /// exist is a genuine spawn failure: the runtime never ran a command, so
    /// there is no command outcome to report. (Under the sandbox the wrapper
    /// process itself runs and reports the exec failure as its own exit code —
    /// a *completed* execution — which is why this test does not confine.)
    #[tokio::test]
    async fn spawn_failure_is_an_execution_failure() {
        let (dir, ctx) = ws_with(PermissionProfile::FullAccess);
        let error = commands()
            .run_foreground(
                "leveler-no-such-binary-7f3c1a",
                vec![],
                Some("."),
                Some(30),
                ctx,
                CancellationToken::new(),
            )
            .await
            .expect_err("a missing binary cannot execute");
        assert!(
            matches!(error, ToolError::Process(_)),
            "expected a process error, got {error:?}"
        );
        let ToolError::Process(process) = error else {
            unreachable!()
        };
        assert_eq!(process.execution_status(), ToolExecutionStatus::SpawnFailed);
        assert!(process.execution_status().is_execution_failure());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn timeout_is_an_execution_failure() {
        let (dir, ctx) = ws();
        let out = commands()
            .run_foreground(
                "sh",
                vec!["-c".into(), "sleep 30".into()],
                Some("."),
                Some(1),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(status_of(&out), ToolExecutionStatus::TimedOut);
        assert_eq!(out.metadata["timed_out"], true);
        assert!(out.content.contains("[timed out after 1s]"), "{out:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn cancellation_is_not_a_command_outcome() {
        let (dir, ctx) = ws();
        let token = CancellationToken::new();
        let canceller = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            canceller.cancel();
        });
        let error = commands()
            .run_foreground(
                "sh",
                vec!["-c".into(), "sleep 30".into()],
                Some("."),
                Some(30),
                ctx,
                token,
            )
            .await
            .expect_err("a cancelled command did not run to completion");
        let ToolError::Process(process) = error else {
            panic!("expected a process error: {error:?}")
        };
        assert!(
            matches!(
                process.execution_status(),
                ToolExecutionStatus::Cancelled | ToolExecutionStatus::CancelUnconfirmed
            ),
            "cancellation must be distinguishable from a command outcome: {process:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

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

    /// macOS seatbelt says EPERM. Linux read-only bind says EROFS. Both are
    /// the sandbox refusing the write. The Linux wording is locked with the
    /// stderr recorded from CI run 36526610858, because this host never emits it.
    fn sandbox_refusal(content: &str) -> bool {
        content.contains("request_permissions")
            || content.contains("Operation not permitted")
            || content.contains("operation not permitted")
            || content.contains("Read-only file system")
    }

    #[test]
    fn sandbox_refusal_accepts_the_linux_read_only_bind_text() {
        assert!(sandbox_refusal(
            "sh: 1: cannot create .git/canary-write: Read-only file system"
        ));
        assert!(sandbox_refusal("Operation not permitted"));
        assert!(!sandbox_refusal("No such file or directory"));
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
                sandbox_refusal(&out.content),
                "failure should surface sandbox/recoverable signal: {}",
                out.content
            );
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Compatibility canary: an unrestricted grant still allows `.git` writes.
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
