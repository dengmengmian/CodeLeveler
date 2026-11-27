//! Host-side provider configuration: the ONE writer for `[providers.*]` and
//! `[models.*]` in the user's global config.
//!
//! The global config file (`~/.leveler/config.toml`) is the authority the
//! runtime loads through [`crate::ConfigManager`]. Before this module, the only
//! writer was `leveler login` (plaintext key upsert); a desktop client had no
//! supported way to configure a provider without leaving the product.
//!
//! Invariants this module exists to keep:
//!
//! * one write path: CLI login and the desktop bridge both edit the same file
//!   with the same `toml_edit` rules, so comments and unrelated tables survive;
//! * a save is only published when the runtime's own loader accepts it — the
//!   candidate text is assembled (global + project sources) and its provider
//!   registry is built *before* the file is replaced, so a rejected edit never
//!   replaces a working generation;
//! * the file is replaced atomically with owner-only permissions, because it
//!   holds plaintext provider keys;
//! * no function here ever returns, logs or formats a key value.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use leveler_model::ModelRef;
use leveler_provider::presets::PRESETS;
use leveler_provider::{ProviderConfig, builtin_model_profile};
use toml_edit::{DocumentMut, Item, Table, value};

use crate::LoadedConfig;
use crate::global_config::GlobalConfig;

/// Bounds shared with the desktop bridge. Ids and env names are also TOML
/// keys, so the bounds are deliberately conservative.
pub const MAX_PROVIDER_ID: usize = 64;
pub const MAX_BASE_URL: usize = 2_048;
pub const MAX_API_KEY_BYTES: usize = 16 * 1024;
pub const MAX_ENV_NAME: usize = 128;
pub const MAX_MODEL_ID: usize = 128;
pub const MAX_MODELS_PER_EDIT: usize = 8;
pub const MIN_CONTEXT_WINDOW: u32 = 1_024;
pub const MAX_CONTEXT_WINDOW: u32 = 16_777_216;

/// One model to ensure exists for the provider being edited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewModel {
    pub id: String,
    /// Hard context window for a model without a bundled profile. `None`
    /// keeps the loader default instead of inventing a fact.
    pub context_window: Option<u32>,
}

/// A structured provider edit. `None` fields mean "leave the stored value
/// alone" — an edit that does not name a key must never clear it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderDraft {
    pub id: String,
    pub protocol: String,
    pub base_url: String,
    /// `Some("")` removes the declared environment variable; `None` keeps it.
    pub api_key_env: Option<String>,
    /// `Some(key)` replaces the stored key; `None` keeps it.
    pub api_key: Option<String>,
    pub models: Vec<NewModel>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpsertMode {
    Add,
    Edit,
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderConfigError {
    #[error("{0}")]
    Invalid(String),
    #[error("provider `{0}` is already configured in the global config")]
    AlreadyExists(String),
    #[error("provider `{0}` is not configured in the global config")]
    NotInGlobal(String),
    #[error("model `{model}` is already defined for provider `{provider}`")]
    ModelConflict { model: String, provider: String },
    #[error("provider `{0}` is still provided by the project configuration")]
    ProjectProvider(String),
    #[error("project configuration still defines models for provider `{0}`")]
    ProjectModels(String),
    #[error("configuration rejected by the runtime loader: {0}")]
    Rejected(String),
    #[error("cannot write {path}: {error}")]
    Write { path: String, error: String },
}

/// What a removal actually changed, so the UI can state it instead of
/// re-deriving it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removal {
    pub text: String,
    pub removed_models: Vec<String>,
    pub default_cleared: bool,
}

/// Facts about the project-level sources that would survive a global removal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectReferences {
    pub providers: Vec<String>,
    pub models: Vec<String>,
}

/// Ensure `[providers.<id>]` and the requested `[models.<id>]` entries exist.
///
/// Existing model tables are never rewritten: their facts belong to the user
/// (or a previous `leveler login`). A model id already bound to a different
/// provider is a conflict, not something to silently reassign.
pub fn apply_upsert(
    text: &str,
    draft: &ProviderDraft,
    mode: UpsertMode,
) -> Result<String, ProviderConfigError> {
    validate_draft(draft, mode)?;
    let mut doc: DocumentMut = parse_document(text)?;
    let provider_id = draft.id.trim();

    let providers = doc["providers"].or_insert(toml_edit::table());
    if let Some(table) = providers.as_table_mut() {
        table.set_implicit(true);
    }
    let provider_table = providers
        .as_table_mut()
        .ok_or_else(|| ProviderConfigError::Invalid("[providers] must be a table".to_string()))?;
    let exists = provider_table.contains_key(provider_id);
    match (mode, exists) {
        (UpsertMode::Add, true) => {
            return Err(ProviderConfigError::AlreadyExists(provider_id.to_string()));
        }
        (UpsertMode::Edit, false) => {
            return Err(ProviderConfigError::NotInGlobal(provider_id.to_string()));
        }
        _ => {}
    }
    let entry = provider_table
        .entry(provider_id)
        .or_insert(toml_edit::table());
    let table = entry.as_table_mut().ok_or_else(|| {
        ProviderConfigError::Invalid(format!("providers.{provider_id} must be a table"))
    })?;
    table["protocol"] = value(draft.protocol.trim());
    table["base_url"] = value(draft.base_url.trim());
    match draft.api_key_env.as_deref() {
        Some(env) if env.trim().is_empty() => {
            table.remove("api_key_env");
        }
        Some(env) => {
            table["api_key_env"] = value(env.trim());
        }
        None => {}
    }
    if let Some(key) = draft.api_key.as_deref() {
        let key = key.trim();
        if !key.is_empty() {
            table["api_key"] = value(key);
            annotate_key(table);
        }
    }
    annotate_protocol(table);

    if !draft.models.is_empty() {
        let models = doc["models"].or_insert(toml_edit::table());
        if let Some(table) = models.as_table_mut() {
            table.set_implicit(true);
        }
        let models = models
            .as_table_mut()
            .ok_or_else(|| ProviderConfigError::Invalid("[models] must be a table".to_string()))?;
        for model in &draft.models {
            let model_id = model.id.trim();
            match models.get(model_id) {
                Some(existing) => {
                    let owner = existing.get("provider").and_then(|item| item.as_str());
                    if owner != Some(provider_id) {
                        return Err(ProviderConfigError::ModelConflict {
                            model: model_id.to_string(),
                            provider: owner.unwrap_or("<none>").to_string(),
                        });
                    }
                    // Keep the user's facts; a later `leveler login` writes the
                    // authoritative profile. Adding a model never overwrites one.
                }
                None => {
                    let mut model_table = Table::new();
                    write_new_model(&mut model_table, provider_id, draft, model)?;
                    models.insert(model_id, Item::Table(model_table));
                }
            }
        }
    }

    // The first configured model becomes the default only when the file has no
    // default at all. An existing choice is the user's, even if it is stale:
    // replacing it here would silently redirect every new session.
    if mode == UpsertMode::Add
        && doc.get("default_model").is_none()
        && let Some(first) = draft.models.first()
    {
        doc["default_model"] = value(format!("{provider_id}/{}", first.id.trim()));
        annotate_default_model(&mut doc);
    }
    Ok(doc.to_string())
}

