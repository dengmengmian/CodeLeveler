//! The `Tool` trait and its supporting types (spec §18.1).

use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use leveler_context::{FileStateTracker, RepeatedReadGuard};
use leveler_execution::{
    Checkpoint, CommandRunner, PermissionProfile, ProcessError, RiskLevel, Workspace,
    WorkspaceError,
};

/// Shared, cheaply-cloneable context handed to every tool invocation,
/// organized by ownership:
///
/// - [`ExecutionResources`] — process-wide execution and write-safety
///   infrastructure (shared `Arc`s, one instance for the whole run including
///   sub-agents).
/// - [`ToolPolicy`] — gates and budgets: the part that varies per engine,
///   per turn, or per invocation. The only security-loosening switches live
///   here behind named grant methods.
/// - [`ToolServices`] — optional long-lived capabilities specific tools use
///   (LSP, artifacts, memory, background tasks).
///
/// ANTI-GROWTH RULE: a new top-level field is not allowed. A new need must
/// name its lifecycle and owner and join the matching facet — or justify, in
/// review, why none of the three fits. Future extension-provided services and
/// secret providers belong in [`ToolServices`]; a future remote executor is
/// an [`ExecutionResources`] resource.
#[derive(Clone)]
pub struct ToolContext {
    pub execution: ExecutionResources,
    pub policy: ToolPolicy,
    pub services: ToolServices,
    /// Stable identity of the session this context serves, used to isolate
    /// per-session capability state (e.g. browser pages/refs — §18). `None` for
    /// non-session contexts (eval, one-shot CLI); those share a default scope.
    pub session_scope: Option<Arc<str>>,
}

/// Process-wide execution and write-safety infrastructure. Every handle is an
/// `Arc` shared across turns and sub-agents: mutating interior state (the
/// checkpoint, file fingerprints, the command gate) is globally visible by
/// design.
#[derive(Clone)]
pub struct ExecutionResources {
    pub workspace: Arc<Workspace>,
    pub runner: Arc<CommandRunner>,
    pub environment: Arc<leveler_core::EnvSnapshot>,
    /// Captures original file content before the first write, for rollback.
    pub checkpoint: Arc<Checkpoint>,
    /// Detects wasteful repeated reads of the same file range.
    pub read_guard: Arc<RepeatedReadGuard>,
    /// Fingerprints files as they were last read, so `apply_patch` can refuse
    /// to overwrite a file something else changed in the meantime.
    pub file_state: Arc<FileStateTracker>,
    /// Serializes workspace-wide commands across concurrent sub-agents.
    ///
    /// Parallel workers own disjoint *files*, but they share one working tree
    /// and one build directory. Without this, agent A's `cargo test` can run
    /// against source that agent B is halfway through rewriting, and the
    /// result looks authoritative while being meaningless. `ToolContext` is
    /// cloned into every child, so the `Arc` makes one gate for the whole run.
    pub command_gate: Arc<tokio::sync::Mutex<()>>,
}

/// Gates and budgets — the scope-varying part of the context. Per-engine
/// (`mode`, `read_only`), per-turn (`tool_output_budget`, network grant), and
/// per-invocation (write constraints, fs grant) knobs live together because
/// they share one change reason: what is this execution allowed to do.
#[derive(Clone)]
pub struct ToolPolicy {
    pub mode: PermissionProfile,
    /// Collaboration-plan / `leveler plan` read-only overlay: only Safe tools.
    /// Orthogonal to the three-tier [`PermissionProfile`].
    pub read_only: bool,
    /// Deny network access to `run_command` processes (OS sandbox). PRIVATE:
    /// loosened only through [`Self::grant_network`], so the elevation surface
    /// stays auditable in one place.
    deny_network: bool,
    /// Turn-scoped grant from `request_permissions`: drop write_root
    /// confinement for `run_command` / `shell_command`. PRIVATE: granted only
    /// through [`Self::grant_unrestricted_fs`].
    turn_unrestricted_fs: bool,
    /// Extra env var names scrubbed from `run_command` children (the
    /// configured providers' `api_key_env` names).
    pub deny_env: Arc<Vec<String>>,
    /// Run the language formatter (gofmt/rustfmt/…) after each edit. Off by
    /// default so unit tests exercise edit logic in isolation; the real agent
    /// path turns it on.
    pub auto_format: bool,
    /// Max files a single `apply_patch` may touch (0 = unlimited).
    pub max_files_per_step: usize,
    /// Paths a command may modify. `None` means unrestricted.
    pub command_write_allowlist: Option<Arc<Vec<String>>>,
    /// How many additional files a command may modify. `None` means unlimited.
    pub command_modified_files_remaining: Option<usize>,
    /// Files already counted against the run budget before this command.
    pub command_previously_modified: Arc<Vec<String>>,
    /// Per-model byte budget for a single tool result (the central cap applied
    /// after every tool call). Defaults to [`crate::registry::MAX_TOOL_OUTPUT`];
    /// weaker models with small reliable contexts may configure less.
    pub tool_output_budget: usize,
}

