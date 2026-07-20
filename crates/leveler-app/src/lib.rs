//! `leveler-app` — the composition root .
//!
//! Wires configuration into a running [`Application`]: loads provider/model/
//! bundles, resolves API keys from the environment, builds the
//! [`ProviderRegistry`], and opens the database. The CLI depends on this; this
//! crate depends on no CLI concerns.
#![forbid(unsafe_code)]

mod active_turns;
pub mod doctor;
mod event_bridge;
pub mod global_config;
mod interactive;
pub mod mcp_config;
mod orchestrate;
mod parallel;
mod prompt_bridge;
mod session;
mod vcs;
mod workspace_view;

pub use global_config::{GlobalConfig, GlobalConfigError};
pub use interactive::InProcessRuntimeClient;
pub use parallel::ParallelEditOutcome;
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
use leveler_tools::{ToolContext, core_registry, full_registry};

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
    #[error("orchestrator error: {0}")]
    Orchestrator(#[from] leveler_orchestrator::OrchestratorError),
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
        }
    }
}

/// CLI `--readonly-root` values registered once at process start (before assemble).
static PROCESS_READONLY_ROOTS: OnceLock<Vec<std::path::PathBuf>> = OnceLock::new();

/// Record extra read-only roots from the CLI (composition root). Safe to call once.
pub fn set_process_readonly_roots(roots: Vec<std::path::PathBuf>) {
    let _ = PROCESS_READONLY_ROOTS.set(roots);
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
    execution_overrides: Option<leveler_engine::ExecutionOverrides>,
    /// Extra trees allowed for read-only file tools (cross-repo compare).
    readonly_roots: Vec<std::path::PathBuf>,
    /// Product work profile (economy / balanced / delivery).
    work_profile: WorkProfile,
    /// Collaboration mode (chat / plan / goal).
    collaboration: CollaborationMode,
    environment: Arc<leveler_core::EnvSnapshot>,
    /// Process-lived background task registry, shared (cloned) into every
    /// engine/turn so `background=true` servers survive between messages. A
    /// per-engine registry was dropped at turn end, and its `KillOnDrop` reaped
    /// every background process — hence servers dying between turns. Only the
    /// process exit drops the last handle.
    background_tasks: Arc<leveler_execution::BackgroundTaskRegistry>,
}

impl Application {
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
        let config = Self::load_config(&layout)?;

