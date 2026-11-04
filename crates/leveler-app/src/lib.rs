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
pub mod config_manager;
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
pub mod usage;
mod user_shell;
mod vcs;
mod workspace_view;

pub use collaboration::CollaborationExecution;
pub use config_manager::{
    ConfigApplied, ConfigChange, ConfigManager, ConfigSnapshot, ReloadOrigin, ReloadOutcome,
};
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
    ModelConfigFile, ProviderConfig, ProviderRegistry, load_model_config, load_provider_config,
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
    /// An earlier tool call was interrupted before its outcome was recorded.
    /// Continuing would risk running it twice, so the task stops here until
    /// the user acknowledges the crash window.
    #[error(
        "The previous {tool} call was interrupted before its result was confirmed (call {call_id}). \
         To avoid running it twice, this task is paused; check the workspace, then resume with \
         --confirm-recovery."
    )]
    RecoveryConfirmationRequired { call_id: String, tool: String },
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
#[derive(Debug, Clone, PartialEq)]
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

/// Load every configured source into one effective bundle.
///
/// This is the ONLY assembly of a configuration generation: startup, the
/// watcher and `/reload` all call it, so they cannot disagree about precedence
/// (global `~/.leveler/config.toml` underneath the repository bundle) or about
/// validation.
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
    let global_config =
        global_config::GlobalConfig::load().map_err(|e| AppError::GlobalConfig(e.to_string()))?;
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

/// The process-lived MCP connection cache, owned by the application and shared
/// with each turn's tool-discovery adapter.
///
/// It is a separate type from `Application` so a turn can hold it for its whole
/// life: the tools a turn may grow into are the same connections every other
/// turn uses, not a second copy.
pub(crate) struct McpToolSource {
    cache: tokio::sync::Mutex<Option<McpToolCache>>,
    environment: leveler_core::EnvSnapshot,
    has_workspace: bool,
}