impl ToolPolicy {
    /// Whether `run_command` children run with network access denied.
    pub fn network_denied(&self) -> bool {
        self.deny_network
    }

    /// Whether this scope holds an unrestricted-filesystem grant.
    pub fn unrestricted_fs(&self) -> bool {
        self.turn_unrestricted_fs
    }

    /// User-approved network grant (from `request_permissions`). The ONLY way
    /// to clear the network denial after construction.
    pub fn grant_network(&mut self) {
        self.deny_network = false;
    }

    /// User-approved filesystem elevation (from `request_permissions` or a
    /// post-approval host escape). The ONLY way to set the grant.
    pub fn grant_unrestricted_fs(&mut self) {
        self.turn_unrestricted_fs = true;
    }

    /// Whether `path` (workspace-relative) is outside the live write allowlist.
    /// `None` allowlist = unrestricted. An empty allowlist is zero authority:
    /// every path is denied (late-bound child before `claim_write_scope`).
    pub fn write_path_denied(&self, path: &str) -> Option<String> {
        let allow = self.command_write_allowlist.as_deref()?;
        let path = path.trim().trim_start_matches("./").trim_end_matches('/');
        if allow.iter().any(|a| {
            let a = a.trim_end_matches('/');
            !a.is_empty() && (path == a || path.starts_with(&format!("{a}/")))
        }) {
            return None;
        }
        Some(if allow.is_empty() {
            format!(
                "Edit rejected: no write scope is currently owned. Read the relevant \
                 code, then use claim_write_scope(paths) before modifying files \
                 (denied: {path})."
            )
        } else {
            format!(
                "Edit rejected: {path} is outside your claimed scope ({}). Claim it \
                 with claim_write_scope first, or stay within your scope.",
                allow.join(", ")
            )
        })
    }

    /// Zero claimed paths and not a structurally read-only overlay: a command
    /// that can mutate the workspace must not run (git-after-the-fact cannot
    /// see empty-directory removals).
    pub fn has_zero_write_authority(&self) -> bool {
        self.command_write_allowlist
            .as_deref()
            .is_some_and(|allow| allow.is_empty())
            && !self.read_only
    }
}

/// Optional long-lived capabilities specific tools use. A future
/// extension-provided service registers here, not as a new ToolContext field.
#[derive(Clone)]
pub struct ToolServices {
    /// Long-lived language-server sessions, keyed by language, reused across
    /// tool calls so servers index the workspace once (spec §26 LSP platform).
    pub lsp_sessions:
        Arc<tokio::sync::Mutex<std::collections::HashMap<String, Arc<leveler_lsp::LspClient>>>>,
    /// Per-language startup locks. Starting a server may take seconds; these
    /// prevent duplicate starts without holding the global sessions mutex.
    pub lsp_start_locks:
        Arc<tokio::sync::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    /// Where oversized command output is spilled (content-addressed) instead of
    /// being silently truncated. `None` disables spilling (output is truncated
    /// with a marker, the pre-artifact behavior).
    pub artifact_store: Option<Arc<leveler_execution::ArtifactStore>>,
    /// Durable project memory root (`Layout::memory_dir`). When set, memory
    /// tools read/write here; when None, tools error clearly (no silent env).
    pub memory_root: Option<std::path::PathBuf>,
    /// Background process task registry (run_command background=true).
    pub background_tasks: Option<std::sync::Arc<leveler_execution::BackgroundTaskRegistry>>,
    /// Daemon-owned browser runtime, shared across turns so the browser (and its
    /// isolated project profile) survives client disconnect. `None` disables the
    /// browser tools (they error clearly). See `leveler_browser::BrowserRuntime`.
    pub browser: Option<std::sync::Arc<leveler_browser::BrowserRuntime>>,
}

impl ToolContext {
    pub fn new(workspace: Workspace, mode: PermissionProfile) -> Self {
        let env = std::sync::Arc::new(leveler_core::EnvSnapshot::new(
            std::env::vars_os(),
            std::env::current_dir().unwrap_or_default(),
            std::env::temp_dir(),
        ));
        Self::with_environment(workspace, mode, env.clone())
    }

