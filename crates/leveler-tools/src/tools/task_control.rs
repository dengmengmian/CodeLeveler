//! get_task / wait_task / kill_task — manage background process tasks.
//!
//! Each tool is CONSTRUCTED with the registry it manages. It is not optional:
//! a background task nobody can observe or stop is an orphan, so the lifecycle
//! tools and the primitive that starts them share one registry by
//! construction rather than by hoping a context field was populated.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::{BackgroundTaskRegistry, BackgroundTaskStatus, RiskLevel};

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

#[derive(Debug, Deserialize, JsonSchema)]
struct TaskIdInput {
    /// Task id returned by `run_command` with background=true.
    task_id: String,
}

/// Default wait interval. Long enough that a model is not forced into
/// sub-second polling; short enough that the AgentLoop recovers control.
const WAIT_DEFAULT_SECS: u64 = 30;
/// Hard cap. A caller may ask for more; we still return running status
/// rather than owning the AgentLoop for minutes.
const WAIT_MAX_SECS: u64 = 120;

#[derive(Debug, Deserialize, JsonSchema)]
struct WaitInput {
    task_id: String,
    /// Seconds to wait before returning current status (default 30, max 120).
    /// Expiry does not cancel the background task.
    #[serde(default)]
    timeout_seconds: Option<u64>,
}

fn wait_interval(timeout_seconds: Option<u64>) -> Duration {
    Duration::from_secs(
        timeout_seconds
            .unwrap_or(WAIT_DEFAULT_SECS)
            .clamp(1, WAIT_MAX_SECS),
    )
}

fn format_snap(snap: &leveler_execution::BackgroundTaskSnapshot) -> String {
    let status = match snap.status {
        BackgroundTaskStatus::Running => "running",
        BackgroundTaskStatus::Killing => "killing",
        BackgroundTaskStatus::Exited => "exited",
        BackgroundTaskStatus::Killed => "killed",
    };
    format!(
        "task_id: {}\nstatus: {status}\nprogram: {}\nargs: {:?}\nexit_code: {:?}\n\
         duration_ms: {}\n--- log ---\n{}",
        snap.id, snap.program, snap.args, snap.exit_code, snap.duration_ms, snap.log
    )
}

pub struct GetTaskTool {
    tasks: Arc<BackgroundTaskRegistry>,
}

impl GetTaskTool {
    pub fn new(tasks: Arc<BackgroundTaskRegistry>) -> Self {
        Self { tasks }
    }
}

#[async_trait]
impl Tool for GetTaskTool {
    fn name(&self) -> &'static str {
        "get_task"
    }

    fn description(&self) -> &'static str {
        "Get status and recent log of a background task started with \
         run_command(background=true)."
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<TaskIdInput>()
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Safe
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _context: ToolContext,
        _cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: TaskIdInput = super::parse_input(self.name(), input)?;
        let reg = &self.tasks;
        match reg.get(input.task_id.trim()).await {
            Some(snap) => Ok(ToolOutput::ok(format_snap(&snap))),
            None => Ok(ToolOutput::error(format!(
                "unknown task_id `{}`",
                input.task_id
            ))),
        }
    }
}

pub struct WaitTaskTool {
    tasks: Arc<BackgroundTaskRegistry>,
}

impl WaitTaskTool {
    pub fn new(tasks: Arc<BackgroundTaskRegistry>) -> Self {
        Self { tasks }
    }
}

