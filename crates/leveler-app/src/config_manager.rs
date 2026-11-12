//! Runtime configuration lifecycle: load → validate → diff → publish.
//!
//! ONE owner for the configuration a running process actually uses. Before
//! this module, `Application::assemble` loaded the bundle once and the
//! resulting `LoadedConfig`/`ProviderRegistry` were immutable for the process
//! lifetime; every client compared a boot-time source digest with the files and
//! asked the runtime to be replaced (`RestartReason::ConfigChanged`), which is
//! why editing the config required quitting the TUI/Desktop.
//!
//! The manager keeps a versioned, atomically published [`ConfigSnapshot`]:
//!
//! * a successful load swaps the whole snapshot at once — provider, models,
//!   defaults and policy never come from different generations;
//! * an invalid or partially written file is rejected and the last successful
//!   snapshot (Last Known Good) stays in force, with the failure recorded;
//! * the version advances only when the *effective* configuration changed, so a
//!   comment edit or a duplicate reload rebuilds no clients;
//! * watchers publish [`ConfigApplied`] so UI layers can reconcile without
//!   owning a second copy of the configuration.
//!
//! Execution isolation is a consequence of the snapshot type: a turn captures
//! one `Arc<ConfigSnapshot>` when it starts and keeps it for its whole life, so
//! a reload can never swap the provider or model under a running request.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::{Duration, SystemTime};

use leveler_project::Layout;
use leveler_provider::{ProviderConfig, ProviderRegistry, RegistryInputs};
use sha2::{Digest, Sha256};
use tokio::sync::broadcast;

use crate::{AppError, LoadedConfig};

/// How often the watcher asks the filesystem for metadata. One `stat` per
/// configured source per tick; contents are hashed only when a stamp changes.
const WATCH_INTERVAL: Duration = Duration::from_millis(750);
/// A change must be stable for this long before it is applied, so an editor
/// that writes the file in several steps is read once, complete.
const DEBOUNCE: Duration = Duration::from_millis(250);
/// Bound on the "still being written" retry loop.
const DEBOUNCE_ROUNDS: usize = 20;

/// The configuration generation a running process is using.
#[derive(Clone)]
pub struct ConfigSnapshot {
    /// Monotonic effective-configuration version. Starts at 1 and advances
    /// only when a reload changed something the runtime actually consumes.
    pub version: u64,
    /// Digest of the configuration sources at the moment this snapshot was
    /// loaded. Clients compare it with the files to answer "is this runtime
    /// serving the current files?".
    pub fingerprint: String,
    /// The effective bundle, exactly as consumers see it.
    pub loaded: LoadedConfig,
    /// Digest of the registry inputs (including resolved credentials). Used to
    /// decide whether a reload needs to rebuild provider clients.
    pub registry_digest: String,
    /// Digest of the repository configuration, re-read by project consumers.
    pub project_digest: String,
    /// The provider registry this generation uses. Holding the `Arc` is what
    /// keeps an in-flight request's clients alive after a reload replaces it.
    pub registry: Arc<ProviderRegistry>,
}

/// Which dimensions of the effective configuration changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigChange {
    pub default_model: bool,
    pub thinking: bool,
    pub models: bool,
    pub providers: bool,
    pub mcp_servers: bool,
    pub context: bool,
    pub web_search: bool,
    /// `[retry]`: the registry generation carries the recovery budget, so a
    /// policy change rebuilds it like any other route fact.
    pub retry: bool,
    pub ui: bool,
    /// `[browser].default`: the browser capability's product is chosen when
    /// the browser handle is built, so a change is reported as restart-scoped
    /// instead of pretending it applied.
    pub browser: bool,
    pub assists: bool,
    pub agents: bool,
    pub vcs: bool,
    /// The repository's project policy file changed.
    pub project: bool,
}

impl ConfigChange {
    /// Whether anything the runtime consumes changed.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Whether provider clients and model profiles must be rebuilt.
    pub fn requires_registry_rebuild(&self) -> bool {
        self.models || self.providers || self.thinking || self.retry
    }