/// Insert or replace one field under `[providers.<id>]`.
///
/// This is the CLI login's field-level edit: everything else — comments,
/// ordering, other providers, model tables — is preserved, because this file
/// is hand-maintained. The desktop editor uses [`apply_upsert`] instead; both
/// reach the same writer.
pub fn set_provider_field_text(
    text: &str,
    provider_id: &str,
    field: &str,
    field_value: &str,
) -> Result<String, ProviderConfigError> {
    if provider_id.trim().is_empty() {
        return Err(ProviderConfigError::Invalid(
            "provider id is empty".to_string(),
        ));
    }
    let mut doc: DocumentMut = parse_document(text)?;
    if !doc.contains_key("providers") {
        doc["providers"] = toml_edit::table();
    }
    let providers = doc
        .get_mut("providers")
        .and_then(|item| item.as_table_mut())
        .ok_or_else(|| ProviderConfigError::Invalid("[providers] must be a table".to_string()))?;
    providers.set_implicit(true);
    let entry = providers.entry(provider_id).or_insert(toml_edit::table());
    let table = entry.as_table_mut().ok_or_else(|| {
        ProviderConfigError::Invalid("provider config entry must be a table".to_string())
    })?;
    table[field] = value(field_value);
    match field {
        "api_key" => annotate_key(table),
        "protocol" => annotate_protocol(table),
        _ => {}
    }
    Ok(doc.to_string())
}

/// Remove `providers.<id>.api_key`; `None` means there was nothing to remove.
pub fn remove_provider_key_text(
    text: &str,
    provider_id: &str,
) -> Result<Option<String>, ProviderConfigError> {
    let mut doc: DocumentMut = parse_document(text)?;
    let Some(providers) = doc
        .get_mut("providers")
        .and_then(|item| item.as_table_mut())
    else {
        return Ok(None);
    };
    let Some(entry) = providers
        .get_mut(provider_id)
        .and_then(|item| item.as_table_mut())
    else {
        return Ok(None);
    };
    if entry.remove("api_key").is_none() {
        return Ok(None);
    }
    Ok(Some(doc.to_string()))
}

/// One stored field of a global provider, when present.
///
/// An edit that does not restate a field (a base URL the person did not
/// change) resolves the stored value here instead of round-tripping it through
/// the client, where a sanitized display value would overwrite the original.
pub fn global_provider_field(
    text: &str,
    provider_id: &str,
    field: &str,
) -> Result<Option<String>, ProviderConfigError> {
    let doc: DocumentMut = parse_document(text)?;
    Ok(doc
        .get("providers")
        .and_then(|item| item.get(provider_id))
        .and_then(|item| item.get(field))
        .and_then(|item| item.as_str())
        .map(str::to_string))
}

/// Remove one provider and the models the global config binds to it.
///
/// A `default_model` that names the removed provider is cleared together with
/// it: leaving it behind would make every new session fail with an error the
/// user cannot connect to the action they just took. The caller receives the
/// facts so the UI can say exactly what changed.
pub fn remove_provider(text: &str, provider_id: &str) -> Result<Removal, ProviderConfigError> {
    let provider_id = provider_id.trim();
    if provider_id.is_empty() || provider_id.len() > MAX_PROVIDER_ID {
        return Err(ProviderConfigError::Invalid(
            "provider id is empty or too long".to_string(),
        ));
    }
    let mut doc: DocumentMut = parse_document(text)?;
    let Some(providers) = doc
        .get_mut("providers")
        .and_then(|item| item.as_table_mut())
    else {
        return Err(ProviderConfigError::NotInGlobal(provider_id.to_string()));
    };
    if providers.remove(provider_id).is_none() {
        return Err(ProviderConfigError::NotInGlobal(provider_id.to_string()));
    }
    if providers.is_empty() {
        doc.remove("providers");
    }

    let mut removed_models = Vec::new();
    if let Some(models) = doc.get_mut("models").and_then(|item| item.as_table_mut()) {
        let doomed: Vec<String> = models
            .iter()
            .filter(|(_, item)| item.get("provider").and_then(|p| p.as_str()) == Some(provider_id))
            .map(|(key, _)| key.to_string())
            .collect();
        for key in &doomed {
            models.remove(key);
            removed_models.push(key.clone());
        }
        if models.is_empty() {
            doc.remove("models");
        }
    }
    removed_models.sort();

    let mut default_cleared = false;
    if let Some(raw) = doc.get("default_model").and_then(|item| item.as_str())
        && ModelRef::parse(raw).is_some_and(|reference| reference.provider == provider_id)
    {
        doc.remove("default_model");
        default_cleared = true;
    }
    Ok(Removal {
        text: doc.to_string(),
        removed_models,
        default_cleared,
    })
}