#[async_trait]
impl Tool for WaitTaskTool {
    fn name(&self) -> &'static str {
        "wait_task"
    }

    fn description(&self) -> &'static str {
        "Wait for a background task for a bounded interval (default 30s, max 120s). \
         If it is still running, returns current status, elapsed time, and recent log \
         without cancelling it. If it has exited, returns final status and log."
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<WaitInput>()
    }

    fn risk(&self) -> RiskLevel {
        // Not Safe, even though the runtime — not this tool — now performs the
        // settlement. Waiting consumes the task's settlement report exactly
        // once, and a crash replay would block recovery for up to two minutes
        // and swallow that report. Neither belongs in an unattended re-run.
        RiskLevel::WorkspaceWrite
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _context: ToolContext,
        cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: WaitInput = super::parse_input(self.name(), input)?;
        let reg = &self.tasks;
        let task_id = input.task_id.trim().to_string();
        let timeout = wait_interval(input.timeout_seconds);
        match reg.wait(&task_id, Some(timeout), &cancellation).await {
            Ok(snap)
                if matches!(
                    snap.status,
                    BackgroundTaskStatus::Running | BackgroundTaskStatus::Killing
                ) =>
            {
                // Interval elapsed (or a kill is in flight). Do not consume the
                // mutation baseline — that runs once, at true terminal.
                Ok(
                    ToolOutput::ok(format_snap(&snap)).with_metadata(serde_json::json!({
                        "exit_code": snap.exit_code,
                        "duration_ms": snap.duration_ms,
                    })),
                )
            }
            Ok(snap) => {
                // The task is terminal, so the runtime already settled it when
                // the process exited: diffed the workspace and, under a write
                // allowlist, restored what the task was not allowed to touch.
                // This reads that result — it does not produce it.
                let settlement = reg.take_settlement(&task_id).await;

                let mut text = format_snap(&snap);
                if let Some(note) = settlement.as_ref().and_then(|s| s.note.as_deref()) {
                    text.push_str("\n[note] ");
                    text.push_str(note);
                    text.push('\n');
                }
                let violation = settlement.as_ref().and_then(|s| s.violation.as_deref());
                if let Some(violation) = violation {
                    text.push_str("\n[mutation rejected] ");
                    text.push_str(violation);
                    text.push('\n');
                }

                let failed = snap.status != BackgroundTaskStatus::Exited
                    || snap.exit_code.unwrap_or(1) != 0
                    || violation.is_some();
                let content = if failed {
                    text
                } else {
                    format!("{text}\n(ok)")
                };
                let out = if failed {
                    ToolOutput::error(content)
                } else {
                    ToolOutput::ok(content)
                };
                Ok(out.with_metadata(serde_json::json!({
                    "exit_code": snap.exit_code,
                    "modified_files": settlement
                        .as_ref()
                        .map(|s| s.modified.clone())
                        .unwrap_or_default(),
                    "workspace_snapshot": settlement.as_ref().map(|s| s.snapshot.0.clone()),
                })))
            }
            Err(e) => Ok(ToolOutput::error(e)),
        }
    }
}

pub struct KillTaskTool {
    tasks: Arc<BackgroundTaskRegistry>,
}

impl KillTaskTool {
    pub fn new(tasks: Arc<BackgroundTaskRegistry>) -> Self {
        Self { tasks }
    }
}

#[async_trait]
impl Tool for KillTaskTool {
    fn name(&self) -> &'static str {
        "kill_task"
    }

    fn description(&self) -> &'static str {
        "Terminate a background task (SIGTERM/kill). Safe if already exited."
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<TaskIdInput>()
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::WorkspaceWrite
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _context: ToolContext,
        _cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: TaskIdInput = super::parse_input(self.name(), input)?;
        let reg = &self.tasks;
        match reg.kill(input.task_id.trim()).await {
            Ok(snap) => Ok(ToolOutput::ok(format_snap(&snap))),
            Err(e) => Ok(ToolOutput::error(e)),
        }
    }
}

