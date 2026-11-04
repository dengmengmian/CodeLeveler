//! `shell_command` — shell string execution.
//!
//! The tool accepts a single `cmd` string and maps it onto `sh -c` / `cmd /C`
//! via the shared process runner, keeping the same sandbox, scrub, and snapshot
//! policy as `run_command`.
//!
//! Hang-prone model patterns are refused up front (see
//! [`super::shell_guard`]) so a bad agent command becomes a recoverable tool
//! error instead of trapping the turn for minutes.

use std::sync::Arc;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::{BackgroundTaskLifetime, RiskLevel};

use super::command_execution::CommandExecution;
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
    /// Execution deadline in seconds. A foreground call waits for the command
    /// until it exits or this deadline ends it; the deadline is counted from
    /// process start. A deadline up to the 120-second foreground ceiling ends
    /// the command in this call. A longer deadline is waited for up to that
    /// ceiling, then follows the process after it moves to the background.
    /// Leave it unset for an ordinary command: the call waits 15 seconds, then
    /// moves a still-running command to the background without killing it.
    #[serde(default)]
    timeout_seconds: Option<u64>,
    /// When true, start the command in the background immediately and return a
    /// task_id. Use get_task / wait_task / kill_task to manage it. This is the
    /// explicit way to start a long-lived service (`make up`, a dev server, a
    /// watcher): a background task has no execution deadline, so it is not
    /// killed by this tool's wait budget.
    #[serde(default)]
    background: Option<bool>,
    /// Cleanup boundary for a background command. Defaults to `goal`, which
    /// stops it when the current goal finishes. Use `runtime`/`session`/
    /// `persistent` only when the user explicitly asks a service to remain
    /// running after task completion.
    #[serde(default)]
    background_lifetime: Option<super::run_command::BackgroundLifetimeInput>,
}

pub struct ShellCommandTool {
    commands: Arc<CommandExecution>,
}

impl ShellCommandTool {
    pub fn new(commands: Arc<CommandExecution>) -> Self {
        Self { commands }
    }
}