    /// Stable labels for logs and user-facing summaries. Never includes values,
    /// so no credential can leak through a change report.
    pub fn labels(&self) -> Vec<&'static str> {
        let mut labels = Vec::new();
        for (changed, label) in [
            (self.default_model, "default_model"),
            (self.thinking, "thinking"),
            (self.models, "models"),
            (self.providers, "providers"),
            (self.mcp_servers, "mcp_servers"),
            (self.context, "context"),
            (self.web_search, "web_search"),
            (self.retry, "retry"),
            (self.ui, "ui"),
            (self.browser, "browser"),
            (self.assists, "assists"),
            (self.agents, "agents"),
            (self.vcs, "vcs"),
            (self.project, "project"),
        ] {
            if changed {
                labels.push(label);
            }
        }
        labels
    }
}

/// One successful publication, for subscribers and UI notification text.
#[derive(Debug, Clone)]
pub struct ConfigApplied {
    pub version: u64,
    pub previous_version: u64,
    pub change: ConfigChange,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub thinking: Option<String>,
}

/// Outcome of one reload attempt.
#[derive(Debug, Clone)]
pub enum ReloadOutcome {
    /// The files are the current generation; nothing was rebuilt.
    Unchanged { version: u64 },
    /// A new generation is in force.
    Applied(ConfigApplied),
    /// Loading, validation or registry assembly failed. The previous snapshot
    /// remains in force (Last Known Good).
    Failed { error: String, version: u64 },
}

impl ReloadOutcome {
    pub fn version(&self) -> u64 {
        match self {
            Self::Unchanged { version } => *version,
            Self::Applied(applied) => applied.version,
            Self::Failed { version, .. } => *version,
        }
    }
}

/// Why a reload happened. Diagnostics only: every origin goes through the same
/// parse → validate → diff → publish chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReloadOrigin {
    /// The initial load during process assembly.
    Startup,
    /// A filesystem change observed by the watcher.
    Watch,
    /// A configuration write this runtime performed (the persisted default
    /// model), applied immediately instead of waiting for the watcher tick.
    Write,
    /// An explicit `/reload`.
    Command,
}

struct Watcher {
    cancel: tokio_util::sync::CancellationToken,
}

pub struct ConfigManager {
    layout: Layout,
    current: RwLock<Arc<ConfigSnapshot>>,
    /// Serializes reloads so two watchers/commands cannot interleave a load
    /// and publish a generation older than the one already in force.
    reload: tokio::sync::Mutex<()>,
    last_error: Mutex<Option<String>>,
    applied: broadcast::Sender<ConfigApplied>,
    /// Failures observed by the watcher. A command-triggered failure already
    /// answers its caller, so it is not broadcast here (no double notice).
    failed: broadcast::Sender<(u64, String)>,
    watcher: Mutex<Option<Watcher>>,
}

impl ConfigManager {
    /// Load the initial generation. A broken config is still a hard startup
    /// error: there is no previous generation to keep.
    pub fn load(layout: Layout) -> Result<Arc<Self>, AppError> {
        let loaded = crate::load_config(&layout)?;
        let fingerprint = crate::runtime_config_fingerprint(&layout)
            .map_err(|error| AppError::ConfigFingerprint(error.to_string()))?;
        let (providers, registry_digest) = registry_inputs(&loaded);
        let registry = ProviderRegistry::build(RegistryInputs {
            providers,
            models: loaded.models.clone(),
        })?
        .with_retry_budget(loaded.retry);
        let project_digest = project_policy_digest(&layout);
        let (applied, _) = broadcast::channel(16);
        let (failed, _) = broadcast::channel(16);
        Ok(Arc::new(Self {
            layout,
            current: RwLock::new(Arc::new(ConfigSnapshot {
                version: 1,
                fingerprint,
                loaded,
                registry_digest,
                project_digest,
                registry: Arc::new(registry),
            })),
            reload: tokio::sync::Mutex::new(()),
            last_error: Mutex::new(None),
            applied,
            failed,
            watcher: Mutex::new(None),
        }))
    }

