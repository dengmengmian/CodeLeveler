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
    /// Optional log cursor for an independent reader. Pass next_cursor from
    /// the prior result; omit to use the task's default incremental cursor.
    #[serde(default)]
    cursor: Option<u64>,
    /// Seconds to wait before returning current status (default 30, max 120).
    /// Expiry does not cancel the background task.
    #[serde(default)]
    timeout_seconds: Option<u64>,
}

/// The bounded interval `wait_task` waits for, shared with a sub-agent wait so
/// both kinds of task id wait the same way.
pub fn wait_interval(timeout_seconds: Option<u64>) -> Duration {
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

fn format_wait_delta(
    snap: &leveler_execution::BackgroundTaskSnapshot,
    log: &str,
    dropped_bytes: u64,
    next_cursor: u64,
    pending: bool,
) -> String {
    let status = match snap.status {
        BackgroundTaskStatus::Running => "running",
        BackgroundTaskStatus::Killing => "killing",
        BackgroundTaskStatus::Exited => "exited",
        BackgroundTaskStatus::Killed => "killed",
    };
    let mut text = format!(
        "task_id: {} status: {status} next_cursor: {next_cursor}",
        snap.id
    );
    if !matches!(
        snap.status,
        BackgroundTaskStatus::Running | BackgroundTaskStatus::Killing
    ) {
        text.push_str(&format!(
            " exit_code: {:?} duration_ms: {}",
            snap.exit_code, snap.duration_ms
        ));
    }
    if dropped_bytes > 0 {
        text.push_str(&format!(
            "\n[log gap: {dropped_bytes} bytes no longer retained]"
        ));
    }
    if !log.is_empty() {
        text.push_str("\n--- new log ---\n");
        text.push_str(log);
    }
    if pending {
        text.push_str("\n[more log remains; call wait_task again]");
    }
    text
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
        "Observe a background task: return when new output arrives, its status changes, \
         or the bounded interval expires (default 30s, max 120s). \
         Returns bounded new log and next_cursor; pass cursor to keep an independent \
         reader position. Without cursor, repeated calls use a shared incremental position. \
         Unchanged running tasks return one status line. get_task reads the full retained log."
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
        context: ToolContext,
        cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: WaitInput = super::parse_input(self.name(), input)?;
        let reg = &self.tasks;
        let task_id = input.task_id.trim().to_string();
        let timeout = wait_interval(input.timeout_seconds);
        // The capability owns observation, waiting and reader positions.
        // Leave room for status and settlement under the model-facing cap.
        let log_budget = context
            .policy
            .tool_output_budget
            .saturating_sub(512)
            .min(16 * 1024);
        let wait_result = reg
            .observe(&task_id, input.cursor, log_budget, timeout, &cancellation)
            .await;
        match wait_result {
            Ok(observation) => {
                let snap = observation.snapshot;
                let log = &snap.log;
                let gap = observation.dropped_bytes;
                let next_cursor = observation.next_cursor;
                let pending = observation.log_remaining;
                if matches!(
                    snap.status,
                    BackgroundTaskStatus::Running | BackgroundTaskStatus::Killing
                ) {
                    // Interval elapsed (or a kill is in flight). Do not consume the
                    // mutation baseline — that runs once, at true terminal.
                    return Ok(ToolOutput::ok(format_wait_delta(
                        &snap,
                        log,
                        gap,
                        next_cursor,
                        pending,
                    ))
                    .with_metadata(serde_json::json!({
                        "exit_code": snap.exit_code,
                        "duration_ms": snap.duration_ms,
                        "next_cursor": next_cursor,
                        "log_remaining": pending,
                    })));
                }
                // The task is terminal, so the runtime already settled it when
                // the process exited: diffed the workspace and, under a write
                // allowlist, restored what the task was not allowed to touch.
                // This reads that result — it does not produce it.
                let settlement = observation.settlement;
                let report = reg.take_settlement(&task_id).await;

                let mut text = format_wait_delta(&snap, log, gap, next_cursor, pending);
                let mut diagnostic = String::new();
                if let Some(note) = settlement.as_ref().and_then(|s| s.note.as_deref()) {
                    diagnostic.push_str("\n[note] ");
                    diagnostic.push_str(note);
                    diagnostic.push('\n');
                }
                let violation = settlement.as_ref().and_then(|s| s.violation.as_deref());
                if let Some(violation) = violation {
                    diagnostic.push_str("\n[mutation rejected] ");
                    diagnostic.push_str(violation);
                    diagnostic.push('\n');
                }
                // Never let verbose diagnostics trigger an outer truncation
                // of bytes the reader cursor has already acknowledged.
                text.push_str(&crate::registry::cap_output_with(
                    &diagnostic,
                    context.policy.tool_output_budget.saturating_sub(text.len()),
                ));

                let failed = snap.status != BackgroundTaskStatus::Exited
                    || snap.exit_code.unwrap_or(1) != 0
                    || violation.is_some();
                let out = if failed {
                    ToolOutput::error(text)
                } else {
                    ToolOutput::ok(text)
                };
                Ok(out.with_metadata(serde_json::json!({
                    "exit_code": snap.exit_code,
                    "modified_files": report
                        .as_ref()
                        .map(|s| s.modified.clone())
                        .unwrap_or_default(),
                    "workspace_snapshot": report.as_ref().map(|s| s.snapshot.0.clone()),
                    "next_cursor": next_cursor,
                    "log_remaining": pending,
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

    #[tokio::test]
    async fn explicit_reader_does_not_consume_default_readers_log() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let start = RunCommandTool::new(commands)
            .execute(
                serde_json::json!({"program":"sh", "args":["-c", "printf 'independent-reader\\n'"], "background":true}),
                ctx.clone(), CancellationToken::new(),
            ).await.unwrap();
        let id = start
            .content
            .lines()
            .find_map(|l| l.strip_prefix("task_id: "))
            .unwrap();
        reg.wait(id, Some(Duration::from_secs(5)), &CancellationToken::new())
            .await
            .unwrap();
        let wait = WaitTaskTool::new(reg);
        let explicit = wait
            .execute(
                serde_json::json!({"task_id":id,"cursor":0}),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let implicit = wait
            .execute(
                serde_json::json!({"task_id":id}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(explicit.content.contains("independent-reader"));
        assert!(
            implicit.content.contains("independent-reader"),
            "an independent reader stole the default reader's output: {}",
            implicit.content
        );
    }

    #[tokio::test]
    async fn wait_delivers_output_arriving_during_the_wait() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        // A filesystem handshake makes output arrive only AFTER wait started.
        let start = RunCommandTool::new(commands).execute(
            serde_json::json!({"program":"sh", "args":["-c", "while [ ! -f release ]; do sleep 0.02; done; printf 'new-output\\n'; sleep 30"], "background":true}),
            ctx.clone(), CancellationToken::new(),
        ).await.unwrap();
        let id = start
            .content
            .lines()
            .find_map(|l| l.strip_prefix("task_id: "))
            .unwrap()
            .to_string();
        let wait_tool = WaitTaskTool::new(reg.clone());
        let wait = wait_tool.execute(
            serde_json::json!({"task_id":id,"timeout_seconds":10}),
            ctx,
            CancellationToken::new(),
        );
        tokio::pin!(wait);
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(wait.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        let cancel = CancellationToken::new();
        let completion = reg.wait(&id, Some(Duration::from_secs(10)), &cancel);
        tokio::pin!(completion);
        std::fs::write(dir.path().join("release"), "go").unwrap();
        let result = tokio::time::timeout(Duration::from_secs(3), &mut wait).await;
        let completion_woke = std::future::poll_fn(|cx| {
            std::task::Poll::Ready(std::future::Future::poll(completion.as_mut(), cx).is_ready())
        })
        .await;
        let _ = reg.kill(&id).await;
        let output = result
            .expect("new output must wake wait before process exit or the polling interval")
            .unwrap();
        assert!(output.content.contains("new-output"), "{}", output.content);
        assert!(
            output.content.contains("status: running"),
            "{}",
            output.content
        );
        assert!(
            !completion_woke,
            "output arrival must not masquerade as process completion"
        );
    }

    #[tokio::test]
    async fn repeated_wait_only_returns_new_log() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let start = RunCommandTool::new(commands)
            .execute(
                serde_json::json!({
                    "program": "sh",
                    "args": ["-c", "printf 'once-only-marker\\n'; sleep 10"],
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
            .find_map(|line| line.strip_prefix("task_id: "))
            .expect("task id");
        let wait_tool = WaitTaskTool::new(reg.clone());
        let first = wait_tool
            .execute(
                serde_json::json!({"task_id": task_id, "timeout_seconds": 1}),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            first.content.contains("once-only-marker"),
            "{}",
            first.content
        );

        let second = wait_tool
            .execute(
                serde_json::json!({"task_id": task_id, "timeout_seconds": 1}),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            !second.content.contains("once-only-marker"),
            "{}",
            second.content
        );
        assert_eq!(second.content.lines().count(), 1, "{}", second.content);

        // Two independent readers may replay from their own explicit cursor
        // without consuming the shared convenience cursor.
        for _ in 0..2 {
            let independent = wait_tool
                .execute(
                    serde_json::json!({"task_id": task_id, "cursor": 0, "timeout_seconds": 1}),
                    ctx.clone(),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            assert!(
                independent.content.contains("once-only-marker"),
                "{}",
                independent.content
            );
            assert!(independent.metadata["next_cursor"].as_u64().unwrap() > 0);
        }
        let invalid = wait_tool
            .execute(
                serde_json::json!({"task_id": task_id, "cursor": u64::MAX}),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(invalid.is_error);
        assert!(invalid.content.contains("beyond"), "{}", invalid.content);

        let full = GetTaskTool::new(reg.clone())
            .execute(
                serde_json::json!({"task_id": task_id}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            full.content.contains("once-only-marker"),
            "{}",
            full.content
        );
        let _ = reg.kill(task_id).await;
    }

    #[tokio::test]
    async fn wait_keeps_large_log_for_later_calls_under_small_output_budget() {
        let dir = scratch_repo();
        let (mut ctx, reg, commands) = ctx_with_reg(dir.path());
        ctx.policy.tool_output_budget = crate::registry::MIN_TOOL_OUTPUT;
        let start = RunCommandTool::new(commands)
            .execute(
                serde_json::json!({
                    "program": "sh",
                    "args": ["-c", "printf '%020000dZ' 0"],
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
            .find_map(|line| line.strip_prefix("task_id: "))
            .expect("task id");
        let waiter = WaitTaskTool::new(reg);
        let mut zeroes = 0;
        let mut saw_end = false;
        for _ in 0..50 {
            let result = waiter
                .execute(
                    serde_json::json!({"task_id": task_id, "timeout_seconds": 1}),
                    ctx.clone(),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            assert!(!result.is_error, "{}", result.content);
            assert!(result.content.len() <= ctx.policy.tool_output_budget);
            let log = result
                .content
                .split("--- new log ---\n")
                .nth(1)
                .unwrap_or("")
                .split("\n[more log remains")
                .next()
                .unwrap();
            zeroes += log.bytes().filter(|byte| *byte == b'0').count();
            saw_end |= log.contains('Z');
            if result.metadata["log_remaining"] == false {
                break;
            }
        }
        assert_eq!(zeroes, 20_000);
        assert!(saw_end, "final byte was lost");
    }

    #[tokio::test]
    async fn large_settlement_diagnostics_do_not_displace_delivered_log() {
        let dir = scratch_repo();
        let (mut ctx, reg, commands) = ctx_with_reg(dir.path());
        ctx.policy.tool_output_budget = crate::registry::MIN_TOOL_OUTPUT;
        let ctx = ctx.with_command_write_constraints(Some(vec!["src".into()]), None, Vec::new());
        let start = RunCommandTool::new(commands)
            .execute(
                serde_json::json!({"program":"sh", "args":["-c", "for i in 1 2 3 4 5 6; do touch outside-$i-$(printf '%0190d' 0); done; printf '%02000dZ' 0"], "background":true}),
                ctx.clone(), CancellationToken::new(),
            ).await.unwrap();
        assert!(!start.is_error, "{}", start.content);
        let id = start
            .content
            .lines()
            .find_map(|l| l.strip_prefix("task_id: "))
            .unwrap();
        reg.wait(id, Some(Duration::from_secs(10)), &CancellationToken::new())
            .await
            .unwrap();
        let waiter = WaitTaskTool::new(reg);
        let mut delivered = String::new();
        for _ in 0..10 {
            let out = waiter
                .execute(
                    serde_json::json!({"task_id":id}),
                    ctx.clone(),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            assert!(out.is_error);
            assert!(
                out.content.len() <= ctx.policy.tool_output_budget,
                "settlement must not make the outer result cap discard consumed log: {}",
                out.content.len()
            );
            let log = out
                .content
                .split("--- new log ---\n")
                .nth(1)
                .unwrap_or("")
                .split("\n[")
                .next()
                .unwrap();
            delivered.push_str(log);
            if out.metadata["log_remaining"] == false {
                break;
            }
        }
        assert_eq!(delivered, format!("{}Z", "0".repeat(2000)));
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

        // Log observation can return before exit. This test asserts the
        // terminal result, so synchronize on completion, not output arrival.
        reg.wait(
            &task_id,
            Some(Duration::from_secs(10)),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
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
                constrained.clone(),
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
        let reread = WaitTaskTool::new(reg)
            .execute(
                serde_json::json!({"task_id": task_id, "cursor": 0}),
                constrained,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(reread.is_error, "reading must not consume terminal failure");
        assert!(reread.content.contains("outside allowed paths"));
        assert!(reread.metadata["workspace_snapshot"].is_null());
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
