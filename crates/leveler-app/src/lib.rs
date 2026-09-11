//! `leveler-app` — the composition root .
//!
//! Wires configuration into a running [`Application`]: loads provider/model/
//! bundles, resolves API keys from the environment, builds the
//! [`ProviderRegistry`], and opens the database. The CLI depends on this; this
//! crate depends on no CLI concerns.
#![forbid(unsafe_code)]

mod active_turns;
mod checkpoints;
pub mod contribution_query;
pub mod doctor;
/// Engine events -> client events. Public so a recorded session can be replayed
/// through the same projection a live client received, instead of a second
/// hand-written dialect that drifts from it.
pub mod event_bridge;
pub mod global_config;
pub mod goal_discovery;
mod goal_recap;
mod interactive;
mod live_view;
pub mod mcp_config;
pub mod observability;
mod parallel;
mod prompt_bridge;
mod runtime_identity;
mod session;
mod user_shell;
mod vcs;
mod workspace_view;

pub use global_config::{GlobalConfig, GlobalConfigError};
pub use interactive::InProcessRuntimeClient;
pub use parallel::{ParallelEditOutcome, acquire_parallel_parent_ownership};
pub use runtime_identity::{RuntimeIdentityError, load_or_create_runtime_id};
pub use session::engine_event_to_agent;
pub use vcs::ShipOptions;

use std::sync::{Arc, OnceLock};

use leveler_agent::{CollaborationMode, WorkProfile};
use leveler_execution::{PermissionProfile, Workspace};
use leveler_model::{ModelRef, ModelRuntime};
use leveler_project::{Layout, layout::yaml_files};
use leveler_provider::{
    ModelConfigFile, ProviderConfig, ProviderRegistry, RegistryInputs, load_model_config,
    load_provider_config, resolve_api_key,
};
use leveler_storage::{Database, SessionRepository};
use leveler_tools::{CapabilityPacks, ToolContext, model_surface};

