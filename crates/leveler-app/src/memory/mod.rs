//! Cross-session memory runtime for the composition root.
//!
//! This module owns the process-side lifecycle of memory: it resolves the
//! store from the global home, builds the turn-stable prompt index, routes
//! ordinary file tools through the access policy, and runs the automatic
//! Capture and Dream workers. The durable semantics (ranges, claims, leases,
//! consolidation, maintenance) stay in `leveler-memory`; this layer only
//! decides when a session asks it to act.
//!
//! Memory is off unless `[memory].rollout` opts in. The automatic pipeline
//! creates nothing and opens no database while the rollout is `off`, so a
//! default installation pays no cost and creates no store. The one exception
//! is a user's explicit `/remember`: it writes the
//! requested note so it survives a later opt-in.

mod batch;
mod capture;
mod dream;
mod status;
mod transcript;
mod ui;

pub(crate) use batch::run as run_batch_dream;
pub(crate) use dream::run_manual as run_manual_dream;

/// The one explanation every surface gives when automatic workspace memory is
/// asked for in a temporary working directory.
///
/// The storage layer deliberately gives such a workspace no scope: temporary
/// directories never accumulate durable memory. Capture, retention and Dream
/// all target the workspace scope, so refusing them here is the authority; a
/// command surface only relays this fact.
pub(crate) const WORKSPACE_EPHEMERAL_MESSAGE: &str = "Workspace memory is unavailable for a temporary working directory; nothing is captured or consolidated here.";

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use leveler_memory::config::{MemoryV2Config, MemoryV2Rollout};
use leveler_memory::{
    MemoryStorage, MemoryV2AccessResource, V2ManifestBudget, V2MemoryAccessPolicy, V2MemoryScope,
    regenerate_scope_manifest,
};

use crate::config_manager::ConfigManager;

pub use status::{
    MemoryActivity, MemoryActivityKind, MemoryScopeSummary, MemoryStatus, SessionMemoryStatus,
};

/// One policy handle exposes both the host-facing resource for file tools and
/// the concrete policy the composition root uses for browser reads and forget.
#[derive(Clone)]
struct MemoryAccess {
    policy: Arc<V2MemoryAccessPolicy>,
    resource: MemoryV2AccessResource,
}

/// Per-process memory owner.
///
/// Cheap to clone: every clone shares the same access policy, worker slots,
/// activity and index cache, so a background worker and the foreground turn
/// observe one memory state.
#[derive(Clone)]
pub struct MemoryRuntime {
    /// Memory store root (`<LevelerHome>/memory`).
    root: PathBuf,
    /// The workspace this process serves, or the no-workspace namespace.
    workspace: PathBuf,
    storage: MemoryStorage,
    config: Arc<ConfigManager>,
    /// Lazily built after the scope directories exist. A policy caches read
    /// snapshots, so it must not be recreated per call.
    access: Arc<tokio::sync::Mutex<Option<MemoryAccess>>>,
    /// Sessions whose capture worker is currently running.
    capture_workers: Arc<Mutex<HashMap<String, tokio_util::sync::CancellationToken>>>,
    /// The automatic Dream worker, shared by the whole process: Dream is a
    /// scope-wide operation and two sessions must not plan it at once.
    dream_worker: Arc<tokio::sync::Mutex<Option<tokio_util::sync::CancellationToken>>>,
    /// Content-free activity for the UI, keyed by session scope.
    activity: Arc<Mutex<HashMap<String, MemoryActivity>>>,
    /// Last manifest text handed to each session's prompt, so a turn that
    /// changes nothing sends byte-identical context.
    index_cache: Arc<Mutex<HashMap<String, Arc<str>>>>,
    /// Sessions the memory page paused. The configured rollout stays the
    /// authority for whether memory may run at all; this only suppresses it
    /// for one session and never promotes an `off` configuration.
    paused_sessions: Arc<Mutex<HashSet<String>>>,
}

