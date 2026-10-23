//! `leveler-app` — the composition root .
//!
//! Wires configuration into a running [`Application`]: loads provider/model/
//! bundles, resolves API keys from the environment, builds the
//! [`ProviderRegistry`], and opens the database. The CLI depends on this; this
//! crate depends on no CLI concerns.
#![forbid(unsafe_code)]

mod active_turns;
pub mod agents;
mod checkpoints;
mod children;
mod collaboration;
pub mod contribution_query;
pub mod doctor;
/// Engine events -> client events. Public so a recorded session can be replayed
/// through the same projection a live client received, instead of a second
/// hand-written dialect that drifts from it.
pub mod event_bridge;
pub mod global_config;
pub mod global_task_index;
pub mod goal_discovery;
mod goal_recap;
mod interactive;
mod live_view;
pub mod mcp_config;
pub mod memory_consolidator;
pub mod memory_status;
pub mod observability;
mod parallel;
mod prompt_assist;
mod prompt_bridge;
pub mod runtime_boot;
mod runtime_identity;
mod session;
pub mod session_history;
pub mod session_projection;
mod side_question;
pub mod skills;
mod user_shell;
mod vcs;
mod workspace_view;

pub use collaboration::CollaborationExecution;
pub use global_config::{GlobalConfig, GlobalConfigError, WebSearchSettings};
pub use interactive::InProcessRuntimeClient;
pub use interactive::ui_thinking_state;
pub use parallel::ParallelEditOutcome;
pub use runtime_identity::{RuntimeIdentityError, load_or_create_runtime_id};
pub use session::engine_event_to_agent;
pub use vcs::ShipOptions;

use std::sync::{Arc, OnceLock};

use leveler_agent::CollaborationMode;
use leveler_execution::{PermissionProfile, Workspace};
use leveler_model::{ModelRef, ModelRuntime};
use leveler_project::{Layout, layout::yaml_files};
use leveler_provider::{
    ModelConfigFile, ProviderConfig, ProviderRegistry, RegistryInputs, load_model_config,
    load_provider_config, resolve_api_key,
};
use leveler_storage::{Database, SessionRepository};
use leveler_tools::search::{
    DeepSeekSearchProvider, SearchProvider, SearchProviderKind, TavilySearchProvider,
};
use leveler_tools::{CapabilityPacks, ToolContext, model_surface};
use sha2::{Digest, Sha256};

/// Stable process-owner state shares the project's private global namespace,
/// independently of the replaceable runtime endpoint and build generation.
pub fn execution_host_config(
    layout: &Layout,
) -> Result<leveler_execution::execution_host::ExecutionHostConfig, AppError> {
    Ok(leveler_execution::execution_host::ExecutionHostConfig {
        state_dir: layout.state_dir.join("execution-host"),
        repo_root: layout
            .require_workspace()
            .map_err(|error| AppError::NotFound(error.to_string()))?
            .to_path_buf(),
        executable: std::env::current_exe().map_err(|source| AppError::Io {
            path: "current executable".to_string(),
            source,
        })?,
    })
}

