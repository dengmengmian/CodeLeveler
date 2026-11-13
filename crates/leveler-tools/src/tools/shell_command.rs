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
         The `background`, `timeout_seconds`, and `background_lifetime` fields \
         describe background and deadline behavior. `&` and nohup require \
         approval outside full-access mode. A `#` comment does \
         not hide a following command from the shell."
    }

    fn input_schema(&self) -> serde_json::Value {
        let mut schema = super::schema_of::<Input>();
        // Only the argv-form keys are closed against: `program`, `args`, and
        // `command` name a different execution model, so carrying one beside
        // `cmd` is ambiguous and must be refused instead of silently ignored.
        // Other extra keys stay tolerated exactly as before — a descriptive
        // field such as `reason` (sent by a real model) changes nothing about
        // what runs, and the grant path reads `env` off the raw call.
        if let Some(object) = schema.as_object_mut() {
            object.insert(
                "not".to_string(),
                serde_json::json!({
                    "anyOf": [
                        {"required": ["program"]},
                        {"required": ["args"]},
                        {"required": ["command"]},
                    ]
                }),
            );
        }
        schema
    }

    /// Model-facing wording for the calls the schema gate refused.
    ///
    /// Every branch is a shape confusion observed in real sessions: the model
    /// wrote `command` for `cmd`, mixed the argv form (`program` / `args`)
    /// into the shell tool, or sent a `cmd` of the wrong type. Nothing is
    /// executed and no input is repaired into the other tool: a shell line and
    /// an argv call run under different execution and approval models, so
    /// silently re-routing either direction would change what the caller
    /// authorized.
    fn invalid_input_guidance(&self, input: &serde_json::Value) -> Option<String> {
        let cmd = input.get("cmd");
        let has_program = input.get("program").is_some();
        let has_args = input.get("args").is_some();
        match cmd {
            Some(serde_json::Value::String(_)) => {
                // `cmd` itself is well formed. Only a mix of the two call
                // forms is phrased here; any other violation keeps the raw
                // schema error, which already names the field and type.
                (has_program || has_args || input.get("command").is_some()).then(|| {
                    mixed_forms_guidance(has_program, has_args, input.get("command").is_some())
                })
            }
            Some(other) => Some(wrong_cmd_type_guidance(other)),
            None => {
                if let Some(command) = input.get("command").and_then(|v| v.as_str()) {
                    Some(command_field_guidance(command, has_program))
                } else if let Some(program) = input.get("program").and_then(|v| v.as_str()) {
                    Some(argv_form_guidance(program, has_args))
                } else if has_args {
                    Some(args_only_guidance())
                } else {
                    Some(missing_cmd_guidance())
                }
            }
        }
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

/// Cap caller-provided text quoted back in an example. The guidance names the
/// mistake; it does not need to repeat a whole paste.
fn capped(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() > 120 {
        let head: String = trimmed.chars().take(120).collect();
        format!("{head}\u{2026}")
    } else {
        trimmed.to_string()
    }
}

/// Render caller text as a JSON string literal so quotes, newlines, and
/// control characters cannot corrupt the example.
fn quoted(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"...\"".to_string())
}

fn missing_cmd_guidance() -> String {
    "shell_command requires `cmd` — one command-line string — and nothing was executed. \
     The call shape is {\"cmd\": \"go test ./...\"} or {\"cmd\": \"git status --short && \
     cargo test\"}. For one executable with an argument array, call run_command with \
     `program` and `args` instead."
        .to_string()
}

fn command_field_guidance(command: &str, has_program: bool) -> String {
    let mixed = if has_program {
        " The call also carried `program`, which belongs to run_command; the two call forms \
         are never combined."
    } else {
        ""
    };
    format!(
        "shell_command does not take a `command` field — nothing was executed.{mixed} The \
         field is `cmd`, one string: {{\"cmd\": {}}}. If you meant one executable plus its \
         arguments, call run_command with `program` and `args`.",
        quoted(&capped(command))
    )
}

fn argv_form_guidance(program: &str, has_args: bool) -> String {
    let args_note = if has_args { " and `args`" } else { "" };
    format!(
        "shell_command does not take `program`{args_note} — nothing was executed. Those \
         fields belong to run_command, which runs one executable under argv semantics: \
         {{\"program\": {}, \"args\": [...]}}. To run a whole command line through a shell, \
         call shell_command with `cmd`, e.g. {{\"cmd\": {}}}.",
        quoted(&capped(program)),
        quoted(&format!("{} ...", capped(program)))
    )
}