/// Errors assembling the application.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("config error: {0}")]
    Config(#[from] leveler_provider::ConfigError),
    #[error("registry error: {0}")]
    Registry(#[from] leveler_provider::RegistryError),
    #[error("storage error: {0}")]
    Storage(#[from] leveler_storage::StorageError),
    #[error("model error: {0}")]
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
    #[error("serialization error: {0}")]
    Serde(String),
    #[error("global config error: {0}")]
    GlobalConfig(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("io error creating {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("runtime identity error: {0}")]
    RuntimeIdentity(#[from] runtime_identity::RuntimeIdentityError),
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
    /// Whether CodeLeveler-authored commits include a model-aware co-author trailer.
    pub vcs_co_author: bool,
    /// External MCP servers to expose to the model.
    pub mcp_servers: Vec<leveler_tools::mcp::McpServerConfig>,
    /// Global multi-agent kill-switch default (project config can override).
    pub agents_delegation: bool,
    /// Whether the harness launches an independent reviewer (default Off).
    pub agents_independent_review: leveler_project::IndependentReview,
    /// `[browser].default`: the browser the browser capability drives. `None`
    /// means the operating system's default browser.
    pub browser_default: Option<leveler_browser::BrowserProduct>,
}

impl Default for LoadedConfig {
    fn default() -> Self {
        Self {
            providers: Vec::new(),
            models: Vec::new(),
            default_model: None,
            lang: None,
            vcs_co_author: true,
            mcp_servers: Vec::new(),
            agents_delegation: true,
            agents_independent_review: leveler_project::IndependentReview::default(),
            browser_default: None,
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

/// A fully-assembled application.
pub struct Application {
    pub layout: Layout,
    pub config: LoadedConfig,
    pub registry: Arc<ProviderRegistry>,
    /// Lazily-connected MCP tools, shared across executors (connect once).
    mcp_tools: Arc<tokio::sync::Mutex<Option<Vec<Arc<dyn leveler_tools::tool::Tool>>>>>,
    /// The session database pool, opened once per process.
    database: Arc<tokio::sync::Mutex<Option<Database>>>,
    /// When set, overrides the resolved execution policy on every execution
    /// path (single-knob ablation runs). `None` = resolver defaults.
    execution_overrides: Option<leveler_agent::coding::ExecutionOverrides>,
    /// Product work profile (economy / balanced / delivery).
    work_profile: WorkProfile,
    /// Collaboration mode (chat / plan / goal).
    collaboration: CollaborationMode,
    /// Round limit for headless goal runs (`leveler run`). `None` keeps a
    /// goal until-terminal; the CLI sets it from `--max-rounds` or the
    /// default. Eval and the interactive UI never set it.
    task_round_limit: Option<u32>,
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
    /// The live permission profile of each session, keyed by session scope.
    ///
    /// The persisted `sessions.mode` column is the durable record; this is the
    /// same value as running state, so a change reaches a turn that is already
    /// executing. Per session — never one process-wide profile — because one
    /// daemon hosts several sessions at different profiles at once.
    permission_profiles: std::sync::Mutex<
        std::collections::HashMap<String, leveler_execution::SharedPermissionProfile>,
    >,
}

impl Application {
    /// The live permission profile for a session, created on first use from
    /// `mode` (the caller's authority — the session row or its cached runtime
    /// config) and reused by every later turn of that session.
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
        let profile = map
            .entry(scope.to_string())
            .or_insert_with(|| leveler_execution::SharedPermissionProfile::new(mode))
            .clone();
        // A turn starts from the current authority, so the cell and the
        // session row cannot drift apart across turns.
        profile.set(mode);
        profile
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
        let global = global_config::GlobalConfig::load()
            .map_err(|e| AppError::GlobalConfig(e.to_string()))?
            .into_bundle();
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
            mcp_servers: global.mcp_servers,
            agents_delegation: global.agents_delegation,
            agents_independent_review: global.agents_independent_review,
            browser_default: global.browser_default,
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
            std::env::current_dir().unwrap_or_else(|_| layout.repo_root.clone()),
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

        let config = Self::load_config(&layout)?;

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
        let background_tasks = Arc::new(
            leveler_execution::BackgroundTaskRegistry::with_environment(environment.clone()),
        );
        // Lazy: this holds only paths and the configured product until the
        // first navigate actually starts a browser.
        let browser = Arc::new(leveler_browser::Browser::new(
            (*environment).clone(),
            layout.browser_profile_dir(),
            config.browser_default,
        ));
        Ok(Self {
            layout,
            config,
            registry: Arc::new(registry),
            mcp_tools: Arc::new(tokio::sync::Mutex::new(None)),
            database: Arc::new(tokio::sync::Mutex::new(None)),
            execution_overrides: None,
            work_profile: WorkProfile::Balanced,
            collaboration: CollaborationMode::Chat,
            task_round_limit: None,
            environment,
            background_tasks,
            browser,
            runtime_id: OnceLock::new(),
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

    /// Set work profile for subsequent engine builds and session creates.
    pub fn with_work_profile(mut self, profile: WorkProfile) -> Self {
        self.work_profile = profile;
        self
    }

    /// Set collaboration mode for subsequent session creates.
    pub fn with_collaboration(mut self, mode: CollaborationMode) -> Self {
        self.collaboration = mode;
        self
    }

    /// Engine-paced task budget for headless goal runs, from the CLI's
    /// `--max-rounds`: absent means the default ([`DEFAULT_TASK_ROUNDS`]),
    /// `0` means unbounded, `n` means `n` rounds. A hard limit: the run
    /// stops there and reports it, nothing extends it.
    pub fn with_task_round_budget(mut self, max_rounds: Option<u32>) -> Self {
        self.task_round_limit = task_round_limit_from_flag(max_rounds);
        self
    }

    pub fn work_profile(&self) -> WorkProfile {
        self.work_profile
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
        Ok(guard.get_or_insert_with(|| db).clone())
    }

    /// All configured model references.
    pub fn model_refs(&self) -> Vec<ModelRef> {
        self.registry.model_refs()
    }

    /// The parsed `.leveler/config.yaml` for the repo (defaults if absent).
    pub fn project_config(&self) -> leveler_project::ProjectConfig {
        leveler_project::ProjectConfig::load(&self.layout.repo_root).unwrap_or_default()
    }

    pub(crate) fn top_level_limits(&self) -> leveler_agent::StepLimits {
        top_level_limits_from_config(&self.project_config().limits)
    }

    /// Connect to the configured MCP servers once and cache their tools, so
    /// every turn reuses the same connections instead of respawning processes.
    async fn mcp_tools(&self) -> Vec<Arc<dyn leveler_tools::tool::Tool>> {
        if self.config.mcp_servers.is_empty() {
            return Vec::new();
        }
        let mut guard = self.mcp_tools.lock().await;
        if guard.is_none() {
            *guard = Some(leveler_tools::mcp::connect_all(&self.config.mcp_servers).await);
        }
        guard.clone().unwrap_or_default()
    }

    /// Build the Coding harness for `model`, rooted at the repository. Uses this Application's work profile
    /// (CLI / create-time default). Resume must call
    /// [`Self::engine_for_with_profile`] with axes loaded from the session row.
    pub async fn engine_for(
        &self,
        model: &ModelRef,
        mode: PermissionProfile,
        sandbox: bool,
        approver: Arc<dyn leveler_execution::Approver>,
        clarifier: Arc<dyn leveler_agent::Clarifier>,
    ) -> Result<leveler_agent::coding::CodingRuntime, AppError> {
        self.engine_for_with_profile(
            model,
            mode,
            sandbox,
            approver,
            clarifier,
            self.work_profile,
            false,
            None,
        )
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
            code_intelligence: true,
            // Both git tools shell out to `git`; without the binary each one
            // can only report that it could not start.
            vcs: leveler_browser::which(environment, "git").is_some(),
            web_fetch: true,
            // Advertising a search tool without a key spends schema on a call
            // that can only fail. A blank value is not a configuration.
            web_search: search_api_key(environment).is_some(),
            // An image is useless to a model that cannot read one.
            media: self
                .registry
                .profile(model)
                .await
                .map(|profile| profile.capabilities.vision)
                .unwrap_or(false),
            // The app always hands the tools a memory root and a workspace to
            // read skills from.
            memory: true,
            skills: true,
            // Not "is a browser installed": is the browser this host would
            // SELECT — the call's, then `[browser].default`, then the system
            // default — one it can actually drive? Answering with a different
            // browser is exactly what the no-fallback rule forbids, so a user
            // whose default is a browser CodeLeveler cannot drive sees no
            // browser tools rather than a surprise one.
            browser: self.browser.resolve(None).is_ok(),
        }
    }

    /// ENABLED: which optional capabilities this product mode ASKS for.
    ///
    /// A user's decision about cost and scope, never an inference about the
    /// task. `Economy` asks for none of them — the primitives and the protocol
    /// only — which is why a machine with a browser runtime installed still
    /// shows an Economy turn zero browser tools.
    fn capability_selection(work_profile: WorkProfile) -> CapabilityPacks {
        match work_profile {
            WorkProfile::Economy => CapabilityPacks::NONE,
            WorkProfile::Balanced | WorkProfile::Delivery => CapabilityPacks::ALL,
        }
    }

    /// EXPOSED: the packs that actually reach the model's surface.
    ///
    /// Enabled ∩ available. Being available buys nothing on its own, and
    /// asking for something this machine cannot do buys nothing either.
    async fn exposed_capabilities(
        &self,
        work_profile: WorkProfile,
        model: &leveler_model::ModelRef,
    ) -> CapabilityPacks {
        Self::capability_selection(work_profile)
            .intersect(self.capability_availability(model).await)
    }

    /// Like [`Self::engine_for`], but force a work profile (resume / axes reload).
    #[allow(clippy::too_many_arguments)]
    pub async fn engine_for_with_profile(
        &self,
        model: &ModelRef,
        mode: PermissionProfile,
        sandbox: bool,
        approver: Arc<dyn leveler_execution::Approver>,
        clarifier: Arc<dyn leveler_agent::Clarifier>,
        work_profile: WorkProfile,
        read_only: bool,
        session_scope: Option<&str>,
    ) -> Result<leveler_agent::coding::CodingRuntime, AppError> {
        let workspace = Workspace::new(&self.layout.repo_root)?;
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
        let bg = self.background_tasks.clone();
        // The capability handles this host owns. They reach the TOOLS at
        // construction (below) and the RUNTIME through its own fields — never
        // through the tool context, which carries authority and nothing else.
        let capabilities = leveler_tools::Capabilities::in_process(self.environment.clone())
            .with_background_tasks(bg.clone())
            .with_artifact_store(artifact_store)
            .with_memory_root(self.layout.memory_dir())
            .with_browser(self.browser.clone())
            .with_search_api_key(search_api_key(self.environment.as_ref()));
        let tool_context = ToolContext::with_environment(workspace, mode, self.environment.clone())
            .with_policy_limits(max_files)
            .with_sandbox(sandbox)
            .with_deny_env(provider_secret_env_names(&self.config.providers))
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
        // The model-visible surface is composed here, from what this host can
        // actually do — never from a guess about the task or the model.
        let mut registry = model_surface(
            self.exposed_capabilities(work_profile, model).await,
            &capabilities,
        );
        // Harness controls are not a capability the host can turn off: they
        // steer the harness, so the harness registers them.
        leveler_agent::register_harness_controls(&mut registry);
        // Attach external MCP tools (connect once, cached across turns).
        for tool in self.mcp_tools().await {
            registry.register(tool);
        }
        let memory_index = load_memory_index(&self.layout.memory_dir());
        // The global home root (never a cwd-relative `.leveler`, which would
        // read/write user config inside whatever directory we launched from).
        let leveler_home = leveler_core::LevelerHome::resolve(leveler_core::environment())
            .root()
            .to_path_buf();
        let merged_rules = leveler_execution::load_merged_rules(
            &leveler_home,
            &self.layout.permissions_path(),
            &self.layout.repo_root,
        );
        let permission_rules = merged_rules.rules;
        let hook_runner =
            leveler_execution::HookRunner::load(&leveler_home, &self.layout.repo_root);
        let runtime: Arc<dyn ModelRuntime> = self.registry.clone();
        Ok(leveler_agent::coding::CodingRuntime {
            engine: leveler_engine::TaskEngine {
                // The composition root chooses the adapter: every engine port
                // backed by this repository's SQLite database (shared pool).
                stores: leveler_storage::EngineStores::from_database(&self.open_database().await?),
                runtime_id: self.runtime_id()?,
            },
            factory: leveler_agent::coding::ExecutorFactory {
                runtime,
                registry: Arc::new(registry),
                tool_context,
                model: model.clone(),
                commit_co_author: self.config.vcs_co_author,
                overrides: self.execution_overrides.clone(),
                memory_index,
                memory_root: Some(self.layout.memory_dir()),
                background_tasks: bg,
                permission_rules,
                permission_rules_path: Some(self.layout.permissions_path()),
                hook_runner,
                // Per-session; attached by the caller that knows the session
                // (see `CodingRuntime::with_steering`).
                steering: None,
                // Project config wins over global when set; both default true.
                allow_delegation: self.project_config().agents.delegation
                    && self.config.agents_delegation,
                independent_review: combine_independent_review(
                    self.config.agents_independent_review,
                    self.project_config().agents.independent_review,
                ),
            },
            approver,
            clarifier,
        })
    }

    /// Product axes stored on the session row (SoT for resume). Independent of
    /// this Application's in-memory defaults.
    pub async fn session_product_axes(
        &self,
        session_id: &leveler_core::SessionId,
    ) -> Result<(WorkProfile, CollaborationMode), AppError> {
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
) -> (WorkProfile, CollaborationMode) {
    use std::str::FromStr;
    let work = WorkProfile::from_str(&record.work_profile).unwrap_or(WorkProfile::Balanced);
    let collab =
        CollaborationMode::from_str(&record.collaboration).unwrap_or(CollaborationMode::Chat);
    (work, collab)
}

/// Load short memory INDEX for system injection (titles only; empty if none).
pub(crate) fn load_memory_index(memory_dir: &std::path::Path) -> String {
    match leveler_memory::MemoryStore::open(memory_dir) {
        Ok(store) => store.index_lines(32).unwrap_or_default(),
        Err(err) => {
            tracing::debug!(error = %err, "memory index unavailable");
            String::new()
        }
    }
}

impl Application {
    /// Enqueue pending memory candidates from this turn's user text and
    /// package-manager signals. Never writes `active/` (K36: accept is separate).
    pub(crate) fn enqueue_memory_candidates(&self, user_text: &str) {
        let memory_dir = self.layout.memory_dir();
        let Ok(store) = leveler_memory::MemoryStore::open(&memory_dir) else {
            return;
        };
        match leveler_memory::collect_turn_candidates(
            &store,
            user_text,
            Some(self.layout.repo_root.as_path()),
        ) {
            Ok(outcomes) => {
                let pending = outcomes
                    .iter()
                    .filter(|o| matches!(o, leveler_memory::ProposeOutcome::Pending(_)))
                    .count();
                if pending > 0 {
                    tracing::info!(pending, "enqueued memory candidates (await user accept)");
                }
            }
            Err(err) => {
                tracing::debug!(error = %err, "memory candidate enqueue skipped");
            }
        }
    }
}

#[cfg(test)]
mod memory_index_tests {
    use super::*;
    use leveler_memory::{ProposeOutcome, collect_turn_candidates};
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn index_lists_titles_not_bodies() {
        let dir = tempdir().unwrap();
        let mem = dir.path().join("memory");
        let store = leveler_memory::MemoryStore::open(&mem).unwrap();
        store
            .remember(leveler_memory::new_entry(
                "Use workspace write",
                "SECRET_BODY_NEVER_IN_INDEX",
                vec![],
            ))
            .unwrap();
        let index = load_memory_index(&mem);
        assert!(index.contains("Use workspace write"), "{index}");
        assert!(!index.contains("SECRET_BODY"), "{index}");
    }

    /// Host path used by `Application::enqueue_memory_candidates` — propose only.
    #[test]
    fn turn_candidate_collect_never_writes_active() {
        let mem = tempdir().unwrap();
        let repo = tempdir().unwrap();
        fs::write(repo.path().join("pnpm-lock.yaml"), "").unwrap();
        let store = leveler_memory::MemoryStore::open(mem.path()).unwrap();
        let outcomes = collect_turn_candidates(&store, "记住：用 pnpm", Some(repo.path())).unwrap();
        assert!(
            outcomes
                .iter()
                .any(|o| matches!(o, ProposeOutcome::Pending(_))),
            "{outcomes:?}"
        );
        assert_eq!(store.list_active().unwrap().len(), 0);
        assert!(!store.list_pending().unwrap().is_empty());
    }

    #[test]
    fn missing_store_yields_empty_index() {
        let dir = tempdir().unwrap();
        // path does not exist yet — open creates it empty
        let index = load_memory_index(&dir.path().join("nope-yet"));
        assert!(index.is_empty() || !index.contains("SECRET"));
    }
}

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
        let cwd = self.layout.repo_root.clone();
        let (program, args) = leveler_execution::shell_invocation(command);
        let mut request = leveler_execution::ProcessRequest::new(program, args, cwd.clone());
        request.timeout = std::time::Duration::from_secs(7 * 24 * 3600);
        request.deny_network = sandbox;
        request.deny_env = provider_secret_env_names(&self.config.providers);
        request.write_scope = mode.write_scope(&cwd);
        let runner = leveler_execution::CommandRunner::with_environment(self.environment.clone());
        Ok((runner, request, cwd))
    }
}

/// The one reader of `LEVELER_SEARCH_API_KEY` (a Tavily key). `None` when
/// unset, empty or whitespace — availability and the tool's construction both
/// come from here, so they can never disagree.
fn search_api_key(environment: &leveler_core::EnvSnapshot) -> Option<String> {
    environment
        .var("LEVELER_SEARCH_API_KEY")
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
}

pub(crate) fn provider_secret_env_names(providers: &[ProviderConfig]) -> Vec<String> {
    let mut names: Vec<String> = providers
        .iter()
        .map(|p| p.api_key_env.clone())
        .filter(|n| !n.trim().is_empty())
        .collect();
    names.push("LEVELER_SEARCH_API_KEY".to_string());
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

    use super::{merge_providers, provider_secret_env_names, top_level_limits_from_config};

    fn provider(id: &str, api_key_env: Option<&str>) -> ProviderConfig {
        ProviderConfig {
            id: id.into(),
            protocol: ProtocolKind::OpenAiChat,
            base_url: "https://example.com".into(),
            api_key_env: api_key_env.unwrap_or_default().to_string(),
            api_key: None,
            headers: Default::default(),
            timeouts: Default::default(),
            retry: Default::default(),
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

    /// Case A — no key at all. `web_search` is not AVAILABLE, so it never
    /// reaches the model surface however much the product mode wants it.
    #[test]
    fn no_search_key_is_not_available() {
        assert_eq!(crate::search_api_key(&env_with(None)), None);
    }

    /// Case B — a key that is present but blank is not a configuration. A
    /// `Some("")` that counted as configured would advertise a tool whose
    /// every call is a 401.
    #[test]
    fn blank_search_key_is_not_available() {
        assert_eq!(crate::search_api_key(&env_with(Some(""))), None);
        assert_eq!(crate::search_api_key(&env_with(Some("   "))), None);
        assert_eq!(crate::search_api_key(&env_with(Some("\t\n"))), None);
    }

    /// Case C — a real key is AVAILABLE, and reaches the tool trimmed.
    #[test]
    fn a_real_search_key_is_available_and_trimmed() {
        assert_eq!(
            crate::search_api_key(&env_with(Some("tvly-test-value"))).as_deref(),
            Some("tvly-test-value")
        );
        assert_eq!(
            crate::search_api_key(&env_with(Some("  tvly-test-value  "))).as_deref(),
            Some("tvly-test-value"),
            "surrounding whitespace is not part of the key"
        );
    }

    #[test]
    fn secret_env_names_cover_all_providers() {
        let a = provider("moonshot", Some("MOONSHOT_KEY"));
        let b = provider("deepseek", Some("DEEPSEEK_API_KEY"));
        let c = provider("keyless", None); // no api_key_env

        let names = provider_secret_env_names(&[a, b, c]);
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

/// The default round limit for `leveler run`. From the C2 batches: every
/// run that closed did so within 169 rounds.
pub const DEFAULT_TASK_ROUNDS: u32 = 200;

/// `--max-rounds` → round limit: absent = default, `0` = unbounded, `n` = n.
pub fn task_round_limit_from_flag(max_rounds: Option<u32>) -> Option<u32> {
    match max_rounds {
        None => Some(DEFAULT_TASK_ROUNDS),
        Some(0) => None,
        Some(n) => Some(n),
    }
}

/// The continuation a goal runs under for a given round limit: pinned to the
/// limit when there is one, until-terminal otherwise.
pub fn goal_continuation_for(limit: Option<u32>) -> leveler_agent::ContinuationPolicy {
    match limit {
        Some(n) => leveler_agent::ContinuationPolicy::bounded(n),
        None => leveler_agent::ContinuationPolicy::UntilTerminal,
    }
}

#[cfg(test)]
mod task_round_limit_tests {
    use super::{DEFAULT_TASK_ROUNDS, goal_continuation_for, task_round_limit_from_flag};

    #[test]
    fn the_flag_maps_to_default_unbounded_or_a_limit() {
        assert_eq!(task_round_limit_from_flag(None), Some(DEFAULT_TASK_ROUNDS));
        assert_eq!(task_round_limit_from_flag(Some(0)), None);
        assert_eq!(task_round_limit_from_flag(Some(40)), Some(40));
    }

    /// The bug exp8 ran with: a limit on the spec and an until-terminal
    /// continuation pinned over it is a limit that never binds.
    #[test]
    fn a_limit_pins_the_continuation() {
        assert_eq!(
            goal_continuation_for(Some(DEFAULT_TASK_ROUNDS)).round_limit(),
            Some(DEFAULT_TASK_ROUNDS)
        );
        assert_eq!(goal_continuation_for(None).round_limit(), None);
    }
}