impl McpToolSource {
    fn new(environment: leveler_core::EnvSnapshot, has_workspace: bool) -> Self {
        Self {
            cache: tokio::sync::Mutex::new(None),
            environment,
            has_workspace,
        }
    }
    /// Tools of the configured MCP servers, connecting at most once per
    /// generation. A reload diffs the server set: unchanged servers keep their
    /// live connection, only a changed/added server is connected again, and a
    /// removed server's tools simply drop out (its child is reaped with them).
    ///
    /// A failed attempt is retried on a later request with backoff, so a server
    /// that starts after CodeLeveler is discovered without a config change.
    async fn tools(
        &self,
        snapshot: &ConfigSnapshot,
        mode: PermissionProfile,
    ) -> Vec<Arc<dyn leveler_tools::tool::Tool>> {
        if !self.has_workspace || snapshot.loaded.mcp_servers.is_empty() {
            // A generation that removed every server ends the connections the
            // previous generation held; an unused connection is not a cache.
            self.cache.lock().await.take();
            return Vec::new();
        }
        let unrestricted = mode == PermissionProfile::FullAccess;
        let desired: std::collections::BTreeMap<String, String> = snapshot
            .loaded
            .mcp_servers
            .iter()
            .map(|server| (server.name.clone(), mcp_server_digest(server)))
            .collect();
        let mut guard = self.cache.lock().await;
        // A server that reported tools/list_changed is rebuilt even when its
        // config digest is unchanged. Other servers keep their live connection.
        let catalog_refresh: std::collections::BTreeSet<String> = guard
            .as_ref()
            .map(|cache| {
                snapshot
                    .loaded
                    .mcp_servers
                    .iter()
                    .filter(|server| {
                        let prefix = format!("mcp__{}__", server.name);
                        cache
                            .tools
                            .iter()
                            .any(|tool| tool.name().starts_with(&prefix) && tool.catalog_changed())
                    })
                    .map(|server| server.name.clone())
                    .collect()
            })
            .unwrap_or_default();
        // A server whose last attempt failed is due once its backoff elapsed.
        // Turn boundaries are the only trigger, so a session that sends no
        // request never retries.
        let now = std::time::Instant::now();
        let due: std::collections::BTreeSet<String> = guard
            .as_ref()
            .map(|cache| {
                cache
                    .retry
                    .iter()
                    .filter(|(_, retry)| retry.next_attempt <= now)
                    .map(|(name, _)| name.clone())
                    .collect()
            })
            .unwrap_or_default();
        let rebuild = guard
            .as_ref()
            .is_none_or(|cache| cache.unrestricted != unrestricted || cache.servers != desired)
            || !catalog_refresh.is_empty()
            || !due.is_empty();
        if rebuild {
            let previous = guard.take();
            let (kept, previous_servers, mut retry) = match previous {
                Some(cache) => (cache.tools, cache.servers, cache.retry),
                None => (Vec::new(), Default::default(), Default::default()),
            };
            // A server that is no longer configured keeps no schedule.
            retry.retain(|server, _| desired.contains_key(server));
            // Keep every tool whose server is unchanged: its connection, its
            // child process and its handshake stay exactly as they were.
            let mut tools: Vec<Arc<dyn leveler_tools::tool::Tool>> = kept
                .into_iter()
                .filter(|tool| {
                    previous_servers.iter().any(|(server, digest)| {
                        desired.get(server) == Some(digest)
                            && !catalog_refresh.contains(server)
                            && !due.contains(server)
                            && tool.name().starts_with(&format!("mcp__{server}__"))
                    })
                })
                .collect();
            for server in &snapshot.loaded.mcp_servers {
                let unchanged = previous_servers.get(&server.name) == desired.get(&server.name);
                if unchanged
                    && !catalog_refresh.contains(&server.name)
                    && !due.contains(&server.name)
                {
                    continue;
                }
                for connection in leveler_tools::mcp::connect_all_detailed(
                    std::slice::from_ref(server),
                    &self.environment,
                    unrestricted,
                )
                .await
                {
                    tools.extend(connection.tools);
                    match connection.error {
                        // Connected (even with a legitimately empty catalog):
                        // this server owes no retry.
                        None => {
                            retry.remove(&connection.server);
                        }
                        Some(reason) => {
                            let failures = retry
                                .get(&connection.server)
                                .map(|retry| retry.failures)
                                .unwrap_or(0)
                                + 1;
                            tracing::warn!(
                                server = %connection.server,
                                attempt = failures,
                                retry_in_seconds = mcp_retry_delay(failures).as_secs(),
                                error = %reason,
                                "MCP server not connected; will retry on a later turn"
                            );
                            retry.insert(
                                connection.server,
                                McpRetry {
                                    failures,
                                    next_attempt: now + mcp_retry_delay(failures),
                                    reason,
                                },
                            );
                        }
                    }
                }
            }
            *guard = Some(McpToolCache {
                unrestricted,
                servers: desired,
                retry,
                tools,
            });
        }
        guard
            .as_ref()
            .map(|cache| cache.tools.clone())
            .unwrap_or_default()
    }

    /// Why the external-tool pack has nothing to offer right now, if a
    /// configured server could not be reached.
    async fn unavailable_reason(&self) -> Option<String> {
        let guard = self.cache.lock().await;
        let cache = guard.as_ref()?;
        let mut reasons: Vec<String> = cache
            .retry
            .iter()
            .map(|(server, retry)| format!("{server}: {}", retry.reason))
            .collect();
        if reasons.is_empty() {
            return None;
        }
        reasons.sort();
        Some(format!(
            "configured MCP server(s) not reachable on this request; retrying on later turns — {}",
            reasons.join("; ")
        ))
    }
}

/// Bridges the host's MCP cache into a running turn, so an external catalog
/// that becomes reachable mid-turn joins this turn's surface instead of waiting
/// for the next user message.
struct McpDiscovery {
    source: Arc<McpToolSource>,
    snapshot: Arc<ConfigSnapshot>,
    mode: PermissionProfile,
}

#[async_trait::async_trait]
impl leveler_tools::tool::ToolDiscovery for McpDiscovery {
    async fn discovered(&self) -> Vec<Arc<dyn leveler_tools::tool::Tool>> {
        self.source.tools(&self.snapshot, self.mode).await
    }
}

