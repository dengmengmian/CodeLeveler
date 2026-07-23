//! The `Tool` trait and its supporting types (spec §18.1).

use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use leveler_context::{FileStateTracker, RepeatedReadGuard};
use leveler_execution::{
    Checkpoint, CommandRunner, PermissionProfile, ProcessError, RiskLevel, Workspace,
    WorkspaceError,
};

/// Shared, cheaply-cloneable context handed to every tool invocation. Carries
/// the resolved execution limits that tighten how much a single
/// step may do — the mechanism that lets weaker models stay reliable.
#[derive(Clone)]
pub struct ToolContext {
    pub workspace: Arc<Workspace>,
    pub runner: Arc<CommandRunner>,
    pub environment: Arc<leveler_core::EnvSnapshot>,
    pub mode: PermissionProfile,
    /// Captures original file content before the first write, for rollback.
    pub checkpoint: Arc<Checkpoint>,
    /// Detects wasteful repeated reads of the same file range.
    pub read_guard: Arc<RepeatedReadGuard>,
    /// Fingerprints files as they were last read, so `apply_patch` can refuse to
    /// overwrite a file something else changed in the meantime.
    pub file_state: Arc<FileStateTracker>,
    /// Max files a single `apply_patch` may touch (0 = unlimited).
    pub max_files_per_step: usize,
    /// Deny network access to `run_command` processes (OS sandbox).
    pub deny_network: bool,
    /// Extra env var names scrubbed from `run_command` children (the
    /// configured providers' `api_key_env` names).
    pub deny_env: Arc<Vec<String>>,
    /// Run the language formatter (gofmt/rustfmt/…) after each edit. Off by
    /// default so unit tests exercise edit logic in isolation; the real agent
    /// path turns it on.
    pub auto_format: bool,
    /// Paths a command may modify. `None` means unrestricted.
    pub command_write_allowlist: Option<Arc<Vec<String>>>,
    /// How many additional files a command may modify. `None` means unlimited.
    pub command_modified_files_remaining: Option<usize>,
    /// Files already counted against the run budget before this command.
    pub command_previously_modified: Arc<Vec<String>>,
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
    /// Collaboration-plan / `leveler plan` read-only overlay: only Safe tools.
    /// Orthogonal to the three-tier [`PermissionProfile`].
    pub read_only: bool,
    /// Turn-scoped grant from `request_permissions`: drop write_root confinement
    /// for `run_command` / `shell_command` (elevated filesystem access).
    pub turn_unrestricted_fs: bool,
    /// Background process task registry (run_command background=true).
    pub background_tasks: Option<std::sync::Arc<leveler_execution::BackgroundTaskRegistry>>,
    /// Per-model byte budget for a single tool result (the central cap applied
    /// after every tool call). Defaults to [`crate::registry::MAX_TOOL_OUTPUT`];
    /// weaker models with small reliable contexts may configure less.
    pub tool_output_budget: usize,
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
            workspace: Arc::new(workspace),
            runner: Arc::new(CommandRunner::with_environment(environment.clone())),
            environment,
            mode,
            checkpoint: Arc::new(Checkpoint::new()),
            read_guard: Arc::new(RepeatedReadGuard::default()),
            file_state: Arc::new(FileStateTracker::default()),
            max_files_per_step: 0,
            deny_network: false,
            deny_env: Arc::new(Vec::new()),
            auto_format: false,
            command_write_allowlist: None,
            command_modified_files_remaining: None,
            command_previously_modified: Arc::new(Vec::new()),
            lsp_sessions: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            lsp_start_locks: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            artifact_store: None,
            memory_root: None,
            read_only: false,
            turn_unrestricted_fs: false,
            background_tasks: None,
            tool_output_budget: crate::registry::MAX_TOOL_OUTPUT,
        }
    }

    /// Share a background task registry across tool calls.
    pub fn with_background_tasks(
        mut self,
        registry: std::sync::Arc<leveler_execution::BackgroundTaskRegistry>,
    ) -> Self {
        self.background_tasks = Some(registry);
        self
    }

    /// Force Safe-only tools (collaboration plan / read-only planning).
    pub fn with_read_only(mut self, on: bool) -> Self {
        self.read_only = on;
        self
    }

    /// Turn-scoped OS write confinement off (after approved `request_permissions`).
    pub fn with_turn_unrestricted_fs(mut self, on: bool) -> Self {
        self.turn_unrestricted_fs = on;
        self
    }

    /// Project memory store directory (active/ + archive/).
    pub fn with_memory_root(mut self, root: impl Into<std::path::PathBuf>) -> Self {
        self.memory_root = Some(root.into());
        self
    }

    /// Spill oversized command output to `store` (content-addressed) instead of
    /// truncating it, so the full output stays retrievable.
    pub fn with_artifact_store(mut self, store: Arc<leveler_execution::ArtifactStore>) -> Self {
        self.artifact_store = Some(store);
        self
    }

    /// Enable network sandboxing for `run_command` processes.
    pub fn with_sandbox(mut self, deny_network: bool) -> Self {
        self.deny_network = deny_network;
        self
    }

    /// Enable post-edit auto-formatting (real agent path; off in unit tests).
    pub fn with_auto_format(mut self, on: bool) -> Self {
        self.auto_format = on;
        self
    }

    /// Env var names to scrub from `run_command` children, on top of the
    /// built-in denylist and secret-suffix patterns.
    pub fn with_deny_env(mut self, names: Vec<String>) -> Self {
        self.deny_env = Arc::new(names);
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
        self.command_write_allowlist = allowlist.map(Arc::new);
        self.command_modified_files_remaining = modified_files_remaining;
        self.command_previously_modified = Arc::new(previously_modified);
        self
    }

    /// Build a context sharing an existing checkpoint (so a whole session's
    /// writes accumulate into one rollback point).
    pub fn with_checkpoint(mut self, checkpoint: Arc<Checkpoint>) -> Self {
        self.checkpoint = checkpoint;
        self
    }

    /// Apply model-policy limits (spec §17): the per-step file cap and whether
    /// the repeated-read guard is active (weaker models get tighter bounds).
    pub fn with_policy_limits(
        mut self,
        max_files_per_step: usize,
        repeated_read_guard: bool,
    ) -> Self {
        self.max_files_per_step = max_files_per_step;
        // A disabled guard uses an effectively-infinite threshold.
        let threshold = if repeated_read_guard { 3 } else { u32::MAX };
        self.read_guard = Arc::new(RepeatedReadGuard::new(threshold));
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
    /// The stable tool name exposed to the model.
    fn name(&self) -> &'static str;

    /// A concise description for the model.
    fn description(&self) -> &'static str;

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

    /// Execute the tool with already-schema-validated arguments.
    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError>;
}