/// Set `default_model` in a config document, preserving everything else.
///
/// The disk write is a minimal change on purpose: an existing file is someone's
/// hand-maintained configuration, so editing the default must not add comments
/// or move the key. (The first-run generator annotates the file it creates.)
pub fn set_default_model_text(text: &str, model: &ModelRef) -> Result<String, ProviderConfigError> {
    let mut doc: DocumentMut = parse_document(text)?;
    doc["default_model"] = value(model.to_string());
    Ok(doc.to_string())
}

/// The project-level provider/model sources that reference `provider_id`.
///
/// Removing the global entry cannot take effect while a project bundle still
/// defines the provider or models bound to it; the caller refuses the removal
/// instead of pretending it happened.
pub fn project_references(
    layout: &leveler_project::Layout,
    provider_id: &str,
) -> Result<ProjectReferences, ProviderConfigError> {
    let mut references = ProjectReferences::default();
    for path in leveler_project::layout::yaml_files(&layout.providers_dir()) {
        let provider = leveler_provider::load_provider_config(&path)
            .map_err(|error| ProviderConfigError::Rejected(error.to_string()))?;
        if provider.id == provider_id {
            references.providers.push(path.display().to_string());
        }
    }
    references.models = project_models(layout)?
        .into_iter()
        .filter(|(_, provider)| provider == provider_id)
        .map(|(id, _)| id)
        .collect();
    Ok(references)
}

/// Every model id a project bundle defines, with its provider.
///
/// The repository bundle wins over the global config on an id collision, so a
/// global entry with the same id would be shadowed — invisible in the very
/// view that just "added" it.
pub fn project_models(
    layout: &leveler_project::Layout,
) -> Result<Vec<(String, String)>, ProviderConfigError> {
    let mut models = Vec::new();
    for path in leveler_project::layout::yaml_files(&layout.models_dir()) {
        let model = leveler_provider::load_model_config(&path)
            .map_err(|error| ProviderConfigError::Rejected(error.to_string()))?;
        models.push((model.profile.id, model.profile.provider));
    }
    Ok(models)
}

/// Validate the candidate global config with the runtime's own assembly and
/// only then replace the file atomically.
///
/// `load_config_with_global` merges the candidate with the project sources and
/// the registry build applies every model/route invariant, so a save can never
/// install a generation the next runtime would refuse to start with.
pub fn validate_candidate(
    layout: &leveler_project::Layout,
    text: &str,
) -> Result<(), ProviderConfigError> {
    let global = GlobalConfig::from_toml_str(text)
        .map_err(|error| ProviderConfigError::Rejected(error.to_string()))?;
    let loaded = crate::load_config_with_global(layout, global)
        .map_err(|error| ProviderConfigError::Rejected(error.to_string()))?;
    crate::config_manager::assemble_registry(&loaded)
        .map_err(|error| ProviderConfigError::Rejected(error.to_string()))?;
    Ok(())
}

/// Read-edit-validate-write the global config under the shared lock.
///
/// `mutate` sees the current text and returns the caller's result plus the
/// complete next text. The candidate is validated with the runtime's own
/// assembly before it may replace the file, and the locked read means a
/// concurrent provider, MCP or search edit is never overwritten from a stale
/// snapshot.
pub fn update<T>(
    layout: &leveler_project::Layout,
    mutate: impl FnMut(&str) -> Result<(T, String), ProviderConfigError>,
) -> Result<T, ProviderConfigError> {
    update_at(&layout.home().config_file(), mutate, |candidate| {
        validate_candidate(layout, candidate)
    })
}

/// [`update`] against an explicit path with a caller-supplied validator.
pub fn update_at<T>(
    path: &std::path::Path,
    mut mutate: impl FnMut(&str) -> Result<(T, String), ProviderConfigError>,
    mut validate: impl FnMut(&str) -> Result<(), ProviderConfigError>,
) -> Result<T, ProviderConfigError> {
    let result = crate::config_edit::edit(
        path,
        |doc| {
            let (out, next) = mutate(&doc.to_string())?;
            *doc = next.parse().map_err(|error: toml_edit::TomlError| {
                ProviderConfigError::Rejected(crate::global_config::safe_toml_parse_error(
                    &next,
                    error.message(),
                    error.span(),
                ))
            })?;
            Ok(out)
        },
        &mut validate,
    );
    match result {
        Ok(out) => Ok(out),
        Err(
            crate::config_edit::EditError::Apply(error)
            | crate::config_edit::EditError::Validate(error),
        ) => Err(error),
        Err(crate::config_edit::EditError::Parse(message)) => Err(ProviderConfigError::Rejected(
            format!("config is not valid TOML: {message}"),
        )),
        Err(crate::config_edit::EditError::Io(error)) => Err(ProviderConfigError::Write {
            path: path.display().to_string(),
            error,
        }),
        Err(crate::config_edit::EditError::ConcurrentModification) => {
            Err(ProviderConfigError::Rejected(
                "the configuration kept changing while it was being updated; nothing was written"
                    .into(),
            ))
        }
    }
}

/// The atomic, owner-only write used for every global-config replacement.
pub fn write_global_atomic(path: &Path, text: &str) -> Result<(), ProviderConfigError> {
    crate::global_config::write_global_config_atomic(path, text).map_err(|error| {
        ProviderConfigError::Write {
            path: path.display().to_string(),
            error: error.to_string(),
        }
    })
}

/// Where a provider's key comes from, when resolved right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    /// Plaintext `api_key` in the global config.
    Config,
    /// The environment variable named by `api_key_env`.
    Environment,
    /// Neither is configured (a keyless endpoint, or a missing credential).
    None,
}

