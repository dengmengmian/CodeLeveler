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

/// The in-repo hooks file, relative to the repository root. Trust-gated.
pub const HOOKS_RELATIVE: &str = crate::trust::TRUSTED_PROJECT_FILES[0];

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
    /// Observational lifecycle hooks; see [`LifecycleEvent`]. One list for all
    /// of them — the hook reads `$LEVELER_HOOK` to tell which fired, so adding
    /// an event never needs a config migration.
    #[serde(default)]
    pub lifecycle: Vec<HookCommand>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct HookCommand {
    /// argv[0], argv[1..]
    pub command: Vec<String>,
}

pub use crate::trust::UntrustedConfig;

#[derive(Debug, Clone, Default)]
pub struct HookRunner {
    pre: Vec<HookCommand>,
    post: Vec<HookCommand>,
    lifecycle: Vec<HookCommand>,
    cwd: PathBuf,
    untrusted: Vec<UntrustedConfig>,
}

// Clone is derived above.

/// Lifecycle moments a hook can observe.
///
/// Distinct from `pre_tool_use`, which is a *gate* (exit 2 denies). These are
/// observational: they report that something happened so a project can react —
/// export a transcript before it is compacted, track a fleet of sub-agents,
/// audit refused actions — without us guessing what each project needs or
/// paying for it in the loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleEvent {
    SessionStart,
    SessionEnd,
    /// Fires before the transcript is folded; the payload carries the token
    /// count that triggered it. Compaction discards detail by design, so this
    /// is the only moment a project can keep something first.
    PreCompact,
    PostCompact,
    SubagentStart,
    SubagentStop,
    /// An action was refused — by policy, by a permission rule, or by the user.
    PermissionDenied,
}