    pub fn with_environment(
        workspace: Workspace,
        mode: PermissionProfile,
        environment: Arc<leveler_core::EnvSnapshot>,
    ) -> Self {
        Self {
            execution: ExecutionResources {
                workspace: Arc::new(workspace),
                runner: Arc::new(CommandRunner::with_environment(environment.clone())),
                environment,
                checkpoint: Arc::new(Checkpoint::new()),
                read_guard: Arc::new(RepeatedReadGuard::default()),
                file_state: Arc::new(FileStateTracker::default()),
                command_gate: Arc::new(tokio::sync::Mutex::new(())),
            },
            policy: ToolPolicy {
                mode,
                read_only: false,
                deny_network: false,
                turn_unrestricted_fs: false,
                deny_env: Arc::new(Vec::new()),
                auto_format: false,
                max_files_per_step: 0,
                command_write_allowlist: None,
                command_modified_files_remaining: None,
                command_previously_modified: Arc::new(Vec::new()),
                tool_output_budget: crate::registry::MAX_TOOL_OUTPUT,
            },
            services: ToolServices {
                lsp_sessions: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
                lsp_start_locks: Arc::new(
                    tokio::sync::Mutex::new(std::collections::HashMap::new()),
                ),
                artifact_store: None,
                memory_root: None,
                background_tasks: None,
                browser: None,
            },
            session_scope: None,
        }
    }

    /// Bind this context to a session identity (browser page/ref isolation).
    pub fn with_session_scope(mut self, scope: impl Into<Arc<str>>) -> Self {
        self.session_scope = Some(scope.into());
        self
    }

    /// The session scope for capability isolation, defaulting to a shared scope
    /// when this context has no session identity.
    pub fn session_scope(&self) -> &str {
        self.session_scope.as_deref().unwrap_or("default")
    }

    /// Share a background task registry across tool calls.
    pub fn with_background_tasks(
        mut self,
        registry: std::sync::Arc<leveler_execution::BackgroundTaskRegistry>,
    ) -> Self {
        self.services.background_tasks = Some(registry);
        self
    }

    /// Share the daemon-owned browser runtime across tool calls.
    pub fn with_browser(
        mut self,
        runtime: std::sync::Arc<leveler_browser::BrowserRuntime>,
    ) -> Self {
        self.services.browser = Some(runtime);
        self
    }

    /// Force Safe-only tools (collaboration plan / read-only planning).
    pub fn with_read_only(mut self, on: bool) -> Self {
        self.policy.read_only = on;
        self
    }

    /// Project memory store directory (active/ + archive/).
    pub fn with_memory_root(mut self, root: impl Into<std::path::PathBuf>) -> Self {
        self.services.memory_root = Some(root.into());
        self
    }

    /// Spill oversized command output to `store` (content-addressed) instead of
    /// truncating it, so the full output stays retrievable.
    pub fn with_artifact_store(mut self, store: Arc<leveler_execution::ArtifactStore>) -> Self {
        self.services.artifact_store = Some(store);
        self
    }

    /// Enable network sandboxing for `run_command` processes.
    pub fn with_sandbox(mut self, deny_network: bool) -> Self {
        self.policy.deny_network = deny_network;
        self
    }

    /// Enable post-edit auto-formatting (real agent path; off in unit tests).
    pub fn with_auto_format(mut self, on: bool) -> Self {
        self.policy.auto_format = on;
        self
    }

    /// Env var names to scrub from `run_command` children, on top of the
    /// built-in denylist and secret-suffix patterns.
    pub fn with_deny_env(mut self, names: Vec<String>) -> Self {
        self.policy.deny_env = Arc::new(names);
        self
    }

    /// Constrain command-driven workspace mutations. Violations are rolled
    /// back to the pre-command snapshot by `run_command`.
    pub fn with_command_write_constraints(
        mut self,
        allowlist: Option<Vec<String>>,
        modified_files_remaining: Option<usize>,
        previously_modified: Vec<String>,
    ) -> Self {
        self.policy.command_write_allowlist = allowlist.map(Arc::new);
        self.policy.command_modified_files_remaining = modified_files_remaining;
        self.policy.command_previously_modified = Arc::new(previously_modified);
        self
    }

