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

/// Default wait bound. `wait_task` waits for the task to reach a terminal
/// status, and this is the ceiling on how long one call blocks. It is long on
/// purpose: a bounded probe that returned "still running" every few seconds
/// made one model round per interval, so a single long command cost dozens of
/// rounds. A caller that wants a short probe passes `timeout_seconds`.
const WAIT_DEFAULT_SECS: u64 = 600;
/// Hard cap. A caller may ask for more; the tool still returns the running
/// status rather than owning the AgentLoop for longer than this.
const WAIT_MAX_SECS: u64 = 600;

#[derive(Debug, Deserialize, JsonSchema)]
struct WaitInput {
    task_id: String,
    /// Optional log cursor for an independent reader. Pass next_cursor from
    /// the prior result; omit to use the task's default incremental cursor.
    #[serde(default)]
    cursor: Option<u64>,
    /// Seconds to wait for the task to finish before returning its current
    /// status (default 600, max 600). Expiry does not cancel the background
    /// task, and a caller wanting a short probe passes a small value here.
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
        context: ToolContext,
        _cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: TaskIdInput = super::parse_input(self.name(), input)?;
        let reg = &self.tasks;
        match reg
            .get_owned(input.task_id.trim(), context.session_scope())
            .await
        {
            Ok(snap) => Ok(ToolOutput::ok(format_snap(&snap))),
            Err(error) => Ok(ToolOutput::error(error)),
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
        "Wait for a background task started with run_command(background=true) to finish. \
         Returns as soon as the task reaches a terminal status (exited/killed), when the \
         bounded interval expires, or when the run is cancelled. \
         Returns bounded new log and next_cursor; pass cursor to keep an independent \
         reader position. Without cursor, repeated calls use a shared incremental position. \
         timeout_seconds defaults to 600s (max 600s); expiry returns the current running \
         status without cancelling the task. get_task reads the full retained log."
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
        // Wait for a TERMINAL status transition, not for the next log line: one
        // `wait_task` costs one model round, so returning while the task is
        // still running turns a long command into a poll loop. `wait` registers
        // its wakeup before re-checking the status and is cancelled by the
        // parent token, so this blocks without spinning and without owning the
        // task. Only once the wait has resolved is the log delta read.
        if let Err(e) = reg
            .wait_owned(
                &task_id,
                context.session_scope(),
                Some(timeout),
                &cancellation,
            )
            .await
        {
            return Ok(ToolOutput::error(e));
        }
        let wait_result = reg
            .observe_owned(
                &task_id,
                context.session_scope(),
                input.cursor,
                log_budget,
                std::time::Duration::ZERO,
                &cancellation,
            )
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
                // entire process group exited and its admitted namespace was diffed.
                // This reads that result — it does not produce it.
                let settlement = observation.settlement;
                let report = match reg
                    .take_settlement_owned(&task_id, context.session_scope())
                    .await
                {
                    Ok(report) => report,
                    Err(error) => return Ok(ToolOutput::error(error)),
                };

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
                    "workspace_snapshot": report.as_ref().and_then(|s| s.snapshot.as_ref()).map(|id| id.0.clone()),
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
        context: ToolContext,
        _cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: TaskIdInput = super::parse_input(self.name(), input)?;
        let reg = &self.tasks;
        match reg
            .kill_owned(input.task_id.trim(), context.session_scope())
            .await
        {
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
        // Waiting observes a previously admitted write process and consumes
        // its settlement exactly once; it is not an auto-replayable read.
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

    /// `wait_task` waits for a TERMINAL status transition, not for output. A
    /// chatty long command used to return control on every log line, which cost
    /// one model round per line; the wait must stay pending while the task is
    /// still running and deliver the accumulated output once it exits.
    #[tokio::test]
    async fn wait_does_not_return_on_output_while_the_task_still_runs() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let start = RunCommandTool::new(commands).execute(
            serde_json::json!({"program":"sh", "args":["-c", "printf 'early-output\\n'; sleep 2; printf 'final-output\\n'"], "background":true}),
            ctx.clone(), CancellationToken::new(),
        ).await.unwrap();
        let id = start
            .content
            .lines()
            .find_map(|l| l.strip_prefix("task_id: "))
            .unwrap()
            .to_string();
        let waiter = WaitTaskTool::new(reg.clone());
        let wait = waiter.execute(
            serde_json::json!({"task_id":id}),
            ctx,
            CancellationToken::new(),
        );
        tokio::pin!(wait);
        // The task prints immediately and keeps running. One second in, the
        // wait must still be pending: output arrival alone must not satisfy it,
        // or the model is handed a "still running" result to poll again.
        assert!(
            tokio::time::timeout(Duration::from_millis(1000), &mut wait)
                .await
                .is_err(),
            "wait_task returned while the task was still running"
        );
        let output = tokio::time::timeout(Duration::from_secs(10), &mut wait)
            .await
            .expect("terminal status must wake the wait")
            .unwrap();
        assert!(
            output.content.contains("status: exited"),
            "{}",
            output.content
        );
        assert!(
            output.content.contains("early-output"),
            "{}",
            output.content
        );
        assert!(
            output.content.contains("final-output"),
            "{}",
            output.content
        );
        assert_eq!(output.metadata["exit_code"], 0);
    }

    /// The `wait_task` contract, one test per terminal outcome. These are the
    /// cases the model must be able to rely on in ONE call: success and failure
    /// wake the wait from the process's own exit, cancellation wakes it from the
    /// parent's token, and expiry returns the running status without touching
    /// the task.
    #[tokio::test]
    async fn wait_task_success_returns_the_terminal_status() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let id = spawn_background(&commands, &ctx, "printf 'ok\\n'").await;
        let out = WaitTaskTool::new(reg)
            .execute(
                serde_json::json!({"task_id": id}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("status: exited"), "{}", out.content);
        assert_eq!(out.metadata["exit_code"], 0);
    }

    #[tokio::test]
    async fn wait_task_failure_returns_the_terminal_error() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let id = spawn_background(&commands, &ctx, "exit 7").await;
        let out = WaitTaskTool::new(reg)
            .execute(
                serde_json::json!({"task_id": id}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            out.is_error,
            "a non-zero exit must be an error: {}",
            out.content
        );
        assert_eq!(out.metadata["exit_code"], 7);
    }

    #[tokio::test]
    async fn wait_task_cancel_interrupts_the_wait_without_killing_the_task() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let id = spawn_background(&commands, &ctx, "sleep 30").await;
        let cancel = CancellationToken::new();
        let waiter = WaitTaskTool::new(reg.clone());
        let wait = waiter.execute(
            serde_json::json!({"task_id": id.clone()}),
            ctx,
            cancel.clone(),
        );
        tokio::pin!(wait);
        tokio::time::sleep(Duration::from_millis(200)).await;
        cancel.cancel();
        let out = tokio::time::timeout(Duration::from_secs(5), &mut wait)
            .await
            .expect("parent cancellation must interrupt wait_task")
            .unwrap();
        assert!(out.is_error, "{}", out.content);
        assert!(out.content.contains("cancelled"), "{}", out.content);
        let snap = reg.get(&id).await.expect("task retained");
        assert_eq!(
            snap.status,
            BackgroundTaskStatus::Running,
            "wait_task cancellation must not kill the task"
        );
        let _ = reg.kill(&id).await;
    }