impl KeySource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::Environment => "environment",
            Self::None => "none",
        }
    }
}

/// One provider, projected for a management surface. Never carries a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderView {
    pub id: String,
    pub protocol: String,
    pub base_url: String,
    pub api_key_env: Option<String>,
    pub api_key_configured: bool,
    pub key_source: KeySource,
    pub model_count: usize,
    /// `global` or `project`.
    pub source: &'static str,
    /// Whether the global config owns this entry (only those are writable).
    pub editable: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelView {
    pub provider: String,
    pub model: String,
    pub thinking: Option<String>,
    pub default: bool,
    /// Read-only capability/limit/pricing facts from the profile the runtime
    /// actually uses. A client displays them; it never edits them as ordinary
    /// settings, because a wrong `vision` or `pricing` changes what the runtime
    /// advertises and what a cost cap may admit.
    pub vision: bool,
    pub reasoning: bool,
    pub context_window: u32,
    pub reliable_context: u32,
    pub max_output_tokens: u32,
    pub pricing: Option<leveler_model::ModelPricing>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConfigView {
    pub config_path: String,
    pub config_exists: bool,
    pub default_model: Option<String>,
    pub resolved_default_model: Option<ModelRef>,
    pub providers: Vec<ProviderView>,
    pub models: Vec<ModelView>,
    /// Structurally usable: at least one provider, at least one model, and no
    /// model bound to a provider that is not configured.
    pub configuration_complete: bool,
    /// Facts a person can act on. Never a guess about credentials.
    pub issues: Vec<String>,
}

/// Project the effective configuration for the desktop management surface.
///
/// Every fact comes from the same `LoadedConfig` the runtime consumes; the
/// only extra read is the global file itself, to attribute a stored key and the
/// provider's origin.
pub fn describe(layout: &leveler_project::Layout, loaded: &LoadedConfig) -> ConfigView {
    let path = layout.home().config_file();
    let text = std::fs::read_to_string(&path).ok();
    let global_doc = text
        .as_deref()
        .and_then(|text| text.parse::<DocumentMut>().ok());
    let project_providers = project_provider_ids(layout);
    let global_ids: BTreeSet<&str> = loaded
        .global_provider_ids
        .iter()
        .map(String::as_str)
        .collect();

    let default_ref = loaded
        .default_model
        .as_deref()
        .and_then(ModelRef::parse)
        .filter(|reference| {
            loaded.models.iter().any(|model| {
                model.profile.provider == reference.provider && model.profile.id == reference.model
            })
        });

    let mut providers = Vec::with_capacity(loaded.providers.len());
    for provider in &loaded.providers {
        let stored_key = global_doc
            .as_ref()
            .and_then(|doc| doc.get("providers"))
            .and_then(|item| item.get(&provider.id))
            .and_then(|item| item.get("api_key"))
            .and_then(|item| item.as_str())
            .is_some_and(|key| !key.trim().is_empty());
        let env_configured = !provider.api_key_env.is_empty()
            && std::env::var(&provider.api_key_env)
                .map(|value| !value.trim().is_empty())
                .unwrap_or(false);
        // A project provider can carry its own plaintext key; the effective
        // provider config (not just the global document) is what can resolve.
        let inline_key = provider
            .api_key
            .as_deref()
            .is_some_and(|key| !key.trim().is_empty());
        let key_source = if stored_key || inline_key {
            KeySource::Config
        } else if env_configured {
            KeySource::Environment
        } else {
            KeySource::None
        };
        let from_project = project_providers.contains(provider.id.as_str());
        providers.push(ProviderView {
            id: provider.id.clone(),
            protocol: protocol_name(provider.protocol),
            base_url: crate::settings_service::display_safe_url(&provider.base_url),
            api_key_env: (!provider.api_key_env.is_empty()).then(|| provider.api_key_env.clone()),
            api_key_configured: inline_key || env_configured,
            key_source,
            model_count: loaded
                .models
                .iter()
                .filter(|model| model.profile.provider == provider.id)
                .count(),
            source: if from_project { "project" } else { "global" },
            editable: !from_project && global_ids.contains(provider.id.as_str()),
        });
    }
    providers.sort_by(|left, right| left.id.cmp(&right.id));

    let mut models: Vec<ModelView> = loaded
        .models
        .iter()
        .map(|model| ModelView {
            provider: model.profile.provider.clone(),
            model: model.profile.id.clone(),
            thinking: model
                .profile
                .thinking
                .map(|level| level.as_str().to_string()),
            default: default_ref.as_ref().is_some_and(|reference| {
                reference.provider == model.profile.provider && reference.model == model.profile.id
            }),
            vision: model.profile.capabilities.vision,
            reasoning: model.profile.capabilities.reasoning,
            context_window: model.profile.limits.context_window,
            reliable_context: model.profile.limits.reliable_context,
            max_output_tokens: model.profile.limits.max_output_tokens,
            pricing: model.profile.pricing,
        })
        .collect();
    models.sort_by(|left, right| {
        (left.provider.as_str(), left.model.as_str())
            .cmp(&(right.provider.as_str(), right.model.as_str()))
    });

    let mut issues = Vec::new();
    let provider_ids: BTreeSet<&str> = providers.iter().map(|p| p.id.as_str()).collect();
    for model in &models {
        if !provider_ids.contains(model.provider.as_str()) {
            issues.push(format!(
                "模型 `{}` 绑定的 Provider `{}` 未配置",
                model.model, model.provider
            ));
        }
    }
    if providers.is_empty() {
        issues.push("尚未配置任何 Provider".to_string());
    }
    if models.is_empty() {
        issues.push("尚未配置任何模型".to_string());
    }
    let configuration_complete = !providers.is_empty() && !models.is_empty() && issues.is_empty();

    ConfigView {
        config_path: path.display().to_string(),
        config_exists: path.is_file(),
        default_model: loaded.default_model.clone(),
        resolved_default_model: default_ref,
        providers,
        models,
        configuration_complete,
        issues,
    }
}

/// Presets for the add-provider form, from the one preset table the CLI uses.
pub fn presets_payload() -> Vec<serde_json::Value> {
    PRESETS
        .iter()
        .map(|preset| {
            serde_json::json!({
                "id": preset.id,
                "label": preset.label,
                "base_url": preset.base_url,
                "protocol": protocol_name(preset.protocol),
                "key_env": preset.key_env,
                "console_url": preset.console_url,
                "suggested_model": preset.suggested_model,
                "suggested_context": preset.suggested_context,
            })
        })
        .collect()
}

/// Whether a key is resolvable for this provider right now, without returning
/// it. Shared with the runtime settings projection so both surfaces agree.
pub(crate) fn api_key_present(provider: &ProviderConfig) -> bool {
    provider
        .api_key
        .as_deref()
        .is_some_and(|key| !key.trim().is_empty())
        || (!provider.api_key_env.is_empty()
            && std::env::var(&provider.api_key_env)
                .map(|value| !value.trim().is_empty())
                .unwrap_or(false))
}

fn project_provider_ids(layout: &leveler_project::Layout) -> BTreeSet<String> {
    leveler_project::layout::yaml_files(&layout.providers_dir())
        .into_iter()
        .filter_map(|path| leveler_provider::load_provider_config(&path).ok())
        .map(|provider| provider.id)
        .collect()
}

fn parse_document(text: &str) -> Result<DocumentMut, ProviderConfigError> {
    if text.trim().is_empty() {
        return Ok(DocumentMut::new());
    }
    text.parse::<DocumentMut>().map_err(|error| {
        // The parser's own message can quote the offending line, and this file
        // holds plaintext keys. Keep the location, drop the content.
        ProviderConfigError::Invalid(crate::global_config::safe_toml_parse_error(
            text,
            error.message(),
            error.span(),
        ))
    })
}

/// Validate the wire-level surface of a provider, independent of what it is
/// being used for. Shared by the editor and the draft probe so a candidate
/// that could never be saved is also never tested against a live endpoint.
pub fn validate_surface(
    id: &str,
    protocol: &str,
    base_url: &str,
    api_key_env: Option<&str>,
    api_key: Option<&str>,
    add: bool,
) -> Result<(), ProviderConfigError> {
    let id = id.trim();
    if id.is_empty() || id.len() > MAX_PROVIDER_ID {
        return Err(ProviderConfigError::Invalid(
            "Provider id 为空或超过 64 字节".to_string(),
        ));
    }
    if add
        && !id.chars().enumerate().all(|(index, ch)| {
            ch.is_ascii_lowercase()
                || ch.is_ascii_digit()
                || ch == '-'
                || ch == '_'
                || (index > 0 && ch == '.')
        })
    {
        return Err(ProviderConfigError::Invalid(
            "Provider id 只能使用小写字母、数字、`-`、`_`、`.`".to_string(),
        ));
    }
    if protocol_from_name(protocol).is_none() {
        return Err(ProviderConfigError::Invalid(format!(
            "不支持的协议 `{protocol}`"
        )));
    }
    let base_url = base_url.trim();
    if base_url.is_empty() || base_url.len() > MAX_BASE_URL {
        return Err(ProviderConfigError::Invalid(
            "Base URL 为空或超过 2048 字节".to_string(),
        ));
    }
    if !(base_url.starts_with("https://") || base_url.starts_with("http://")) {
        return Err(ProviderConfigError::Invalid(
            "Base URL 必须以 https:// 或 http:// 开头".to_string(),
        ));
    }
    if base_url.contains(' ') || base_url.contains('\n') || base_url.contains('\r') {
        return Err(ProviderConfigError::Invalid(
            "Base URL 不能包含空白".to_string(),
        ));
    }
    if let Some(env) = api_key_env
        && !env.trim().is_empty()
        && !valid_env_name(env.trim())
    {
        return Err(ProviderConfigError::Invalid(
            "环境变量名只能使用字母、数字和下划线，且不能以数字开头".to_string(),
        ));
    }
    if let Some(key) = api_key {
        if key.trim().is_empty() {
            return Err(ProviderConfigError::Invalid(
                "API Key 不能是空字符串；不修改请留空".to_string(),
            ));
        }
        if key.len() > MAX_API_KEY_BYTES {
            return Err(ProviderConfigError::Invalid(
                "API Key 超过 16 KiB 上限".to_string(),
            ));
        }
    }
    Ok(())
}

/// The protocol a stored/draft wire name denotes, or `None` when unknown.
/// The config loader silently defaults an unknown name to `openai_chat`;
/// a management surface must not.
pub fn protocol_from_name(name: &str) -> Option<leveler_model::ProtocolKind> {
    match name.trim() {
        "openai_chat" => Some(leveler_model::ProtocolKind::OpenAiChat),
        "openai_responses" => Some(leveler_model::ProtocolKind::OpenAiResponses),
        "anthropic_messages" => Some(leveler_model::ProtocolKind::AnthropicMessages),
        "gemini_generate_content" => Some(leveler_model::ProtocolKind::GeminiGenerateContent),
        _ => None,
    }
}

fn validate_draft(draft: &ProviderDraft, mode: UpsertMode) -> Result<(), ProviderConfigError> {
    validate_surface(
        &draft.id,
        &draft.protocol,
        &draft.base_url,
        draft.api_key_env.as_deref(),
        draft.api_key.as_deref(),
        mode == UpsertMode::Add,
    )?;
    if draft.models.len() > MAX_MODELS_PER_EDIT {
        return Err(ProviderConfigError::Invalid(format!(
            "一次最多添加 {MAX_MODELS_PER_EDIT} 个模型"
        )));
    }
    let mut seen = BTreeSet::new();
    for model in &draft.models {
        let model_id = model.id.trim();
        if model_id.is_empty() || model_id.len() > MAX_MODEL_ID {
            return Err(ProviderConfigError::Invalid(
                "模型 id 为空或超过 128 字节".to_string(),
            ));
        }
        if model_id.contains('/') || model_id.chars().any(char::is_whitespace) {
            return Err(ProviderConfigError::Invalid(format!(
                "模型 id `{model_id}` 不能包含 `/` 或空白"
            )));
        }
        if !seen.insert(model_id.to_string()) {
            return Err(ProviderConfigError::Invalid(format!(
                "模型 id `{model_id}` 重复"
            )));
        }
        if let Some(window) = model.context_window
            && !(MIN_CONTEXT_WINDOW..=MAX_CONTEXT_WINDOW).contains(&window)
        {
            return Err(ProviderConfigError::Invalid(format!(
                "上下文窗口必须在 {MIN_CONTEXT_WINDOW}..={MAX_CONTEXT_WINDOW} 之间"
            )));
        }
    }
    Ok(())
}

fn valid_env_name(name: &str) -> bool {
    if name.is_empty() || name.len() > MAX_ENV_NAME {
        return false;
    }
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn write_new_model(
    table: &mut Table,
    provider_id: &str,
    draft: &ProviderDraft,
    model: &NewModel,
) -> Result<(), ProviderConfigError> {
    let model_id = model.id.trim();
    table["provider"] = value(provider_id);
    let profile = builtin_model_profile(provider_id, model_id)
        .map_err(|error| ProviderConfigError::Invalid(error.to_string()))?;
    if let Some(profile) = profile {
        // One authoritative profile: the bundled facts the CLI login also
        // writes, so a desktop-added model cannot drift from a CLI-added one.
        crate::global_config::write_model_facts(table, &profile);
        if profile.id != model_id {
            table["model_id"] = value(model_id);
        }
        return Ok(());
    }
    // A model without bundled facts still has to declare the provider's wire
    // protocol: the route's reasoning-replay contract is resolved from it, and
    // the loader would otherwise default every entry to `openai_chat`.
    table["protocol"] = value(draft.protocol.trim());
    if let Some(window) = model.context_window {
        table["context_window"] = value(i64::from(window));
    }
    Ok(())
}

fn annotate_key(table: &mut Table) {
    if let Some(mut key) = table.key_mut("api_key") {
        key.leaf_decor_mut()
            .set_prefix("# 本机保存的 API Key，请勿提交到版本库。\n# Locally stored API key; never commit this file.\n");
    }
}

fn annotate_protocol(table: &mut Table) {
    if let Some(mut key) = table.key_mut("protocol") {
        key.leaf_decor_mut()
            .set_prefix("# 供应商使用的接口协议。\n# API protocol used by this provider.\n");
    }
}

fn annotate_default_model(doc: &mut DocumentMut) {
    if let Some(mut key) = doc.as_table_mut().key_mut("default_model") {
        key.leaf_decor_mut().set_prefix(
            "# 默认使用的模型，格式为 provider/model。\n# Default model, in provider/model form.\n",
        );
    }
}

fn protocol_name(protocol: leveler_model::ProtocolKind) -> String {
    serde_json::to_value(protocol)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string())
}

/// A model profile shaped like the generic entries `leveler init` writes.
/// Used by tests and by callers that need a profile without bundled facts.
#[cfg(test)]
fn generic_profile(provider: &str, model_id: &str, context: u32) -> leveler_model::ModelProfile {
    let text = crate::global_config::render_init_config(
        provider,
        "https://example.invalid",
        "",
        model_id,
        u64::from(context),
    );
    let global = GlobalConfig::from_toml_str(&text).unwrap();
    global.into_bundle().models.remove(0).profile
}

/// The file path a global edit targets for this layout.
pub fn global_config_path(layout: &leveler_project::Layout) -> PathBuf {
    layout.home().config_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"default_model = "deepseek/deepseek-flash"
lang = "zh"

[providers.deepseek]
base_url = "https://api.deepseek.com"
api_key_env = "DEEPSEEK_API_KEY"
api_key = "sk-stored"

[providers.kimi]
base_url = "https://api.moonshot.cn/v1"

[models."deepseek-flash"]
provider = "deepseek"
context_window = 131072

[models.kimi-basic]
provider = "kimi"
context_window = 262144
"#;

    fn draft(id: &str) -> ProviderDraft {
        ProviderDraft {
            id: id.to_string(),
            protocol: "openai_chat".to_string(),
            base_url: "https://api.example.com/v1".to_string(),
            api_key_env: None,
            api_key: Some("sk-new".to_string()),
            models: vec![NewModel {
                id: "example-model".to_string(),
                context_window: Some(131_072),
            }],
        }
    }

    #[test]
    fn adding_a_provider_writes_provider_model_and_default() {
        let out = apply_upsert(SAMPLE, &draft("example"), UpsertMode::Add).unwrap();
        let doc: DocumentMut = out.parse().unwrap();
        assert_eq!(
            doc["providers"]["example"]["base_url"].as_str(),
            Some("https://api.example.com/v1")
        );
        assert_eq!(
            doc["providers"]["example"]["api_key"].as_str(),
            Some("sk-new")
        );
        assert_eq!(
            doc["models"]["example-model"]["provider"].as_str(),
            Some("example")
        );
        assert_eq!(
            doc["models"]["example-model"]["context_window"].as_integer(),
            Some(131_072)
        );
        // An existing default model is the user's choice and must survive.
        assert_eq!(
            doc["default_model"].as_str(),
            Some("deepseek/deepseek-flash")
        );
        assert_eq!(doc["lang"].as_str(), Some("zh"));
    }

    #[test]
    fn the_first_provider_becomes_the_default_only_when_none_is_set() {
        let empty = "";
        let out = apply_upsert(empty, &draft("example"), UpsertMode::Add).unwrap();
        let doc: DocumentMut = out.parse().unwrap();
        assert_eq!(doc["default_model"].as_str(), Some("example/example-model"));

        let with_default = apply_upsert(SAMPLE, &draft("example"), UpsertMode::Add).unwrap();
        let doc: DocumentMut = with_default.parse().unwrap();
        assert_eq!(
            doc["default_model"].as_str(),
            Some("deepseek/deepseek-flash")
        );
    }

    #[test]
    fn editing_without_a_key_never_clears_the_stored_key() {
        let mut edit = draft("deepseek");
        edit.api_key = None;
        edit.api_key_env = None;
        edit.models = Vec::new();
        edit.base_url = "https://api.deepseek.com/v2".to_string();
        let out = apply_upsert(SAMPLE, &edit, UpsertMode::Edit).unwrap();
        let doc: DocumentMut = out.parse().unwrap();
        assert_eq!(
            doc["providers"]["deepseek"]["api_key"].as_str(),
            Some("sk-stored")
        );
        assert_eq!(
            doc["providers"]["deepseek"]["base_url"].as_str(),
            Some("https://api.deepseek.com/v2")
        );
    }

    #[test]
    fn adding_a_duplicate_provider_is_refused() {
        let error = apply_upsert(SAMPLE, &draft("deepseek"), UpsertMode::Add).unwrap_err();
        assert!(matches!(error, ProviderConfigError::AlreadyExists(id) if id == "deepseek"));
    }

    #[test]
    fn editing_an_unknown_provider_is_refused() {
        let error = apply_upsert(SAMPLE, &draft("nope"), UpsertMode::Edit).unwrap_err();
        assert!(matches!(error, ProviderConfigError::NotInGlobal(id) if id == "nope"));
    }

    #[test]
    fn a_model_bound_to_another_provider_is_a_conflict() {
        let mut add = draft("example");
        add.models = vec![NewModel {
            id: "deepseek-flash".to_string(),
            context_window: None,
        }];
        let error = apply_upsert(SAMPLE, &add, UpsertMode::Add).unwrap_err();
        assert!(matches!(error, ProviderConfigError::ModelConflict { .. }));
    }

    #[test]
    fn removing_a_provider_removes_its_models_and_clears_its_default() {
        let removal = remove_provider(SAMPLE, "deepseek").unwrap();
        let doc: DocumentMut = removal.text.parse().unwrap();
        assert!(doc["providers"].get("deepseek").is_none());
        assert!(doc["models"].get("deepseek-flash").is_none());
        assert!(doc["models"].get("kimi-basic").is_some());
        assert!(doc.get("default_model").is_none());
        assert!(removal.default_cleared);
        assert_eq!(removal.removed_models, vec!["deepseek-flash".to_string()]);
        // Unrelated data survives.
        assert_eq!(doc["lang"].as_str(), Some("zh"));
        assert_eq!(
            doc["providers"]["kimi"]["base_url"].as_str(),
            Some("https://api.moonshot.cn/v1")
        );
    }

    #[test]
    fn removal_reports_when_the_default_did_not_point_at_the_provider() {
        let removal = remove_provider(SAMPLE, "kimi").unwrap();
        assert!(!removal.default_cleared);
        assert_eq!(
            removal.text.parse::<DocumentMut>().unwrap()["default_model"].as_str(),
            Some("deepseek/deepseek-flash")
        );
    }

    #[test]
    fn project_models_do_not_get_removed_with_a_global_provider() {
        let text = r#"default_model = "p/m"
[providers.p]
base_url = "https://e"
[models.m]
provider = "p"
"#;
        let removal = remove_provider(text, "p").unwrap();
        let doc: DocumentMut = removal.text.parse().unwrap();
        assert!(doc.get("providers").is_none());
        assert!(doc.get("models").is_none());
        assert!(doc.get("default_model").is_none());
    }

    #[test]
    fn a_key_is_never_part_of_an_error() {
        let error = apply_upsert(SAMPLE, &draft("deepseek"), UpsertMode::Add).unwrap_err();
        assert!(!error.to_string().contains("sk-"));
        let mut bad = draft("x");
        bad.api_key = Some(String::new());
        let error = apply_upsert(SAMPLE, &bad, UpsertMode::Add).unwrap_err();
        assert!(!error.to_string().contains("sk-"));
    }

    #[test]
    fn invalid_protocol_and_base_url_are_refused_by_name_not_value() {
        let mut bad = draft("x");
        bad.protocol = "made_up".to_string();
        assert!(matches!(
            apply_upsert(SAMPLE, &bad, UpsertMode::Add).unwrap_err(),
            ProviderConfigError::Invalid(_)
        ));
        bad.protocol = "openai_chat".to_string();
        bad.base_url = "file:///etc/passwd".to_string();
        assert!(matches!(
            apply_upsert(SAMPLE, &bad, UpsertMode::Add).unwrap_err(),
            ProviderConfigError::Invalid(_)
        ));
    }

    #[test]
    fn a_bundled_model_gets_the_same_facts_as_cli_login() {
        let mut add = draft("bigmodel");
        add.base_url = "https://open.bigmodel.cn/api/coding/paas/v4".to_string();
        add.models = vec![NewModel {
            id: "glm-5.3".to_string(),
            context_window: None,
        }];
        let out = apply_upsert("", &add, UpsertMode::Add).unwrap();
        let doc: DocumentMut = out.parse().unwrap();
        let facts = &doc["models"]["glm-5.3"];
        assert_eq!(facts["model_id"].as_str(), Some("glm-5.3"));
        assert_eq!(facts["vision"].as_bool(), Some(false));
        assert_eq!(facts["reasoning"].as_bool(), Some(true));
        assert_eq!(facts["reasoning_style"].as_str(), Some("thinking_flag"));
        assert_eq!(facts["context_window"].as_integer(), Some(1_048_576));
        // The generated fact set must satisfy the runtime's route check.
        let global = GlobalConfig::from_toml_str(&out).unwrap();
        let bundle = global.into_bundle();
        leveler_model::ReasoningReplayContract::check_route(&bundle.models[0].profile).unwrap();
    }

    #[test]
    fn an_unknown_model_gets_the_provider_protocol() {
        let mut add = draft("custom");
        add.protocol = "anthropic_messages".to_string();
        add.models = vec![NewModel {
            id: "claude-x".to_string(),
            context_window: Some(200_000),
        }];
        let out = apply_upsert("", &add, UpsertMode::Add).unwrap();
        let doc: DocumentMut = out.parse().unwrap();
        assert_eq!(
            doc["models"]["claude-x"]["protocol"].as_str(),
            Some("anthropic_messages")
        );
        assert_eq!(
            doc["models"]["claude-x"]["context_window"].as_integer(),
            Some(200_000)
        );
    }

    #[test]
    fn generic_profiles_are_what_the_loader_defaults_to() {
        let profile = generic_profile("p", "m", 131_072);
        assert_eq!(profile.protocol, leveler_model::ProtocolKind::OpenAiChat);
        assert!(profile.capabilities.tool_calling);
        assert_eq!(profile.limits.context_window, 131_072);
    }

    #[test]
    fn default_model_text_edit_preserves_unrelated_tables() {
        let out = set_default_model_text(SAMPLE, &ModelRef::new("kimi", "kimi-basic")).unwrap();
        let doc: DocumentMut = out.parse().unwrap();
        assert_eq!(doc["default_model"].as_str(), Some("kimi/kimi-basic"));
        assert_eq!(
            doc["providers"]["deepseek"]["api_key"].as_str(),
            Some("sk-stored")
        );
    }

    /// Re-setting the value that is already stored must be a no-op on disk:
    /// the real-profile check moves the default and restores it, and the
    /// user's hand-maintained file must come back byte-identical.
    #[test]
    fn a_non_table_providers_section_is_an_error_not_a_panic() {
        let error = set_provider_field_text("providers = 3\n", "p", "api_key", "sk-x").unwrap_err();
        assert!(
            matches!(error, ProviderConfigError::Invalid(_)),
            "{error:?}"
        );
        assert!(apply_upsert("providers = 3\n", &draft("p"), UpsertMode::Add).is_err());
    }

    #[test]
    fn setting_the_stored_default_is_byte_identical() {
        let out =
            set_default_model_text(SAMPLE, &ModelRef::new("deepseek", "deepseek-flash")).unwrap();
        assert_eq!(out, SAMPLE);
    }

    #[test]
    fn a_draft_with_an_empty_key_is_rejected_instead_of_clearing() {
        let mut add = draft("x");
        add.api_key = Some("   ".to_string());
        assert!(matches!(
            apply_upsert(SAMPLE, &add, UpsertMode::Add).unwrap_err(),
            ProviderConfigError::Invalid(_)
        ));
    }

    /// The pre-write validation must reject a candidate the runtime loader
    /// would refuse, and leave the previous working file untouched.
    #[test]
    fn commit_never_replaces_a_working_file_with_a_rejected_generation() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let layout = leveler_project::Layout::ephemeral(workspace, None, &dir.path().join("home"));
        let good = apply_upsert("", &draft("p"), UpsertMode::Add).unwrap();
        update(&layout, |_| Ok(((), good.clone()))).unwrap();
        let path = global_config_path(&layout);
        let before = std::fs::read_to_string(&path).unwrap();

        // reliable_context >= context_window violates the registry's overflow
        // invariant, so the loader refuses this generation.
        let mut bad_doc: DocumentMut = good.parse().unwrap();
        bad_doc["models"]["example-model"]["reliable_context"] = value(200_000);
        let bad = bad_doc.to_string();
        let error = update(&layout, |_| Ok(((), bad.clone()))).unwrap_err();
        assert!(
            matches!(error, ProviderConfigError::Rejected(_)),
            "{error:?}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        assert!(
            error
                .to_string()
                .to_lowercase()
                .contains("reliable_context")
        );
    }

    #[test]
    fn describe_reports_structure_and_defaults_without_a_key_value() {
        let dir = tempfile::tempdir().unwrap();
        let text = SAMPLE.to_string();
        let global = GlobalConfig::from_toml_str(&text).unwrap();
        let loaded = crate::load_config_with_global(
            &leveler_project::Layout::ephemeral(
                dir.path().join("ws"),
                None,
                &dir.path().join("home"),
            ),
            global,
        )
        .unwrap();
        let layout = leveler_project::Layout::ephemeral(
            dir.path().join("ws"),
            None,
            &dir.path().join("home"),
        );
        let view = describe(&layout, &loaded);
        assert!(view.configuration_complete);
        let deepseek = view
            .providers
            .iter()
            .find(|provider| provider.id == "deepseek")
            .unwrap();
        // The global document is not on disk in this fixture, so the stored
        // key is not attributable; the value never appears either way.
        assert!(!format!("{view:?}").contains("sk-stored"));
        assert_eq!(deepseek.model_count, 1);
        let model = view
            .models
            .iter()
            .find(|model| model.model == "deepseek-flash")
            .unwrap();
        assert!(model.default);
        assert_eq!(
            view.resolved_default_model.as_ref().map(|r| r.to_string()),
            Some("deepseek/deepseek-flash".to_string())
        );
    }

    #[test]
    fn env_removal_is_explicit() {
        let mut edit = draft("deepseek");
        edit.api_key = None;
        edit.api_key_env = Some(String::new());
        edit.models = Vec::new();
        let out = apply_upsert(SAMPLE, &edit, UpsertMode::Edit).unwrap();
        let doc: DocumentMut = out.parse().unwrap();
        assert!(doc["providers"]["deepseek"].get("api_key_env").is_none());
        assert_eq!(
            doc["providers"]["deepseek"]["api_key"].as_str(),
            Some("sk-stored")
        );
    }
}