struct McpToolCache {
    unrestricted: bool,
    /// Per-server input digest. An unchanged server keeps its live connection;
    /// only a changed/added server is reconnected.
    servers: std::collections::BTreeMap<String, String>,
    /// Servers whose last attempt did not produce a catalog. A failed attempt is
    /// not a connection: the server stays configured and is retried with
    /// backoff, because an MCP server commonly starts after CodeLeveler does.
    retry: std::collections::BTreeMap<String, McpRetry>,
    tools: Vec<Arc<dyn leveler_tools::tool::Tool>>,
}

/// One server's retry schedule after a failed connection attempt, and the
/// reason the last attempt failed (shown to the model for this turn only).
struct McpRetry {
    failures: u32,
    next_attempt: std::time::Instant,
    reason: String,
}

/// First retry delay, and the cap a persistently unreachable server backs off
/// to. The trigger is a turn boundary, never a timer loop: an idle session
/// makes no attempt at all.
const MCP_RETRY_INITIAL: std::time::Duration = std::time::Duration::from_secs(5);
const MCP_RETRY_MAX: std::time::Duration = std::time::Duration::from_secs(60);

fn mcp_retry_delay(failures: u32) -> std::time::Duration {
    let exponent = failures.saturating_sub(1).min(8);
    MCP_RETRY_INITIAL
        .saturating_mul(1 << exponent)
        .min(MCP_RETRY_MAX)
}

/// A fully-assembled application.
pub struct Application {
    pub layout: Layout,
    /// The ONE owner of the effective configuration. Consumers capture a
    /// snapshot per turn; nothing caches a second copy of provider or model
    /// state for the process lifetime.
    config: Arc<ConfigManager>,
    /// MCP tools cached under the credential inheritance authority of their launch.
    mcp_tools: Arc<McpToolSource>,
    /// The session database pool, opened once per process.
    database: Arc<tokio::sync::Mutex<Option<Database>>>,
    /// When set, overrides the resolved execution policy on every execution
    /// path (single-knob ablation runs). `None` = resolver defaults.
    execution_overrides: Option<leveler_agent::coding::ExecutionOverrides>,
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
    /// Process-lived native desktop backend. The handle is lazy: no worker
    /// thread starts until the first computer-use call. One handle serves
    /// every session; per-session observations are separated by session scope.
    computer: Arc<leveler_cua::ComputerUse>,
    /// Deterministic actions over installed system applications. Stateless:
    /// every call re-probes the machine, and each action runs through the
    /// host execution authority.
    native: Arc<leveler_native::NativeActions>,
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
    /// The activated capability packs of each session, keyed by session scope.
    ///
    /// Same owner pattern as `permission_profiles`, and for the same reason:
    /// every turn builds its own engine, so the engine cannot own the answer
    /// to "which packs did the model enable". `capability(enable)` promises the
    /// tools on the following request, and that request may belong to a later
    /// turn or to a session reopened with `--session` in this same process.
    /// Availability and permission are NOT here: they are recomputed from the
    /// current host facts on every turn, so a narrowed profile still gates.
    capability_activations: std::sync::Mutex<
        std::collections::HashMap<String, leveler_agent::capability::SharedCapabilityActivation>,
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

impl Application {
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
    /// The activated capability packs of a session, seeded empty on first use
    /// and shared by every turn of that session.
    ///
    /// No session scope (eval, one-shot CLI) means nobody can send the session
    /// a second request, so a private cell is the whole truth.
    fn capability_activation_for(
        &self,
        scope: Option<&str>,
    ) -> leveler_agent::capability::SharedCapabilityActivation {
        use leveler_agent::capability::SharedCapabilityActivation;
        let Some(scope) = scope else {
            return SharedCapabilityActivation::default();
        };
        let mut map = self
            .capability_activations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.entry(scope.to_string()).or_default().clone()
    }

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
    ///
    /// Prefer [`Self::config_snapshot`] in a long-lived process: this reads the
    /// files and is the primitive the config manager owns, not a cache.
    pub fn load_config(layout: &Layout) -> Result<LoadedConfig, AppError> {
        load_config(layout)
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

        let config = ConfigManager::load(layout.clone())?;
        let snapshot = config.snapshot();

        // Project config + env (composition root may read env — AGENTS.md).
        let background_tasks =
            leveler_execution::BackgroundTaskRegistry::with_environment(environment.clone())
                .with_stable_task_ids();
        let background_tasks = Arc::new(match layout.primary_workspace() {
            Some(_) => background_tasks.with_execution_host(execution_host_config(&layout)?),
            None => background_tasks,
        });
        // Lazy: this holds only paths and the configured product until the
        // first navigate actually starts a browser.
        let browser = Arc::new(leveler_browser::Browser::new(
            (*environment).clone(),
            layout.browser_profile_dir(),
            snapshot.loaded.browser_default,
        ));
        let computer = Arc::new(leveler_cua::ComputerUse::new());
        let native = Arc::new(leveler_native::NativeActions::new());
        let has_workspace = layout.primary_workspace().is_some();
        Ok(Self {
            layout,
            config,
            mcp_tools: Arc::new(McpToolSource::new((*environment).clone(), has_workspace)),
            database: Arc::new(tokio::sync::Mutex::new(None)),

            execution_overrides: None,
            // One product default for a new Coding Session: Goal. Callers
            // that resolved an explicit axis (`--collaboration`, the transport
            // request) override it via `with_collaboration` or the create path.
            collaboration: CollaborationMode::default(),
            model_step_ceiling: None,
            environment,
            background_tasks,
            browser,
            computer,
            native,
            runtime_id: OnceLock::new(),
            boot: std::sync::Mutex::new(None),
            in_flight_commands: Default::default(),
            default_model_notice: std::sync::Mutex::new(None),
            permission_profiles: std::sync::Mutex::new(std::collections::HashMap::new()),
            capability_activations: std::sync::Mutex::new(std::collections::HashMap::new()),
        })
    }