    #[tokio::test]
    async fn wait_task_timeout_returns_running_without_killing() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let id = spawn_background(&commands, &ctx, "sleep 30").await;
        let out = WaitTaskTool::new(reg.clone())
            .execute(
                serde_json::json!({"task_id": id.clone(), "timeout_seconds": 1}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("status: running"), "{}", out.content);
        let snap = reg.get(&id).await.expect("task retained");
        assert_eq!(snap.status, BackgroundTaskStatus::Running);
        let _ = reg.kill(&id).await;
    }

    /// Spawn one background shell command into the shared registry and return
    /// its task id; the wait contract tests all start the same way.
    async fn spawn_background(
        commands: &Arc<crate::tools::CommandExecution>,
        ctx: &ToolContext,
        script: &str,
    ) -> String {
        let start = RunCommandTool::new(commands.clone())
            .execute(
                serde_json::json!({"program": "sh", "args": ["-c", script], "background": true}),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!start.is_error, "spawn: {}", start.content);
        start
            .content
            .lines()
            .find_map(|line| line.strip_prefix("task_id: "))
            .expect("task id")
            .to_string()
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
    async fn failed_scoped_commands_do_not_displace_delivered_log() {
        let dir = scratch_repo();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        let (mut ctx, reg, commands) = ctx_with_reg(dir.path());
        ctx.policy.tool_output_budget = crate::registry::MIN_TOOL_OUTPUT;
        let ctx = ctx.with_command_write_constraints(Some(vec!["src".into()]), None, Vec::new());
        let start = RunCommandTool::new(commands)
            .execute(
                serde_json::json!({"program":"sh", "args":["-c", "for i in 1 2 3 4 5 6; do touch outside-$i-$(printf '%0190d' 0) 2>/dev/null; done; printf '%02000dZ' 0; exit 1"], "background":true}),
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
        assert_eq!(delivered, format!("[stdout] {}Z", "0".repeat(2000)));
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
    async fn wait_preserves_owned_edits_and_reports_denied_foreign_write() {
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
                    "args": ["-c", "echo ok > src/lib.rs; echo bad > outside.txt"],
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
            wait.content.contains("Operation not permitted")
                || wait.content.contains("Permission denied")
                || wait.content.contains("Read-only file system"),
            "expected scope message: {}",
            wait.content
        );
        assert!(
            !dir.path().join("outside.txt").exists(),
            "out-of-scope create must never occur"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("src/lib.rs"))
                .unwrap()
                .trim(),
            "ok",
            "owned edits must survive a later denied write"
        );
        let modified = wait
            .metadata
            .get("modified_files")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        assert!(
            modified == vec![serde_json::json!("src/lib.rs")],
            "settlement contains only owned writes: {modified:?}"
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
        assert_ne!(reread.metadata["exit_code"], serde_json::json!(0));
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

    #[tokio::test]
    async fn execution_boundary_refuses_foreign_owned_file_before_mutation() {
        let dir = scratch_repo();
        std::fs::create_dir_all(dir.path().join("owned")).unwrap();
        std::fs::write(dir.path().join("foreign.txt"), "unchanged").unwrap();
        let (ctx, _, commands) = ctx_with_reg(dir.path());
        let ctx = ctx
            .with_command_write_constraints(Some(vec!["owned".into()]), None, Vec::new())
            .with_foreign_owned_paths(vec!["foreign.txt".into()]);
        let out = commands
            .run_foreground(
                "sh",
                vec!["-c".into(), "printf corrupted > foreign.txt".into()],
                None,
                Some(5),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            out.is_error,
            "unauthorized write must fail: {}",
            out.content
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("foreign.txt")).unwrap(),
            "unchanged"
        );
    }

    #[tokio::test]
    async fn task_tools_reject_another_session_and_allow_the_creator() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let owner = ctx.clone().with_session_scope("owner");
        let stranger = ctx.with_session_scope("stranger");
        let id = spawn_background(&commands, &owner, "sleep 30").await;
        let input = serde_json::json!({"task_id": id});
        let get = GetTaskTool::new(reg.clone())
            .execute(input.clone(), stranger.clone(), CancellationToken::new())
            .await
            .unwrap();
        let kill = KillTaskTool::new(reg.clone())
            .execute(input.clone(), stranger, CancellationToken::new())
            .await
            .unwrap();
        let still_running = reg.get(&id).await.unwrap().status;
        let _ = reg.kill(&id).await;
        assert!(get.is_error, "foreign task log must not be disclosed");
        assert!(kill.is_error, "foreign task must not be signalled");
        assert_eq!(still_running, BackgroundTaskStatus::Running);
        let own = GetTaskTool::new(reg)
            .execute(input, owner, CancellationToken::new())
            .await
            .unwrap();
        assert!(!own.is_error, "the same session may operate across turns");
    }

    #[tokio::test]
    async fn parent_excludes_foreign_paths_and_symlink_aliases_before_write() {
        let dir = scratch_repo();
        std::fs::create_dir_all(dir.path().join("owned")).unwrap();
        std::fs::create_dir_all(dir.path().join("foreign")).unwrap();
        std::fs::write(dir.path().join("foreign/keep"), "original").unwrap();
        std::os::unix::fs::symlink("../foreign", dir.path().join("owned/alias")).unwrap();
        let (ctx, _, commands) = ctx_with_reg(dir.path());
        let ctx = ctx.with_foreign_owned_paths(vec!["foreign".into()]);
        for path in ["foreign/keep", "owned/alias/keep"] {
            let out = commands
                .run_foreground(
                    "sh",
                    vec!["-c".into(), format!("printf changed > {path}")],
                    None,
                    Some(5),
                    ctx.clone(),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            assert!(out.is_error, "{path}: {}", out.content);
            assert_eq!(
                std::fs::read_to_string(dir.path().join("foreign/keep")).unwrap(),
                "original"
            );
        }
        let out = commands
            .run_foreground(
                "sh",
                vec!["-c".into(), "printf own > owned/new".into()],
                None,
                Some(5),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            out.metadata["modified_files"],
            serde_json::json!(["owned/new"])
        );
        assert!(
            out.metadata["workspace_snapshot"].is_null(),
            "a scoped accounting tree must never be exposed as a full rollback snapshot"
        );
    }

    #[tokio::test]
    async fn zero_write_scope_keeps_background_read_capability() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let ctx = ctx.with_command_write_constraints(Some(Vec::new()), None, Vec::new());
        let id = spawn_background(&commands, &ctx, "printf observed").await;
        let snap = reg
            .wait(&id, Some(Duration::from_secs(5)), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(snap.exit_code, Some(0));
        assert!(snap.log.contains("observed"));
    }

    #[tokio::test]
    async fn scoped_execution_rejects_hardlinks_to_other_owners() {
        let dir = scratch_repo();
        std::fs::create_dir(dir.path().join("owned")).unwrap();
        std::fs::write(dir.path().join("foreign"), "original").unwrap();
        std::fs::hard_link(dir.path().join("foreign"), dir.path().join("owned/alias")).unwrap();
        let (ctx, _, commands) = ctx_with_reg(dir.path());
        let ctx = ctx.with_command_write_constraints(Some(vec!["owned".into()]), None, Vec::new());
        let out = commands
            .run_foreground(
                "sh",
                vec!["-c".into(), "printf changed > owned/alias".into()],
                None,
                Some(5),
                ctx,
                CancellationToken::new(),
            )
            .await;
        assert_eq!(
            std::fs::read_to_string(dir.path().join("foreign")).unwrap(),
            "original",
            "an allowed path must not alias a foreign inode: {out:?}"
        );
        assert!(out.is_err() || out.unwrap().is_error);
    }

    #[tokio::test]
    async fn scoped_command_cannot_create_a_foreign_hardlink_then_write_it() {
        let dir = scratch_repo();
        std::fs::create_dir(dir.path().join("owned")).unwrap();
        std::fs::write(dir.path().join("foreign"), "original").unwrap();
        let (ctx, _, commands) = ctx_with_reg(dir.path());
        let ctx = ctx.with_command_write_constraints(Some(vec!["owned".into()]), None, Vec::new());
        let out = commands
            .run_foreground(
                "sh",
                vec![
                    "-c".into(),
                    "ln foreign owned/alias && printf changed > owned/alias".into(),
                ],
                None,
                Some(5),
                ctx,
                CancellationToken::new(),
            )
            .await
            .expect("the process must actually run under the sandbox");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("foreign")).unwrap(),
            "original",
            "runtime link creation must not acquire foreign write authority: {out:?}"
        );
        assert!(out.is_error, "the OS must reject the hardlink: {out:?}");
        assert_eq!(
            out.metadata["execution_status"], "completed",
            "spawn failure is not evidence of a denied link"
        );
        assert_ne!(out.metadata["exit_code"], 0);
        assert!(
            !dir.path().join("owned/alias").exists(),
            "the alias must never be created"
        );
        #[cfg(target_os = "macos")]
        assert!(
            out.content.contains("ln:") && out.content.contains("Operation not permitted"),
            "expected Seatbelt to deny link creation: {out:?}"
        );
    }

