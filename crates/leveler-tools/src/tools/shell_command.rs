//! `shell_command` — shell string execution.
//!
//! The tool accepts a single `cmd` string and maps it onto `sh -c` / `cmd /C`
//! via the shared process runner, keeping the same sandbox, scrub, and snapshot
//! policy as `run_command`.
//!
//! Hang-prone model patterns are refused up front (see
//! [`super::shell_guard`]) so a bad agent command becomes a recoverable tool
//! error instead of trapping the turn for minutes.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::RiskLevel;

use super::run_command::execute_program;
use super::shell_guard::refuse_shell_script;
use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

#[derive(Debug, Deserialize, JsonSchema)]
struct Input {
    /// Shell command string.
    cmd: String,
    /// Working directory relative to the workspace root. Defaults to ".".
    /// Accepts `workdir` as an alias for `cwd`.
    #[serde(default, alias = "workdir")]
    cwd: Option<String>,
    /// Timeout in seconds. Defaults to 120.
    #[serde(default)]
    timeout_seconds: Option<u64>,
}

pub struct ShellCommandTool;

#[async_trait]
impl Tool for ShellCommandTool {
    fn name(&self) -> &'static str {
        "shell_command"
    }

    fn description(&self) -> &'static str {
        "Run a shell command string in the workspace. Prefer \
         this when you have a full command line (e.g. `git pull --rebase`, \
         `cargo test -q`). For structured argv without a shell, use `run_command`. \
         Same sandbox as `run_command`: broad reads, writes confined to the \
         workspace (plus temp/toolchain caches) unless full-access. \
         Default timeout 120s. Do NOT start long-lived servers here with `&` or \
         nohup — they are refused and hang the turn if forced. Use `run_command` \
         with background=true for dev servers, then curl/get_task in a separate call. \
         Do not put real commands after `#` comments."
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
        let cmd = input.cmd.trim();
        if cmd.is_empty() {
            return Ok(ToolOutput::error("cmd must not be empty"));
        }
        if let Some(reason) = refuse_shell_script(cmd) {
            return Ok(ToolOutput::error(reason));
        }
        let (program, args) = shell_invocation(cmd);
        execute_program(
            &program,
            args,
            input.cwd.as_deref(),
            input.timeout_seconds,
            context,
            cancellation,
        )
        .await
    }
}

fn shell_invocation(cmd: &str) -> (String, Vec<String>) {
    #[cfg(windows)]
    {
        ("cmd".into(), vec!["/C".into(), cmd.to_string()])
    }
    #[cfg(not(windows))]
    {
        ("sh".into(), vec!["-c".into(), cmd.to_string()])
    }
}

#[cfg(test)]
mod tests {
    use super::super::shell_guard::HANG_ANTI_PATTERN;
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn shell_invocation_uses_platform_shell() {
        let (program, args) = shell_invocation("echo hi");
        #[cfg(windows)]
        {
            assert_eq!(program, "cmd");
            assert_eq!(args, vec!["/C".to_string(), "echo hi".to_string()]);
        }
        #[cfg(not(windows))]
        {
            assert_eq!(program, "sh");
            assert_eq!(args, vec!["-c".to_string(), "echo hi".to_string()]);
        }
    }

    #[tokio::test]
    async fn shell_command_runs_echo() {
        let dir =
            std::env::temp_dir().join(format!("leveler-shell-{}", super::super::test_ordinal()));
        std::fs::create_dir_all(&dir).unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted);
        let out = ShellCommandTool
            .execute(
                serde_json::json!({"cmd": "echo shell-ok"}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{out:?}");
        assert!(out.content.contains("shell-ok"), "{out:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn hang_anti_pattern_errors_in_under_100ms_without_spawn() {
        let dir =
            std::env::temp_dir().join(format!("leveler-shell-{}", super::super::test_ordinal()));
        std::fs::create_dir_all(&dir).unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted);
        let start = Instant::now();
        let out = ShellCommandTool
            .execute(
                serde_json::json!({ "cmd": HANG_ANTI_PATTERN }),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let elapsed = start.elapsed();
        assert!(out.is_error, "must not spawn: {out:?}");
        assert!(out.content.contains("background=true"), "{out:?}");
        assert!(
            elapsed < Duration::from_millis(100),
            "anti-pattern must fail closed immediately, took {elapsed:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
