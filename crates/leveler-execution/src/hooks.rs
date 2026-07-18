//! Local PreToolUse / PostToolUse hooks (SEC-8).
//!
//! Configured via YAML; hooks are external commands. Pre exit code 2 = hard deny.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

const HOOK_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_ARGS_JSON: usize = 8 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct HooksFile {
    #[serde(default)]
    pub hooks: HooksSection,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct HooksSection {
    #[serde(default)]
    pub pre_tool_use: Vec<HookCommand>,
    #[serde(default)]
    pub post_tool_use: Vec<HookCommand>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct HookCommand {
    /// argv[0], argv[1..]
    pub command: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct HookRunner {
    pre: Vec<HookCommand>,
    post: Vec<HookCommand>,
    cwd: PathBuf,
}

// Clone is derived above.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreHookResult {
    Allow,
    Deny(String),
}

impl HookRunner {
    pub fn empty(cwd: PathBuf) -> Self {
        Self {
            pre: Vec::new(),
            post: Vec::new(),
            cwd,
        }
    }

    pub fn from_file(file: HooksFile, cwd: PathBuf) -> Self {
        Self {
            pre: file.hooks.pre_tool_use,
            post: file.hooks.post_tool_use,
            cwd,
        }
    }

    pub fn load(global_home: &Path, repo_root: &Path) -> Self {
        let mut pre = Vec::new();
        let mut post = Vec::new();
        for path in [
            global_home.join("hooks.yaml"),
            repo_root.join(".leveler/hooks.yaml"),
        ] {
            if let Ok(raw) = std::fs::read_to_string(&path)
                && let Ok(file) = serde_yaml::from_str::<HooksFile>(&raw)
            {
                pre.extend(file.hooks.pre_tool_use);
                post.extend(file.hooks.post_tool_use);
            }
        }
        Self {
            pre,
            post,
            cwd: repo_root.to_path_buf(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.pre.is_empty() && self.post.is_empty()
    }

    pub async fn run_pre(
        &self,
        tool: &str,
        args_json: &str,
        cancellation: &CancellationToken,
    ) -> PreHookResult {
        for hook in &self.pre {
            match run_one(
                hook,
                "pre_tool_use",
                tool,
                args_json,
                &self.cwd,
                cancellation,
            )
            .await
            {
                Ok(0) => {}
                Ok(2) => {
                    return PreHookResult::Deny(format!(
                        "pre_tool_use hook denied tool `{tool}` (exit 2)"
                    ));
                }
                Ok(code) => {
                    return PreHookResult::Deny(format!(
                        "pre_tool_use hook failed for `{tool}` (exit {code})"
                    ));
                }
                Err(e) => {
                    return PreHookResult::Deny(format!(
                        "pre_tool_use hook error for `{tool}`: {e}"
                    ));
                }
            }
        }
        PreHookResult::Allow
    }

    pub async fn run_post(
        &self,
        tool: &str,
        args_json: &str,
        ok: bool,
        cancellation: &CancellationToken,
    ) {
        for hook in &self.post {
            // Post is observational — ignore failures.
            let _ = run_one(
                hook,
                if ok {
                    "post_tool_use"
                } else {
                    "post_tool_use_error"
                },
                tool,
                args_json,
                &self.cwd,
                cancellation,
            )
            .await;
        }
    }
}

async fn run_one(
    hook: &HookCommand,
    phase: &str,
    tool: &str,
    args_json: &str,
    cwd: &Path,
    cancellation: &CancellationToken,
) -> Result<i32, String> {
    if hook.command.is_empty() {
        return Err("empty hook command".into());
    }
    let program = &hook.command[0];
    let args = &hook.command[1..];
    let capped = if args_json.len() > MAX_ARGS_JSON {
        &args_json[..MAX_ARGS_JSON]
    } else {
        args_json
    };

    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env("LEVELER_HOOK", phase)
        .env("LEVELER_TOOL", tool)
        .env("LEVELER_TOOL_ARGS_JSON", capped);
    cmd.env_clear();
    for (name, value) in leveler_core::environment().vars_os() {
        if !name.to_str().is_some_and(crate::is_credential_env_name) {
            cmd.env(name, value);
        }
    }

    let mut child = cmd.spawn().map_err(|e| format!("spawn {program}: {e}"))?;
    let result = tokio::select! {
        biased;
        _ = cancellation.cancelled() => {
            let _ = child.kill().await;
            return Err("hook cancelled".into());
        }
        status = tokio::time::timeout(HOOK_TIMEOUT, child.wait()) => status,
    };
    match result {
        Ok(Ok(status)) => Ok(status.code().unwrap_or(1)),
        Ok(Err(e)) => Err(format!("wait: {e}")),
        Err(_) => {
            let _ = child.kill().await;
            Err("hook timed out".into())
        }
    }
}

pub fn load_hooks_file(path: &Path) -> Result<HooksFile, String> {
    if !path.is_file() {
        return Ok(HooksFile::default());
    }
    let raw = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    serde_yaml::from_str(&raw).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pre_exit_2_denies() {
        let dir = tempfile::tempdir().unwrap();
        // portable: use /bin/sh -c 'exit 2'
        let runner = HookRunner {
            pre: vec![HookCommand {
                command: vec!["/bin/sh".into(), "-c".into(), "exit 2".into()],
            }],
            post: vec![],
            cwd: dir.path().to_path_buf(),
        };
        let r = runner
            .run_pre("run_command", "{}", &CancellationToken::new())
            .await;
        assert!(matches!(r, PreHookResult::Deny(_)), "{r:?}");
    }

    #[tokio::test]
    async fn pre_exit_0_allows() {
        let dir = tempfile::tempdir().unwrap();
        let runner = HookRunner {
            pre: vec![HookCommand {
                command: vec!["/bin/sh".into(), "-c".into(), "exit 0".into()],
            }],
            post: vec![],
            cwd: dir.path().to_path_buf(),
        };
        let r = runner
            .run_pre("run_command", "{}", &CancellationToken::new())
            .await;
        assert_eq!(r, PreHookResult::Allow);
    }
}