    /// The configuration generation in force right now.
    ///
    /// Every turn captures this once and keeps the `Arc` for its whole life;
    /// that is the execution-isolation boundary between a running turn and a
    /// later reload.
    pub fn config_snapshot(&self) -> Arc<ConfigSnapshot> {
        self.config.snapshot()
    }

    /// The effective bundle of the current generation.
    pub fn effective_config(&self) -> Arc<LoadedConfig> {
        Arc::new(self.config.snapshot().loaded.clone())
    }

    /// The provider registry of the current generation. Long-lived consumers
    /// that need a stable generation (a turn) must capture
    /// [`Self::config_snapshot`] instead of calling this per request.
    pub fn registry(&self) -> Arc<ProviderRegistry> {
        self.config.snapshot().registry.clone()
    }

    /// The config manager itself (watcher lifecycle, `/reload`, subscribers).
    pub fn config_manager(&self) -> &Arc<ConfigManager> {
        &self.config
    }

    /// Re-read the configured sources and publish the result. The caller owns
    /// the user-facing report; this returns the mechanical outcome.
    pub async fn reload_config(&self, origin: ReloadOrigin, force: bool) -> ReloadOutcome {
        self.config.reload(origin, force).await
    }

    /// Start watching the configuration sources for this process.
    pub fn start_config_watcher(&self) {
        ConfigManager::start_watcher(&self.config);
    }