#[async_trait]
impl Tool for ShellCommandTool {
    fn name(&self) -> &'static str {
        "shell_command"
    }

    fn description(&self) -> &'static str {
        "Run one shell command string (`cmd`) in the workspace. `run_command` \
         is the argv form and does not start a shell. The result starts with \
         `exit: N`; `exit: 0` means the command succeeded. A pipe such as \
         `grep` or `tail` replaces that exit code with the pipe's. Temporary \
         files are writable in the workspace, in `$TMPDIR`, and under the \
         system `/tmp`; other host paths are not writable. Reads of system and toolchain paths \
         are allowed. Writes outside the workspace, other user directories, \
         and readonly roots require approval outside full-access mode. \
         A foreground command with no `timeout_seconds` still running after 15 \
         seconds is moved to the background and this tool returns a task id \
         while the process keeps running. That is not success, and the move \
         does not kill it. Set `timeout_seconds` to wait for a command: a \
         deadline up to the 120-second foreground ceiling ends it in this call; \
         a longer deadline is waited for up to that ceiling, then the \
         still-running process moves to the background and the deadline \
         continues to follow it. Set `background` \
         true to start a long-lived service in the background immediately. \
         `background_lifetime` decides what stops it — `goal` (the default) stops \
         it when the goal finishes, so a service the user asked to keep running \
         needs `session` or `persistent`; a foreground command that is moved to the \
         background keeps the lifetime you set. `wait_task` reads the eventual \
         result and does not kill the task when its own bound expires. `&` and nohup require \
         approval outside full-access mode. A `#` comment does \
         not hide a following command from the shell."
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

    fn approval_reason(&self, input: &serde_json::Value, context: &ToolContext) -> Option<String> {
        if context.policy.unrestricted_execution() {
            return None;
        }
        let input: Input = serde_json::from_value(input.clone()).ok()?;
        refuse_shell_script(input.cmd.trim())
            .or_else(|| crate::workspace::cwd_approval_reason(context, input.cwd.as_deref()))
    }

    async fn command_grant_request(
        &self,
        input: &serde_json::Value,
        context: &ToolContext,
    ) -> Result<Option<leveler_core::GrantRequest>, String> {
        if input
            .get("env")
            .is_some_and(|v| v.as_object().is_none_or(|vars| !vars.is_empty()))
        {
            return Ok(None);
        }
        let input: Input = serde_json::from_value(input.clone()).map_err(|e| e.to_string())?;
        let root = context
            .execution
            .workspace
            .as_ref()
            .ok_or("no project resource is attached")?
            .root();
        let cwd = input
            .cwd
            .as_deref()
            .map(std::path::PathBuf::from)
            .map(|p| if p.is_absolute() { p } else { root.join(p) })
            .unwrap_or_else(|| root.to_path_buf());
        let (program, args) = leveler_execution::shell_invocation(input.cmd.trim());
        leveler_execution::resolve_git_grant_with_environment(
            &program,
            &args,
            &cwd,
            &context.execution.environment,
        )
        .await
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
        if !context.policy.unrestricted_execution()
            && let Some(reason) = refuse_shell_script(cmd)
        {
            return Ok(ToolOutput::error(reason));
        }
        // A foreground command can still be moved to the background after the
        // handoff threshold, and when that happens it keeps the boundary the
        // caller chose. Rejecting the field here used to leave the caller no way
        // to say "keep this service" about a command it had not yet classified.
        let (program, args) = leveler_execution::shell_invocation(cmd);
        if input.background.unwrap_or(false) {
            let lifetime: BackgroundTaskLifetime = input
                .background_lifetime
                .map(BackgroundTaskLifetime::from)
                .unwrap_or(BackgroundTaskLifetime::Goal);
            return self
                .commands
                .start_background(&program, args, input.cwd.as_deref(), lifetime, context)
                .await;
        }
        self.commands
            .run_foreground_with_lifetime(
                &program,
                args,
                input.cwd.as_deref(),
                input.timeout_seconds,
                input.background_lifetime.map(BackgroundTaskLifetime::from),
                context,
                cancellation,
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::super::shell_guard::HANG_ANTI_PATTERN;
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn external_cwd_is_an_approval_reason_for_both_command_tools() {
        let (ctx, dir) =
            super::super::test_ctx(leveler_execution::PermissionProfile::Assisted, &[]);
        let outside = tempfile::tempdir().unwrap();
        assert!(
            ShellCommandTool::new(crate::tools::test_commands())
                .approval_reason(&serde_json::json!({"cmd":"pwd","cwd":outside.path()}), &ctx)
                .is_some()
        );
        assert!(
            super::super::run_command::RunCommandTool::new(crate::tools::test_commands())
                .approval_reason(
                    &serde_json::json!({"program":"pwd","cwd":outside.path()}),
                    &ctx
                )
                .is_some()
        );
        // Best-effort teardown: on Windows the still-open workspace root
        // owns the directory, exactly as its siblings in this module do.
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn auto_credential_commands_request_approval_before_execution() {
        let (ctx, dir) =
            super::super::test_ctx(leveler_execution::PermissionProfile::Assisted, &[]);
        let shell = ShellCommandTool::new(crate::tools::test_commands());
        let argv = super::super::run_command::RunCommandTool::new(crate::tools::test_commands());
        let shell_args = serde_json::json!({"cmd":"cat .env"});
        let argv_args = serde_json::json!({"program":"cat","args":[".env"]});
        assert!(shell.approval_reason(&shell_args, &ctx).is_some());
        assert!(argv.approval_reason(&argv_args, &ctx).is_some());
        assert!(
            shell
                .execute(shell_args, ctx.clone(), CancellationToken::new())
                .await
                .unwrap()
                .is_error
        );
        assert!(
            argv.execute(argv_args, ctx, CancellationToken::new())
                .await
                .unwrap()
                .is_error
        );
        // Best-effort teardown: on Windows the still-open workspace root
        // owns the directory, exactly as its siblings in this module do.
        std::fs::remove_dir_all(dir).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unrestricted_calls_read_credentials_through_both_command_tools() {
        use leveler_execution::{
            AuthorizationEvidence, NetworkScope, PermissionProfile, ResolvedExecutionPolicy,
            WriteScope,
        };
        let (full, dir) = super::super::test_ctx(PermissionProfile::FullAccess, &[]);
        let outside = tempfile::tempdir().unwrap();
        let path = outside.path().join(".env");
        std::fs::write(&path, "credential-fixture-visible\n").unwrap();
        let mut contexts = vec![full];
        let (auto, auto_dir) = super::super::test_ctx(PermissionProfile::Assisted, &[]);
        contexts.push(auto.with_resolved_policy(ResolvedExecutionPolicy {
            write: WriteScope::Unrestricted,
            network_scope: NetworkScope::Internet,
            authorization: AuthorizationEvidence::ApprovedOnce,
        }));
        for ctx in contexts {
            let shell = ShellCommandTool::new(crate::tools::test_commands())
                .execute(
                    serde_json::json!({"cmd":format!("cat '{}'", path.display())}),
                    ctx.clone(),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            assert!(
                !shell.is_error && shell.content.contains("credential-fixture-visible"),
                "{}",
                shell.content
            );
            let argv =
                super::super::run_command::RunCommandTool::new(crate::tools::test_commands())
                    .execute(
                        serde_json::json!({"program":"cat", "args":[path]}),
                        ctx,
                        CancellationToken::new(),
                    )
                    .await
                    .unwrap();
            assert!(
                !argv.is_error && argv.content.contains("credential-fixture-visible"),
                "{}",
                argv.content
            );
        }
        // Best-effort teardown: on Windows the still-open workspace root
        // owns the directory, exactly as its siblings in this module do.
        std::fs::remove_dir_all(dir).ok();
        std::fs::remove_dir_all(auto_dir).unwrap();
    }

    #[test]
    fn shell_invocation_uses_platform_shell() {
        let (program, args) = leveler_execution::shell_invocation("echo hi");
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

    /// SH-E2 RC-1: the shell tool description must carry the same
    /// verification exit-code contract as `run_command` so the model stops
    /// piping verification through grep and stops writing to the sandbox-
    /// blocked `/tmp`.
    #[test]
    fn description_states_the_verification_exit_code_contract() {
        let tool = ShellCommandTool::new(crate::tools::test_commands());
        let description = tool.description();

        assert!(description.contains("exit: 0"));
        assert!(description.contains("replaces that exit code"));
        assert!(description.contains("$TMPDIR"));
        assert!(description.contains("/tmp"));
    }

    #[tokio::test]
    async fn shell_command_runs_echo() {
        let dir =
            std::env::temp_dir().join(format!("leveler-shell-{}", super::super::test_ordinal()));
        std::fs::create_dir_all(&dir).unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted);
        let out = ShellCommandTool::new(crate::tools::test_commands())
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
        let out = ShellCommandTool::new(crate::tools::test_commands())
            .execute(
                serde_json::json!({ "cmd": HANG_ANTI_PATTERN }),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let elapsed = start.elapsed();
        assert!(out.is_error, "must not spawn: {out:?}");
        // Unix refuses via the job-control (`&`) guard (`background=true`
        // guidance); Windows refuses the same string via the `#`-comment guard.
        #[cfg(not(windows))]
        assert!(out.content.contains("job-control"), "{out:?}");
        #[cfg(windows)]
        assert!(out.content.contains("comment"), "{out:?}");
        assert!(
            elapsed < Duration::from_millis(100),
            "anti-pattern must fail closed immediately, took {elapsed:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
    /// A shell command is the most common way a project starts its services
    /// (`make up`, a dev server). It must have the same explicit hosting path as
    /// the argv tool: a task id immediately, no 15s block, no execution
    /// deadline, and a lifetime the caller chose.
    #[cfg(unix)]
    #[tokio::test]
    async fn background_true_hosts_a_shell_service_without_waiting() {
        let dir =
            std::env::temp_dir().join(format!("leveler-shell-bg-{}", super::super::test_ordinal()));
        std::fs::create_dir_all(&dir).unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted);
        let environment = std::sync::Arc::new(leveler_core::EnvSnapshot::new(
            std::env::vars_os(),
            std::env::current_dir().unwrap_or_default(),
            std::env::temp_dir(),
        ));
        let registry = std::sync::Arc::new(
            leveler_execution::BackgroundTaskRegistry::with_environment(environment),
        );
        let tool = ShellCommandTool::new(std::sync::Arc::new(CommandExecution::new(
            registry.clone(),
            None,
        )));
        let start = Instant::now();
        let out = tool
            .execute(
                serde_json::json!({"cmd": "sleep 20", "background": true}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{out:?}");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "an explicit background command must not wait for the handoff: {:?}",
            start.elapsed()
        );
        let task_id = out.metadata["task_id"]
            .as_str()
            .expect("the task identity is a structured fact")
            .to_string();
        assert!(out.content.contains(&task_id), "{out:?}");
        assert!(out.content.contains("lifetime: goal"), "{out:?}");
        registry
            .kill(&task_id)
            .await
            .expect("stop the service explicitly");
        std::fs::remove_dir_all(dir).ok();
    }
}