// Every test here drives background mutations through a POSIX `sh -c` + git
// fixture; Windows background/rollback behavior is covered by the `windows_`
// canary tests.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::tool::{Tool, ToolContext};
    use crate::tools::RunCommandTool;
    use leveler_execution::{BackgroundTaskRegistry, PermissionProfile, Workspace};
    use leveler_test_support::git::{run, scratch_repo};
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn wait_task_is_workspace_write_risk() {
        // wait_task can roll the whole workspace back to a snapshot when a
        // background task violates its write allowlist — that is a mutation,
        // not a Safe read (and Safe implies auto-replay on crash recovery).
        let reg = Arc::new(BackgroundTaskRegistry::new());
        assert_eq!(
            WaitTaskTool::new(reg.clone()).risk(),
            RiskLevel::WorkspaceWrite
        );
        // get_task stays a pure status read.
        assert_eq!(GetTaskTool::new(reg).risk(), RiskLevel::Safe);
    }

    #[test]
    fn wait_interval_is_capped() {
        assert_eq!(wait_interval(None).as_secs(), WAIT_DEFAULT_SECS);
        assert_eq!(wait_interval(Some(1)).as_secs(), 1);
        assert_eq!(wait_interval(Some(900)).as_secs(), WAIT_MAX_SECS);
    }

    #[tokio::test]
    async fn wait_interval_returns_running_as_ok_without_killing() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let start = RunCommandTool::new(commands.clone())
            .execute(
                serde_json::json!({
                    "program": "sleep",
                    "args": ["30"],
                    "background": true,
                }),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!start.is_error, "spawn: {}", start.content);
        let task_id = start
            .content
            .lines()
            .find_map(|l| l.strip_prefix("task_id: "))
            .expect("task_id")
            .to_string();

        let wait = WaitTaskTool::new(reg.clone())
            .execute(
                serde_json::json!({"task_id": task_id, "timeout_seconds": 1}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            !wait.is_error,
            "still-running wait must not be a tool error: {}",
            wait.content
        );
        assert!(
            wait.content.contains("status: running"),
            "expected running snapshot: {}",
            wait.content
        );
        let snap = reg.get(&task_id).await.expect("retained");
        assert_eq!(snap.status, BackgroundTaskStatus::Running);
        let _ = reg.kill(&task_id).await;
    }

    /// The context, the ONE background registry, and the command runtime built
    /// over it. `run_command(background)` and the lifecycle tools share the
    /// registry by construction — a task started into one registry and waited
    /// on through another is exactly the orphan this ownership makes
    /// impossible.
    fn ctx_with_reg(
        dir: &std::path::Path,
    ) -> (
        ToolContext,
        Arc<BackgroundTaskRegistry>,
        Arc<crate::tools::CommandExecution>,
    ) {
        let ws = Workspace::new(dir).unwrap();
        // Library tests do not install the application's global environment
        // capability. Give both the tool context and its background registry
        // the same explicit snapshot used by the composition root; otherwise
        // confined background commands fail before spawning.
        let environment = Arc::new(leveler_core::EnvSnapshot::new(
            std::env::vars_os(),
            std::env::current_dir().unwrap_or_default(),
            std::env::temp_dir(),
        ));
        let reg = Arc::new(BackgroundTaskRegistry::with_environment(
            environment.clone(),
        ));
        let commands = Arc::new(crate::tools::CommandExecution::new(reg.clone(), None));
        let ctx = ToolContext::with_environment(ws, PermissionProfile::Assisted, environment);
        (ctx, reg, commands)
    }

    /// A background command that exits non-zero must surface as a tool ERROR
    /// carrying its exit code — never as a quiet success.
    ///
    /// Spawn Reliability Gate, Experiment 3c. The Multi-Agent shape this
    /// guards is a parent that fans out children *and* leaves a verification
    /// command running in the background: if that command's failure arrives as
    /// `ok`, the parent synthesises a conclusion on top of a check that did not
    /// pass, and reports success it has not earned.
    #[tokio::test]
    async fn a_failing_background_command_is_reported_as_an_error_with_its_exit_code() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());

        let start = RunCommandTool::new(commands.clone())
            .execute(
                serde_json::json!({
                    "program": "sh",
                    "args": ["-c", "echo failing >&2; exit 3"],
                    "background": true,
                }),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!start.is_error, "spawn should succeed: {}", start.content);
        let task_id = start
            .content
            .lines()
            .find_map(|l| l.strip_prefix("task_id: "))
            .expect("task_id in spawn output")
            .to_string();

        let wait = WaitTaskTool::new(reg.clone())
            .execute(
                serde_json::json!({"task_id": task_id, "timeout_seconds": 10}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert!(
            wait.is_error,
            "a non-zero background exit must not read as success: {}",
            wait.content
        );
        assert_eq!(
            wait.metadata.get("exit_code").and_then(|v| v.as_i64()),
            Some(3),
            "the exit code has to reach the caller, not just the word 'failed': {:?}",
            wait.metadata
        );
    }

    #[tokio::test]
    async fn wait_accounts_modified_files_without_restoring_when_no_allowlist() {
        // Default Goal background (dev server / watcher): account diffs, never
        // auto-restore intentional long-lived mutations (K17 / PR-3b).
        let dir = scratch_repo();
        std::fs::write(dir.path().join("keep.txt"), "original\n").unwrap();
        run(dir.path(), &["add", "-A"]);
        run(dir.path(), &["commit", "-qm", "init"]);

        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let start = RunCommandTool::new(commands.clone())
            .execute(
                serde_json::json!({
                    "program": "sh",
                    "args": ["-c", "echo new > created.txt && echo changed > keep.txt"],
                    "background": true,
                }),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!start.is_error, "spawn should succeed: {}", start.content);
        let task_id = start
            .content
            .lines()
            .find_map(|l| l.strip_prefix("task_id: "))
            .expect("task_id in spawn output")
            .to_string();

        let wait = WaitTaskTool::new(reg.clone())
            .execute(
                serde_json::json!({"task_id": task_id, "timeout_seconds": 10}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!wait.is_error, "wait should succeed: {}", wait.content);

        let modified: Vec<String> = wait
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
                && modified.contains(&"keep.txt".to_string()),
            "wait must account background mutations: {modified:?}"
        );
        // No allowlist → files must remain (do not destroy intentional mutations).
        assert!(
            dir.path().join("created.txt").exists(),
            "without allowlist, wait must not restore away created files"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("keep.txt"))
                .unwrap()
                .trim(),
            "changed"
        );
    }

    #[tokio::test]
    async fn wait_restores_out_of_allowlist_mutations() {
        let dir = scratch_repo();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "original\n").unwrap();
        run(dir.path(), &["add", "-A"]);
        run(dir.path(), &["commit", "-qm", "init"]);

        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let constrained =
            ctx.with_command_write_constraints(Some(vec!["src".to_string()]), None, Vec::new());

        let start = RunCommandTool::new(commands.clone())
            .execute(
                serde_json::json!({
                    "program": "sh",
                    "args": ["-c", "echo bad > outside.txt && echo ok > src/lib.rs"],
                    "background": true,
                }),
                constrained.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!start.is_error, "spawn should succeed: {}", start.content);
        let task_id = start
            .content
            .lines()
            .find_map(|l| l.strip_prefix("task_id: "))
            .expect("task_id")
            .to_string();

        let wait = WaitTaskTool::new(reg.clone())
            .execute(
                serde_json::json!({"task_id": task_id, "timeout_seconds": 10}),
                constrained,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert!(wait.is_error, "allowlist violation must fail wait");
        assert!(
            wait.content.contains("outside allowed paths"),
            "expected scope message: {}",
            wait.content
        );
        assert!(
            !dir.path().join("outside.txt").exists(),
            "out-of-allowlist create must be restored"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("src/lib.rs"))
                .unwrap()
                .trim(),
            "original",
            "restore rolls back the whole snapshot, including in-scope edits"
        );
        let modified = wait
            .metadata
            .get("modified_files")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        assert!(
            modified.is_empty(),
            "restored violations clear modified_files: {modified:?}"
        );
    }

    #[tokio::test]
    async fn wait_keeps_in_allowlist_mutations() {
        let dir = scratch_repo();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "original\n").unwrap();
        run(dir.path(), &["add", "-A"]);
        run(dir.path(), &["commit", "-qm", "init"]);

        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let constrained =
            ctx.with_command_write_constraints(Some(vec!["src".to_string()]), None, Vec::new());

        let start = RunCommandTool::new(commands.clone())
            .execute(
                serde_json::json!({
                    "program": "sh",
                    "args": ["-c", "echo patched > src/lib.rs"],
                    "background": true,
                }),
                constrained.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let task_id = start
            .content
            .lines()
            .find_map(|l| l.strip_prefix("task_id: "))
            .expect("task_id")
            .to_string();

        let wait = WaitTaskTool::new(reg.clone())
            .execute(
                serde_json::json!({"task_id": task_id, "timeout_seconds": 10}),
                constrained,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            !wait.is_error,
            "in-scope edit should pass: {}",
            wait.content
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("src/lib.rs"))
                .unwrap()
                .trim(),
            "patched"
        );
        let modified: Vec<String> = wait
            .metadata
            .get("modified_files")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(modified, vec!["src/lib.rs".to_string()]);
    }
}