    /// The configuration generation in force. Consumers capture this once per
    /// turn; holding the `Arc` keeps that generation's provider clients alive
    /// even after a later reload replaces the registry.
    pub fn snapshot(&self) -> Arc<ConfigSnapshot> {
        self.current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The last load/validation failure, or `None` when the current generation
    /// came from a clean load.
    pub fn last_error(&self) -> Option<String> {
        self.last_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Subscribe to successfully applied generations.
    pub fn subscribe(&self) -> broadcast::Receiver<ConfigApplied> {
        self.applied.subscribe()
    }

    /// Subscribe to watcher-observed failures: `(version in force, reason)`.
    pub fn subscribe_failures(&self) -> broadcast::Receiver<(u64, String)> {
        self.failed.subscribe()
    }

    /// The explicitly configured default model, when this generation can
    /// resolve it. `None` means "no configured default": callers keep the
    /// model they already have rather than inventing one.
    pub fn configured_default_model(&self) -> Option<leveler_model::ModelRef> {
        let snapshot = self.snapshot();
        let preferred = snapshot
            .loaded
            .default_model
            .as_deref()
            .and_then(leveler_model::ModelRef::parse)?;
        snapshot
            .registry
            .model_refs()
            .contains(&preferred)
            .then_some(preferred)
    }

    /// Re-read every configured source and publish the result.
    ///
    /// `force` re-reads and re-parses even when the source digest already
    /// matches; it never bumps the version when the effective configuration is
    /// unchanged.
    pub async fn reload(&self, origin: ReloadOrigin, force: bool) -> ReloadOutcome {
        let _guard = self.reload.lock().await;
        let previous = self.snapshot();
        let fingerprint_before = match crate::runtime_config_fingerprint(&self.layout) {
            Ok(fingerprint) => fingerprint,
            Err(error) => {
                return self.fail(
                    format!("cannot read configuration sources: {error}"),
                    &previous,
                    origin,
                );
            }
        };
        if !force && fingerprint_before == previous.fingerprint {
            // No source bytes changed; do not even re-parse. `force` still
            // re-reads because `/reload` promises to read the files again.
            self.last_error
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            return ReloadOutcome::Unchanged {
                version: previous.version,
            };
        }

        let loaded = match crate::load_config(&self.layout) {
            Ok(loaded) => loaded,
            Err(error) => {
                return self.fail(error.to_string(), &previous, origin);
            }
        };
        let fingerprint = match crate::runtime_config_fingerprint(&self.layout) {
            Ok(fingerprint) => fingerprint,
            Err(error) => {
                return self.fail(
                    format!("cannot read configuration sources: {error}"),
                    &previous,
                    origin,
                );
            }
        };
        if fingerprint != fingerprint_before {
            // The file changed while it was being parsed. Never publish a
            // generation assembled from two different states; the watcher will
            // see the newer stamp and retry.
            return self.fail(
                "configuration changed while it was being loaded; retrying on the next event"
                    .to_string(),
                &previous,
                origin,
            );
        }

        let (providers, registry_digest) = registry_inputs(&loaded);
        let registry_changed = registry_digest != previous.registry_digest;
        let project_digest = project_policy_digest(&self.layout);
        let mut change = diff(&previous.loaded, &loaded);
        if project_digest != previous.project_digest {
            change.project = true;
        }
        // A changed resolved credential (a new environment value read through
        // `/reload`, with the file itself untouched) is a real change to the
        // provider connection even though every declared field is identical.
        if registry_changed && change.is_empty() {
            change.providers = true;
        }
        let registry = if registry_changed {
            match ProviderRegistry::build(RegistryInputs {
                providers,
                models: loaded.models.clone(),
            }) {
                Ok(registry) => Arc::new(registry.with_retry_budget(loaded.retry)),
                Err(error) => return self.fail(error.to_string(), &previous, origin),
            }
        } else {
            previous.registry.clone()
        };

        // The source bytes moved but nothing the runtime consumes did (a
        // comment, key order, re-indenting). Adopt the new fingerprint without
        // advancing the version or rebuilding anything.
        if change.is_empty() {
            let replacement = Arc::new(ConfigSnapshot {
                version: previous.version,
                fingerprint,
                loaded: previous.loaded.clone(),
                registry_digest: previous.registry_digest.clone(),
                project_digest: previous.project_digest.clone(),
                registry: previous.registry.clone(),
            });
            self.publish_snapshot(replacement);
            self.last_error
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            return ReloadOutcome::Unchanged {
                version: previous.version,
            };
        }

        let version = previous.version.saturating_add(1);
        let default_ref = loaded
            .default_model
            .as_deref()
            .and_then(leveler_model::ModelRef::parse);
        let default_profile = default_ref.as_ref().and_then(|reference| {
            loaded.models.iter().find(|model| {
                reference.provider == model.profile.provider && reference.model == model.profile.id
            })
        });
        let provider = default_profile
            .map(|model| model.profile.provider.clone())
            .or_else(|| default_ref.as_ref().map(|r| r.provider.clone()));
        let model = loaded.default_model.clone();
        let thinking = default_profile
            .and_then(|entry| entry.profile.thinking)
            .map(|level| level.as_str().to_string());
        let applied = ConfigApplied {
            version,
            previous_version: previous.version,
            change,
            provider,
            model,
            thinking,
        };
        let replacement = Arc::new(ConfigSnapshot {
            version,
            fingerprint,
            loaded,
            registry_digest,
            project_digest,
            registry,
        });
        self.publish_snapshot(replacement);
        self.last_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let _ = self.applied.send(applied.clone());
        tracing::info!(
            origin = ?origin,
            version = applied.version,
            previous = applied.previous_version,
            changed = ?applied.change.labels(),
            "runtime configuration reloaded"
        );
        ReloadOutcome::Applied(applied)
    }

    fn fail(
        &self,
        error: String,
        previous: &Arc<ConfigSnapshot>,
        origin: ReloadOrigin,
    ) -> ReloadOutcome {
        let error = redact(&error);
        tracing::warn!(
            version = previous.version,
            error = %error,
            "configuration reload failed; keeping the last known good generation"
        );
        *self
            .last_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(error.clone());
        if origin == ReloadOrigin::Watch {
            let _ = self.failed.send((previous.version, error.clone()));
        }
        ReloadOutcome::Failed {
            error,
            version: previous.version,
        }
    }

    fn publish_snapshot(&self, snapshot: Arc<ConfigSnapshot>) {
        *self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = snapshot;
    }

    /// Start the filesystem watcher. Idempotent: a second call keeps the first
    /// watcher instead of racing a second one against it.
    pub fn start_watcher(manager: &Arc<Self>) {
        let mut slot = manager
            .watcher
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.is_some() {
            return;
        }
        let cancel = tokio_util::sync::CancellationToken::new();
        *slot = Some(Watcher {
            cancel: cancel.clone(),
        });
        drop(slot);
        let weak: Weak<Self> = Arc::downgrade(manager);
        // Take the baseline BEFORE spawning: a write that happens between this
        // call and the task's first tick is a change, not the initial state.
        let mut stamp = ConfigSources::stamp(&weak);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep(WATCH_INTERVAL) => {}
                }
                let Some(manager) = weak.upgrade() else {
                    break;
                };
                let current = ConfigSources::stamp(&weak);
                if current.is_some() && current == stamp {
                    continue;
                }
                // An editor may be mid-write. Wait for the stamp to settle, then
                // read the complete file; a parse failure keeps the previous
                // generation and the next settled change retries.
                let mut settled = current;
                for _ in 0..DEBOUNCE_ROUNDS {
                    tokio::time::sleep(DEBOUNCE).await;
                    let next = ConfigSources::stamp(&weak);
                    if next == settled {
                        break;
                    }
                    settled = next;
                }
                stamp = settled;
                let _ = manager.reload(ReloadOrigin::Watch, false).await;
            }
        });
    }

    /// Stop the watcher (process teardown).
    pub fn stop_watcher(&self) {
        if let Some(watcher) = self
            .watcher
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            watcher.cancel.cancel();
        }
    }
}