fn args_only_guidance() -> String {
    "shell_command does not take an `args` array — nothing was executed. It runs one \
     command line as the `cmd` string, e.g. {\"cmd\": \"go test ./...\"}. The argv form \
     {\"program\": \"go\", \"args\": [\"test\", \"./...\"]} belongs to run_command."
        .to_string()
}

fn wrong_cmd_type_guidance(value: &serde_json::Value) -> String {
    let kind = match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    };
    format!(
        "shell_command requires `cmd` to be a string, not {kind} — nothing was executed. \
         Send the whole command line as one string: {{\"cmd\": \"go test ./...\"}}."
    )
}

fn mixed_forms_guidance(has_program: bool, has_args: bool, has_command: bool) -> String {
    let mut fields = Vec::new();
    if has_program {
        fields.push("`program`");
    }
    if has_args {
        fields.push("`args`");
    }
    if has_command {
        fields.push("`command`");
    }
    format!(
        "shell_command was not executed: the call carried {} in addition to `cmd`. \
         shell_command runs one `cmd` string through a shell; run_command runs one \
         executable from `program` plus `args`. Send only `cmd` here — or only `program` + \
         `args` to run_command.",
        fields.join(" and ")
    )
}

#[cfg(test)]
mod tests {
    use super::super::shell_guard::HANG_ANTI_PATTERN;
    use super::*;
    use std::time::{Duration, Instant};