impl LifecycleEvent {
    /// Stable wire name, seen by hooks as `$LEVELER_HOOK`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SessionStart => "session_start",
            Self::SessionEnd => "session_end",
            Self::PreCompact => "pre_compact",
            Self::PostCompact => "post_compact",
            Self::SubagentStart => "subagent_start",
            Self::SubagentStop => "subagent_stop",
            Self::PermissionDenied => "permission_denied",
        }
    }
}

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
            lifecycle: Vec::new(),
            cwd,
            untrusted: Vec::new(),
        }
    }

    /// In-repo hooks files that were present but skipped for lack of trust.
    pub fn untrusted(&self) -> &[UntrustedConfig] {
        &self.untrusted
    }

    /// Load global hooks unconditionally and in-repo hooks only when the user
    /// has trusted that repository's exact file contents.
    ///
    /// The global file is the user's own, outside any repository, so it needs
    /// no gate. `<repo>/.leveler/hooks.yaml` travels with a clone and runs
    /// before every tool call, so it does — see [`crate::trust`].
    pub fn load(global_home: &Path, repo_root: &Path) -> Self {
        let mut pre = Vec::new();
        let mut post = Vec::new();
        let mut lifecycle = Vec::new();
        let mut untrusted = Vec::new();

        let mut absorb = |raw: &str| {
            if let Ok(file) = serde_yaml::from_str::<HooksFile>(raw) {
                pre.extend(file.hooks.pre_tool_use);
                post.extend(file.hooks.post_tool_use);
                lifecycle.extend(file.hooks.lifecycle);
            }
        };

        if let Ok(raw) = std::fs::read_to_string(global_home.join("hooks.yaml")) {
            absorb(&raw);
        }
        match crate::trust::read_trusted_project_file(global_home, repo_root, HOOKS_RELATIVE) {
            crate::trust::TrustedRead::Absent => {}
            crate::trust::TrustedRead::Trusted(raw) => absorb(&raw),
            crate::trust::TrustedRead::Untrusted { path, digest } => {
                tracing::warn!(
                    path = %path.display(),
                    "ignoring untrusted in-repo hooks file; run `leveler trust` to enable it"
                );
                untrusted.push(UntrustedConfig { path, digest });
            }
        }

        Self {
            pre,
            post,
            lifecycle,
            cwd: repo_root.to_path_buf(),
            untrusted,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.pre.is_empty() && self.post.is_empty() && self.lifecycle.is_empty()
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

impl HookRunner {
    /// Fire the observational hooks for `event`.
    ///
    /// Returns nothing on purpose: a lifecycle hook reports, it does not decide.
    /// A failing or slow hook must never take the turn with it — that is what
    /// `pre_tool_use` is for.
    pub async fn run_lifecycle(
        &self,
        event: LifecycleEvent,
        payload_json: &str,
        cancellation: &CancellationToken,
    ) {
        for hook in &self.lifecycle {
            let _ = run_one(
                hook,
                event.as_str(),
                "",
                payload_json,
                &self.cwd,
                cancellation,
            )
            .await;
        }
    }

    /// Whether any lifecycle hook is configured — callers skip building a
    /// payload when nobody is listening.
    pub fn has_lifecycle(&self) -> bool {
        !self.lifecycle.is_empty()
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
        .kill_on_drop(true);
    // Clear FIRST, then populate: `env_clear` wipes everything set before it,
    // so setting the hook's own variables earlier left every hook running
    // blind — no event name, no tool, no arguments.
    cmd.env_clear();
    cmd.envs(leveler_core::scrubbed_environment());
    cmd.env("LEVELER_HOOK", phase)
        .env("LEVELER_TOOL", tool)
        .env("LEVELER_TOOL_ARGS_JSON", capped)
        // Lifecycle events carry a payload rather than tool arguments; the same
        // bytes under a name that does not lie about what they are.
        .env("LEVELER_HOOK_PAYLOAD", capped);

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

#[cfg(test)]
mod trust_gate_tests {
    use super::*;
    use tempfile::tempdir;

    const HOOKS: &str =
        "hooks:\n  pre_tool_use:\n    - command: [\"/bin/sh\", \"-c\", \"exit 0\"]\n";

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn in_repo_hooks_do_not_run_until_the_repository_is_trusted() {
        let home = tempdir().unwrap();
        let repo = tempdir().unwrap();
        write(&repo.path().join(".leveler/hooks.yaml"), HOOKS);

        // Cloning a repository must not be enough to execute what it ships.
        let runner = HookRunner::load(home.path(), repo.path());
        assert!(runner.is_empty(), "untrusted in-repo hooks must not load");
        assert_eq!(runner.untrusted().len(), 1, "the skip must be reportable");
        assert!(
            runner.untrusted()[0].path.ends_with(".leveler/hooks.yaml"),
            "{:?}",
            runner.untrusted()[0]
        );
    }

    #[test]
    fn in_repo_hooks_run_once_trusted() {
        let home = tempdir().unwrap();
        let repo = tempdir().unwrap();
        write(&repo.path().join(".leveler/hooks.yaml"), HOOKS);

        let mut store = crate::trust::TrustStore::load(home.path());
        store.trust(repo.path(), ".leveler/hooks.yaml", HOOKS.as_bytes());
        store.save().unwrap();

        let runner = HookRunner::load(home.path(), repo.path());
        assert!(!runner.is_empty(), "trusted in-repo hooks must load");
        assert!(runner.untrusted().is_empty());
    }

    #[test]
    fn editing_trusted_in_repo_hooks_stops_them_running() {
        let home = tempdir().unwrap();
        let repo = tempdir().unwrap();
        let path = repo.path().join(".leveler/hooks.yaml");
        write(&path, HOOKS);

        let mut store = crate::trust::TrustStore::load(home.path());
        store.trust(repo.path(), ".leveler/hooks.yaml", HOOKS.as_bytes());
        store.save().unwrap();

        write(
            &path,
            "hooks:\n  pre_tool_use:\n    - command: [\"/bin/sh\", \"-c\", \"id\"]\n",
        );
        let runner = HookRunner::load(home.path(), repo.path());
        assert!(
            runner.is_empty(),
            "edited hooks must fall back to untrusted"
        );
        assert_eq!(runner.untrusted().len(), 1);
    }

    #[test]
    fn global_hooks_need_no_trust() {
        let home = tempdir().unwrap();
        let repo = tempdir().unwrap();
        // The global file is the user's own, outside any repository.
        write(&home.path().join("hooks.yaml"), HOOKS);

        let runner = HookRunner::load(home.path(), repo.path());
        assert!(!runner.is_empty(), "global hooks must load unconditionally");
        assert!(runner.untrusted().is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hook fixture shell exiting with `code`: `/bin/sh` on Unix, `cmd` on
    /// Windows (`/bin/sh` does not exist there and would fail at spawn).
    pub(super) fn shell_exit(code: u32) -> Vec<String> {
        if cfg!(windows) {
            vec!["cmd".into(), "/c".into(), format!("exit {code}")]
        } else {
            vec!["/bin/sh".into(), "-c".into(), format!("exit {code}")]
        }
    }

    #[tokio::test]
    async fn pre_exit_2_denies() {
        let dir = tempfile::tempdir().unwrap();
        let runner = HookRunner {
            pre: vec![HookCommand {
                command: shell_exit(2),
            }],
            post: vec![],
            lifecycle: Vec::new(),
            cwd: dir.path().to_path_buf(),
            untrusted: Vec::new(),
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
                command: shell_exit(0),
            }],
            post: vec![],
            lifecycle: Vec::new(),
            cwd: dir.path().to_path_buf(),
            untrusted: Vec::new(),
        };
        let r = runner
            .run_pre("run_command", "{}", &CancellationToken::new())
            .await;
        assert_eq!(r, PreHookResult::Allow);
    }
}

#[cfg(test)]
mod lifecycle_hook_tests {
    use super::tests::shell_exit;
    use super::*;

    /// A hook fixture that records the event it saw into `path`. Unix-shaped
    /// (`/bin/sh`), like the two tests that use it.
    #[cfg(unix)]
    fn record_to(path: &Path) -> Vec<String> {
        let p = path.display().to_string();
        vec![
            "/bin/sh".into(),
            "-c".into(),
            format!("printf '%s|%s\\n' \"$LEVELER_HOOK\" \"$LEVELER_HOOK_PAYLOAD\" >> {p}"),
        ]
    }

    /// Compaction throws away detail by design. A hook here is what lets a user
    /// keep what matters (export it, push it to memory) without us guessing
    /// what "matters" means or paying for an extra model call on every compact.
    #[tokio::test]
    #[cfg(unix)]
    async fn a_compaction_hook_sees_the_event_and_its_payload() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("seen.txt");
        let runner = HookRunner {
            pre: vec![],
            post: vec![],
            lifecycle: vec![HookCommand {
                command: record_to(&log),
            }],
            cwd: dir.path().to_path_buf(),
            untrusted: Vec::new(),
        };
        runner
            .run_lifecycle(
                LifecycleEvent::PreCompact,
                r#"{"tokens":120000}"#,
                &CancellationToken::new(),
            )
            .await;
        let seen = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(seen.contains("pre_compact"), "{seen:?}");
        assert!(
            seen.contains("120000"),
            "payload must reach the hook: {seen:?}"
        );
    }

    /// Sub-agents run concurrently and silently; without an event here there is
    /// no way to observe what a fleet of them is doing.
    #[tokio::test]
    #[cfg(unix)]
    async fn subagent_lifecycle_events_are_distinguishable() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("seen.txt");
        let runner = HookRunner {
            pre: vec![],
            post: vec![],
            lifecycle: vec![HookCommand {
                command: record_to(&log),
            }],
            cwd: dir.path().to_path_buf(),
            untrusted: Vec::new(),
        };
        let token = CancellationToken::new();
        runner
            .run_lifecycle(LifecycleEvent::SubagentStart, "{}", &token)
            .await;
        runner
            .run_lifecycle(LifecycleEvent::SubagentStop, "{}", &token)
            .await;
        let seen = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(seen.contains("subagent_start"), "{seen:?}");
        assert!(seen.contains("subagent_stop"), "{seen:?}");
    }

    /// Lifecycle hooks are observational: a failing one must not take the turn
    /// with it, unlike `pre_tool_use` which is a gate.
    #[tokio::test]
    async fn a_failing_lifecycle_hook_is_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let runner = HookRunner {
            pre: vec![],
            post: vec![],
            lifecycle: vec![HookCommand {
                command: shell_exit(3),
            }],
            cwd: dir.path().to_path_buf(),
            untrusted: Vec::new(),
        };
        // Returns unit: there is no failure channel by construction.
        runner
            .run_lifecycle(
                LifecycleEvent::SessionStart,
                "{}",
                &CancellationToken::new(),
            )
            .await;
    }

    #[test]
    fn every_event_has_a_stable_wire_name() {
        for (event, name) in [
            (LifecycleEvent::SessionStart, "session_start"),
            (LifecycleEvent::SessionEnd, "session_end"),
            (LifecycleEvent::PreCompact, "pre_compact"),
            (LifecycleEvent::PostCompact, "post_compact"),
            (LifecycleEvent::SubagentStart, "subagent_start"),
            (LifecycleEvent::SubagentStop, "subagent_stop"),
            (LifecycleEvent::PermissionDenied, "permission_denied"),
        ] {
            assert_eq!(event.as_str(), name);
        }
    }
}
