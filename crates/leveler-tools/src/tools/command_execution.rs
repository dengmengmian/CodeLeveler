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
    AdoptedTaskSpec, ArtifactStore, BackgroundTaskLifetime, BackgroundTaskRegistry,
    MutationBaseline, OutputChunk, ProcessHandoff, ProcessOutput, ProcessRequest, StreamOutcome,
    WorkspaceSnapshot,
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
            .execution_workspace()?
            .resolve_command_cwd(&rel, &context.write_scope())?;

        // Pre-spawn baseline for namespace accounting after process-group exit.
        let root = context.require_workspace()?.root().to_path_buf();
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
        if !context.policy.unrestricted_execution()
            && context.policy.command_write_allowlist.is_some()
            && !context.policy.has_zero_write_authority()
            && mutation_baseline.is_none()
            && !context.policy.read_only
        {
            return Ok(ToolOutput::error(
                "Refused: command mutation constraints require a git workspace mutation baseline.\n",
            ));
        }

        let isolation = Self::frozen_git_invocation(program, &args, &cwd, &context).await?;
        // A detached process outlives this call, so its frozen directory cannot
        // be reaped when the call returns. The directory is not a secret, but an
        // approved credential staged inside it is; the runtime cannot bound that
        // value's lifetime to the task, so it refuses instead of leaving it on
        // disk for the operating system's temp cleanup. Run the effect in the
        // foreground, where the guard reaps it when the process exits.
        if isolation
            .as_ref()
            .is_some_and(|iso| iso.carries_credential())
        {
            return Ok(ToolOutput::error(
                "Refused: an approved Git credential is bound to one foreground \
                 execution and cannot be used by a detached background task. Run \
                 this remote Git command in the foreground.\n",
            ));
        }
        let (program, args, authority_env, isolation_deny) = match &isolation {
            Some(iso) => (
                iso.program().to_string(),
                iso.args().to_vec(),
                iso.authority_env().to_vec(),
                iso.deny_env().to_vec(),
            ),
            None => (program.to_string(), args, Vec::new(), Vec::new()),
        };
        let isolation_write_roots = isolation
            .as_ref()
            .map(|iso| vec![iso.root().to_path_buf()])
            .unwrap_or_default();
        // A detached process outlives this call, so its frozen directory must too.
        if let Some(iso) = isolation {
            let _ = iso.keep();
        }
        let req = Self::background_process_request(
            &program,
            args.clone(),
            cwd,
            &context,
            authority_env,
            isolation_deny,
            isolation_write_roots,
        );
        // Always session-owned so the creator can observe and stop it. Lifetime
        // decides whether goal terminal cleanup includes it; daemon-scoped
        // spawning remains reserved for runtime-internal services.
        match reg
            .spawn_with_origin(
                req,
                mutation_baseline,
                Some(context.session_scope()),
                context.writer_scope(),
                lifetime,
                leveler_execution::BackgroundTaskOrigin {
                    call_id: context.tool_call_scope.as_deref().map(str::to_string),
                    goal_id: context.background_goal.clone(),
                },
            )
            .await
        {
            Ok(task_id) => {
                Ok(ToolOutput::ok(format!(
                    "background task started\ntask_id: {task_id}\nprogram: {program}\n\
                     args: {args:?}\nstatus: running\nlifetime: {} (stopped when {})\n\
                     execution deadline: none (background hosting owns the process lifetime; \
                     kill_task stops it and its lifetime ends it)\n\
                     Use get_task/wait_task/kill_task with this task_id.",
                    lifetime.as_str(),
                    lifetime.stop_condition(),
                ))
                // The task identity is a structured fact, not only prose: the
                // executor links this start to the task whose terminal result
                // will become this goal's evidence.
                .with_metadata(serde_json::json!({
                    "task_id": task_id,
                    "status": "running",
                    "background": true,
                    "lifetime": lifetime.as_str(),
                    "stops_when": lifetime.stop_condition(),
                })))
            }
            Err(e) => Ok(ToolOutput::error(format!("background spawn failed: {e}"))),
        }
    }

    /// Resolve the frozen execution target for a Git resource grant. `Ok(None)`
    /// means this policy carries no Git grant; a grant that cannot be reproduced
    /// from frozen bytes is an error, so the command never falls back to live
    /// mutable configuration.
    async fn frozen_git_invocation(
        program: &str,
        args: &[String],
        cwd: &std::path::Path,
        context: &ToolContext,
    ) -> Result<Option<leveler_execution::GitIsolation>, ToolError> {
        let Some(grant) = context.policy.resource_grant() else {
            return Ok(None);
        };
        let Some(target) =
            leveler_execution::approved_git_target(grant, program, args).map_err(ToolError::Io)?
        else {
            return Ok(None);
        };
        leveler_execution::isolate_git_command(&target, args, cwd, &context.execution.environment)
            .await
            .map(Some)
            .map_err(|reason| ToolError::Io(format!("frozen Git execution refused: {reason}")))
    }

    /// Build a [`ProcessRequest`] for background spawn with the same sandbox fields
    /// as [`Self::run_foreground`] (PR-3a). Non-FullAccess / non-turn-unrestricted
    /// → write confinement; network follows `context.policy.network_denied()`.
    pub(super) fn background_process_request(
        program: &str,
        args: Vec<String>,
        cwd: std::path::PathBuf,
        context: &ToolContext,
        authority_env: Vec<(String, String)>,
        isolation_deny: Vec<String>,
        isolation_write_roots: Vec<std::path::PathBuf>,
    ) -> ProcessRequest {
        let mut req = ProcessRequest::new(program, args, cwd);
        req.network_scope = context.policy.network_scope();
        req.deny_network = matches!(req.network_scope, leveler_execution::NetworkScope::None);
        req.unrestricted_execution = context.policy.unrestricted_execution();
        req.deny_env = context.policy.deny_env.as_ref().clone();
        req.deny_env.extend(isolation_deny);
        req.authority_env = authority_env;
        req.sandbox_write_roots = isolation_write_roots;
        req.write_scope = context.write_scope();
        req
    }

    /// Run a program in the foreground: the shared path both command tools take
    /// once they have decoded their own arguments.
    ///
    /// When the process is still the foreground driver's after
    /// [`FOREGROUND_HANDOFF_AFTER`] and this call is not bounded below that
    /// window, the same process moves to the background registry and this call
    /// returns while it runs.
    ///
    /// Two durations are kept apart there:
    ///
    /// - The caller's `timeout_seconds` is an EXECUTION deadline. It follows the
    ///   process into the registry, still counted from process start.
    /// - [`DEFAULT_TIMEOUT_SECS`] is only this call's WAIT budget when the caller
    ///   set no deadline. It bounds how long one tool call blocks, not how long
    ///   the process may live: a 20-minute service is not killed at 2 minutes.
    ///
    /// A credential-bearing Git command stays here until it exits, so the staged
    /// secret is not left on disk.
    /// Run one command in the foreground with the default cleanup boundary for
    /// anything the handoff moves to the background.
    pub async fn run_foreground(
        &self,
        program: &str,
        args: Vec<String>,
        cwd_rel: Option<&str>,
        timeout_seconds: Option<u64>,
        context: ToolContext,
        cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        self.run_foreground_with_lifetime(
            program,
            args,
            cwd_rel,
            timeout_seconds,
            None,
            context,
            cancellation,
        )
        .await
    }

    /// Run one command in the foreground, keeping the caller's own cleanup
    /// boundary if the wait budget moves it to the background.
    ///
    /// The boundary is the caller's decision: a command that a tool call
    /// happens to outlive must not be silently reclassified as goal-owned work,
    /// which is what happened when every handoff was adopted as `Goal`.
    pub async fn run_foreground_with_lifetime(
        &self,
        program: &str,
        args: Vec<String>,
        cwd_rel: Option<&str>,
        timeout_seconds: Option<u64>,
        handoff_lifetime: Option<BackgroundTaskLifetime>,
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
            .execution_workspace()?
            .resolve_command_cwd(&rel, &context.write_scope())?;
        let executed_commands = leveler_execution::proven_executed_commands(program, &args);

        // A Git resource grant executes its frozen approved target instead of the
        // live command line. The guard owns the private git directory until the
        // process has finished.
        let isolation = Self::frozen_git_invocation(program, &args, &cwd, &context).await?;
        let (program, args, authority_env, isolation_deny) = match &isolation {
            Some(iso) => (
                iso.program().to_string(),
                iso.args().to_vec(),
                iso.authority_env().to_vec(),
                iso.deny_env().to_vec(),
            ),
            None => (program.to_string(), args, Vec::new(), Vec::new()),
        };

        let scope = context.write_scope();
        let mut request = ProcessRequest::new(program, args, cwd);
        // A deadline the CALLER stated, as opposed to this call's own wait
        // budget. Only the former may outlive the call it was written on.
        let caller_deadline = caller_deadline(timeout_seconds);
        // What one call may block for. This is the caller's deadline when they
        // stated one, and the default budget otherwise — never "the process may
        // run forever".
        request.timeout = resolve_timeout(timeout_seconds);
        let foreground_timeout = request.timeout;
        let network_scope = context.policy.network_scope();
        let network_denied = matches!(network_scope, leveler_execution::NetworkScope::None);
        request.network_scope = network_scope.clone();
        request.deny_network =
            matches!(request.network_scope, leveler_execution::NetworkScope::None);
        request.unrestricted_execution = context.policy.unrestricted_execution();
        request.deny_env = context.policy.deny_env.as_ref().clone();
        request.deny_env.extend(isolation_deny);
        request.authority_env = authority_env;
        request.sandbox_write_roots = isolation
            .as_ref()
            .map(|iso| vec![iso.root().to_path_buf()])
            .unwrap_or_default();
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
        let root = context.require_workspace()?.root().to_path_buf();
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
        let constrained = !context.policy.unrestricted_execution()
            && (context.policy.command_write_allowlist.is_some()
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
        // The handoff is a transfer of this driver, not a second liveness check.
        // A timeout at or under the threshold kills on the original path, and a
        // staged Git credential never leaves this call.
        let handoff = request.timeout > FOREGROUND_HANDOFF_AFTER
            && !isolation
                .as_ref()
                .is_some_and(|iso| iso.carries_credential());
        let output = if handoff {
            let shown_program = request.program.clone();
            let shown_args = request.args.clone();
            let baseline = snapshot.as_ref().map(|id| MutationBaseline {
                snapshot: id.clone(),
                workspace_root: root.clone(),
                write_scope: request.write_scope.clone(),
            });
            match self
                .hand_off_if_still_running(
                    request,
                    caller_deadline,
                    handoff_lifetime,
                    cancellation,
                    context.output.clone(),
                    baseline,
                    &context,
                )
                .await?
            {
                ForegroundDrive::Finished(output) => output,
                ForegroundDrive::StillRunning(task_id) => {
                    if let Some(iso) = isolation {
                        let _ = iso.keep();
                    }
                    return Ok(foreground_handoff_output(
                        &shown_program,
                        &shown_args,
                        &task_id,
                        caller_deadline,
                        handoff_lifetime.unwrap_or(BackgroundTaskLifetime::Goal),
                    ));
                }
            }
        } else {
            match context.output.clone() {
                Some(chunks) => {
                    context
                        .execution
                        .runner
                        .run_streaming(request, cancellation, chunks)
                        .await?
                }
                None => context.execution.runner.run(request, cancellation).await?,
            }
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
        if snapshot.is_some() && !context.policy.unrestricted_execution() {
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
            let source = if caller_deadline.is_some() {
                "the execution deadline this call set"
            } else {
                "this call's default wait budget"
            };
            body.push_str(&format!(
                "[timed out after {}s] ({source})\n",
                foreground_timeout.as_secs()
            ));
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
                "network_scope": network_scope.label(),
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

    /// Drive one foreground command that may be adopted at [`FOREGROUND_HANDOFF_AFTER`].
    ///
    /// The driver's own select decides the race: process exit and cancellation
    /// beat the handoff. This function does not probe the process and then adopt.
    async fn hand_off_if_still_running(
        &self,
        request: ProcessRequest,
        caller_deadline: Option<Duration>,
        handoff_lifetime: Option<BackgroundTaskLifetime>,
        cancellation: CancellationToken,
        live: Option<tokio::sync::mpsc::Sender<OutputChunk>>,
        baseline: Option<MutationBaseline>,
        context: &ToolContext,
    ) -> Result<ForegroundDrive, ToolError> {
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let handoff = CancellationToken::new();
        let timer = {
            let handoff = handoff.clone();
            tokio::spawn(async move {
                tokio::time::sleep(FOREGROUND_HANDOFF_AFTER).await;
                handoff.cancel();
            })
        };
        let runner = context.execution.runner.clone();
        let run_cancel = cancellation.clone();
        let mut run = tokio::spawn(async move {
            runner
                .run_streaming_handoff(request, run_cancel, tx, handoff)
                .await
        });
        let mut initial_output = String::new();
        let joined = loop {
            tokio::select! {
                joined = &mut run => break joined,
                chunk = rx.recv() => match chunk {
                    Some(chunk) => {
                        forward_live(&live, &chunk);
                        push_output_tail(&mut initial_output, &chunk.text);
                    }
                    None => break run.await,
                },
            }
        };
        timer.abort();
        let outcome =
            joined.map_err(|error| ToolError::Io(format!("command driver failed: {error}")))?;
        match outcome {
            StreamOutcome::Completed(result) => {
                while let Some(chunk) = rx.recv().await {
                    forward_live(&live, &chunk);
                }
                Ok(ForegroundDrive::Finished(result?))
            }
            StreamOutcome::HandedOff(parts) => {
                if cancellation.is_cancelled() {
                    let output = finish_foreground_handoff(
                        &context.execution.runner,
                        *parts,
                        &mut rx,
                        cancellation,
                        &live,
                    )
                    .await?;
                    return Ok(ForegroundDrive::Finished(output));
                }
                let started = parts.started;
                let spec = AdoptedTaskSpec {
                    owner_scope: Some(context.session_scope().to_string()),
                    goal_id: context.background_goal.clone(),
                    writer_scope: context.writer_scope().to_string(),
                    // The call that is being handed over: its result is still
                    // the START, so the eventual exit has to be attributable to
                    // it for the goal's evidence.
                    origin_call_id: context.tool_call_scope.as_deref().map(str::to_string),
                    program: parts.request.program.clone(),
                    args: parts.request.args.clone(),
                    cwd: parts.request.cwd.clone(),
                    write_scope: parts.request.write_scope.clone(),
                    // The caller's boundary survives the handoff; the default
                    // stays `Goal` for a command that expressed none.
                    lifetime: handoff_lifetime.unwrap_or(BackgroundTaskLifetime::Goal),
                    started,
                    // Only a deadline the caller stated follows the execution
                    // here; the call's own wait budget ended with the call.
                    deadline: caller_deadline,
                    initial_output,
                    observer: None,
                    on_adopted: None,
                    mutation_baseline: baseline,
                };
                match self
                    .background_tasks
                    .adopt(*parts, rx, spec, CancellationToken::new())
                    .await
                {
                    Ok(task_id) => Ok(ForegroundDrive::StillRunning(task_id)),
                    Err(error) => {
                        let mut chunks = error.chunks;
                        let output = finish_foreground_handoff(
                            &context.execution.runner,
                            error.handoff,
                            &mut chunks,
                            cancellation,
                            &live,
                        )
                        .await?;
                        Ok(ForegroundDrive::Finished(output))
                    }
                }
            }
        }
    }
}

enum ForegroundDrive {
    Finished(ProcessOutput),
    StillRunning(String),
}

fn foreground_handoff_output(
    program: &str,
    args: &[String],
    task_id: &str,
    deadline: Option<Duration>,
    lifetime: BackgroundTaskLifetime,
) -> ToolOutput {
    // The consequence is stated by the layer that decides it: a caller who
    // asked for a 600s limit must be told the service ends at 600s, and a
    // caller who set no limit must not be told the tool call's own wait budget
    // is still running.
    let (lifetime_line, deadline_secs) = match deadline {
        Some(deadline) => (
            format!(
                "execution deadline: timeout_seconds={} set by this call; the process and its \
                 process group will be terminated {}s after it started.\n\
                 For a service that must keep running, start it with background=true (or stop it \
                 explicitly with kill_task).\n",
                deadline.as_secs(),
                deadline.as_secs(),
            ),
            Some(deadline.as_secs()),
        ),
        None => (
            "execution deadline: none. This call set no timeout_seconds, so the process is now \
             owned by its task lifecycle. It keeps running until it exits, kill_task stops it, \
             or the boundary above ends it.\n"
                .to_string(),
            None,
        ),
    };
    ToolOutput::ok(format!(
        "foreground command moved to the background after {}s\n\
         task_id: {task_id}\n\
         program: {program}\n\
         args: {args:?}\n\
         status: running\n\
         lifetime: {} (stopped when {})\n\
         The process is still running. This is not a successful exit.\n\
         {lifetime_line}\
         Use wait_task to read the final result, get_task to observe it, or kill_task to stop it.\n",
        FOREGROUND_HANDOFF_AFTER.as_secs(),
        lifetime.as_str(),
        lifetime.stop_condition(),
    ))
    .with_metadata(serde_json::json!({
        "task_id": task_id,
        "status": "running",
        "background": true,
        "lifetime": lifetime.as_str(),
        "deadline_seconds": deadline_secs,
    }))
}

fn forward_live(live: &Option<tokio::sync::mpsc::Sender<OutputChunk>>, chunk: &OutputChunk) {
    if let Some(live) = live {
        let _ = live.try_send(chunk.clone());
    }
}

fn push_output_tail(buf: &mut String, text: &str) {
    buf.push_str(text);
    if buf.len() <= HANDOFF_OUTPUT_TAIL {
        return;
    }
    let start = leveler_core::ceil_char_boundary(buf, buf.len() - HANDOFF_OUTPUT_TAIL);
    buf.drain(..start);
}

async fn finish_foreground_handoff(
    runner: &leveler_execution::CommandRunner,
    handoff: ProcessHandoff,
    chunks: &mut tokio::sync::mpsc::Receiver<OutputChunk>,
    cancellation: CancellationToken,
    live: &Option<tokio::sync::mpsc::Sender<OutputChunk>>,
) -> Result<ProcessOutput, ToolError> {
    let mut finish = Box::pin(runner.finish_handoff(handoff, cancellation));
    let result = loop {
        tokio::select! {
            result = &mut finish => break result,
            chunk = chunks.recv() => match chunk {
                Some(chunk) => forward_live(live, &chunk),
                None => break finish.await,
            },
        }
    };
    while let Some(chunk) = chunks.recv().await {
        forward_live(live, &chunk);
    }
    Ok(result?)
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

/// How long an agent foreground command stays on the tool call before the same
/// process is adopted by the background registry. This is not a timeout and
/// not a failure: the execution timeout still starts at process spawn.
const FOREGROUND_HANDOFF_AFTER: Duration = Duration::from_secs(15);

/// Output already streamed before a handoff, kept so the background log does
/// not start after the transfer. Only the tail is retained.
const HANDOFF_OUTPUT_TAIL: usize = 64 * 1024;

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
    caller_deadline(timeout_seconds).unwrap_or(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
}

/// The execution deadline the CALLER stated, if any.
///
/// `None` (or zero, which cannot mean "expire immediately") is not a deadline:
/// it is the absence of one. [`resolve_timeout`] turns that absence into this
/// call's own wait budget, which is a different thing — a wait budget ends with
/// the call, an execution deadline follows the process.
fn caller_deadline(timeout_seconds: Option<u64>) -> Option<Duration> {
    timeout_seconds
        .filter(|&secs| secs > 0)
        .map(|secs| Duration::from_secs(secs.min(MAX_TIMEOUT_SECS)))
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

#[cfg(test)]
mod foreground_handoff_tests {
    use super::*;
    #[cfg(unix)]
    use crate::tool::Tool;
    use crate::tool::ToolContext;
    #[cfg(unix)]
    use crate::tools::GetTaskTool;
    #[cfg(unix)]
    use crate::tools::WaitTaskTool;
    #[cfg(unix)]
    use leveler_execution::BackgroundTaskStatus;
    use leveler_execution::{
        BackgroundTaskLifetime, BackgroundTaskRegistry, PermissionProfile, Workspace,
    };
    use leveler_test_support::git::scratch_repo;
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tokio_util::sync::CancellationToken;

    fn harness(
        dir: &std::path::Path,
    ) -> (ToolContext, Arc<BackgroundTaskRegistry>, CommandExecution) {
        let ws = Workspace::new(dir).unwrap();
        let environment = Arc::new(leveler_core::EnvSnapshot::new(
            std::env::vars_os(),
            std::env::current_dir().unwrap_or_default(),
            std::env::temp_dir(),
        ));
        let reg = Arc::new(BackgroundTaskRegistry::with_environment(
            environment.clone(),
        ));
        let commands = CommandExecution::new(reg.clone(), None);
        let ctx = ToolContext::with_environment(ws, PermissionProfile::Assisted, environment);
        (ctx, reg, commands)
    }

    fn task_id_of(content: &str) -> Option<String> {
        content
            .lines()
            .find_map(|line| line.strip_prefix("task_id: "))
            .map(str::trim)
            .map(str::to_string)
    }

    #[tokio::test]
    async fn a_short_command_still_returns_its_exit_and_registers_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, reg, commands) = harness(dir.path());
        let started = Instant::now();
        let out = commands
            .run_foreground(
                "sh",
                vec!["-c".into(), "echo hi".into()],
                Some("."),
                Some(60),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
        assert!(out.content.contains("exit: 0"), "{out:?}");
        assert!(task_id_of(&out.content).is_none(), "{out:?}");
        assert!(
            reg.active_snapshots().await.is_empty(),
            "a finished command must not become a background task"
        );
    }

    #[tokio::test]
    async fn a_short_timeout_kills_before_the_foreground_handoff() {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, reg, commands) = harness(dir.path());
        let started = Instant::now();
        let out = commands
            .run_foreground(
                "sh",
                vec!["-c".into(), "sleep 30".into()],
                Some("."),
                Some(2),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(out.metadata["timed_out"], true, "{out:?}");
        assert!(out.content.contains("[timed out after 2s]"), "{out:?}");
        assert!(
            reg.active_snapshots().await.is_empty(),
            "a timeout must kill the command, not move it to the background: {out:?}"
        );
    }

    /// `setsid` leaves the sleep outside the shell's process group. The shell
    /// itself exits at once, so the command is not handed off, and the orphan
    /// is not registered as its own task.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_shell_that_exits_does_not_register_a_detached_child() {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, reg, commands) = harness(dir.path());
        let out = commands
            .run_foreground(
                "sh",
                vec![
                    "-c".into(),
                    "setsid sleep 40 >/dev/null 2>&1 </dev/null & echo $!".into(),
                ],
                Some("."),
                Some(30),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let orphan = out
            .content
            .lines()
            .filter_map(|line| line.trim().parse::<u32>().ok())
            .next();
        assert!(out.content.contains("exit: 0"), "{out:?}");
        assert!(reg.active_snapshots().await.is_empty(), "{out:?}");
        if let Some(pid) = orphan {
            let _ = std::process::Command::new("kill")
                .arg(pid.to_string())
                .status();
        }
    }

    /// The caller's cleanup boundary survives the handoff. A command the tool
    /// call outlives must not be silently reclassified as goal-owned work: that
    /// is what let a service the model asked to keep running be stopped when the
    /// goal settled.
    #[tokio::test]
    async fn a_handed_off_command_keeps_the_lifetime_the_caller_chose() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = harness(dir.path());
        let out = commands
            .run_foreground_with_lifetime(
                "sh",
                vec!["-c".into(), "sleep 16".into()],
                Some("."),
                Some(60),
                Some(BackgroundTaskLifetime::Session),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{out:?}");
        let task_id = task_id_of(&out.content).expect("task id");
        assert!(
            out.content.contains("lifetime: session"),
            "the result states the caller's own boundary: {out:?}"
        );
        let snap = reg.get(&task_id).await.expect("registered");
        assert_eq!(
            snap.lifetime,
            BackgroundTaskLifetime::Session,
            "the adopted task must keep the boundary the call chose"
        );
        let _ = reg.kill(&task_id).await;
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn a_command_still_running_after_15s_is_handed_to_the_existing_registry() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = harness(dir.path());
        let started = Instant::now();
        let out = commands
            .run_foreground(
                "sh",
                vec![
                    "-c".into(),
                    "echo before; sleep 16; echo hi > created.txt".into(),
                ],
                Some("."),
                Some(60),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(20),
            "the tool call must return at the handoff, not when the command exits: {elapsed:?}"
        );
        assert!(!out.is_error, "{out:?}");
        assert!(
            out.content.contains("status: running"),
            "running is not an exit: {out:?}"
        );
        assert!(
            !out.content.contains("exit:"),
            "a handoff must not report an exit code: {out:?}"
        );
        let task_id = task_id_of(&out.content).expect("task id");
        let snap = reg.get(&task_id).await.expect("registered");
        assert_eq!(snap.status, BackgroundTaskStatus::Running);
        let pid = snap.pid.expect("the adopted process keeps its pid");
        let probe = std::process::Command::new("ps")
            .args(["-p", &pid.to_string()])
            .status()
            .expect("ps");
        assert!(
            probe.success(),
            "pid {pid} must be the still-running process"
        );

        let wait = WaitTaskTool::new(reg.clone())
            .execute(
                serde_json::json!({"task_id": task_id, "timeout_seconds": 20}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!wait.is_error, "the command then exits 0: {}", wait.content);
        let modified: Vec<String> = wait
            .metadata
            .get("modified_files")
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            modified.iter().any(|path| path == "created.txt"),
            "settlement still belongs to the adopted task: {modified:?} {}",
            wait.content
        );
    }
    /// LT-02 on the adopted path: a `timeout_seconds` the caller set is an
    /// execution deadline, and the move to the background does not extend it.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_caller_deadline_still_ends_an_adopted_command() {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, reg, commands) = harness(dir.path());
        let started = Instant::now();
        let out = commands
            .run_foreground(
                "sh",
                vec!["-c".into(), "sleep 60".into()],
                Some("."),
                // Longer than the 15s handoff, short enough to observe.
                Some(17),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            out.content
                .contains("execution deadline: timeout_seconds=17"),
            "the handoff must state the consequence of the deadline: {out:?}"
        );
        let task_id = task_id_of(&out.content).expect("task id");
        assert_eq!(out.metadata["deadline_seconds"], 17, "{out:?}");
        let wait = WaitTaskTool::new(reg.clone())
            .execute(
                serde_json::json!({"task_id": task_id, "timeout_seconds": 20}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(25),
            "the deadline must fire on schedule: {:?}",
            started.elapsed()
        );
        assert!(
            wait.content.contains("timeout_seconds=17"),
            "the terminal result must name the deadline that ended it: {wait:?}"
        );
        assert!(
            reg.active_snapshots().await.is_empty(),
            "an execution deadline ends the task: {wait:?}"
        );
    }

    /// LT-04/LT-05: no caller deadline means no process deadline. The call's
    /// own wait budget ends with the call; the process keeps running and stays
    /// observable until its owner stops it.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_command_with_no_caller_deadline_keeps_running_after_the_handoff() {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, reg, commands) = harness(dir.path());
        let out = commands
            .run_foreground(
                "sh",
                vec!["-c".into(), "echo serving; sleep 60".into()],
                Some("."),
                None,
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            out.content.contains("execution deadline: none"),
            "a hosted execution must not be described as deadline-bound: {out:?}"
        );
        assert!(
            out.metadata["deadline_seconds"].is_null(),
            "no deadline is a fact, not a number: {out:?}"
        );
        let task_id = task_id_of(&out.content).expect("task id");
        // Past the handoff, observed again later: the call's budget is gone,
        // the process is not.
        tokio::time::sleep(Duration::from_secs(3)).await;
        let still = reg.get(&task_id).await.expect("registered");
        assert_eq!(still.status, BackgroundTaskStatus::Running, "{still:?}");
        let get = GetTaskTool::new(reg.clone())
            .execute(
                serde_json::json!({"task_id": task_id}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            get.content.contains("status: running"),
            "the service must stay observable: {get:?}"
        );
        reg.kill(&task_id).await.expect("stop it explicitly");
    }

    /// LT-04: `background=true` is the explicit hosting path, and it reports a
    /// structured task identity so a start can be linked to its result.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_explicit_background_command_reports_its_task_and_has_no_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, reg, commands) = harness(dir.path());
        let out = commands
            .start_background(
                "sh",
                vec!["-c".into(), "sleep 20".into()],
                Some("."),
                BackgroundTaskLifetime::Goal,
                ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{out:?}");
        let task_id = task_id_of(&out.content).expect("task id");
        assert_eq!(out.metadata["task_id"], task_id);
        assert_eq!(out.metadata["status"], "running");
        assert!(out.content.contains("execution deadline: none"), "{out:?}");
        reg.kill(&task_id).await.expect("stop it explicitly");
    }
}