    #[tokio::test]
    async fn hardlinks_entirely_within_owned_scope_remain_writable() {
        let dir = scratch_repo();
        std::fs::create_dir(dir.path().join("owned")).unwrap();
        std::fs::write(dir.path().join("owned/source"), "original").unwrap();
        std::fs::hard_link(
            dir.path().join("owned/source"),
            dir.path().join("owned/alias"),
        )
        .unwrap();
        let (ctx, _, commands) = ctx_with_reg(dir.path());
        let ctx = ctx.with_command_write_constraints(Some(vec!["owned".into()]), None, Vec::new());
        let out = commands
            .run_foreground(
                "sh",
                vec!["-c".into(), "printf changed > owned/alias".into()],
                None,
                Some(5),
                ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(out.is_ok(), "all inode names are owned: {out:?}");
        assert!(!out.unwrap().is_error);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("owned/source")).unwrap(),
            "changed"
        );
    }
    #[tokio::test]
    async fn foreign_wait_cannot_consume_the_owners_log_or_settlement() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let owner = ctx.clone().with_session_scope("owner");
        let id = spawn_background(
            &commands,
            &owner,
            "printf private-log; printf changed > own.txt",
        )
        .await;
        reg.wait(&id, Some(Duration::from_secs(5)), &CancellationToken::new())
            .await
            .unwrap();
        let waiter = WaitTaskTool::new(reg);
        let args = serde_json::json!({"task_id":id});
        let foreign = waiter
            .execute(
                args.clone(),
                ctx.with_session_scope("foreign"),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(foreign.is_error);
        assert!(!foreign.content.contains("private-log"));
        let own = waiter
            .execute(args, owner, CancellationToken::new())
            .await
            .unwrap();
        assert!(own.content.contains("private-log"));
        assert_eq!(
            own.metadata["modified_files"],
            serde_json::json!(["own.txt"])
        );
    }
}
