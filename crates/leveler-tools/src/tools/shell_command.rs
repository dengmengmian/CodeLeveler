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

use leveler_execution::RiskLevel;

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
    /// Timeout in seconds. Defaults to 120.
    #[serde(default)]
    timeout_seconds: Option<u64>,
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
         files written to the system `/tmp` are not writable in the sandbox; \
         the workspace and `$TMPDIR` are. Reads of system and toolchain paths \
         are allowed. Writes outside the workspace, other user directories, \
         and readonly roots require approval outside full-access mode. \
         Default timeout 120s. `&` and nohup require approval outside full-access mode. A `#` comment does \
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
        let (program, args) = leveler_execution::shell_invocation(cmd);
        self.commands
            .run_foreground(
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
        std::fs::remove_dir_all(dir).unwrap();
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
        std::fs::remove_dir_all(dir).unwrap();
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
        std::fs::remove_dir_all(dir).unwrap();
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
}