/// Errors assembling the application.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("config error: {0}")]
    Config(#[from] leveler_provider::ConfigError),
    #[error("execution host error: {0}")]
    ExecutionHost(String),
    #[error("registry error: {0}")]
    Registry(#[from] leveler_provider::RegistryError),
    #[error("storage error: {0}")]
    Storage(#[from] leveler_storage::StorageError),
    // `ModelError` already renders as "model error [Kind]: …" — do not prepend
    // another "model error:" (that produced "model error: model error").
    #[error("{0}")]
    Model(#[from] leveler_model::ModelError),
    #[error("workspace error: {0}")]
    Workspace(#[from] leveler_execution::WorkspaceError),
    #[error("agent error: {0}")]
    Agent(#[from] leveler_agent::AgentError),
    #[error("vcs error: {0}")]
    Vcs(#[from] leveler_vcs::VcsError),
    #[error("verification failed: {0}")]
    VerificationFailed(String),
    #[error("engine error: {0}")]
    Engine(String),
    /// A truth-affecting child started but could not be durably settled. This
    /// is intentionally distinct from a terminal engine failure: clients must
    /// surface a recovery fault without inventing a terminal event.
    #[error("terminal evidence boundary is not closed: {0}")]
    UnclosedTerminalBoundary(String),
    /// The runtime reached the terminal authority, but its atomic commit did
    /// not land. The UI remains nonterminal until recovery can settle it.
    #[error("task terminal was not committed: {0}")]
    TerminalCommitFailed(String),
    #[error("serialization error: {0}")]
    Serde(String),
    #[error("global config error: {0}")]
    GlobalConfig(String),
    #[error("cannot fingerprint runtime config: {0}")]
    ConfigFingerprint(String),
    #[error("not found: {0}")]
    NotFound(String),
    /// A product-level axis combination the execution entry cannot honor.
    /// Refused before any provider call, session row, or worktree exists, so
    /// the caller gets the contract error instead of a half-started run.
    #[error("{0}")]
    UnsupportedCombination(String),
    #[error("io error creating {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("runtime identity error: {0}")]
    RuntimeIdentity(#[from] runtime_identity::RuntimeIdentityError),
    /// Another live CodeLeveler process is executing this session. Nothing
    /// was taken from it.
    #[error(
        "这个会话正在另一个 CodeLeveler 进程中运行。请等待当前任务结束，或回到正在运行该任务的窗口。"
    )]
    SessionRunningElsewhere,
    /// Whether another process is still executing this session cannot be
    /// established, so nothing was taken over.
    #[error("无法确认这个会话是否仍在另一个 CodeLeveler 进程中运行，未做任何更改。")]
    SessionOwnershipUnknown,
}

/// The loaded configuration bundle (kept around for `config show` / `doctor`).
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub providers: Vec<ProviderConfig>,
    pub models: Vec<ModelConfigFile>,
    /// Default model from the global config (`~/.leveler/config.toml`), if set.
    pub default_model: Option<String>,
    /// TUI language from global config (`zh` / `en`), if set.
    pub lang: Option<String>,
    /// `[develop].model`, verbatim. Resolved (and refused) only by `/develop`
    /// itself, so a bad value cannot break a turn that never asked for it.
    pub develop_model: Option<String>,
    /// Whether CodeLeveler-authored commits include a model-aware co-author trailer.
    pub vcs_co_author: bool,
    /// External MCP servers to expose to the model.
    pub mcp_servers: Vec<leveler_tools::mcp::McpServerConfig>,
    /// Global multi-agent kill-switch default (project config can override).
    pub agents_delegation: bool,
    /// Whether the harness launches an independent reviewer (default Off).
    pub agents_independent_review: leveler_project::IndependentReview,
    /// `[browser].default`: the browser the browser capability drives. `None`
    /// uses the host's CDP automation default.
    pub browser_default: Option<leveler_browser::BrowserProduct>,
    /// `[assist].prompt_suggestions`: the idle prompt-suggestion affordance.
    pub assist_prompt_suggestions: bool,
    /// `[assist].away_summary`: the one-line recap after each successful turn.
    pub assist_away_summary: bool,
    /// `[context].soft_percent`: the harness soft compaction percent for every
    /// model that does not override it. `None` → the product default.
    pub context_soft_percent: Option<u8>,
    /// `[models.<id>].soft_compaction_percent`, keyed by the table id as
    /// written. A per-model share of the effective input capacity.
    pub model_context_soft_percent: Vec<(String, u8)>,
    /// `[web.search]`, resolved. `None` means web search is unavailable; there
    /// is no legacy environment-only fallback.
    pub web_search: Option<WebSearchSettings>,
}

impl LoadedConfig {
    /// The soft compaction percent for `model`: its own entry first, then the
    /// global default. `None` leaves the product default in force.
    pub fn soft_percent_for(&self, model: &leveler_model::ModelRef) -> Option<u8> {
        self.model_context_soft_percent
            .iter()
            .find(|(name, _)| name == model.model.as_str())
            .map(|(_, percent)| *percent)
            .or(self.context_soft_percent)
    }
}

/// The ONE decision about whether durable project memory is part of this
/// project at all.
///
/// Independent project permission for durable memory. Capability disclosure
/// controls model exposure; this policy controls whether loading is permitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryPolicy {
    enabled: bool,
    user_enabled: bool,
    experience_enabled: bool,
}

impl MemoryPolicy {
    /// Resolve from the project's `.leveler/config.yaml`.
    pub fn from_project(config: &leveler_project::ProjectConfig) -> Self {
        Self {
            enabled: config.memory.enabled,
            user_enabled: config.memory.user_enabled,
            experience_enabled: config.memory.experience_enabled,
        }
    }

    /// Whether the user wants memory enabled for this project.
    pub fn enabled(self) -> bool {
        self.enabled
    }

    /// Whether user facts are effective. The total switch owns both lanes.
    pub fn user_enabled(self) -> bool {
        self.enabled && self.user_enabled
    }

    /// Whether agent experience is effective. The total switch owns both
    /// lanes: `enabled = false` closes experience even if its own bit is true.
    pub fn experience_enabled(self) -> bool {
        self.enabled && self.experience_enabled
    }

    /// Whether any lane can reach the model or the background worker.
    pub fn any_lane(self) -> bool {
        self.user_enabled() || self.experience_enabled()
    }

    /// The lane value every read, tool and worker uses.
    pub fn lanes(self) -> leveler_memory::MemoryLanes {
        leveler_memory::MemoryLanes {
            user: self.user_enabled(),
            experience: self.experience_enabled(),
        }
    }
}

/// The one memory service this runtime hands to tools and background work.
///
/// The Markdown roots are configured lazily: nothing opens (or creates) a
/// scope until an operation addresses it, so a project run that never touches
/// global memory never creates the user's home store.
pub fn memory_service(layout: &leveler_project::Layout) -> leveler_memory::MemoryService {
    use leveler_memory::v2::{MemoryScope, ScopeRoots};
    let mut roots = leveler_memory::MemoryRoots::new();
    if let Some(current) = layout.markdown_memory_dir() {
        let mut scopes = ScopeRoots::new(
            MemoryScope::Project,
            current,
            layout.memory_v2_state_dir("project"),
        );
        if let Some(repo) = layout.repo_root.clone() {
            scopes = scopes.with_repo_root(repo);
        }
        roots = roots.with_project(scopes);
    }
    if let Some(current) = layout.local_memory_dir() {
        let mut scopes = ScopeRoots::new(
            MemoryScope::Local,
            current,
            layout.memory_v2_state_dir("local"),
        );
        if let Some(repo) = layout.repo_root.clone() {
            scopes = scopes.with_repo_root(repo);
        }
        roots = roots.with_local(scopes);
    }
    roots = roots.with_global(ScopeRoots::new(
        MemoryScope::Global,
        layout.global_memory_dir(),
        layout.memory_v2_state_dir("global"),
    ));
    leveler_memory::MemoryService::new(roots)
}

/// Stable digest of the configuration sources a repository runtime loads.
///
/// Keeping this as a source digest lets a client compare its files with a
/// daemon's boot snapshot without assembling a second application.
pub fn runtime_config_fingerprint(layout: &Layout) -> std::io::Result<String> {
    fn hash_bytes(hasher: &mut Sha256, label: &[u8], bytes: &[u8]) {
        hasher.update(label.len().to_le_bytes());
        hasher.update(label);
        hasher.update(bytes.len().to_le_bytes());
        hasher.update(bytes);
    }

    fn hash_optional_file(
        hasher: &mut Sha256,
        label: &[u8],
        path: &std::path::Path,
    ) -> std::io::Result<()> {
        match std::fs::read(path) {
            Ok(bytes) => hash_bytes(hasher, label, &bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                hash_bytes(hasher, label, b"<absent>")
            }
            Err(error) => return Err(error),
        }
        Ok(())
    }

    let mut hasher = Sha256::new();
    hash_optional_file(&mut hasher, b"global-config", &layout.home().config_file())?;
    if let Some(root) = layout.primary_workspace() {
        hash_optional_file(
            &mut hasher,
            b"project-config",
            &root.join(".leveler/config.yaml"),
        )?;
    }
    for (kind, files) in [
        (b"provider".as_slice(), yaml_files(&layout.providers_dir())),
        (b"model".as_slice(), yaml_files(&layout.models_dir())),
    ] {
        hash_bytes(&mut hasher, kind, files.len().to_string().as_bytes());
        for path in files {
            let name = path
                .file_name()
                .map(|name| name.as_encoded_bytes())
                .unwrap_or_default();
            hash_bytes(&mut hasher, b"file-name", name);
            hash_optional_file(&mut hasher, b"file-content", &path)?;
        }
    }
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

#[cfg(test)]
mod memory_policy_tests {
    use super::MemoryPolicy;
    use leveler_project::{MemoryConfig, ProjectConfig};

    #[test]
    fn a_disabled_project_never_exposes_memory() {
        let policy = MemoryPolicy::from_project(&ProjectConfig {
            memory: MemoryConfig {
                enabled: false,
                user_enabled: true,
                experience_enabled: true,
                deprecated_backend: None,
            },
            ..ProjectConfig::default()
        });
        assert!(!policy.enabled());
        assert!(!policy.user_enabled());
        assert!(!policy.experience_enabled());
        assert!(!policy.any_lane());
    }

    #[test]
    fn an_enabled_project_permits_memory() {
        let policy = MemoryPolicy::from_project(&ProjectConfig {
            memory: MemoryConfig {
                enabled: true,
                user_enabled: true,
                experience_enabled: true,
                deprecated_backend: None,
            },
            ..ProjectConfig::default()
        });
        assert!(policy.enabled());
        assert!(policy.user_enabled());
        assert!(policy.experience_enabled());
        assert!(policy.any_lane());
    }

    /// The built-in policy is "user facts on, automatic experience off". An
    /// absent `memory:` block must still give the user a working lane.
    #[test]
    fn an_unconfigured_project_opens_only_the_user_lane() {
        let policy = MemoryPolicy::from_project(&ProjectConfig::default());
        assert!(policy.enabled());
        assert!(policy.user_enabled());
        assert!(!policy.experience_enabled());
        assert!(policy.any_lane());
    }

    #[test]
    fn each_lane_can_be_closed_without_closing_the_other() {
        let user_only = MemoryPolicy::from_project(&ProjectConfig {
            memory: MemoryConfig {
                enabled: true,
                user_enabled: true,
                experience_enabled: false,
                deprecated_backend: None,
            },
            ..ProjectConfig::default()
        });
        assert!(user_only.user_enabled());
        assert!(!user_only.experience_enabled());
        assert!(user_only.any_lane());

        let experience_only = MemoryPolicy::from_project(&ProjectConfig {
            memory: MemoryConfig {
                enabled: true,
                user_enabled: false,
                experience_enabled: true,
                deprecated_backend: None,
            },
            ..ProjectConfig::default()
        });
        assert!(!experience_only.user_enabled());
        assert!(experience_only.experience_enabled());
        assert!(experience_only.any_lane());

        let none = MemoryPolicy::from_project(&ProjectConfig {
            memory: MemoryConfig {
                enabled: true,
                user_enabled: false,
                experience_enabled: false,
                deprecated_backend: None,
            },
            ..ProjectConfig::default()
        });
        assert!(!none.any_lane());
    }
}

#[cfg(test)]
mod runtime_config_fingerprint_tests {
    use super::runtime_config_fingerprint;
    use leveler_project::Layout;

    #[test]
    fn model_bundle_change_creates_a_new_runtime_generation() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("configs");
        std::fs::create_dir_all(config.join("models")).unwrap();
        let model = config.join("models/m.yaml");
        std::fs::write(&model, "id: old\n").unwrap();
        let layout = Layout::from_parts(tmp.path().join("repo"), config, tmp.path().join("state"));

        let before = runtime_config_fingerprint(&layout).unwrap();
        std::fs::write(model, "id: new\n").unwrap();
        let after = runtime_config_fingerprint(&layout).unwrap();

        assert_ne!(before, after);
    }

    #[test]
    fn global_config_change_creates_a_new_runtime_generation() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::from_parts(
            tmp.path().join("repo"),
            tmp.path().join("configs"),
            tmp.path().join("state"),
        );
        std::fs::write(
            tmp.path().join("config.toml"),
            "default_model = \"p/old\"\n",
        )
        .unwrap();
        let before = runtime_config_fingerprint(&layout).unwrap();
        std::fs::write(
            tmp.path().join("config.toml"),
            "default_model = \"p/new\"\n",
        )
        .unwrap();
        let after = runtime_config_fingerprint(&layout).unwrap();

        assert_ne!(before, after);
    }
}

impl Default for LoadedConfig {
    fn default() -> Self {
        Self {
            providers: Vec::new(),
            models: Vec::new(),
            default_model: None,
            lang: None,
            develop_model: None,
            vcs_co_author: true,
            mcp_servers: Vec::new(),
            agents_delegation: true,
            agents_independent_review: leveler_project::IndependentReview::default(),
            browser_default: None,
            assist_prompt_suggestions: true,
            assist_away_summary: true,
            context_soft_percent: None,
            model_context_soft_percent: Vec::new(),
            web_search: None,
        }
    }
}

fn combine_independent_review(
    global: leveler_project::IndependentReview,
    project: leveler_project::IndependentReview,
) -> leveler_agent::coding::IndependentReviewPolicy {
    use leveler_agent::coding::IndependentReviewPolicy as P;
    use leveler_project::IndependentReview as I;
    // Explicit only: a review runs when either layer requires it.
    match (global, project) {
        (I::Required, _) | (_, I::Required) => P::Required,
        (I::Off, I::Off) => P::Off,
    }
}

struct McpToolCache {
    unrestricted: bool,
    tools: Vec<Arc<dyn leveler_tools::tool::Tool>>,
}

/// A fully-assembled application.
pub struct Application {
    pub layout: Layout,
    pub config: LoadedConfig,
    pub registry: Arc<ProviderRegistry>,
    /// Configuration generation loaded by this process. Immutable for the
    /// process lifetime; a changed generation gets a replacement daemon.
    config_fingerprint: String,
    /// MCP tools cached under the credential inheritance authority of their launch.
    mcp_tools: Arc<tokio::sync::Mutex<Option<McpToolCache>>>,
    /// The session database pool, opened once per process.
    database: Arc<tokio::sync::Mutex<Option<Database>>>,
    /// One project-scoped durable memory worker. Runtime composition roots
    /// start it explicitly; every client of this application then shares it.
    memory_consolidator: OnceLock<Arc<memory_consolidator::MemoryConsolidator>>,
    memory_events: tokio::sync::broadcast::Sender<memory_consolidator::MemoryConsolidationEvent>,
    /// When set, overrides the resolved execution policy on every execution
    /// path (single-knob ablation runs). `None` = resolver defaults.
    execution_overrides: Option<leveler_agent::coding::ExecutionOverrides>,
    /// Whether durable project memory is part of this project. Resolved once at
    /// assembly from `.leveler/config.yaml`; a config change is a new process
    /// generation, so the value is immutable for this Application's lifetime.
    memory: MemoryPolicy,
    /// Collaboration mode (chat / plan / goal).
    collaboration: CollaborationMode,
    /// Model-step safety ceiling for this application's top-level runs, from
    /// the CLI's `--max-model-steps` (alias `--max-rounds`). `None` pins no
    /// ceiling; the CLI resolves "flag absent" to
    /// [`DEFAULT_MODEL_STEP_CEILING`]. Eval sets its own bounded window and the
    /// interactive UI pins no ceiling.
    ///
    /// This is a mechanical circuit breaker against a runaway model↔tool
    /// loop, never a task budget: a top-level task ends on its goal lifecycle
    /// or a resource budget (tokens/cost/duration), all of which are carried
    /// across continues and resumes by the epoch ledger.
    model_step_ceiling: Option<u32>,
    environment: Arc<leveler_core::EnvSnapshot>,
    /// Process-lived background task registry, shared (cloned) into every
    /// engine/turn so `background=true` servers survive between messages. A
    /// per-engine registry was dropped at turn end, and its `KillOnDrop` reaped
    /// every background process — hence servers dying between turns. Only the
    /// process exit drops the last handle.
    background_tasks: Arc<leveler_execution::BackgroundTaskRegistry>,
    /// Daemon-owned, lazily-started browser, shared (cloned `Arc`) into
    /// every engine/turn so the browser and its isolated project profile survive
    /// across turns and client disconnect. Nothing starts until a browser tool
    /// is first used.
    browser: Arc<leveler_browser::Browser>,
    /// Durable runtime identity, loaded (and minted on first use) lazily from
    /// the state directory. Cached: the id cannot change within one process.
    runtime_id: OnceLock<leveler_core::RuntimeId>,
    /// This process's boot: started before the first ownership-sensitive
    /// write, held until the application is dropped. Command receipts, task
    /// ownership and running turns all carry its one id.
    boot: std::sync::Mutex<Option<runtime_boot::RuntimeBootLease>>,
    /// Commands this boot is handling — see
    /// [`interactive::InProcessRuntimeClient`]. Kept beside the boot because
    /// "a receipt of this boot that is not in flight" is a fact about the
    /// boot, not about one client of it.
    in_flight_commands: interactive::InFlightCommands,
    /// The live permission profile of each session, keyed by session scope.
    ///
    /// The persisted `sessions.mode` column is the durable record; this is the
    /// same value as running state, so a change reaches a turn that is already
    /// executing. Per session — never one process-wide profile — because one
    /// daemon hosts several sessions at different profiles at once.
    permission_profiles: std::sync::Mutex<
        std::collections::HashMap<String, leveler_execution::SharedPermissionProfile>,
    >,
    /// A startup fact this process learned while resolving the default model and
    /// that the next created session's client must be told: today, the persisted
    /// default was not resolvable here and a configured model was used instead.
    ///
    /// It lives here because the daemon's own stderr is a log file and its event
    /// stream has no subscriber at creation time; the created session's
    /// bootstrap is the one channel a client is guaranteed to read.
    default_model_notice: std::sync::Mutex<Option<String>>,
}

impl Drop for Application {
    fn drop(&mut self) {
        if let Some(worker) = self.memory_consolidator.get() {
            worker.cancel();
        }
    }
}

impl Application {
    /// A handle on the memory store. The service is stateless beyond its
    /// roots; constructing it per command keeps the layout the only input, so
    /// every caller reaches the same Markdown scopes.
    pub fn memory_service(&self) -> leveler_memory::MemoryService {
        memory_service(&self.layout)
    }

    /// A notice for a config that still carries the removed `memory.backend`
    /// key. `None` when the config never named a backend.
    pub fn deprecated_memory_backend(&self) -> Option<String> {
        self.project_config().memory.deprecated_backend()
    }

    /// The lanes this project has open. Reads and writes both honor them.
    pub fn memory_lanes(&self) -> leveler_memory::MemoryLanes {
        self.memory.lanes()
    }

    /// The live permission profile for a session, SEEDED once from `mode` (the
    /// caller's authority — the session row or its cached runtime config) and
    /// reused by every later turn of that session.
    ///
    /// Seeding is not reconciliation: once a session has a live cell, that cell
    /// is the one authority for its running execution. A caller's `mode` is a
    /// snapshot taken when a turn was staged, so writing it back would let a
    /// turn whose snapshot predates a `SetPermissionProfile` silently restore
    /// the older profile — the UI chip says `full` while the turn keeps
    /// authorizing under `auto` and asks for approval. Live changes go through
    /// [`Self::set_live_permission_profile`] only.
    fn permission_profile_for(
        &self,
        scope: Option<&str>,
        mode: PermissionProfile,
    ) -> leveler_execution::SharedPermissionProfile {
        let Some(scope) = scope else {
            // No session identity (eval, one-shot CLI): nobody can change the
            // profile mid-run, so a private cell is the whole truth.
            return leveler_execution::SharedPermissionProfile::new(mode);
        };
        let mut map = self
            .permission_profiles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.entry(scope.to_string())
            .or_insert_with(|| leveler_execution::SharedPermissionProfile::new(mode))
            .clone()
    }

    /// Apply a permission change to a session's RUNNING execution.
    ///
    /// Returns whether a live cell existed — false simply means no turn of
    /// that session has built one yet, and the next turn will start from the
    /// persisted value.
    pub fn set_live_permission_profile(&self, scope: &str, mode: PermissionProfile) -> bool {
        let map = self
            .permission_profiles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match map.get(scope) {
            Some(profile) => {
                profile.set(mode);
                true
            }
            None => false,
        }
    }

    /// The profile a session's execution is authorizing under right now.
    /// `None` when that session has never run a turn in this process.
    pub fn live_permission_profile(&self, scope: &str) -> Option<PermissionProfile> {
        self.permission_profiles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(scope)
            .map(leveler_execution::SharedPermissionProfile::get)
    }

    /// The shared background-task registry (R004 F7 shutdown reaping).
    pub fn background_tasks(&self) -> &Arc<leveler_execution::BackgroundTaskRegistry> {
        &self.background_tasks
    }

    /// The daemon-owned browser (shutdown teardown).
    pub fn browser(&self) -> &Arc<leveler_browser::Browser> {
        &self.browser
    }

    /// Load all config bundles from the layout's config directory.
    pub fn load_config(layout: &Layout) -> Result<LoadedConfig, AppError> {
        let mut providers = Vec::new();
        for path in yaml_files(&layout.providers_dir()) {
            providers.push(load_provider_config(&path)?);
        }
        let mut models = Vec::new();
        for path in yaml_files(&layout.models_dir()) {
            models.push(load_model_config(&path)?);
        }
        // Merge the global config (`~/.leveler/config.toml`) underneath the repo
        // bundle: the repo wins on any id collision, but global entries fill in
        // so `leveler` works without a per-repo `configs/` directory.
        let global_config = global_config::GlobalConfig::load()
            .map_err(|e| AppError::GlobalConfig(e.to_string()))?;
        for model in &mut models {
            if model.profile.thinking.is_none() {
                model.profile.thinking = Some(global_config.thinking_level_for(&model.profile.id));
            }
        }
        let global = global_config.into_bundle();
        merge_providers(&mut providers, global.providers);
        for model in global.models {
            let exists = models.iter().any(|m| {
                m.profile.id == model.profile.id && m.profile.provider == model.profile.provider
            });
            if !exists {
                models.push(model);
            }
        }
        Ok(LoadedConfig {
            providers,
            models,
            default_model: global.default_model,
            lang: global.lang,
            vcs_co_author: global.vcs_co_author,
            develop_model: global.develop_model,
            mcp_servers: global.mcp_servers,
            agents_delegation: global.agents_delegation,
            agents_independent_review: global.agents_independent_review,
            browser_default: global.browser_default,
            assist_prompt_suggestions: global.assist_prompt_suggestions,
            assist_away_summary: global.assist_away_summary,
            context_soft_percent: global.context_soft_percent,
            model_context_soft_percent: global.model_soft_percent,
            web_search: global.web_search,
        })
    }

    /// Assemble the application: load config and build the registry.
    ///
    /// Missing API keys do not fail assembly — the provider is registered with
    /// no key so `doctor` can report the gap and probes fail with a clear auth
    /// error, rather than blocking every command.
    pub fn assemble(layout: Layout) -> Result<Self, AppError> {
        let environment = Arc::new(leveler_core::EnvSnapshot::new(
            std::env::vars_os(),
            std::env::current_dir().map_err(|source| AppError::Io {
                path: "process environment cwd".into(),
                source,
            })?,
            std::env::temp_dir(),
        ));
        let _ = leveler_core::install_environment((*environment).clone());

        // A Windows run killed with a write root still lowered leaves that
        // directory at Low integrity, and no destructor is left to put it
        // back. Repair it here, at the first point the environment is known,
        // rather than waiting for the same repository to be opened again.
        //
        // A failure does not fail assembly, for the same reason a missing API
        // key does not: it would block every command over something only
        // confined execution depends on. `lease_write_roots` retries this and
        // refuses the command outright if it still cannot be done, so nothing
        // runs confined over label state we could not put straight.
        if let Err(error) = leveler_execution::recover_stale_write_roots(&environment) {
            tracing::warn!(
                %error,
                "could not restore write-root integrity labels left by an earlier run; \
                 confined execution will refuse until this is resolved"
            );
        }

        let fingerprint_before = runtime_config_fingerprint(&layout)
            .map_err(|error| AppError::ConfigFingerprint(error.to_string()))?;
        let config = Self::load_config(&layout)?;
        let config_fingerprint = runtime_config_fingerprint(&layout)
            .map_err(|error| AppError::ConfigFingerprint(error.to_string()))?;
        if fingerprint_before != config_fingerprint {
            return Err(AppError::ConfigFingerprint(
                "configuration changed while the runtime was loading it; retry startup".into(),
            ));
        }

        let providers = config
            .providers
            .iter()
            .map(|cfg| {
                let key = match resolve_api_key(cfg) {
                    Ok(key) => key,
                    Err(error) => {
                        // Keep assembling: `doctor` reports the gap for every
                        // provider, and a run refuses before its first request
                        // with a message naming the variable and the fix. This
                        // fires for providers the user is not even using, so it
                        // is a diagnostic, not a warning to put above that.
                        tracing::debug!("provider `{}` has no usable API key: {error}", cfg.id);
                        None
                    }
                };
                (cfg.clone(), key)
            })
            .collect();

        let registry = ProviderRegistry::build(RegistryInputs {
            providers,
            models: config.models.clone(),
        })?;

        // Project config + env (composition root may read env — AGENTS.md).
        let background_tasks =
            leveler_execution::BackgroundTaskRegistry::with_environment(environment.clone());
        let background_tasks = Arc::new(match layout.primary_workspace() {
            Some(_) => background_tasks.with_execution_host(execution_host_config(&layout)?),
            None => background_tasks,
        });
        // Lazy: this holds only paths and the configured product until the
        // first navigate actually starts a browser.
        let browser = Arc::new(leveler_browser::Browser::new(
            (*environment).clone(),
            layout.browser_profile_dir(),
            config.browser_default,
        ));
        let (memory_events, _) = tokio::sync::broadcast::channel(128);
        let memory = MemoryPolicy::from_project(
            &layout
                .primary_workspace()
                .and_then(leveler_project::ProjectConfig::load)
                .unwrap_or_default(),
        );
        Ok(Self {
            layout,
            config,
            registry: Arc::new(registry),
            config_fingerprint,
            mcp_tools: Arc::new(tokio::sync::Mutex::new(None)),
            database: Arc::new(tokio::sync::Mutex::new(None)),
            memory_consolidator: OnceLock::new(),
            memory_events,
            execution_overrides: None,
            memory,
            // One product default for a new Coding Session: Goal. Callers
            // that resolved an explicit axis (`--collaboration`, the transport
            // request) override it via `with_collaboration` or the create path.
            collaboration: CollaborationMode::default(),
            model_step_ceiling: None,
            environment,
            background_tasks,
            browser,
            runtime_id: OnceLock::new(),
            boot: std::sync::Mutex::new(None),
            in_flight_commands: Default::default(),
            default_model_notice: std::sync::Mutex::new(None),
            permission_profiles: std::sync::Mutex::new(std::collections::HashMap::new()),
        })
    }

    /// This runtime's durable identity: read from the state directory, minted
    /// and persisted on first use, stable across process restarts. Fails on a
    /// corrupt identity file rather than silently minting a new identity.
    pub fn runtime_id(&self) -> Result<leveler_core::RuntimeId, AppError> {
        if let Some(id) = self.runtime_id.get() {
            return Ok(id.clone());
        }
        let id = runtime_identity::load_or_create_runtime_id(&self.layout.state_dir)?;
        Ok(self.runtime_id.get_or_init(|| id).clone())
    }

    /// The immutable configuration generation loaded by this application.
    pub fn config_fingerprint(&self) -> &str {
        &self.config_fingerprint
    }

    /// This process's boot id, starting the boot on first use. Concurrent
    /// first callers get the same boot: there is one per application.
    pub fn boot_id(&self) -> Result<leveler_core::BootId, AppError> {
        let mut boot = self
            .boot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(lease) = boot.as_ref() {
            return Ok(lease.id().clone());
        }
        let lease =
            runtime_boot::RuntimeBootLease::acquire(&self.layout.state_dir).map_err(|source| {
                AppError::Io {
                    path: self.layout.state_dir.join("boots").display().to_string(),
                    source,
                }
            })?;
        Ok(boot.insert(lease).id().clone())
    }

    /// The boot, if it has started. Reading it never starts one.
    pub(crate) fn started_boot_id(&self) -> Option<leveler_core::BootId> {
        self.boot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|lease| lease.id().clone())
    }

    pub(crate) fn in_flight_commands(&self) -> interactive::InFlightCommands {
        self.in_flight_commands.clone()
    }

    /// Compose the domain-neutral lifecycle engine over this application's
    /// single durable store, executing as this process's boot.
    pub fn task_engine(
        &self,
        db: &leveler_storage::Database,
    ) -> Result<leveler_engine::TaskEngine, AppError> {
        Ok(leveler_engine::TaskEngine {
            stores: leveler_storage::EngineStores::from_database(db),
            runtime_id: self.runtime_id()?,
            boot: leveler_engine::EngineBoot {
                id: self.boot_id()?,
                liveness: Arc::new(runtime_boot::StateDirBootLiveness::new(
                    &self.layout.state_dir,
                )),
            },
        })
    }

    /// Set collaboration mode for subsequent session creates.
    pub fn with_collaboration(mut self, mode: CollaborationMode) -> Self {
        self.collaboration = mode;
        self
    }

    /// Model-step safety ceiling for this application's top-level runs, from
    /// the CLI's `--max-model-steps` (alias `--max-rounds`): absent means the
    /// default ([`DEFAULT_MODEL_STEP_CEILING`]), `0` means no ceiling, `n`
    /// means `n` model steps.
    ///
    /// A ceiling, not a task budget: reaching it stops a runaway model↔tool
    /// loop and reports `TurnLimitReached`. Nothing about a task's remaining
    /// work is decided by a model-step count.
    pub fn with_model_step_ceiling(mut self, max_model_steps: Option<u32>) -> Self {
        self.model_step_ceiling = model_step_ceiling_from_flag(max_model_steps);
        self
    }

    pub fn collaboration(&self) -> CollaborationMode {
        self.collaboration
    }

    /// Override the resolved execution policy for every execution path — the
    /// `leveler eval ablate` seam. Use on a freshly assembled Application so
    /// control and ablated runs differ in exactly the flipped knob.
    pub fn with_execution_overrides(
        mut self,
        overrides: leveler_agent::coding::ExecutionOverrides,
    ) -> Self {
        self.execution_overrides = Some(overrides);
        self
    }

    /// Open (creating dirs as needed) the session database. The pool is
    /// created once and shared — concurrent pools on one SQLite file contend
    /// for the write lock (`database is locked`).
    pub async fn open_database(&self) -> Result<Database, AppError> {
        {
            let guard = self.database.lock().await;
            if let Some(db) = guard.as_ref() {
                return Ok(db.clone());
            }
        }
        std::fs::create_dir_all(&self.layout.state_dir).map_err(|source| AppError::Io {
            path: self.layout.state_dir.display().to_string(),
            source,
        })?;
        let db = Database::connect(&self.layout.database_path()).await?;
        let mut guard = self.database.lock().await;
        let db = guard.get_or_insert_with(|| db).clone();
        Ok(db)
    }

    /// Recover hosted process settlements at an explicit runtime startup
    /// boundary. Merely reading the session database never performs this work.
    pub async fn reconcile_execution_services(&self) -> Result<(), AppError> {
        self.background_tasks
            .reconcile_hosted()
            .await
            .map_err(AppError::ExecutionHost)
    }

    /// Start this runtime's project memory worker explicitly. Database-only
    /// commands must not acquire provider-side effects merely by opening the
    /// store; runtime composition roots call this after startup recovery.
    pub async fn start_memory_consolidator(&self) -> Result<(), AppError> {
        let db = self.open_database().await?;
        self.ensure_memory_consolidator(&db)
    }

    fn ensure_memory_consolidator(&self, db: &Database) -> Result<(), AppError> {
        // A project with both lanes closed runs no memory worker at all: no
        // inbox admission (the SQL trigger reads the session snapshot), no
        // claim, no provider request. Existing memory data is untouched and
        // becomes usable again when the user re-enables a lane (which restarts
        // the process).
        if !self.memory.any_lane() {
            return Ok(());
        }
        if self.memory_consolidator.get().is_some() {
            return Ok(());
        }
        let events = self.memory_events.clone();
        let worker = memory_consolidator::MemoryConsolidator::new(
            db.clone(),
            self.boot_id()?,
            self.layout.state_dir.clone(),
            memory_service(&self.layout),
            self.registry.clone(),
            Arc::new(move |event| {
                let _ = events.send(event);
            }),
        )
        .with_lanes(self.memory.lanes())
        .with_memory_service(memory_service(&self.layout));
        if self.memory_consolidator.set(worker.clone()).is_ok() {
            drop(worker.spawn());
        }
        Ok(())
    }

    /// Signal that a terminal turn may have admitted new memory work. The
    /// signal is non-durable and lossy by design; startup/max-age polling and
    /// the SQLite inbox are the durable truth.
    pub fn notify_memory_consolidator(&self) {
        if let Some(worker) = self.memory_consolidator.get() {
            worker.notify_turn_terminal();
        }
    }

    /// Drain ready memory work before a headless process exits.
    ///
    /// A long-lived host leaves consolidation to the worker's idle cadence.
    /// A one-shot process must flush explicitly or its last turn is only
    /// consumed by the next launch. Returns `None` when this runtime has no
    /// memory worker (both lanes closed).
    pub async fn flush_memory_for_headless(
        &self,
    ) -> Option<memory_consolidator::MemoryFlushOutcome> {
        let worker = self.memory_consolidator.get()?.clone();
        Some(worker.flush().await)
    }

    /// Stamp this session's memory-admission flag from the project policy.
    ///
    /// Must run before the turn row is inserted: admission is a SQL trigger on
    /// that insert, so the column has to be current by then. A no-op when the
    /// value already matches.
    async fn sync_session_memory_policy(
        &self,
        db: &Database,
        session_id: &leveler_core::SessionId,
    ) {
        if let Err(error) = SessionRepository::new(db)
            .set_memory_lanes(
                session_id,
                self.memory.enabled(),
                self.memory.lanes().user,
                self.memory.lanes().experience,
            )
            .await
        {
            // Admission fails closed only when the write fails: the column
            // keeps its previous value, which is the last policy actually
            // recorded for this session. Surfacing the error is a diagnostic,
            // not a reason to abort the turn.
            tracing::warn!(%error, "could not persist the session memory policy");
        }
    }

    pub fn subscribe_memory_events(
        &self,
    ) -> tokio::sync::broadcast::Receiver<memory_consolidator::MemoryConsolidationEvent> {
        self.memory_events.subscribe()
    }

    /// All configured model references.
    pub fn model_refs(&self) -> Vec<ModelRef> {
        self.registry.model_refs()
    }

    /// Record a startup fact for the next created session's client. Set by the
    /// composition root right after it resolves the default model; see
    /// [`Self::take_default_model_notice`].
    pub fn set_default_model_notice(&self, notice: String) {
        let mut slot = self
            .default_model_notice
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *slot = Some(notice);
    }

    /// Take the startup fact, so it is reported once rather than on every new
    /// session: the person has been told which model is in force.
    pub fn take_default_model_notice(&self) -> Option<String> {
        self.default_model_notice
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    /// Whether a PERSISTENT default may name `model` in this repository.
    ///
    /// The persisted default is read by the next bare launch, which resolves
    /// the DEFAULT environment: this repository's `<repo>/configs` bundle (or
    /// `LEVELER_CONFIG_DIR`) plus the global config. A model that exists only in
    /// an explicitly overridden bundle (`--config-dir`) would therefore make the
    /// next `leveler` fail to start with `model … is not configured`, so such a
    /// choice stays session-scoped. This is evidence, not policy: it answers
    /// what a bare launch would resolve, and the caller decides.
    pub fn model_is_in_default_environment(&self, model: &ModelRef) -> bool {
        let Some(repo_root) = self.layout.repo_root.clone() else {
            // No workspace: the default environment IS the home-level bundle the
            // running layout already resolved.
            return true;
        };
        let default_layout = leveler_project::Layout::resolve(repo_root, None);
        if default_layout.config_dir == self.layout.config_dir {
            // The running catalog came from the default bundle: whatever it
            // offers, a bare launch in this repository offers too.
            return true;
        }
        match Self::load_config(&default_layout) {
            Ok(config) => config.models.iter().any(|candidate| {
                candidate.profile.provider == model.provider && candidate.profile.id == model.model
            }),
            // The default catalog could not even be read, so nothing can be
            // proven either way. Honor the user's explicit request (a broken
            // config reports its own failure on write) and let the startup
            // fallback rescue a default that turns out to be unusable — a
            // silently skipped persist would report success and lose the write
            // failure the person needs to see.
            Err(_) => true,
        }
    }

    /// A configured model to use when the persisted default does not resolve in
    /// THIS environment.
    ///
    /// Order: the global config's own default when this environment can resolve
    /// it, else the first configured model in stable order — the same last
    /// resort a bare launch uses when no default is set. `None` only when
    /// nothing at all is configured.
    pub fn fallback_default_model(&self) -> Option<ModelRef> {
        let mut configured = self.model_refs();
        configured.sort_by_key(|reference| reference.to_string());
        if let Some(preferred) = self
            .config
            .default_model
            .as_deref()
            .and_then(ModelRef::parse)
            && configured.contains(&preferred)
        {
            return Some(preferred);
        }
        configured.into_iter().next()
    }

    pub(crate) fn validate_session_workspace(
        &self,
        repository: Option<&str>,
    ) -> Result<(), AppError> {
        let expected = self
            .layout
            .primary_workspace()
            .map(|root| root.display().to_string());
        if repository != expected.as_deref() {
            return Err(AppError::NotFound(
                "session workspace association does not match this runtime source".into(),
            ));
        }
        Ok(())
    }

    /// The parsed `.leveler/config.yaml` for the repo (defaults if absent).
    pub fn project_config(&self) -> leveler_project::ProjectConfig {
        self.layout
            .primary_workspace()
            .and_then(leveler_project::ProjectConfig::load)
            .unwrap_or_default()
    }

    /// The single memory-capability decision for this project.
    pub fn memory_policy(&self) -> MemoryPolicy {
        self.memory
    }

    /// Whether an idle assist affordance is switched on.
    pub(crate) fn assist_enabled(&self, kind: crate::prompt_assist::AssistKind) -> bool {
        match kind {
            crate::prompt_assist::AssistKind::PromptSuggestion => {
                self.config.assist_prompt_suggestions
            }
            crate::prompt_assist::AssistKind::AwaySummary => self.config.assist_away_summary,
        }
    }

    pub(crate) fn top_level_limits(&self) -> leveler_agent::StepLimits {
        let mut limits = top_level_limits_from_config(&self.project_config().limits);
        // The one place a top-level run's model-step safety ceiling is pinned.
        // `None` here means "this application pins no ceiling" (the
        // interactive UI), not "unbounded by accident".
        limits.max_model_steps = self.model_step_ceiling;
        limits
    }

    /// Connect to the configured MCP servers once and cache their tools, so
    /// every turn reuses the same connections instead of respawning processes.
    async fn mcp_tools(&self, mode: PermissionProfile) -> Vec<Arc<dyn leveler_tools::tool::Tool>> {
        if self.layout.primary_workspace().is_none() || self.config.mcp_servers.is_empty() {
            return Vec::new();
        }
        let unrestricted = mode == PermissionProfile::FullAccess;
        let mut guard = self.mcp_tools.lock().await;
        if guard
            .as_ref()
            .is_none_or(|cache| cache.unrestricted != unrestricted)
        {
            let tools = leveler_tools::mcp::connect_all_with_authority(
                &self.config.mcp_servers,
                &self.environment,
                unrestricted,
            )
            .await;
            *guard = Some(McpToolCache {
                unrestricted,
                tools,
            });
        }
        guard
            .as_ref()
            .map(|cache| cache.tools.clone())
            .unwrap_or_default()
    }

    /// Compose one turn's model-visible tool surface from what this host can
    /// actually do.
    ///
    /// The SINGLE owner of tool-surface composition: the coding engine and the
    /// read-only `/btw` side surface both come through here, so the two cannot
    /// drift into different answers about what tools exist. Harness controls
    /// and MCP tools are attached by the caller that wants them — they steer a
    /// harness or live in another process, so they are not part of the
    /// capability composition.
    #[allow(clippy::too_many_arguments)]
    async fn compose_tool_surface(
        &self,
        model: &ModelRef,
        mode: PermissionProfile,
        sandbox: bool,
        read_only: bool,
        session_scope: Option<&str>,
    ) -> Result<(CapabilityPacks, ToolContext, leveler_tools::ToolRegistry), AppError> {
        let workspace = self
            .layout
            .primary_workspace()
            .map(Workspace::new)
            .transpose()?;
        // The ablation seam (`leveler eval ablate`): overrides reach BOTH
        // consumers — the executor factory's resolver and the tool-context
        // limits — so a run differs from control in exactly the flipped knob.
        // Every execution path (direct, orchestrated, bare) funnels through
        // here.
        let max_files =
            leveler_agent::coding::resolve_tool_limits(self.execution_overrides.as_ref());
        let artifact_store = std::sync::Arc::new(leveler_execution::ArtifactStore::new(
            self.layout.state_dir.join("artifacts"),
        ));
        // Reuse the process-lived registry so background servers/watchers
        // survive across turns. A fresh per-engine registry was dropped when the
        // turn's engine went out of scope, and its KillOnDrop killed every
        // background process (and the next turn's registry no longer knew the
        // task id) — the "服务活不过一个回合" bug.
        let mut capabilities = leveler_tools::Capabilities::in_process(self.environment.clone())
            .with_background_tasks(self.background_tasks.clone())
            .with_artifact_store(artifact_store)
            .with_browser(self.browser.clone())
            .with_search_provider(resolve_search_provider(
                self.config.web_search.as_ref(),
                self.environment.as_ref(),
            ));
        // The handle follows the same project lanes as the pack. A project
        // with both lanes closed has no store to hand a tool even if a caller
        // later asks for one.
        if self.memory.any_lane() {
            capabilities = capabilities
                .with_memory_service(memory_service(&self.layout))
                .with_memory_lanes(self.memory.lanes());
        }
        let tool_context = match workspace {
            Some(workspace) => {
                ToolContext::with_environment(workspace, mode, self.environment.clone())
            }
            None => ToolContext::without_workspace_with_environment(mode, self.environment.clone()),
        }
        .with_policy_limits(max_files)
        .with_sandbox(sandbox)
        .with_deny_env(provider_secret_env_names(
            &self.config.providers,
            self.config.web_search.as_ref(),
        ))
        .with_read_only(read_only);
        // The permission profile this turn authorizes under is the SESSION's
        // live cell, not the value captured here: a user who switches profile
        // while this turn runs must be obeyed by it and by every agent it has
        // already delegated to, at their next authorization decision.
        let tool_context =
            tool_context.with_permission_profile(self.permission_profile_for(session_scope, mode));
        let tool_context = match session_scope {
            Some(scope) => tool_context.with_session_scope(scope),
            None => tool_context,
        };
        // Build all mechanically available tools. The executor discloses them
        // progressively, independently of this registry's construction.
        let available_packs = self.capability_availability(model).await;
        let registry = model_surface(available_packs, &capabilities);
        let registry = if self.layout.primary_workspace().is_none() {
            registry.workspace_independent_subset()
        } else {
            registry
        };
        Ok((available_packs, tool_context, registry))
    }

    /// The observe-only tool surface a `/btw` side question may use.
    ///
    /// Same composition owner as a normal turn, then narrowed to the side
    /// question's own capability surface. The narrowing is decided by THIS
    /// surface, never by the permission profile: a side question is an
    /// observer of the main task, so it must not gain a lifecycle-bound tool
    /// (`wait_task`, `run_command`, `kill_task`) or a mutating one merely
    /// because the session runs with Full access. Full then governs how the
    /// tools that DO belong to this surface are authorized — unrestricted, as
    /// the permission contract requires — so nothing here is a DENY.
    ///
    /// Capability surface and permission policy stay separate owners:
    /// [`PermissionProfile`] never adds a tool back to this list.
    pub async fn side_question_tools(
        &self,
        model: &ModelRef,
        mode: PermissionProfile,
        sandbox: bool,
        session_scope: Option<&str>,
    ) -> Result<(leveler_tools::ToolRegistry, ToolContext), AppError> {
        let (_available, tool_context, registry) = self
            .compose_tool_surface(model, mode, sandbox, true, session_scope)
            .await?;
        // A bounded side question has no goal or capability loading protocol.
        // It may inspect base primitives without implicitly exposing packs.
        //
        // This runs for EVERY profile. `read_only_subset` keeps observe-class
        // tools whose own risk is `Safe`, which is a mechanical property of
        // the tool, not a permission decision: no `wait_task`, `run_command`,
        // `kill_task`, `apply_patch`, `write_file` or `update_plan` can be in
        // it, whatever `mode` says.
        let registry = registry.read_only_subset();
        let definitions = registry.definitions();
        let optional: Vec<&str> = definitions
            .iter()
            .filter(|tool| leveler_agent::capability::tool_capability(&tool.name).is_some())
            .map(|tool| tool.name.as_str())
            .collect();
        Ok((registry.without_named(&optional), tool_context))
    }

    /// Build the coding harness with the minimal initial model surface.
    pub async fn engine_for(
        &self,
        model: &ModelRef,
        mode: PermissionProfile,
        sandbox: bool,
        approver: Arc<dyn leveler_execution::Approver>,
        clarifier: Arc<dyn leveler_agent::Clarifier>,
    ) -> Result<leveler_agent::coding::CodingRuntime, AppError> {
        self.engine_for_session(model, mode, sandbox, approver, clarifier, false, None)
            .await
    }

    /// AVAILABLE: which optional capabilities this MACHINE can provide.
    ///
    /// Every answer is a mechanical fact — is a browser runtime installed, is
    /// a search provider configured, is `git` on PATH, does this model accept
    /// an image. Nothing here is a product choice, and nothing here consults
    /// the model's ability: a surface that grew because a task looked hard, or
    /// shrank because a model looked weak, would be the harness deciding for
    /// the model (`docs/ARCHITECTURE.md` §1.1).
    async fn capability_availability(&self, model: &leveler_model::ModelRef) -> CapabilityPacks {
        let environment = self.environment.as_ref();
        CapabilityPacks {
            // The symbol tools ask a language server when one is installed and
            // otherwise answer from a scan that needs nothing, so the
            // capability itself is always providable here.
            code_intelligence: self.layout.primary_workspace().is_some(),
            // Both git tools shell out to `git`; without the binary each one
            // can only report that it could not start.
            vcs: self.layout.primary_workspace().is_some()
                && leveler_browser::which(environment, "git").is_some(),
            web_fetch: true,
            // Advertising a search tool without a provider spends schema on a
            // call that can only fail. The backend is chosen by configuration,
            // never by the conversation model.
            web_search: resolve_search_provider(
                self.config.web_search.as_ref(),
                self.environment.as_ref(),
            )
            .is_some(),
            // An image is useless to a model that cannot read one.
            media: self
                .registry
                .profile(model)
                .await
                .map(|profile| profile.capabilities.vision)
                .unwrap_or(false),
            // Memory is a project switch, not a default capability: a
            // project with both lanes closed does not register the tools at
            // all, so Full Access has nothing to expose and no permission mode
            // can override the user's explicit disable. One open lane is
            // enough for the pack: the tools themselves follow the lanes.
            memory: self.memory.any_lane(),
            skills: self.layout.primary_workspace().is_some(),
            // Not "is a browser installed": is the browser this host would
            // SELECT — the call's, then `[browser].default`, then the system
            // default — one it can actually drive? Answering with a different
            // browser is exactly what the no-fallback rule forbids, so a user
            // whose default is a browser CodeLeveler cannot drive sees no
            // browser tools rather than a surprise one.
            browser: self.browser.resolve(None).is_ok(),
        }
    }

    /// Compose a turn's overrides from the durable session intent. Clone the
    /// application policy so one session cannot mutate another's main seat.
    pub(crate) async fn execution_overrides_for_session(
        &self,
        session_scope: Option<&str>,
        model: Option<&leveler_model::ModelRef>,
    ) -> Result<Option<leveler_agent::coding::ExecutionOverrides>, AppError> {
        let mut overrides = self.execution_overrides.clone();
        // Harness context policy: the model's own `soft_compaction_percent`,
        // else `[context].soft_percent`. A model that declares neither keeps the
        // product default (`None`), so the resolution chain stays one ladder:
        // explicit override → model entry → global default → product default.
        if let Some(model) = model
            && let Some(percent) = self.config.soft_percent_for(model)
        {
            overrides
                .get_or_insert_with(Default::default)
                .context_soft_percent = Some(percent);
        }
        if let Some(scope) = session_scope {
            let db = self.open_database().await?;
            let raw = SessionRepository::new(&db)
                .thinking(&leveler_core::SessionId::new(scope))
                .await?;
            if let Some(raw) = raw {
                let level = leveler_model::ThinkingLevel::parse(&raw).ok_or_else(|| {
                    AppError::GlobalConfig(format!(
                        "session {scope} stores invalid Thinking Level `{raw}`"
                    ))
                })?;
                overrides.get_or_insert_with(Default::default).main_thinking = Some(level);
            }
        }
        Ok(overrides)
    }

    /// Build a session harness with its live permission cell and planning overlay.
    #[allow(clippy::too_many_arguments)]
    pub async fn engine_for_session(
        &self,
        model: &ModelRef,
        mode: PermissionProfile,
        sandbox: bool,
        approver: Arc<dyn leveler_execution::Approver>,
        clarifier: Arc<dyn leveler_agent::Clarifier>,
        read_only: bool,
        session_scope: Option<&str>,
    ) -> Result<leveler_agent::coding::CodingRuntime, AppError> {
        self.reconcile_execution_services().await?;
        let (available_packs, tool_context, mut registry) = self
            .compose_tool_surface(model, mode, sandbox, read_only, session_scope)
            .await?;
        let unrestricted = tool_context.policy.unrestricted_execution();
        // Register available controls; exposure is owned by CapabilityDisclosure.
        leveler_agent::register_harness_controls_with(
            &mut registry,
            leveler_agent::HarnessControls::ALL,
        );
        // Attach external MCP tools (connect once, cached across turns).
        for tool in self.mcp_tools(tool_context.policy.mode()).await {
            registry.register(tool);
        }
        use leveler_agent::capability::{CapabilityDisclosure, CapabilityId};
        let mut available = vec![
            CapabilityId::Authoring,
            CapabilityId::HostInteraction,
            CapabilityId::MultiAgent,
        ];
        for (present, id) in [
            (available_packs.browser, CapabilityId::Browser),
            (available_packs.memory, CapabilityId::Memory),
            (available_packs.skills, CapabilityId::Skills),
            (
                available_packs.code_intelligence,
                CapabilityId::CodeIntelligence,
            ),
            (
                available_packs.web_fetch || available_packs.web_search,
                CapabilityId::Web,
            ),
            (available_packs.media, CapabilityId::Media),
            (
                registry
                    .definitions()
                    .iter()
                    .any(|tool| tool.name.starts_with("mcp__")),
                CapabilityId::ExternalTools,
            ),
        ] {
            if present {
                available.push(id);
            }
        }
        let permitted = available
            .iter()
            .copied()
            .filter(|id| {
                unrestricted
                    || match id {
                        CapabilityId::Memory => self.memory.any_lane(),
                        CapabilityId::MultiAgent => {
                            self.project_config().agents.delegation && self.config.agents_delegation
                        }
                        CapabilityId::Authoring => !read_only,
                        _ => true,
                    }
            })
            .collect();
        // Memory is part of the first-round surface when the project enables
        // it and the host can provide it. It stays a default EXPOSURE: no
        // grant is persisted, and tool approval rules are unchanged.
        let default_active = if available_packs.memory && self.memory.any_lane() {
            vec![CapabilityId::Memory]
        } else {
            Vec::new()
        };
        let capabilities = Arc::new(
            CapabilityDisclosure::new(available, permitted, vec![])
                .and_then(|state| state.with_default_active(default_active))
                .map_err(AppError::Engine)?,
        );
        // The global home root (never a cwd-relative `.leveler`, which would
        // read/write user config inside whatever directory we launched from).
        let leveler_home = leveler_core::LevelerHome::resolve(leveler_core::environment())
            .root()
            .to_path_buf();
        let permission_rules = self
            .layout
            .primary_workspace()
            .map(|root| {
                leveler_execution::load_merged_rules(
                    &leveler_home,
                    &self.layout.permissions_path(),
                    root,
                )
                .rules
            })
            .unwrap_or_default();
        let hook_runner = match self.layout.primary_workspace() {
            Some(root) => leveler_execution::HookRunner::load(&leveler_home, root),
            None => leveler_execution::HookRunner::empty(self.layout.state_dir.clone()),
        };
        let runtime: Arc<dyn ModelRuntime> = self.registry.clone();
        let db = self.open_database().await?;
        Ok(leveler_agent::coding::CodingRuntime {
            // The composition root chooses the adapter: every engine port is
            // backed by this repository's one SQLite database.
            engine: self.task_engine(&db)?,
            factory: leveler_agent::coding::ExecutorFactory {
                runtime,
                registry: Arc::new(registry),
                tool_context,
                model: model.clone(),
                commit_co_author: self.config.vcs_co_author,
                overrides: self
                    .execution_overrides_for_session(session_scope, Some(model))
                    .await?,
                capabilities: Some(capabilities),
                memory_expose: available_packs.memory,
                // One service for tools, recall and the background worker: a
                // run has no second store to fall back to.
                memory_service: self
                    .memory
                    .any_lane()
                    .then(|| Arc::new(memory_service(&self.layout))),
                memory_lanes: self.memory.lanes(),
                background_tasks: self.background_tasks.clone(),
                permission_rules,
                resource_grants: Arc::new(db.clone()),
                permission_rules_path: Some(self.layout.permissions_path()),
                hook_runner,
                // Per-session; attached by the caller that knows the session
                // (see `CodingRuntime::with_steering`).
                steering: None,
                // Project config wins over global when set; both default true.
                allow_delegation: unrestricted
                    || (self.project_config().agents.delegation && self.config.agents_delegation),
                allow_host_input: true,
                independent_review: combine_independent_review(
                    self.config.agents_independent_review,
                    self.project_config().agents.independent_review,
                ),
                // Carried, not resolved: `/develop` refuses a bad value where
                // the user can see which command it belongs to.
                develop_model: self.config.develop_model.clone(),
            },
            approver,
            clarifier,
            task_cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }

    /// Collaboration stored on the session row (SoT for resume). Independent of
    /// this Application's in-memory defaults.
    pub async fn session_product_axes(
        &self,
        session_id: &leveler_core::SessionId,
    ) -> Result<CollaborationMode, AppError> {
        let db = self.open_database().await?;
        let record = SessionRepository::new(&db)
            .get(session_id)
            .await?
            .ok_or_else(|| AppError::NotFound(session_id.to_string()))?;
        Ok(axes_from_session_record(&record))
    }
}

/// Decode product axes from a session row; unknown wire values fall back safely.
pub(crate) fn axes_from_session_record(
    record: &leveler_storage::SessionRecord,
) -> CollaborationMode {
    collaboration::collaboration_mode(&record.collaboration)
}

/// Compatibility marker for old wire clients. The retired database column
/// carries no runtime authority and every historical value has one semantics.
pub(crate) fn canonical_work_profile(_raw: &str) -> String {
    "single".to_string()
}

// Turn-time candidate extraction lives in the durable consolidator: the SQL
// inbox trigger records the turn, and the background worker calls the model
// extractor. A synchronous, rule-based second extractor here would be a
// competing policy owner and a second source of truth.

fn top_level_limits_from_config(
    config: &leveler_project::RunLimitsConfig,
) -> leveler_agent::StepLimits {
    leveler_agent::StepLimits {
        max_duration: config
            .max_duration_seconds
            .map(std::time::Duration::from_secs),
        max_model_tokens: config.max_model_tokens,
        max_cost_usd_micros: config.max_cost_usd_micros,
        ..leveler_agent::StepLimits::default()
    }
}

/// The env var names holding credentials for the configured providers (plus
/// the built-in search key). Scrubbed from every `run_command` child.
impl Application {
    /// The runner + request for one user shell execution (`!command`),
    /// mapped from the SAME policy inputs as agent shell execution: the
    /// session's permission profile decides write confinement, `sandbox`
    /// decides network denial, provider secrets are scrubbed, and cwd is the
    /// repository root. No default timeout — the user cancels explicitly;
    /// a 7-day backstop guards a forgotten process.
    pub(crate) fn user_shell_execution(
        &self,
        mode: leveler_execution::PermissionProfile,
        sandbox: bool,
        command: &str,
    ) -> Result<
        (
            leveler_execution::CommandRunner,
            leveler_execution::ProcessRequest,
            std::path::PathBuf,
        ),
        AppError,
    > {
        let cwd = self
            .layout
            .require_workspace()
            .map_err(|error| AppError::NotFound(error.to_string()))?
            .to_path_buf();
        let (program, args) = leveler_execution::shell_invocation(command);
        let mut request = leveler_execution::ProcessRequest::new(program, args, cwd.clone());
        request.timeout = std::time::Duration::from_secs(7 * 24 * 3600);
        request.deny_network = sandbox;
        request.deny_env =
            provider_secret_env_names(&self.config.providers, self.config.web_search.as_ref());
        request.write_scope = mode.write_scope(&cwd);
        let runner = leveler_execution::CommandRunner::with_environment(self.environment.clone());
        Ok((runner, request, cwd))
    }
}

/// Resolve the configured search backend.
///
/// Deliberately takes no model: the conversation model and the search vendor
/// are independent facts. `None` means search is unavailable, and the tool is
/// never constructed. `[web.search]` is the ONLY way to enable search: its
/// explicit provider and credential variable are the opt-in.
fn resolve_search_provider(
    settings: Option<&WebSearchSettings>,
    environment: &leveler_core::EnvSnapshot,
) -> Option<Arc<dyn SearchProvider>> {
    let settings = settings?;
    let key = environment
        .var(&settings.api_key_env)
        .map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty());
    let Some(key) = key else {
        tracing::warn!(
            provider = settings.provider.as_str(),
            api_key_env = %settings.api_key_env,
            "web search is configured but its credential variable is unset or blank; \
             web_search stays unavailable"
        );
        return None;
    };
    let resolved = match settings.provider {
        SearchProviderKind::DeepSeek => DeepSeekSearchProvider::new(
            key,
            settings.base_url.as_deref(),
            settings.max_results,
            settings.timeout,
        )
        .map(|provider| Arc::new(provider) as Arc<dyn SearchProvider>),
        SearchProviderKind::Tavily => TavilySearchProvider::new(
            key,
            settings.base_url.as_deref(),
            settings.max_results,
            settings.timeout,
        )
        .map(|provider| Arc::new(provider) as Arc<dyn SearchProvider>),
    };
    match resolved {
        Ok(provider) => Some(provider),
        Err(error) => {
            tracing::warn!(
                provider = settings.provider.as_str(),
                %error,
                "web search provider configuration is unusable; search stays unavailable"
            );
            None
        }
    }
}

pub(crate) fn provider_secret_env_names(
    providers: &[ProviderConfig],
    web_search: Option<&WebSearchSettings>,
) -> Vec<String> {
    let mut names: Vec<String> = providers
        .iter()
        .map(|p| p.api_key_env.clone())
        .filter(|n| !n.trim().is_empty())
        .collect();
    // The old Tavily variable is no longer a configuration source, but a
    // leftover value is still scrubbed so it cannot reach a child process.
    names.push("LEVELER_SEARCH_API_KEY".to_string());
    if let Some(search) = web_search {
        names.push(search.api_key_env.clone());
    }
    names.sort();
    names.dedup();
    names
}

/// Merge global providers underneath the repo bundle. A repo entry wins on id
/// collision, but credentials it does not carry are filled from the global
/// entry: a committed repo bundle never ships keys, so dropping the personal
/// global entry wholesale produced key-less requests that only failed at the
/// provider with a confusing upstream auth error.
fn merge_providers(
    repo: &mut Vec<leveler_provider::ProviderConfig>,
    global: Vec<leveler_provider::ProviderConfig>,
) {
    for provider in global {
        match repo.iter_mut().find(|p| p.id == provider.id) {
            Some(existing) => {
                if existing.api_key_env.trim().is_empty() {
                    existing.api_key_env = provider.api_key_env;
                }
                // Repo bundles rarely ship keys; fill plaintext key from global.
                let repo_has_key = existing
                    .api_key
                    .as_ref()
                    .map(|s| !s.trim().is_empty())
                    .unwrap_or(false);
                if !repo_has_key && let Some(key) = provider.api_key {
                    existing.api_key = Some(key);
                }
            }
            None => repo.push(provider),
        }
    }
}

#[cfg(test)]
mod merge_tests {
    use leveler_model::ProtocolKind;
    use leveler_provider::ProviderConfig;

    use super::{
        WebSearchSettings, merge_providers, provider_secret_env_names, resolve_search_provider,
        top_level_limits_from_config,
    };
    use leveler_tools::search::SearchProviderKind;

    fn provider(id: &str, api_key_env: Option<&str>) -> ProviderConfig {
        ProviderConfig {
            id: id.into(),
            protocol: ProtocolKind::OpenAiChat,
            base_url: "https://example.com".into(),
            api_key_env: api_key_env.unwrap_or_default().to_string(),
            api_key: None,
            headers: Default::default(),
            timeouts: Default::default(),
        }
    }

    #[test]
    fn repo_entry_without_key_gets_the_global_key() {
        let mut repo = vec![provider("deepseek", None)];
        merge_providers(
            &mut repo,
            vec![provider("deepseek", Some("DEEPSEEK_API_KEY"))],
        );
        assert_eq!(repo[0].api_key_env, "DEEPSEEK_API_KEY");
    }

    #[test]
    fn repo_env_ref_is_not_overwritten() {
        let mut repo = vec![provider("deepseek", Some("REPO_KEY_ENV"))];
        merge_providers(
            &mut repo,
            vec![provider("deepseek", Some("GLOBAL_KEY_ENV"))],
        );
        assert_eq!(repo[0].api_key_env, "REPO_KEY_ENV");
    }

    #[test]
    fn global_only_provider_is_appended() {
        let mut repo = vec![provider("deepseek", None)];
        merge_providers(&mut repo, vec![provider("bigmodel", Some("BIGMODEL_KEY"))]);
        assert_eq!(repo.len(), 2);
        assert_eq!(repo[1].id, "bigmodel");
    }

    fn env_with(search_key: Option<&str>) -> leveler_core::EnvSnapshot {
        let values = search_key.into_iter().map(|k| {
            (
                std::ffi::OsString::from("LEVELER_SEARCH_API_KEY"),
                std::ffi::OsString::from(k),
            )
        });
        leveler_core::EnvSnapshot::new(
            values,
            std::path::PathBuf::from("/tmp"),
            std::path::PathBuf::from("/tmp"),
        )
    }

    fn env_named(name: &str, value: Option<&str>) -> leveler_core::EnvSnapshot {
        let values = value
            .into_iter()
            .map(|k| (std::ffi::OsString::from(name), std::ffi::OsString::from(k)));
        leveler_core::EnvSnapshot::new(
            values,
            std::path::PathBuf::from("/tmp"),
            std::path::PathBuf::from("/tmp"),
        )
    }

    /// `[web.search]` is the only way to enable search. A leftover legacy
    /// Tavily variable does not turn it on.
    #[test]
    fn an_absent_search_section_disables_search() {
        assert!(resolve_search_provider(None, &env_with(Some("tvly-test-value"))).is_none());
        assert!(resolve_search_provider(None, &env_with(None)).is_none());
    }

    /// A configured DeepSeek search provider resolves from its own credential
    /// variable, with the official endpoint, and takes no model input at all.
    #[test]
    fn deepseek_search_resolves_independently_of_any_model() {
        let settings = WebSearchSettings {
            provider: SearchProviderKind::DeepSeek,
            api_key_env: "DEEPSEEK_API_KEY".into(),
            base_url: None,
            max_results: 5,
            timeout: std::time::Duration::from_secs(15),
        };
        let provider = resolve_search_provider(
            Some(&settings),
            &env_named("DEEPSEEK_API_KEY", Some("ds-test-key")),
        )
        .expect("configured and credentialed");
        assert_eq!(provider.id(), "deepseek");
        assert_eq!(
            provider.endpoint(),
            "https://api.deepseek.com/v1/tools/search"
        );
        assert_eq!(provider.max_results(), 5);
    }

    #[test]
    fn a_missing_search_credential_makes_search_unavailable() {
        let settings = WebSearchSettings {
            provider: SearchProviderKind::DeepSeek,
            api_key_env: "DEEPSEEK_API_KEY".into(),
            base_url: None,
            max_results: 5,
            timeout: std::time::Duration::from_secs(15),
        };
        assert!(resolve_search_provider(Some(&settings), &env_with(None)).is_none());
        assert!(
            resolve_search_provider(Some(&settings), &env_named("DEEPSEEK_API_KEY", Some("   ")))
                .is_none()
        );
    }

    /// Tavily remains available, but only through an explicit `[web.search]`
    /// section. Its default credential variable is the historical one.
    #[test]
    fn a_configured_tavily_provider_still_resolves() {
        let settings = WebSearchSettings {
            provider: SearchProviderKind::Tavily,
            api_key_env: "LEVELER_SEARCH_API_KEY".into(),
            base_url: None,
            max_results: 5,
            timeout: std::time::Duration::from_secs(15),
        };
        let provider = resolve_search_provider(Some(&settings), &env_with(Some("tvly-test-value")))
            .expect("explicitly configured");
        assert_eq!(provider.id(), "tavily");
        assert_eq!(provider.endpoint(), "https://api.tavily.com/search");
    }

    #[test]
    fn configured_search_secrets_are_scrubbed_from_children() {
        let settings = WebSearchSettings {
            provider: SearchProviderKind::DeepSeek,
            api_key_env: "DEEPSEEK_API_KEY".into(),
            base_url: None,
            max_results: 5,
            timeout: std::time::Duration::from_secs(15),
        };
        let names = provider_secret_env_names(&[provider("p", Some("P_KEY"))], Some(&settings));
        assert!(names.contains(&"P_KEY".to_string()));
        assert!(names.contains(&"DEEPSEEK_API_KEY".to_string()));
        assert!(names.contains(&"LEVELER_SEARCH_API_KEY".to_string()));
    }

    #[test]
    fn secret_env_names_cover_all_providers() {
        let a = provider("moonshot", Some("MOONSHOT_KEY"));
        let b = provider("deepseek", Some("DEEPSEEK_API_KEY"));
        let c = provider("keyless", None); // no api_key_env

        let names = provider_secret_env_names(&[a, b, c], None);
        assert!(names.contains(&"MOONSHOT_KEY".to_string()));
        assert!(names.contains(&"DEEPSEEK_API_KEY".to_string()));
        assert!(names.contains(&"LEVELER_SEARCH_API_KEY".to_string()));
        assert!(
            !names.iter().any(|n| n.is_empty()),
            "empty api_key_env must not produce an entry"
        );
    }

    #[test]
    fn project_config_maps_only_explicit_top_level_limits() {
        let limits = top_level_limits_from_config(&leveler_project::RunLimitsConfig {
            max_model_tokens: Some(200_000),
            max_cost_usd_micros: Some(500_000),
            max_duration_seconds: Some(7200),
        });
        assert_eq!(limits.max_model_tokens, Some(200_000));
        assert_eq!(limits.max_cost_usd_micros, Some(500_000));
        assert_eq!(
            limits.max_duration,
            Some(std::time::Duration::from_secs(7200))
        );
        assert_eq!(limits.max_commands, None);
        assert_eq!(limits.max_modified_files, None);
    }
}

/// The default model-step safety ceiling for a top-level run.
///
/// One source of truth with the kernel: this is the ceiling applied when a
/// host pins none, and the value `leveler run` uses when
/// `--max-model-steps`/`--max-rounds` is absent. It is a mechanical circuit
/// breaker against a runaway model↔tool loop, deliberately an order of
/// magnitude above the measured normal range (task runs close in 9–25 model
/// steps), and it must never be lowered to "bound useful work".
pub const DEFAULT_MODEL_STEP_CEILING: u32 = leveler_agent::DEFAULT_MODEL_STEP_CEILING;

/// `--max-model-steps` → safety ceiling: absent = default, `0` = no ceiling,
/// `n` = n.
pub fn model_step_ceiling_from_flag(max_model_steps: Option<u32>) -> Option<u32> {
    match max_model_steps {
        None => Some(DEFAULT_MODEL_STEP_CEILING),
        Some(0) => None,
        Some(n) => Some(n),
    }
}

#[cfg(test)]
mod model_step_ceiling_tests {
    use super::{DEFAULT_MODEL_STEP_CEILING, model_step_ceiling_from_flag};

    #[test]
    fn the_flag_maps_to_default_no_ceiling_or_a_ceiling() {
        assert_eq!(
            model_step_ceiling_from_flag(None),
            Some(DEFAULT_MODEL_STEP_CEILING)
        );
        assert_eq!(model_step_ceiling_from_flag(Some(0)), None);
        assert_eq!(model_step_ceiling_from_flag(Some(40)), Some(40));
    }

    /// The product default must BE the kernel's ceiling: one concept, one
    /// value. (The "far above normal runs" invariant is a compile-time
    /// assertion next to the kernel constant.)
    #[test]
    fn the_product_default_is_the_kernel_ceiling() {
        assert_eq!(
            DEFAULT_MODEL_STEP_CEILING,
            leveler_agent::DEFAULT_MODEL_STEP_CEILING
        );
        assert_eq!(
            model_step_ceiling_from_flag(None),
            Some(leveler_agent::DEFAULT_MODEL_STEP_CEILING),
            "a run with no flag must still get the mechanical breaker"
        );
    }
}

#[cfg(test)]
mod canonical_work_profile_tests {
    use super::canonical_work_profile;

    #[test]
    fn every_legacy_value_maps_to_single_semantics() {
        assert_eq!(canonical_work_profile("delivery"), "single");
        assert_eq!(canonical_work_profile("balanced"), "single");
        assert_eq!(canonical_work_profile("economy"), "single");
        // Unknown values fall back to the default, never to a retired value.
        assert_eq!(canonical_work_profile("nonsense"), "single");
    }
}