    /// Apply model-policy limits (spec §17): the per-step file cap and whether
    /// the repeated-read guard is active (weaker models get tighter bounds).
    pub fn with_policy_limits(
        mut self,
        max_files_per_step: usize,
        repeated_read_guard: bool,
    ) -> Self {
        self.policy.max_files_per_step = max_files_per_step;
        // A disabled guard uses an effectively-infinite threshold.
        let threshold = if repeated_read_guard { 3 } else { u32::MAX };
        self.execution.read_guard = Arc::new(RepeatedReadGuard::new(threshold));
        self
    }
}

/// The result of a tool invocation. `is_error` tells the agent loop to present
/// this to the model as a failure it should react to (vs. infrastructure errors,
/// which surface as [`ToolError`]).
#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    pub metadata: serde_json::Value,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            metadata: serde_json::Value::Null,
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            metadata: serde_json::Value::Null,
        }
    }

    pub fn with_metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = metadata;
        self
    }
}

/// Infrastructure-level tool errors (as opposed to model-visible failures, which
/// are returned as [`ToolOutput`] with `is_error = true`).
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("unknown tool `{0}`")]
    NotFound(String),
    #[error("invalid arguments for `{tool}`: {message}")]
    InvalidArguments { tool: String, message: String },
    #[error("tool `{tool}` is not permitted in {mode:?} mode (risk {risk:?})")]
    NotPermitted {
        tool: String,
        mode: PermissionProfile,
        risk: RiskLevel,
    },
    #[error("workspace error: {0}")]
    Workspace(#[from] WorkspaceError),
    #[error("process error: {0}")]
    Process(#[from] ProcessError),
    #[error("io error: {0}")]
    Io(String),
}

/// A tool the model can call. Implementations must be stateless (or internally
/// synchronized) since one instance is shared across the process.
#[async_trait]
pub trait Tool: Send + Sync {
    /// The stable tool name exposed to the model. Borrowed from the tool
    /// instance so runtime-discovered tools (MCP, future extensions) can own
    /// their metadata; built-ins keep returning `&'static str` literals.
    fn name(&self) -> &str;

    /// A concise description for the model. Same ownership contract as
    /// [`Self::name`].
    fn description(&self) -> &str;

    /// The JSON Schema for this tool's arguments.
    fn input_schema(&self) -> serde_json::Value;

    /// Repair a narrow, known-compatible argument shape before schema
    /// validation. The normalized value is still validated against
    /// [`Self::input_schema`]; implementations must not use this to weaken the
    /// tool's argument contract.
    fn normalize_input(&self, input: serde_json::Value) -> serde_json::Value {
        input
    }

    /// The risk class of this tool.
    fn risk(&self) -> RiskLevel;

    /// Whether this tool is a pure, read-only lookup that is safe to run
    /// concurrently with other parallel-safe tools requested in the same round.
    /// Defaults to `false` (serialized). Any tool with side effects — edits,
    /// commands, plan updates, checkpoints — MUST leave this `false` so the
    /// executor keeps running it in order.
    fn supports_parallel(&self) -> bool {
        false
    }

    /// Whether this tool edits workspace files through its own arguments (a
    /// patch, a replacement), as opposed to a command that happens to write.
    ///
    /// The agent loop reads this instead of matching on tool names, so it
    /// charges the per-step file budget, records a mutation on the evidence
    /// ledger, and re-fingerprints touched files for any such tool — including
    /// ones registered outside this crate.
    fn mutates_files(&self) -> bool {
        false
    }

    /// Whether this tool executes a shell command.
    ///
    /// The agent loop reads this instead of matching on tool names, so command
    /// budgets, verification detection, and the failing-command stagnation
    /// signal apply to any command-executing tool.
    fn runs_command(&self) -> bool {
        false
    }

    /// Whether re-running this call after a crash, with the same arguments, is
    /// guaranteed to have no external effect beyond what the first (possibly
    /// completed) attempt already had.
    ///
    /// **Defaults to `false`, and that default is the safe one.** Crash
    /// recovery may auto-replay a tool ONLY when this is `true`; everything
    /// else stops for human reconciliation.
    ///
    /// This is deliberately NOT derived from [`Self::risk`]. `RiskLevel::Safe`
    /// answers "does this need approval", and it admits side effects:
    /// `create_checkpoint` resets the rollback baseline and `wait_task` can
    /// restore a workspace snapshot, yet both are Safe. Replaying either after
    /// a crash would silently undo work. Only a tool that reads and returns —
    /// with no write, no process, no external call — may set this.
    fn replay_is_side_effect_free(&self) -> bool {
        false
    }

    /// Execute the tool with already-schema-validated arguments.
    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError>;
}