impl MemoryRuntime {
    /// Build the runtime for one application workspace.
    ///
    /// No I/O happens here: the layout is resolved, directories are created
    /// only when a memory capability is first used.
    pub fn new(layout: &leveler_project::Layout, config: Arc<ConfigManager>) -> Self {
        let home = layout.home();
        let workspace = layout
            .primary_workspace()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| home.no_workspace_state_dir());
        let root = home.memory_dir();
        let storage = MemoryStorage::for_workspace(&workspace, &root);
        Self {
            root,
            workspace,
            storage,
            config,
            access: Arc::new(tokio::sync::Mutex::new(None)),
            capture_workers: Arc::new(Mutex::new(HashMap::new())),
            dream_worker: Arc::new(tokio::sync::Mutex::new(None)),
            activity: Arc::new(Mutex::new(HashMap::new())),
            index_cache: Arc::new(Mutex::new(HashMap::new())),
            paused_sessions: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// The memory configuration of the current generation.
    pub fn config(&self) -> MemoryV2Config {
        self.config.snapshot().loaded.memory
    }

    /// The configuration this session's turns obey after the page toggle.
    ///
    /// A pause only narrows the rollout to `off`; it never widens what the
    /// configuration permits. `None` is the process-level view used by
    /// surfaces that are not acting for one session.
    pub fn config_for(&self, session_scope: Option<&str>) -> MemoryV2Config {
        let mut config = self.config();
        if let Some(scope) = session_scope
            && self
                .paused_sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(scope)
        {
            config.rollout = MemoryV2Rollout::Off;
        }
        config
    }

    /// Pause or resume memory for one session.
    ///
    /// The configured rollout remains the authority: an `off` configuration
    /// is refused rather than silently promoted by a UI toggle. A pause takes
    /// effect on the next turn and on the capture worker's next claim; a
    /// request already in flight is allowed to settle.
    pub fn set_session_enabled(
        &self,
        session_id: &leveler_core::SessionId,
        enabled: bool,
    ) -> Result<MemoryV2Config, String> {
        let configured = self.config();
        if enabled && configured.rollout == MemoryV2Rollout::Off {
            return Err(
                "Memory is disabled by configuration (`[memory].rollout = \"off\"`). Set \
                 rollout = \"active\" in config.toml to enable it."
                    .to_owned(),
            );
        }
        let mut paused = self
            .paused_sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if enabled {
            paused.remove(session_id.as_str());
        } else {
            paused.insert(session_id.as_str().to_owned());
        }
        drop(paused);
        Ok(self.config_for(Some(session_id.as_str())))
    }

    /// The explicitly configured memory route, if the user set one.
    pub fn model_override(&self) -> Option<leveler_model::ModelRef> {
        self.config
            .snapshot()
            .loaded
            .memory_model
            .as_deref()
            .and_then(leveler_model::ModelRef::parse)
    }

    /// Store root (`<LevelerHome>/memory`).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The workspace the runtime serves (the no-workspace namespace when the
    /// process has no repository).
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub fn storage(&self) -> &MemoryStorage {
        &self.storage
    }

    pub fn is_workspace_ephemeral(&self) -> bool {
        self.storage.is_ephemeral()
    }

    /// Initialize both scopes. Idempotent and cheap after the first call.
    pub fn ensure_initialized(&self) -> Result<(), String> {
        self.storage
            .ensure_initialized()
            .map_err(|error| error.to_string())
    }

    /// The host access policy ordinary file tools route memory paths through.
    ///
    /// `None` means the pipeline is off for this session in the current
    /// generation. The policy is built only once; a config reload that turns
    /// memory off does not retroactively remove an already-built policy from a
    /// running process, but every turn composes its tool context from the
    /// current generation, so new turns stop receiving it.
    pub async fn access_resource(
        &self,
        session_scope: Option<&str>,
    ) -> Option<MemoryV2AccessResource> {
        self.load_access(session_scope)
            .await
            .map(|access| access.resource)
    }

    /// The concrete policy, for the composition root's own browser reads.
    pub(crate) async fn access_policy(&self) -> Option<Arc<V2MemoryAccessPolicy>> {
        self.load_access(None).await.map(|access| access.policy)
    }

    async fn load_access(&self, session_scope: Option<&str>) -> Option<MemoryAccess> {
        let config = self.config_for(session_scope);
        if !config.can_capture() && !config.can_expose_memory() {
            return None;
        }
        let mut slot = self.access.lock().await;
        if let Some(access) = slot.as_ref() {
            return Some(access.clone());
        }
        let storage = self.storage.clone();
        let built = tokio::task::spawn_blocking(move || {
            storage
                .ensure_initialized()
                .map_err(|error| error.to_string())?;
            V2MemoryAccessPolicy::new(storage.global_dir(), storage.workspace_dir())
                .map_err(|error| error.to_string())
        })
        .await;
        match built {
            Ok(Ok(policy)) => {
                let policy = Arc::new(policy);
                let access = MemoryAccess {
                    resource: MemoryV2AccessResource(policy.clone()),
                    policy,
                };
                *slot = Some(access.clone());
                Some(access)
            }
            Ok(Err(error)) => {
                tracing::warn!(%error, "memory-v2 access policy unavailable");
                None
            }
            Err(error) => {
                tracing::warn!(%error, "memory-v2 access policy task failed");
                None
            }
        }
    }

    /// The memory index for this turn's prompt.
    ///
    /// Rendered once per turn (the tool context is composed once per turn) and
    /// cached by session so a later turn whose memory did not change sends the
    /// same bytes. `None` while the rollout does not expose memory.
    pub async fn prompt_index(&self, session_scope: Option<&str>) -> Option<Arc<str>> {
        let config = self.config_for(session_scope);
        if !config.can_expose_memory() {
            return None;
        }
        let cache_key = session_scope.unwrap_or("__process__").to_string();
        let storage = self.storage.clone();
        let rendered = tokio::task::spawn_blocking(move || build_prompt_index(&storage, config))
            .await
            .ok()?
            .ok()?;
        {
            let mut cache = self
                .index_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(previous) = cache.get(&cache_key)
                && previous.as_ref() == rendered.as_ref()
            {
                return Some(previous.clone());
            }
            cache.insert(cache_key, rendered.clone());
        }
        Some(rendered)
    }

    /// Make the current memory visible at once (rollout promotion, `/dream`).
    pub async fn promote_hidden_observations(&self) -> Result<u64, String> {
        if !self.config().can_expose_memory() {
            return Ok(0);
        }
        let storage = self.storage.clone();
        tokio::task::spawn_blocking(move || {
            storage
                .ensure_initialized()
                .map_err(|error| error.to_string())?;
            let store = leveler_memory::V2CaptureStore::open(
                storage.workspace_dir(),
                V2MemoryScope::Workspace,
            )
            .map_err(|error| error.to_string())?;
            store
                .promote_hidden_observations_now()
                .map_err(|error| error.to_string())
        })
        .await
        .map_err(|error| format!("memory promotion task failed: {error}"))?
    }

    /// Drop a session's cached index and activity after it is deleted.
    pub fn forget_session(&self, session_scope: &str) {
        self.index_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session_scope);
        self.paused_sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session_scope);
        self.activity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session_scope);
    }

    pub(crate) fn record_activity(&self, session_scope: &str, update: MemoryActivity) {
        let mut activity = self
            .activity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = activity.entry(session_scope.to_string()).or_default();
        entry.merge(update);
    }

    pub fn activity(&self, session_scope: &str) -> MemoryActivity {
        self.activity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session_scope)
            .cloned()
            .unwrap_or_default()
    }

    /// Atomically reserve a session's single capture worker slot.
    ///
    /// Returns the worker's cancellation token, or `None` when a worker is
    /// already running. The check and the insert share one lock so two turns
    /// finishing together cannot both start a worker.
    pub(crate) fn try_begin_capture_worker(
        &self,
        session_scope: &str,
    ) -> Option<tokio_util::sync::CancellationToken> {
        let mut workers = self
            .capture_workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if workers.contains_key(session_scope) {
            return None;
        }
        let token = tokio_util::sync::CancellationToken::new();
        workers.insert(session_scope.to_string(), token.clone());
        Some(token)
    }

    pub(crate) fn finish_capture_worker(&self, session_scope: &str) {
        self.capture_workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session_scope);
    }

    pub(crate) async fn acquire_dream_worker(&self) -> Option<tokio_util::sync::CancellationToken> {
        let mut slot = self.dream_worker.lock().await;
        if slot.is_some() {
            return None;
        }
        let token = tokio_util::sync::CancellationToken::new();
        *slot = Some(token.clone());
        Some(token)
    }

    pub(crate) async fn release_dream_worker(&self) {
        *self.dream_worker.lock().await = None;
    }

    /// Shut down every background worker. Called when the application drops.
    pub fn cancel_workers(&self) {
        let workers = {
            let mut map = self
                .capture_workers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::take(&mut *map)
        };
        for token in workers.into_values() {
            token.cancel();
        }
        if let Ok(mut slot) = self.dream_worker.try_lock()
            && let Some(token) = slot.take()
        {
            token.cancel();
        }
    }

    /// Persist a user-authored `/remember` note in the global scope.
    pub async fn remember(&self, content: &str) -> Result<Option<PathBuf>, String> {
        let storage = self.storage.clone();
        let content = content.trim().to_string();
        if content.is_empty() {
            return Ok(None);
        }
        tokio::task::spawn_blocking(move || {
            storage
                .save_remember_note(&content)
                .map_err(|error| error.to_string())
        })
        .await
        .map_err(|error| format!("memory remember task failed: {error}"))?
    }
}