    /// The explicitly configured default model in the CURRENT generation, when
    /// this environment can resolve it. `None` means no configured default;
    /// callers must keep the model they already have.
    pub fn configured_default_model(&self) -> Option<ModelRef> {
        self.config.configured_default_model()
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

    /// The configuration generation loaded by this application (and any later
    /// reload).
    pub fn config_fingerprint(&self) -> String {
        self.config.snapshot().fingerprint.clone()
    }

    /// The effective-configuration version in force.
    pub fn config_version(&self) -> u64 {
        self.config.snapshot().version
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
        let mut guard = self.database.lock().await;
        if let Some(db) = guard.as_ref() {
            return Ok(db.clone());
        }
        std::fs::create_dir_all(&self.layout.state_dir).map_err(|source| AppError::Io {
            path: self.layout.state_dir.display().to_string(),
            source,
        })?;
        let db = Database::connect(&self.layout.database_path()).await?;
        *guard = Some(db.clone());
        Ok(db)
    }

    /// Recover hosted process settlements at an explicit runtime startup
    /// boundary. Merely reading the session database never performs this work.
    pub async fn reconcile_execution_services(
        &self,
    ) -> Result<Option<leveler_execution::HostRecovery>, AppError> {
        self.background_tasks
            .reconcile_hosted()
            .await
            .map_err(AppError::ExecutionHost)
    }

    /// All configured model references.
    pub fn model_refs(&self) -> Vec<ModelRef> {
        self.config.snapshot().registry.model_refs()
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
            .snapshot()
            .loaded
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

    /// Whether an idle assist affordance is switched on.
    pub(crate) fn assist_enabled(&self, kind: crate::prompt_assist::AssistKind) -> bool {
        let snapshot = self.config.snapshot();
        match kind {
            crate::prompt_assist::AssistKind::PromptSuggestion => {
                snapshot.loaded.assist_prompt_suggestions
            }
            crate::prompt_assist::AssistKind::AwaySummary => snapshot.loaded.assist_away_summary,
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

    /// Connect to the configured MCP servers once and cache their tools.
    async fn mcp_tools(
        &self,
        snapshot: &ConfigSnapshot,
        mode: PermissionProfile,
    ) -> Vec<Arc<dyn leveler_tools::tool::Tool>> {
        self.mcp_tools.tools(snapshot, mode).await
    }

    /// Why the external-tool pack has nothing to offer on this turn, if a
    /// configured MCP server could not be reached. Read after `mcp_tools` for
    /// the same turn, so the reason and the (empty) tool set describe the same
    /// attempt.
    async fn mcp_unavailable_reason(&self) -> Option<String> {
        self.mcp_tools.unavailable_reason().await
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
        snapshot: &ConfigSnapshot,
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
        let capabilities = leveler_tools::Capabilities::in_process(self.environment.clone())
            .with_background_tasks(self.background_tasks.clone())
            .with_artifact_store(artifact_store)
            .with_browser(self.browser.clone())
            .with_computer_use(self.computer.clone())
            .with_native_actions(self.native.clone())
            .with_search_provider(resolve_search_provider(
                snapshot.loaded.web_search.as_ref(),
                self.environment.as_ref(),
            ));
        let tool_context = match workspace {
            Some(workspace) => {
                ToolContext::with_environment(workspace, mode, self.environment.clone())
            }
            None => ToolContext::without_workspace_with_environment(mode, self.environment.clone()),
        }
        .with_policy_limits(max_files)
        .with_sandbox(sandbox)
        .with_deny_env(provider_secret_env_names(
            &snapshot.loaded.providers,
            snapshot.loaded.web_search.as_ref(),
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
        let available_packs = self.capability_availability(snapshot, model).await;
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
        let snapshot = self.config.snapshot();
        let (_available, tool_context, registry) = self
            .compose_tool_surface(&snapshot, model, mode, sandbox, true, session_scope)
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
    async fn capability_availability(
        &self,
        snapshot: &ConfigSnapshot,
        model: &leveler_model::ModelRef,
    ) -> CapabilityPacks {
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
                snapshot.loaded.web_search.as_ref(),
                self.environment.as_ref(),
            )
            .is_some(),
            // An image is useless to a model that cannot read one.
            media: snapshot
                .registry
                .profile(model)
                .await
                .map(|profile| profile.capabilities.vision)
                .unwrap_or(false),
            skills: self.layout.primary_workspace().is_some(),
            // Not "is a browser installed": is the browser this host would
            // SELECT — the call's, then `[browser].default`, then the system
            // default — one it can actually drive? Answering with a different
            // browser is exactly what the no-fallback rule forbids, so a user
            // whose default is a browser CodeLeveler cannot drive sees no
            // browser tools rather than a surprise one.
            browser: self.browser.resolve(None).is_ok(),
            // A compiled platform backend is the mechanical half. TCC grants
            // and the interactive session are call-time facts reported by
            // `computer_observe availability`, so this does not start a
            // desktop worker on every turn.
            computer_use: leveler_cua::ComputerUse::platform_supported(),
            // A compiled backend is the mechanical half; the mail application,
            // its account and its automation permission are call-time facts
            // reported by `native_capabilities`, so advertising the tools does
            // not start an application or prompt.
            native_actions: leveler_native::NativeActions::platform_supported(),
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
            && let Some(percent) = self.config.snapshot().loaded.soft_percent_for(model)
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
        // One generation for the whole turn: provider clients, models, MCP
        // servers and tool policy all come from this snapshot, so a reload in
        // the middle of the turn cannot mix two configurations.
        let snapshot = self.config.snapshot();
        let (available_packs, tool_context, mut registry) = self
            .compose_tool_surface(&snapshot, model, mode, sandbox, read_only, session_scope)
            .await?;
        let unrestricted = tool_context.policy.unrestricted_execution();
        // Register available controls; exposure is owned by CapabilityDisclosure.
        leveler_agent::register_harness_controls_with(
            &mut registry,
            leveler_agent::HarnessControls::ALL,
        );
        // Attach external MCP tools (connect once, cached across turns).
        for tool in self.mcp_tools(&snapshot, tool_context.policy.mode()).await {
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
            (available_packs.computer_use, CapabilityId::ComputerUse),
            (available_packs.native_actions, CapabilityId::NativeActions),
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
        // Permission is an AUTHORITY fact about this host and mode; it must not
        // be derived from which tools happen to exist right now. A pack whose
        // catalog starts late (an MCP server that comes up mid-turn) is still
        // permitted, and the model must be able to tell "not allowed" from
        // "not reachable yet" — deriving permission from availability made
        // `capability enable` answer "not permitted" for a mode that permits it.
        let permitted = CapabilityId::ALL
            .into_iter()
            .filter(|id| {
                unrestricted
                    || match id {
                        CapabilityId::MultiAgent => {
                            self.project_config().agents.delegation
                                && snapshot.loaded.agents_delegation
                        }
                        CapabilityId::Authoring => !read_only,
                        _ => true,
                    }
            })
            .collect();
        // A configured MCP server that is not up yet leaves the pack with no
        // tools; say so instead of presenting the gap as "nothing configured".
        let mut unavailable = std::collections::BTreeMap::new();
        if !registry
            .definitions()
            .iter()
            .any(|tool| tool.name.starts_with("mcp__"))
            && let Some(reason) = self.mcp_unavailable_reason().await
        {
            unavailable.insert(CapabilityId::ExternalTools, reason);
        }
        let capabilities = Arc::new(
            CapabilityDisclosure::for_session(
                available,
                permitted,
                self.capability_activation_for(session_scope),
            )
            .with_default_active(Vec::new())
            .map_err(AppError::Engine)?
            .with_unavailable(unavailable),
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
        let runtime: Arc<dyn ModelRuntime> = snapshot.registry.clone();
        let db = self.open_database().await?;
        Ok(leveler_agent::coding::CodingRuntime {
            // The composition root chooses the adapter: every engine port is
            // backed by this repository's one SQLite database.
            engine: self.task_engine(&db)?,
            factory: leveler_agent::coding::ExecutorFactory {
                runtime,
                registry: Arc::new(registry),
                // A turn outlives the moment its surface was composed; the
                // catalog it grows into is the application's one MCP cache.
                tool_discovery: Some(Arc::new(McpDiscovery {
                    source: Arc::clone(&self.mcp_tools),
                    snapshot: Arc::clone(&snapshot),
                    mode: tool_context.policy.mode(),
                })),
                tool_context,
                model: model.clone(),
                commit_co_author: snapshot.loaded.vcs_co_author,
                overrides: self
                    .execution_overrides_for_session(session_scope, Some(model))
                    .await?,
                capabilities: Some(capabilities),
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
                    || (self.project_config().agents.delegation
                        && snapshot.loaded.agents_delegation),
                allow_host_input: true,
                independent_review: combine_independent_review(
                    snapshot.loaded.agents_independent_review,
                    self.project_config().agents.independent_review,
                ),
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
        let snapshot = self.config.snapshot();
        let mut request = leveler_execution::ProcessRequest::new(program, args, cwd.clone());
        request.timeout = std::time::Duration::from_secs(7 * 24 * 3600);
        request.deny_network = sandbox;
        request.deny_env = provider_secret_env_names(
            &snapshot.loaded.providers,
            snapshot.loaded.web_search.as_ref(),
        );
        request.write_scope = mode.write_scope(&cwd);
        let runner = leveler_execution::CommandRunner::with_environment(self.environment.clone());
        Ok((runner, request, cwd))
    }

    /// Build the user's interactive terminal request.
    ///
    /// This shell is the person's own process, not an Agent command: the
    /// Permission Mode and its OS write/network fence do not apply. Session
    /// ownership, PTY lifecycle, the immutable environment snapshot and
    /// provider-credential scrubbing still apply. `unrestricted_execution`
    /// stays false on purpose so a sealed eval host keeps its read denials and
    /// the child still loses provider secrets.
    pub(crate) fn user_terminal_execution(
        &self,
        shell: String,
    ) -> Result<leveler_execution::ProcessRequest, AppError> {
        let snapshot = self.config.snapshot();
        let cwd = self
            .layout
            .require_workspace()
            .map_err(|error| AppError::NotFound(error.to_string()))?
            .to_path_buf();
        let mut request = leveler_execution::ProcessRequest::new(shell, vec!["-l".into()], cwd);
        request.timeout = std::time::Duration::from_secs(7 * 24 * 3600);
        request.deny_network = false;
        request.write_scope = leveler_execution::WriteScope::Unrestricted;
        request.deny_env = provider_secret_env_names(
            &snapshot.loaded.providers,
            snapshot.loaded.web_search.as_ref(),
        );
        // The persistent Execution Host rebuilds the child environment from its
        // own snapshot; no app-side CommandRunner is involved in this path.
        Ok(request)
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

/// Stable digest of one MCP server's launch configuration. Two generations
/// with the same digest keep the SAME live connection; a changed command, args
/// or env reconnects only that server.
fn mcp_server_digest(server: &leveler_tools::mcp::McpServerConfig) -> String {
    let mut hasher = Sha256::new();
    if server.is_streamable_http() {
        hasher.update(b"streamable_http\0");
        hasher.update(server.url.as_deref().unwrap_or("").as_bytes());
        for (key, value) in &server.headers {
            hasher.update(b"\0h\0");
            hasher.update(key.as_bytes());
            hasher.update(b"=");
            hasher.update(Sha256::digest(value.as_bytes()));
        }
        if let Some(token) = &server.bearer_token {
            hasher.update(b"\0bearer\0");
            hasher.update(Sha256::digest(token.as_bytes()));
        }
    } else {
        hasher.update(b"stdio\0");
        hasher.update(server.command.as_bytes());
        for arg in &server.args {
            hasher.update(b"\0");
            hasher.update(arg.as_bytes());
        }
        for (key, value) in &server.env {
            hasher.update(b"\0");
            hasher.update(key.as_bytes());
            hasher.update(b"=");
            // The value is a resolved credential; hash it so the digest itself
            // can never be printed as a secret.
            hasher.update(Sha256::digest(value.as_bytes()));
        }
    }
    format!("sha256:{:x}", hasher.finalize())
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
mod user_terminal_execution_tests {
    use super::Application;
    use leveler_execution::WriteScope;
    use leveler_project::Layout;

    /// The interactive terminal is the person's own shell. Its request must not
    /// inherit the Agent profile's write fence or network denial, while the
    /// credential scrub and the trusted-host default stay in force.
    #[test]
    fn the_user_terminal_is_not_agent_confined() {
        let directory = tempfile::tempdir().unwrap();
        let app = Application::assemble(Layout::from_parts(
            directory.path().to_path_buf(),
            directory.path().join("configs"),
            directory.path().join("state"),
        ))
        .unwrap();
        let request = app.user_terminal_execution("/bin/zsh".into()).unwrap();
        assert_eq!(request.cwd.as_path(), directory.path());
        assert_eq!(request.program, "/bin/zsh");
        assert_eq!(request.args, vec!["-l".to_string()]);
        assert_eq!(request.write_scope, WriteScope::Unrestricted);
        assert!(!request.deny_network);
        assert!(
            !request.unrestricted_execution,
            "the user shell must still lose provider credentials"
        );
        assert!(
            request
                .deny_env
                .iter()
                .any(|name| name == "LEVELER_SEARCH_API_KEY")
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