        let providers = config
            .providers
            .iter()
            .map(|cfg| {
                let key = match resolve_api_key(cfg) {
                    Ok(key) => key,
                    Err(error) => {
                        // Keep assembling (doctor reports the gap), but say so:
                        // a silently key-less provider fails much later with a
                        // confusing upstream auth error.
                        tracing::warn!("provider `{}` has no usable API key: {error}", cfg.id);
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
        let readonly_roots = Self::default_readonly_roots(&layout.repo_root);
        let background_tasks = Arc::new(
            leveler_execution::BackgroundTaskRegistry::with_environment(environment.clone()),
        );
        Ok(Self {
            layout,
            config,
            registry: Arc::new(registry),
            mcp_tools: Arc::new(tokio::sync::Mutex::new(None)),
            database: Arc::new(tokio::sync::Mutex::new(None)),
            execution_overrides: None,
            readonly_roots,
            work_profile: WorkProfile::Balanced,
            collaboration: CollaborationMode::Chat,
            environment,
            background_tasks,
        })
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

    pub fn work_profile(&self) -> WorkProfile {
        self.work_profile
    }

    pub fn collaboration(&self) -> CollaborationMode {
        self.collaboration
    }

    /// Paths from `.leveler/config.yaml` `readonly_roots` and optional
    /// `LEVELER_READONLY_ROOTS` (OS path-list separator: `:` on Unix, `;` on Windows).
    fn default_readonly_roots(repo_root: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        if let Some(cfg) = leveler_project::ProjectConfig::load(repo_root) {
            for entry in cfg.readonly_roots {
                let p = std::path::PathBuf::from(entry.trim());
                if p.as_os_str().is_empty() {
                    continue;
                }
                out.push(if p.is_absolute() {
                    p
                } else {
                    repo_root.join(p)
                });
            }
        }
        if let Ok(raw) = std::env::var("LEVELER_READONLY_ROOTS") {
            let sep = if cfg!(windows) { ';' } else { ':' };
            for part in raw.split(sep) {
                let part = part.trim();
                if !part.is_empty() {
                    out.push(std::path::PathBuf::from(part));
                }
            }
        }
        if let Some(cli) = PROCESS_READONLY_ROOTS.get() {
            out.extend(cli.iter().cloned());
        }
        out
    }

    /// Extra directories the agent may read (not write). Missing paths are ignored.
    pub fn with_readonly_roots(
        mut self,
        roots: impl IntoIterator<Item = std::path::PathBuf>,
    ) -> Self {
        for root in roots {
            if !self.readonly_roots.iter().any(|r| r == &root) {
                self.readonly_roots.push(root);
            }
        }
        self
    }

    /// Override the resolved execution policy for every execution path — the
    /// `leveler eval ablate` seam. Use on a freshly assembled Application so
    /// control and ablated runs differ in exactly the flipped knob.
    pub fn with_execution_overrides(
        mut self,
        overrides: leveler_engine::ExecutionOverrides,
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

    /// Look up the provider config backing a model reference.
    pub fn provider_for(&self, model: &ModelRef) -> Option<&ProviderConfig> {
        self.config
            .providers
            .iter()
            .find(|p| p.id == model.provider)
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

    /// Build the persistent [`leveler_engine::TaskEngine`] for `model`,
    /// rooted at the repository. Uses this Application's work profile
    /// (CLI / create-time default). Resume must call
    /// [`Self::engine_for_with_profile`] with axes loaded from the session row.
    pub async fn engine_for(
        &self,
        model: &ModelRef,
        mode: PermissionProfile,
        sandbox: bool,
        approver: Arc<dyn leveler_execution::Approver>,
        clarifier: Arc<dyn leveler_agent::Clarifier>,
    ) -> Result<leveler_engine::TaskEngine, AppError> {
        self.engine_for_with_profile(
            model,
            mode,
            sandbox,
            approver,
            clarifier,
            self.work_profile,
            false,
        )
        .await
    }

    /// Like [`Self::engine_for`], but force a work profile (resume / axes reload).
    pub async fn engine_for_with_profile(
        &self,
        model: &ModelRef,
        mode: PermissionProfile,
        sandbox: bool,
        approver: Arc<dyn leveler_execution::Approver>,
        clarifier: Arc<dyn leveler_agent::Clarifier>,
        work_profile: WorkProfile,
        read_only: bool,
    ) -> Result<leveler_engine::TaskEngine, AppError> {
        let workspace = Workspace::new(&self.layout.repo_root)?
            .with_readonly_roots(self.readonly_roots.iter().cloned());
        // The ablation seam (`leveler eval ablate`): overrides reach BOTH
        // consumers — the executor factory's resolver and the tool-context
        // limits — so a run differs from control in exactly the flipped knob.
        // Every execution path (direct, orchestrated, bare) funnels through
        // here.
        let (max_files, read_guard) =
            leveler_engine::resolve_tool_limits(self.execution_overrides.as_ref());
        let artifact_store = std::sync::Arc::new(leveler_execution::ArtifactStore::new(
            self.layout.state_dir.join("artifacts"),
        ));
        // Reuse the process-lived registry so background servers/watchers
        // survive across turns. A fresh per-engine registry was dropped when the
        // turn's engine went out of scope, and its KillOnDrop killed every
        // background process (and the next turn's registry no longer knew the
        // task id) — the "服务活不过一个回合" bug.
        let bg = self.background_tasks.clone();
        let tool_context = ToolContext::with_environment(workspace, mode, self.environment.clone())
            .with_policy_limits(max_files, read_guard)
            .with_sandbox(sandbox)
            .with_auto_format(true)
            .with_deny_env(provider_secret_env_names(&self.config.providers))
            .with_artifact_store(artifact_store)
            .with_memory_root(self.layout.memory_dir())
            .with_read_only(read_only)
            .with_background_tasks(bg);
        // Economy ships Core tool surface; balanced/delivery use Full.
        let mut registry = match work_profile {
            WorkProfile::Economy => core_registry(),
            WorkProfile::Balanced | WorkProfile::Delivery => full_registry(),
        };
        // Attach external MCP tools (connect once, cached across turns).
        for tool in self.mcp_tools().await {
            registry.register(tool);
        }
        let memory_index = load_memory_index(&self.layout.memory_dir());
        let leveler_home = leveler_core::leveler_home_dir_from(|k| std::env::var_os(k))
            .unwrap_or_else(|| std::path::PathBuf::from(".leveler"));
        let permission_rules =
            leveler_execution::load_merged_rules(&leveler_home, &self.layout.repo_root);
        let hook_runner =
            leveler_execution::HookRunner::load(&leveler_home, &self.layout.repo_root);
        let runtime: Arc<dyn ModelRuntime> = self.registry.clone();
        Ok(leveler_engine::TaskEngine {
            db: self.open_database().await?,
            factory: leveler_engine::ExecutorFactory {
                runtime,
                registry: Arc::new(registry),
                tool_context,
                model: model.clone(),
                commit_co_author: self.config.vcs_co_author,
                overrides: self.execution_overrides.clone(),
                work_profile,
                memory_index,
                permission_rules,
                permission_rules_path: Some(leveler_execution::project_rules_path(
                    &self.layout.repo_root,
                )),
                hook_runner,
                grants_state_dir: Some(self.layout.state_dir.clone()),
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

#[cfg(test)]
mod memory_index_tests {
    use super::*;
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