/// Render the two scope manifests and the model-facing index preamble.
///
/// Regenerating is also how `MEMORY.md` is refreshed, so the file a user opens
/// is exactly the index the model saw.
fn build_prompt_index(storage: &MemoryStorage, config: MemoryV2Config) -> Result<Arc<str>, String> {
    storage
        .ensure_initialized()
        .map_err(|error| error.to_string())?;
    let ephemeral = storage.is_ephemeral();
    // A rollout may have been promoted from record-only/shadow to active
    // between sessions. Expose what those earlier turns captured before the
    // index is rendered, so the manifest the model reads matches the store.
    // A lease conflict only defers promotion; it must not remove memory from
    // this turn's prompt.
    if !ephemeral
        && let Err(error) =
            leveler_memory::V2CaptureStore::open(storage.workspace_dir(), V2MemoryScope::Workspace)
                .and_then(|store| store.promote_hidden_observations_now())
    {
        tracing::warn!(%error, "memory-v2 observation promotion deferred");
    }
    let budget = if config.compact_index_enabled {
        V2ManifestBudget::compact()
    } else {
        V2ManifestBudget::default()
    };
    let global = regenerate_scope_manifest(storage.global_dir(), V2MemoryScope::Global, budget)
        .map_err(|error| error.to_string())?;
    let global_body = global.content.trim();
    // A temporary working directory has no workspace scope, so there is no
    // manifest to regenerate and no root worth advertising. Saying so keeps
    // the model from writing to a directory the store refuses to create.
    let workspace_section = if ephemeral {
        "### Workspace memory\nThis workspace is a temporary working directory, so no workspace \
         memory is stored. Use Global memory for notes that must persist."
            .to_owned()
    } else {
        let workspace =
            regenerate_scope_manifest(storage.workspace_dir(), V2MemoryScope::Workspace, budget)
                .map_err(|error| error.to_string())?;
        let workspace_body = workspace.content.trim();
        format!(
            "### Workspace memory\nScope root: `{}`\n\n{}",
            storage.workspace_dir().display(),
            if workspace_body.is_empty() {
                "(empty)"
            } else {
                workspace_body
            }
        )
    };
    let text = format!(
        "## Cross-session memory\n\n\
         Durable notes from earlier sessions. Treat them as historical context, not as the \
         current plan: verify recalled paths, commands and repository facts with live tools, \
         and prefer current evidence when memory and the workspace disagree. The index below \
         is generated and bounded; read a topic file with `read_file` before relying on its \
         facts. Do not edit `MEMORY.md` — it is regenerated. When the user asks you to \
         remember something durable, write it to a topic file under the scope root with the \
         ordinary file tools.\n\n\
         ### Global memory\nScope root: `{}`\n\n{}\n\n{}",
        storage.global_dir().display(),
        if global_body.is_empty() {
            "(empty)"
        } else {
            global_body
        },
        workspace_section,
    );
    Ok(Arc::from(text))
}