    /// Real-session shape: the model wrote `command` for `cmd`, sometimes
    /// together with the argv field `program`. The refusal must name the
    /// actual field, show the corrected shell call, and point the argv form
    /// at run_command. Nothing executes.
    #[test]
    fn a_command_field_is_refused_with_the_real_cmd_shape() {
        let tool = ShellCommandTool::new(crate::tools::test_commands());
        let guidance = tool
            .invalid_input_guidance(&serde_json::json!({
                "command": "cargo test -q",
                "program": "bash"
            }))
            .expect("a known shape confusion gets guidance");
        assert!(
            guidance.contains("does not take a `command` field"),
            "{guidance}"
        );
        assert!(
            guidance.contains(r#"{"cmd": "cargo test -q"}"#),
            "{guidance}"
        );
        assert!(guidance.contains("run_command"), "{guidance}");
        assert!(guidance.contains("nothing was executed"), "{guidance}");
        assert!(guidance.contains("`program`"), "{guidance}");
    }

    /// The argv form sent to the shell tool names where it belongs. The
    /// example is the caller's own program, not a guessed command.
    #[test]
    fn an_argv_call_is_refused_with_the_run_command_shape() {
        let tool = ShellCommandTool::new(crate::tools::test_commands());
        let guidance = tool
            .invalid_input_guidance(&serde_json::json!({
                "program": "bash",
                "args": ["-lc", "echo hi"]
            }))
            .expect("a known shape confusion gets guidance");
        assert!(
            guidance.contains("does not take `program` and `args`"),
            "{guidance}"
        );
        assert!(
            guidance.contains(r#"{"program": "bash", "args": [...]}"#),
            "{guidance}"
        );
        assert!(guidance.contains("nothing was executed"), "{guidance}");
        assert!(guidance.contains("`cmd`"), "{guidance}");

        let args_only = tool
            .invalid_input_guidance(&serde_json::json!({"args": ["go", "test", "./..."]}))
            .expect("an args-only call gets guidance");
        assert!(
            args_only.contains("does not take an `args` array"),
            "{args_only}"
        );
        assert!(args_only.contains("run_command"), "{args_only}");
    }

    /// A wrong-typed `cmd` is a type fact, not a command to guess at. Other
    /// invalid fields keep the raw schema error (None), which already names
    /// the field and type.
    #[test]
    fn a_wrong_typed_cmd_is_named_without_repair() {
        let tool = ShellCommandTool::new(crate::tools::test_commands());
        let guidance = tool
            .invalid_input_guidance(&serde_json::json!({"cmd": 42}))
            .expect("a wrong-typed cmd gets guidance");
        assert!(
            guidance.contains("requires `cmd` to be a string"),
            "{guidance}"
        );
        assert!(guidance.contains("a number"), "{guidance}");
        assert!(guidance.contains("nothing was executed"), "{guidance}");

        assert!(
            tool.invalid_input_guidance(&serde_json::json!({"cmd": "ls", "timeout_seconds": "5"}))
                .is_none(),
            "an unrelated schema violation keeps the raw error"
        );
        let missing = tool
            .invalid_input_guidance(&serde_json::json!({"timeout_seconds": 5}))
            .expect("a call without the required field gets guidance");
        assert!(missing.contains("requires `cmd`"), "{missing}");
        assert!(missing.contains("nothing was executed"), "{missing}");
        assert!(
            tool.invalid_input_guidance(&serde_json::json!({"cmd": "ls"}))
                .is_none(),
            "a valid call is never guided: this hook only words a rejection"
        );
    }

    /// Mixing the two call forms is ambiguous and must not execute. Only the
    /// argv-form keys are closed in the schema: a descriptive extra such as
    /// the `reason` a real session sent changes nothing about what runs and
    /// must stay tolerated.
    #[test]
    fn a_mixed_call_is_refused_while_descriptive_extras_are_tolerated() {
        let tool = ShellCommandTool::new(crate::tools::test_commands());
        let schema = tool.input_schema();
        assert!(
            schema.get("not").is_some(),
            "the argv-form keys must be refused beside `cmd`: {schema}"
        );
        assert_eq!(
            schema.get("additionalProperties"),
            None,
            "unrelated extras must not be closed: {schema}"
        );
        let guidance = tool
            .invalid_input_guidance(&serde_json::json!({
                "cmd": "cargo test",
                "program": "cargo",
                "args": ["test"]
            }))
            .expect("a mixed call gets guidance");
        assert!(guidance.contains("was not executed"), "{guidance}");
        assert!(guidance.contains("`program` and `args`"), "{guidance}");
        assert!(guidance.contains("Send only `cmd`"), "{guidance}");
    }

    /// The production gate: a rejected call never reaches execution. A marker
    /// file proves the command did not run, and the model receives the
    /// guidance instead of a bare schema error.
    #[tokio::test]
    async fn malformed_calls_are_refused_through_the_registry_without_executing() {
        let dir = std::env::temp_dir().join(format!(
            "leveler-shell-guid-{}",
            super::super::test_ordinal()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted);
        let mut registry = crate::ToolRegistry::new();
        registry.register(std::sync::Arc::new(ShellCommandTool::new(
            crate::tools::test_commands(),
        )));
        let marker = dir.join("must-not-exist");
        let marker_arg = marker.display().to_string();

        for (label, input) in [
            (
                "command-field",
                serde_json::json!({"command": format!("touch {}", marker.display())}),
            ),
            (
                "program-field",
                serde_json::json!({"program": "touch", "args": [marker_arg.clone()]}),
            ),
            (
                "mixed-fields",
                serde_json::json!({"cmd": format!("touch {}", marker.display()), "program": "bash"}),
            ),
            ("wrong-type", serde_json::json!({"cmd": 42})),
            ("missing-fields", serde_json::json!({"timeout_seconds": 5})),
        ] {
            let out = registry
                .execute(
                    "shell_command",
                    input,
                    ctx.clone(),
                    CancellationToken::new(),
                )
                .await
                .expect("a guidance result, not a schema rejection");
            assert!(out.is_error, "{label}: {out:?}");
            assert!(
                out.content.contains("nothing was executed")
                    || out.content.contains("was not executed"),
                "{label}: the refusal says nothing ran: {}",
                out.content
            );
            assert!(!marker.exists(), "{label}: the malformed call executed");
        }
        let out = registry
            .execute(
                "shell_command",
                serde_json::json!({"cmd": "echo shell-guidance-ok"}),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .expect("a valid shell call still executes");
        assert!(!out.is_error, "{out:?}");
        assert!(
            out.content.contains("shell-guidance-ok"),
            "the guidance must not change normal calls: {out:?}"
        );
        // Real-session shape: `reason` is descriptive, not a second call form.
        // The gate must not refuse a call that worked before the guidance.
        let out = registry
            .execute(
                "shell_command",
                serde_json::json!({"cmd": "echo shell-extra-ok", "reason": "run the recorded check"}),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .expect("a descriptive extra is tolerated");
        assert!(!out.is_error, "{out:?}");
        assert!(
            out.content.contains("shell-extra-ok"),
            "a descriptive extra must not block execution: {out:?}"
        );
        // `workdir` is the documented alias for `cwd`; the schema advertises
        // `cwd`, so a closed schema would refuse the alias the contract allows.
        let out = registry
            .execute(
                "shell_command",
                serde_json::json!({"cmd": "echo shell-alias-ok", "workdir": "."}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .expect("the documented alias is tolerated");
        assert!(!out.is_error, "{out:?}");
        assert!(
            out.content.contains("shell-alias-ok"),
            "the documented `workdir` alias must still execute: {out:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

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
