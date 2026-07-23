//! get_task / wait_task / kill_task — manage background process tasks.

use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::{BackgroundTaskStatus, MutationBaseline, RiskLevel, WorkspaceSnapshot};

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

#[derive(Debug, Deserialize, JsonSchema)]
struct TaskIdInput {
    /// Task id returned by `run_command` with background=true.
    task_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WaitInput {
    task_id: String,
    /// Max seconds to wait (default 600).
    #[serde(default)]
    timeout_seconds: Option<u64>,
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

pub struct GetTaskTool;

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
        let Some(reg) = &context.background_tasks else {
            return Ok(ToolOutput::error("no background task registry"));
        };
        match reg.get(input.task_id.trim()).await {
            Some(snap) => Ok(ToolOutput::ok(format_snap(&snap))),
            None => Ok(ToolOutput::error(format!(
                "unknown task_id `{}`",
                input.task_id
            ))),
        }
    }
}

pub struct WaitTaskTool;

#[async_trait]
impl Tool for WaitTaskTool {
    fn name(&self) -> &'static str {
        "wait_task"
    }

    fn description(&self) -> &'static str {
        "Block until a background task exits (or timeout). Returns final status and log."
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<WaitInput>()
    }

    fn risk(&self) -> RiskLevel {
        // Not Safe: under a command_write_allowlist this tool can restore the
        // whole workspace to a snapshot (see account_background_mutations) —
        // Safe would auto-replay it on crash recovery and admit it into
        // read-only tool subsets.
        RiskLevel::WorkspaceWrite
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: WaitInput = super::parse_input(self.name(), input)?;
        let Some(reg) = &context.background_tasks else {
            return Ok(ToolOutput::error("no background task registry"));
        };
        let task_id = input.task_id.trim().to_string();
        let timeout = Duration::from_secs(input.timeout_seconds.unwrap_or(600).max(1));
        match reg.wait(&task_id, Some(timeout), &cancellation).await {
            Ok(snap) => {
                // PR-3b: account file diffs at wait end; auto-restore only when
                // command_write_allowlist is set (worker/node constrained).
                // Default Goal background (dev server/watcher): account only.
                let baseline = reg.take_mutation_baseline(&task_id).await;
                let (command_modified, mutation_error, snapshot_note) =
                    account_background_mutations(baseline.as_ref(), &context).await;

                let mut text = format_snap(&snap);
                if let Some(note) = snapshot_note {
                    text.push_str(&note);
                }
                if let Some(error) = &mutation_error {
                    text.push_str("\n[mutation rejected] ");
                    text.push_str(error);
                    text.push('\n');
                }

                let failed = snap.status != BackgroundTaskStatus::Exited
                    || snap.exit_code.unwrap_or(1) != 0
                    || mutation_error.is_some();
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
                    "modified_files": command_modified,
                    "workspace_snapshot": baseline.as_ref().map(|b| b.snapshot.0.clone()),
                })))
            }
            Err(e) => Ok(ToolOutput::error(e)),
        }
    }
}

/// Wait-end mutation accounting (PR-3b).
///
/// Always records `changed_since` when a baseline exists. Auto-restore runs
/// **only** when `command_write_allowlist` is set and a path falls outside it.
/// Budget alone does not restore (dev-server safety: K17).
async fn account_background_mutations(
    baseline: Option<&MutationBaseline>,
    context: &ToolContext,
) -> (Vec<String>, Option<String>, Option<String>) {
    let Some(baseline) = baseline else {
        return (Vec::new(), None, None);
    };
    let root = &baseline.workspace_root;
    let id = &baseline.snapshot;

    let mut command_modified = Vec::new();
    let mut snapshot_note = None;
    match WorkspaceSnapshot::changed_since(root, id).await {
        Ok(changed) => command_modified = changed,
        Err(error) => {
            snapshot_note = Some(format!(
                "\n[note] could not diff the workspace after this background task ({error}); \
                 its file changes were not tracked.\n"
            ));
        }
    }

    // Restore ONLY under allowlist constraint (design §2.4 / K17).
    let mut mutation_error = None;
    if let Some(allowlist) = context.command_write_allowlist.as_deref() {
        let outside: Vec<&str> = command_modified
            .iter()
            .map(String::as_str)
            .filter(|path| !allowlist.iter().any(|allowed| path_allows(allowed, path)))
            .collect();
        if !outside.is_empty() {
            let violation = format!(
                "background task modified files outside allowed paths: {}",
                outside.join(", ")
            );
            match WorkspaceSnapshot::restore(root, id).await {
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

    (command_modified, mutation_error, snapshot_note)
}

fn path_allows(allowed: &str, modified: &str) -> bool {
    let allowed = allowed.trim_end_matches('/');
    modified == allowed || modified.starts_with(&format!("{allowed}/"))
}

pub struct KillTaskTool;

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
        let Some(reg) = &context.background_tasks else {
            return Ok(ToolOutput::error("no background task registry"));
        };
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
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn wait_task_is_workspace_write_risk() {
        // wait_task can roll the whole workspace back to a snapshot when a
        // background task violates its write allowlist — that is a mutation,
        // not a Safe read (and Safe implies auto-replay on crash recovery).
        assert_eq!(WaitTaskTool.risk(), RiskLevel::WorkspaceWrite);
        // get_task stays a pure status read.
        assert_eq!(GetTaskTool.risk(), RiskLevel::Safe);
    }

    async fn sh_in(dir: &std::path::Path, script: &str) {
        let out = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .current_dir(dir)
            .output()
            .await
            .unwrap();
        assert!(
            out.status.success(),
            "{script} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn ctx_with_reg(dir: &std::path::Path) -> (ToolContext, Arc<BackgroundTaskRegistry>) {
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
        let ctx = ToolContext::with_environment(ws, PermissionProfile::Assisted, environment)
            .with_background_tasks(reg.clone());
        (ctx, reg)
    }

    #[tokio::test]
    async fn wait_accounts_modified_files_without_restoring_when_no_allowlist() {
        // Default Goal background (dev server / watcher): account diffs, never
        // auto-restore intentional long-lived mutations (K17 / PR-3b).
        let dir = tempfile::tempdir().unwrap();
        sh_in(
            dir.path(),
            "git init -q && git config user.email t@t && git config user.name t \
             && echo original > keep.txt && git add -A && git commit -qm init",
        )
        .await;

        let (ctx, _reg) = ctx_with_reg(dir.path());
        let start = RunCommandTool
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

        let wait = WaitTaskTool
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
        let dir = tempfile::tempdir().unwrap();
        sh_in(
            dir.path(),
            "git init -q && git config user.email t@t && git config user.name t \
             && mkdir src && echo original > src/lib.rs && git add -A && git commit -qm init",
        )
        .await;

        let (ctx, _reg) = ctx_with_reg(dir.path());
        let constrained =
            ctx.with_command_write_constraints(Some(vec!["src".to_string()]), None, Vec::new());

        let start = RunCommandTool
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

        let wait = WaitTaskTool
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
        let dir = tempfile::tempdir().unwrap();
        sh_in(
            dir.path(),
            "git init -q && git config user.email t@t && git config user.name t \
             && mkdir src && echo original > src/lib.rs && git add -A && git commit -qm init",
        )
        .await;

        let (ctx, _reg) = ctx_with_reg(dir.path());
        let constrained =
            ctx.with_command_write_constraints(Some(vec!["src".to_string()]), None, Vec::new());

        let start = RunCommandTool
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

        let wait = WaitTaskTool
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