impl Drop for ConfigManager {
    fn drop(&mut self) {
        self.stop_watcher();
    }
}

/// Metadata stamps for every configuration source. Contents are never read
/// here: a change only has to be detected, and the loader re-reads the files
/// through the one validated path.
struct ConfigSources;

impl ConfigSources {
    fn stamp(manager: &Weak<ConfigManager>) -> Option<Vec<(PathBuf, Option<FileStamp>)>> {
        let manager = manager.upgrade()?;
        let layout = &manager.layout;
        let mut paths = vec![layout.home().config_file()];
        if let Some(root) = layout.primary_workspace() {
            paths.push(root.join(".leveler/config.yaml"));
        }
        paths.extend(leveler_project::layout::yaml_files(&layout.providers_dir()));
        paths.extend(leveler_project::layout::yaml_files(&layout.models_dir()));
        paths.sort();
        paths.dedup();
        Some(
            paths
                .into_iter()
                .map(|path| {
                    let stamp = std::fs::metadata(&path).ok().map(|metadata| FileStamp {
                        len: metadata.len(),
                        modified: metadata.modified().ok(),
                    });
                    (path, stamp)
                })
                .collect(),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileStamp {
    len: u64,
    modified: Option<SystemTime>,
}

/// Provider inputs for the registry, with credentials resolved HERE so a
/// reload picks up a changed key or `api_key_env` value.
fn registry_inputs(loaded: &LoadedConfig) -> (Vec<(ProviderConfig, Option<String>)>, String) {
    let mut hasher = Sha256::new();
    let mut providers = Vec::with_capacity(loaded.providers.len());
    for config in &loaded.providers {
        let key = leveler_provider::resolve_api_key(config).ok().flatten();
        // The serialized config omits the plaintext key; hash the resolved key
        // separately so a credential change forces a new client without ever
        // putting the secret into the digest input that could be logged.
        hasher.update(serde_json::to_vec(config).unwrap_or_default());
        if let Some(key) = &key {
            hasher.update(Sha256::digest(key.as_bytes()));
        }
        providers.push((config.clone(), key));
    }
    for model in &loaded.models {
        hasher.update(serde_json::to_vec(model).unwrap_or_default());
    }
    // The registry generation owns the recovery budget, so a budget-only edit
    // must still produce a new generation instead of silently keeping the old
    // policy in force.
    hasher.update(serde_json::to_vec(&loaded.retry).unwrap_or_default());
    (providers, format!("sha256:{:x}", hasher.finalize()))
}

/// Digest of the repository policy file the runtime reads per consumer. `None`
/// (absent) is a distinct state from an empty file.
fn project_policy_digest(layout: &Layout) -> String {
    let Some(root) = layout.primary_workspace() else {
        return "sha256:<no-workspace>".to_string();
    };
    let path = root.join(".leveler/config.yaml");
    let mut hasher = Sha256::new();
    match std::fs::read(&path) {
        Ok(bytes) => hasher.update(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => hasher.update(b"<absent>"),
        Err(error) => {
            hasher.update(b"<unreadable>");
            hasher.update(error.kind().to_string().as_bytes());
        }
    }
    format!("sha256:{:x}", hasher.finalize())
}

/// Compare two generations field by field. Every variant is a fact about what
/// the runtime consumes, never a heuristic about the file text.
pub fn diff(previous: &LoadedConfig, next: &LoadedConfig) -> ConfigChange {
    let previous_models = model_map(previous);
    let next_models = model_map(next);
    ConfigChange {
        default_model: previous.default_model != next.default_model,
        thinking: previous_models != next_models,
        models: model_catalog(previous) != model_catalog(next),
        providers: previous.providers != next.providers,
        mcp_servers: mcp_map(previous) != mcp_map(next),
        context: previous.context_soft_percent != next.context_soft_percent
            || previous.model_context_soft_percent != next.model_context_soft_percent,
        web_search: previous.web_search != next.web_search,
        retry: previous.retry != next.retry,
        ui: previous.lang != next.lang,
        browser: previous.browser_default != next.browser_default,
        assists: previous.assist_prompt_suggestions != next.assist_prompt_suggestions
            || previous.assist_away_summary != next.assist_away_summary,
        agents: previous.agents_delegation != next.agents_delegation
            || previous.agents_independent_review != next.agents_independent_review,
        vcs: previous.vcs_co_author != next.vcs_co_author,
        // Filled in by the manager from the project file digest; the bundle
        // itself does not carry project policy.
        project: false,
    }
}

fn model_map(config: &LoadedConfig) -> BTreeMap<(String, String), String> {
    config
        .models
        .iter()
        .map(|model| {
            (
                (model.profile.provider.clone(), model.profile.id.clone()),
                format!("{:?}", model.profile.thinking),
            )
        })
        .collect()
}

/// Everything about a model profile except its thinking level (reported
/// separately), so a context-window or capability edit is its own change.
fn model_catalog(config: &LoadedConfig) -> Vec<String> {
    let mut rows: Vec<String> = config
        .models
        .iter()
        .map(|model| {
            let mut copy = model.clone();
            copy.profile.thinking = None;
            serde_json::to_string(&copy).unwrap_or_default()
        })
        .collect();
    rows.sort();
    rows
}

fn mcp_map(
    config: &LoadedConfig,
) -> BTreeMap<String, (String, Vec<String>, Vec<String>, String, Vec<String>)> {
    config
        .mcp_servers
        .iter()
        .map(|server| {
            let mut secrets = server
                .headers
                .iter()
                .map(|(key, value)| format!("{key}={:x}", Sha256::digest(value.as_bytes())))
                .collect::<Vec<_>>();
            if let Some(token) = &server.bearer_token {
                secrets.push(format!("bearer={:x}", Sha256::digest(token.as_bytes())));
            }
            (
                server.name.clone(),
                (
                    server.command.clone(),
                    server.args.clone(),
                    server
                        .env
                        .iter()
                        .map(|(key, value)| format!("{key}={value}"))
                        .collect(),
                    server.url.clone().unwrap_or_default(),
                    secrets,
                ),
            )
        })
        .collect()
}

/// Never let a credential reach a log line, a notification or an error body.
fn redact(message: &str) -> String {
    leveler_core::sanitize_model_visible(message).0
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_provider::ModelConfigFile;

    fn config(default_model: &str, model: (&str, &str)) -> LoadedConfig {
        LoadedConfig {
            default_model: Some(default_model.to_string()),
            providers: vec![ProviderConfig {
                id: model.0.to_string(),
                protocol: leveler_model::ProtocolKind::OpenAiChat,
                base_url: "https://example.invalid".into(),
                api_key_env: String::new(),
                api_key: None,
                headers: BTreeMap::new(),
                timeouts: Default::default(),
            }],
            models: vec![ModelConfigFile {
                profile: leveler_model::ModelProfile {
                    id: model.1.to_string(),
                    provider: model.0.to_string(),
                    model_id: model.1.to_string(),
                    protocol: leveler_model::ProtocolKind::OpenAiChat,
                    capabilities: leveler_model::ModelCapabilities {
                        streaming: true,
                        tool_calling: true,
                        parallel_tool_calls: false,
                        structured_output: true,
                        reasoning: false,
                        vision: false,
                    },
                    limits: leveler_model::ModelLimits {
                        context_window: 131_072,
                        reliable_context: 65_536,
                        max_output_tokens: 8_192,
                        max_tool_schema_bytes: 32_768,
                        max_parallel_tool_calls: 1,
                        max_tool_output_bytes: None,
                    },
                    context_quality: None,
                    thinking: None,
                    reasoning: Default::default(),
                    compatibility: Default::default(),
                    pricing: None,
                },
                policy: None,
            }],
            ..LoadedConfig::default()
        }
    }

    #[test]
    fn an_identical_bundle_reports_no_change() {
        let a = config("p/m", ("p", "m"));
        let b = config("p/m", ("p", "m"));
        assert!(diff(&a, &b).is_empty());
    }

    #[test]
    fn a_default_model_change_is_its_own_dimension() {
        let a = config("p/m", ("p", "m"));
        let b = config("p/n", ("p", "m"));
        let change = diff(&a, &b);
        assert!(change.default_model);
        assert!(!change.models);
        assert!(!change.requires_registry_rebuild());
    }

    /// The recovery budget lives on the registry generation. A budget-only
    /// edit is therefore a real change, not a comment, and must rebuild the
    /// generation that carries it.
    #[test]
    fn a_retry_budget_change_is_its_own_registry_dimension() {
        let mut a = config("p/m", ("p", "m"));
        let mut b = config("p/m", ("p", "m"));
        b.retry = leveler_model::RetryBudget {
            max_retries: 3,
            recovery_seconds: Some(60),
        };
        assert!(diff(&a, &b).retry);
        assert!(diff(&a, &b).requires_registry_rebuild());
        assert_eq!(diff(&a, &b).labels(), vec!["retry"]);
        assert!(!diff(&a, &b).models);
        // The digest the manager uses to decide on a rebuild follows it too.
        let (_, before) = registry_inputs(&a);
        let (_, after) = registry_inputs(&b);
        assert_ne!(before, after);
        a.retry = b.retry;
        assert!(diff(&a, &b).is_empty());
    }

    #[test]
    fn a_registry_digest_never_carries_a_plaintext_key() {
        let mut loaded = config("p/m", ("p", "m"));
        loaded.providers[0].api_key = Some("sk-live-do-not-log".into());
        let (_, digest) = registry_inputs(&loaded);
        assert!(!digest.contains("sk-live"));
    }
}
