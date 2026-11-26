//! Host-side provider management for the desktop bridge.
//!
//! The Desktop never receives a provider key back: this module accepts a value
//! once, hands it to the shared [`leveler_app::provider_config`] writer, and
//! returns only masked facts. Connection checks call the real endpoint through
//! the same discovery/probe code the CLI uses, and every failure is classified
//! from structured kinds (HTTP status, `ModelErrorKind`, transport fault) —
//! never from message text.
//!
//! Runtime consistency: the config file is the authority. A write is validated
//! with the runtime's own assembly before it lands, and an attached runtime is
//! asked to reload so the next turn cannot start on the previous generation.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use leveler_app::{ConfigManager, provider_config};
use leveler_client_protocol::ClientCommand;
use leveler_local_transport::LocalSocketRuntimeClient;
use leveler_model::{ModelErrorKind, ModelRef, ProtocolKind, TransportFault};
use leveler_project::Layout;
use leveler_provider::discovery::{DiscoveryError, list_remote_models};
use leveler_provider::probe_basic;
use leveler_provider::{ProviderConfig, ProviderRegistry, RegistryInputs};
use serde_json::{Value, json};

use leveler_provider::config::ConfigError;

/// The endpoint listing is a bounded diagnostic, not a catalog export.
const MAX_LISTED_MODELS: usize = 2_000;
const MAX_LISTED_MODEL_ID: usize = 256;
/// Outer bounds so the desktop request settles before its own transport
/// timeout. The inner clients have their own (15 s discovery, 120 s request).
const DISCOVERY_DEADLINE: Duration = Duration::from_secs(20);
const PROBE_DEADLINE: Duration = Duration::from_secs(40);

/// The full management projection plus the static presets the form offers.
pub(crate) async fn view(
    layout: Layout,
    client: Option<&Arc<LocalSocketRuntimeClient>>,
    installed: Option<&Layout>,
) -> anyhow::Result<Value> {
    let manager = ConfigManager::load(layout.clone())?;
    let snapshot = manager.snapshot();
    let described = provider_config::describe(&layout, &snapshot.loaded);
    let mut payload = json!({
        "config_path": described.config_path,
        "config_exists": described.config_exists,
        "default_model": described.default_model,
        "resolved_default_model": described.resolved_default_model,
        "providers": described.providers.iter().map(provider_json).collect::<Vec<_>>(),
        "models": described.models.iter().map(|model| json!({
            "provider": model.provider,
            "model": model.model,
            "thinking": model.thinking,
            "default": model.default,
        })).collect::<Vec<_>>(),
        "configuration_complete": described.configuration_complete,
        "issues": described.issues,
        "presets": provider_config::presets_payload(),
    });
    payload["runtime"] = runtime_state(client, installed, &layout, &snapshot.fingerprint).await;
    Ok(payload)
}

fn provider_json(provider: &provider_config::ProviderView) -> Value {
    json!({
        "id": provider.id,
        "protocol": provider.protocol,
        "base_url": provider.base_url,
        "api_key_env": provider.api_key_env,
        "api_key_configured": provider.api_key_configured,
        "key_source": provider.key_source.as_str(),
        "model_count": provider.model_count,
        "source": provider.source,
        "editable": provider.editable,
    })
}

/// Whether an attached runtime is serving the file this view just read.
///
/// `null` means the question is not answerable (no runtime, or a runtime that
/// predates `config_fingerprint`). Never reports `true` without both digests.
async fn runtime_state(
    client: Option<&Arc<LocalSocketRuntimeClient>>,
    installed: Option<&Layout>,
    file_layout: &Layout,
    file_fingerprint: &str,
) -> Value {
    let Some(client) = client else {
        return json!({"connected": false, "config_current": null});
    };
    // A fingerprint describes the sources of ONE environment. Comparing it
    // with a different workspace's files would be a false mismatch, so the
    // question is only answered for the layout the runtime was attached to.
    let same_environment = installed
        .is_some_and(|layout| layout.primary_workspace() == file_layout.primary_workspace());
    match leveler_local_transport::LocalRuntimeService::runtime_info(client.as_ref()).await {
        Ok(info) => {
            let matches = same_environment
                .then(|| {
                    info.config_fingerprint
                        .as_deref()
                        .map(|served| served == file_fingerprint)
                })
                .flatten();
            json!({
                "connected": true,
                "version": info.version,
                "config_current": matches,
                "same_environment": same_environment,
            })
        }
        Err(error) => json!({
            "connected": false,
            "config_current": null,
            "error": sanitize(&error.to_string()),
        }),
    }
}

/// Create or edit one provider from a structured draft.
pub(crate) async fn save(
    layout: Layout,
    client: Option<&Arc<LocalSocketRuntimeClient>>,
    installed: Option<&Layout>,
    params: &Value,
) -> anyhow::Result<Value> {
    let mode = match params.get("mode").and_then(Value::as_str) {
        Some("add") => provider_config::UpsertMode::Add,
        Some("edit") => provider_config::UpsertMode::Edit,
        _ => anyhow::bail!("mode must be `add` or `edit`"),
    };
    let mut draft = parse_draft(
        params
            .get("provider")
            .ok_or_else(|| anyhow::anyhow!("provider must describe the provider to save"))?,
        mode,
    )?;
    // The project bundle wins over the global config on an id collision. A
    // global entry added for an id the project already provides would be
    // shadowed: the save would "succeed" while the visible configuration did
    // not change. Refuse and name the real owner instead.
    if mode == provider_config::UpsertMode::Add {
        let references = provider_config::project_references(&layout, &draft.id)?;
        if !references.providers.is_empty() {
            anyhow::bail!("Provider `{}` 已由项目配置提供，不能重复添加", draft.id);
        }
    }
    if !draft.models.is_empty() {
        let project_models = provider_config::project_models(&layout)?;
        for model in &draft.models {
            if let Some((_, provider)) = project_models.iter().find(|(id, _)| id == model.id.trim())
            {
                anyhow::bail!(
                    "模型 `{}` 已由项目配置提供（Provider `{provider}`）",
                    model.id
                );
            }
        }
    }
    let path = provider_config::global_config_path(&layout);
    let existing = read_config_text(&path)?;
    if draft.base_url.trim().is_empty() {
        // The form omits a base URL it did not change; resolve the stored one
        // rather than replacing it with a display value.
        draft.base_url = provider_config::global_provider_field(&existing, &draft.id, "base_url")?
            .ok_or_else(|| {
                anyhow::anyhow!("provider `{}` has no stored base URL to keep", draft.id)
            })?;
        provider_config::validate_surface(
            &draft.id,
            &draft.protocol,
            &draft.base_url,
            draft.api_key_env.as_deref(),
            draft.api_key.as_deref(),
            false,
        )?;
    }
    let next = provider_config::apply_upsert(&existing, &draft, mode)?;
    provider_config::commit(&layout, &next)?;
    let reloaded = reload(client).await;
    let mut payload = view(layout, client, installed).await?;
    payload["runtime_reload"] = reloaded;
    Ok(payload)
}

/// Remove one provider from the global config, refusing while a project
/// source still owns or references it.
pub(crate) async fn remove(
    layout: Layout,
    client: Option<&Arc<LocalSocketRuntimeClient>>,
    installed: Option<&Layout>,
    params: &Value,
) -> anyhow::Result<Value> {
    let id = required_string(params, "id")?;
    let references = provider_config::project_references(&layout, id)?;
    if !references.providers.is_empty() {
        anyhow::bail!("Provider `{id}` 仍由项目配置提供，请在项目配置中移除");
    }
    if !references.models.is_empty() {
        anyhow::bail!(
            "项目配置仍有 {} 个模型使用 Provider `{id}`，请先移除这些模型",
            references.models.len()
        );
    }
    let path = provider_config::global_config_path(&layout);
    let existing = read_config_text(&path)?;
    let removal = provider_config::remove_provider(&existing, id)?;
    provider_config::commit(&layout, &removal.text)?;
    let reloaded = reload(client).await;
    let mut payload = view(layout, client, installed).await?;
    payload["removed_models"] = json!(removal.removed_models);
    payload["default_cleared"] = json!(removal.default_cleared);
    payload["runtime_reload"] = reloaded;
    Ok(payload)
}

/// Persist the user's default model and reconcile the attached runtime.
pub(crate) async fn set_default_model(
    layout: Layout,
    client: Option<&Arc<LocalSocketRuntimeClient>>,
    installed: Option<&Layout>,
    params: &Value,
) -> anyhow::Result<Value> {
    let model = parse_model_ref(
        params
            .get("model")
            .ok_or_else(|| anyhow::anyhow!("model must name the default"))?,
    )?;
    let manager = ConfigManager::load(layout.clone())?;
    let loaded = manager.snapshot().loaded.clone();
    if !loaded
        .models
        .iter()
        .any(|entry| entry.profile.provider == model.provider && entry.profile.id == model.model)
    {
        anyhow::bail!("model `{model}` is not configured");
    }
    let path = provider_config::global_config_path(&layout);
    let existing = read_config_text(&path)?;
    let next = provider_config::set_default_model_text(&existing, &model)?;
    provider_config::commit(&layout, &next)?;
    let reloaded = reload(client).await;
    let mut payload = view(layout, client, installed).await?;
    payload["runtime_reload"] = reloaded;
    Ok(payload)
}

/// Ask the attached runtime to re-read every configured source.
///
/// The watcher would apply the same generation within about a second; this is
/// the explicit path that closes the window between the write and the next
/// turn. It reports what was requested, never that the runtime finished.
pub(crate) async fn reload(client: Option<&Arc<LocalSocketRuntimeClient>>) -> Value {
    use leveler_client_protocol::InteractiveRuntimeClient;
    let Some(client) = client else {
        return json!({"requested": false, "reason": "no_runtime"});
    };
    let query_id = leveler_core::CommandId::generate();
    match client
        .send(ClientCommand::ReloadConfig {
            query_id: Some(query_id),
        })
        .await
    {
        Ok(()) => json!({"requested": true}),
        Err(error) => json!({
            "requested": false,
            "reason": "request_failed",
            "message": sanitize(&error.to_string()),
        }),
    }
}

/// List the models a provider endpoint advertises.
///
/// Accepts either a saved provider (`{id}`) or a draft
/// (`{provider: {...}}`) so first-run can check a key before writing it.
pub(crate) async fn models(layout: &Layout, params: &Value) -> anyhow::Result<Value> {
    let config = match params.get("provider") {
        Some(draft) if !draft.is_null() => parse_probe_provider(draft)?,
        _ => {
            let id = required_string(params, "id")?;
            let manager = ConfigManager::load(layout.clone())?;
            let snapshot = manager.snapshot();
            snapshot
                .loaded
                .providers
                .iter()
                .find(|provider| provider.id == id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("provider `{id}` is not configured"))?
        }
    };
    // A declared environment variable that is unset is a configuration fact,
    // not a network fact: report it without making a request.
    if let Err(error) = leveler_provider::resolve_api_key(&config) {
        return Ok(json!({
            "status": "incomplete",
            "message": sanitize(&error.to_string()),
            "models": [],
        }));
    }
    let started = std::time::Instant::now();
    let outcome = tokio::time::timeout(DISCOVERY_DEADLINE, list_remote_models(&config)).await;
    let mut payload = match outcome {
        Err(_) => json!({"status": "timeout", "message": "Provider 未在时限内响应", "models": []}),
        Ok(Ok(models)) => {
            let truncated = models.len() > MAX_LISTED_MODELS;
            let models: Vec<String> = models
                .into_iter()
                .take(MAX_LISTED_MODELS)
                .map(|model| model.chars().take(MAX_LISTED_MODEL_ID).collect())
                .collect();
            json!({
                "status": "ok",
                "message": "已读取模型列表",
                "models": models,
                "truncated": truncated,
            })
        }
        Ok(Err(error)) => discovery_failure(&error),
    };
    payload["elapsed_ms"] = json!(started.elapsed().as_millis() as u64);
    Ok(payload)
}

/// Run a real text + streaming request against one configured model.
pub(crate) async fn test(layout: &Layout, params: &Value) -> anyhow::Result<Value> {
    let provider_id = required_string(params, "id")?;
    let manager = ConfigManager::load(layout.clone())?;
    let loaded = manager.snapshot().loaded.clone();
    let provider = loaded
        .providers
        .iter()
        .find(|provider| provider.id == provider_id)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("provider `{provider_id}` is not configured"))?;
    let explicit = params
        .get("model")
        .filter(|value| !value.is_null())
        .map(parse_model_ref)
        .transpose()?;
    let model_id = match explicit {
        Some(reference) if reference.provider != provider_id => {
            anyhow::bail!("model `{reference}` does not belong to provider `{provider_id}`")
        }
        Some(reference) => reference.model,
        None => {
            let configured_default = loaded
                .default_model
                .as_deref()
                .and_then(ModelRef::parse)
                .filter(|reference| reference.provider == provider_id)
                .map(|reference| reference.model);
            match configured_default.or_else(|| {
                loaded
                    .models
                    .iter()
                    .find(|entry| entry.profile.provider == provider_id)
                    .map(|entry| entry.profile.id.clone())
            }) {
                Some(model) => model,
                None => {
                    return Ok(json!({
                        "status": "incomplete",
                        "message": "请先为该 Provider 添加模型，再测试连接",
                        "model": null,
                    }));
                }
            }
        }
    };
    let entry = loaded
        .models
        .iter()
        .find(|entry| entry.profile.provider == provider_id && entry.profile.id == model_id)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("model `{provider_id}/{model_id}` is not configured"))?;
    let key = match leveler_provider::resolve_api_key(&provider) {
        Ok(key) => key,
        Err(error) => {
            return Ok(json!({
                "status": "incomplete",
                "message": sanitize(&error.to_string()),
                "model": model_id,
            }));
        }
    };
    let registry = ProviderRegistry::build(RegistryInputs {
        providers: vec![(provider, key)],
        models: vec![entry],
    })
    .map_err(|error| anyhow::anyhow!("配置无法被运行时加载：{error}"))?;
    let started = std::time::Instant::now();
    let reference = ModelRef::new(provider_id, model_id.clone());
    let outcome = tokio::time::timeout(
        PROBE_DEADLINE,
        probe_basic(
            &registry,
            &reference,
            tokio_util::sync::CancellationToken::new(),
        ),
    )
    .await;
    let mut payload = match outcome {
        Err(_) => json!({
            "status": "timeout",
            "message": "Provider 在时限内未完成响应",
            "model": model_id,
        }),
        Ok(report) if report.healthy() => json!({
            "status": "ok",
            "message": "模型已返回响应（文本与流式）",
            "model": model_id,
        }),
        Ok(report) => probe_failure(&report, &model_id),
    };
    payload["elapsed_ms"] = json!(started.elapsed().as_millis() as u64);
    Ok(payload)
}

fn probe_failure(report: &leveler_provider::probe::BasicProbeReport, model: &str) -> Value {
    let status = classify_probe(report);
    let message = match status {
        "auth" => "认证失败：Provider 拒绝了这组凭据".to_string(),
        "rate_limit" => "Provider 暂时限流，请稍后重试".to_string(),
        "server" => "Provider 服务端错误，请稍后重试".to_string(),
        "network" => "无法连接 Provider：网络不可达".to_string(),
        "timeout" => "Provider 响应超时".to_string(),
        "model_not_found" => format!("模型 `{model}` 不存在或当前凭据不可用"),
        _ => sanitize(
            report
                .error
                .as_deref()
                .unwrap_or("Provider 返回了空的探测响应"),
        ),
    };
    json!({
        "status": status,
        "message": message,
        "model": model,
        "text_ok": report.text_ok,
        "stream_ok": report.stream_ok,
    })
}

/// Classify one failed probe from its structured facts. The message is never
/// inspected: two providers can describe the same HTTP status differently.
fn classify_probe(report: &leveler_provider::probe::BasicProbeReport) -> &'static str {
    if matches!(report.error_status, Some(401 | 403))
        || report.error_kind == Some(ModelErrorKind::Auth)
    {
        return "auth";
    }
    if matches!(report.error_status, Some(429))
        || report.error_kind == Some(ModelErrorKind::RateLimit)
    {
        return "rate_limit";
    }
    if report
        .error_status
        .is_some_and(|status| (500..=599).contains(&status))
    {
        return "server";
    }
    match report.error_kind {
        Some(ModelErrorKind::Timeout) => "timeout",
        Some(ModelErrorKind::Transport) => "network",
        Some(ModelErrorKind::ProviderUnavailable) => {
            if report.transport_fault == Some(TransportFault::ConnectionRefused) {
                "network"
            } else {
                "server"
            }
        }
        Some(ModelErrorKind::Decode) => "malformed",
        Some(ModelErrorKind::InvalidRequest) if report.error_status == Some(404) => {
            "model_not_found"
        }
        _ => "error",
    }
}

fn discovery_failure(error: &DiscoveryError) -> Value {
    let (status, message) = match error {
        DiscoveryError::Config(ConfigError::MissingEnv(name)) => (
            "incomplete",
            format!("环境变量 `{name}` 未设置，Provider 缺少凭据"),
        ),
        DiscoveryError::Config(other) => ("incomplete", sanitize(&other.to_string())),
        DiscoveryError::Auth { status, .. } => (
            "auth",
            format!(
                "认证失败：Provider 拒绝了这组凭据（HTTP {}）",
                status.as_u16()
            ),
        ),
        DiscoveryError::RateLimited { .. } => {
            ("rate_limit", "Provider 暂时限流，请稍后重试".to_string())
        }
        DiscoveryError::Server { status, .. } => (
            "server",
            format!("Provider 服务端错误（HTTP {}）", status.as_u16()),
        ),
        DiscoveryError::Timeout { .. } => ("timeout", "Provider 未在时限内响应".to_string()),
        DiscoveryError::Transport { .. } => {
            ("network", "无法连接 Provider：网络不可达".to_string())
        }
        DiscoveryError::Unsupported { status, .. } => (
            "unsupported",
            format!(
                "该服务未提供模型列表接口（HTTP {}）；可直接填写模型 ID",
                status.as_u16()
            ),
        ),
        DiscoveryError::Malformed { .. } => (
            "malformed",
            "服务返回了无法识别的模型列表；可直接填写模型 ID".to_string(),
        ),
    };
    json!({"status": status, "message": message, "models": []})
}

fn parse_draft(
    value: &Value,
    mode: provider_config::UpsertMode,
) -> anyhow::Result<provider_config::ProviderDraft> {
    let object = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("provider must be an object"))?;
    let id = required_string(value, "id")?;
    let protocol = required_string(value, "protocol")?;
    // `null` means "keep the stored value" on an edit. Add must state one.
    let base_url = optional_string(value, "base_url")?.unwrap_or_default();
    let api_key_env = optional_string(value, "api_key_env")?;
    let api_key = optional_string(value, "api_key")?;
    if mode == provider_config::UpsertMode::Add || !base_url.trim().is_empty() {
        provider_config::validate_surface(
            id,
            protocol,
            &base_url,
            api_key_env.as_deref(),
            api_key.as_deref(),
            mode == provider_config::UpsertMode::Add,
        )?;
    }
    let models = match object.get("models") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => {
            if items.len() > provider_config::MAX_MODELS_PER_EDIT {
                anyhow::bail!(
                    "at most {} models may be added at once",
                    provider_config::MAX_MODELS_PER_EDIT
                );
            }
            let mut models = Vec::with_capacity(items.len());
            for item in items {
                let model = required_string(item, "id")?;
                let context = item.get("context_window").and_then(Value::as_u64);
                models.push(provider_config::NewModel {
                    id: model.to_string(),
                    context_window: context.map(|window| {
                        u32::try_from(window).unwrap_or(provider_config::MAX_CONTEXT_WINDOW + 1)
                    }),
                });
            }
            models
        }
        Some(_) => anyhow::bail!("models must be an array"),
    };
    Ok(provider_config::ProviderDraft {
        id: id.to_string(),
        protocol: protocol.to_string(),
        base_url: base_url.to_string(),
        api_key_env,
        api_key,
        models,
    })
}

fn parse_probe_provider(value: &Value) -> anyhow::Result<ProviderConfig> {
    let id = value.get("id").and_then(Value::as_str).unwrap_or("draft");
    let protocol = required_string(value, "protocol")?;
    let base_url = required_string(value, "base_url")?;
    let api_key_env = optional_string(value, "api_key_env")?.unwrap_or_default();
    let api_key = optional_string(value, "api_key")?;
    provider_config::validate_surface(
        id,
        protocol,
        base_url,
        Some(&api_key_env),
        api_key.as_deref(),
        true,
    )?;
    let protocol: ProtocolKind =
        provider_config::protocol_from_name(protocol).expect("validated above");
    Ok(ProviderConfig {
        id: id.to_string(),
        protocol,
        base_url: base_url.trim().to_string(),
        api_key_env,
        api_key,
        headers: Default::default(),
        timeouts: Default::default(),
    })
}

fn parse_model_ref(value: &Value) -> anyhow::Result<ModelRef> {
    let provider = required_string(value, "provider")?;
    let model = required_string(value, "model")?;
    Ok(ModelRef::new(provider, model))
}

fn required_string<'a>(value: &'a Value, key: &str) -> anyhow::Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| anyhow::anyhow!("{key} must be a nonempty string"))
}

fn optional_string(value: &Value, key: &str) -> anyhow::Result<Option<String>> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => anyhow::bail!("{key} must be a string"),
    }
}

fn read_config_text(path: &Path) -> anyhow::Result<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(anyhow::anyhow!("cannot read {}: {error}", path.display())),
    }
}

/// Never let a provider response or an OS message carry a credential into a
/// client payload. Model mistakes do not either, but this is the one boundary.
fn sanitize(message: &str) -> String {
    leveler_core::sanitize_model_visible(message).0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_draft_with_a_foreign_protocol_is_refused_before_any_request() {
        let draft = json!({
            "id": "x",
            "protocol": "not_a_protocol",
            "base_url": "https://example.invalid",
        });
        assert!(parse_draft(&draft, provider_config::UpsertMode::Add).is_err());
        let draft = json!({
            "id": "x",
            "protocol": "openai_chat",
            "base_url": "ftp://example.invalid",
        });
        assert!(parse_draft(&draft, provider_config::UpsertMode::Add).is_err());
    }

    #[test]
    fn a_draft_never_echoes_its_key_in_an_error() {
        let draft = json!({
            "id": "x",
            "protocol": "openai_chat",
            "base_url": "https://example.invalid",
            "api_key": "   ",
        });
        let error = parse_draft(&draft, provider_config::UpsertMode::Add)
            .unwrap_err()
            .to_string();
        assert!(!error.contains("   "), "{error}");
    }

    #[test]
    fn a_probe_draft_requires_a_bounded_model_list() {
        let draft = json!({
            "id": "x",
            "protocol": "openai_chat",
            "base_url": "https://example.invalid",
            "models": vec![json!({"id": "m"}); provider_config::MAX_MODELS_PER_EDIT + 1],
        });
        assert!(parse_draft(&draft, provider_config::UpsertMode::Add).is_err());
    }

    #[test]
    fn probe_classification_uses_structured_facts_only() {
        let mut report = leveler_provider::probe::BasicProbeReport {
            error: Some("你被限流了".into()),
            error_kind: Some(ModelErrorKind::Auth),
            ..Default::default()
        };
        assert_eq!(classify_probe(&report), "auth");
        report.error_kind = Some(ModelErrorKind::RateLimit);
        assert_eq!(classify_probe(&report), "rate_limit");
        report.error_kind = Some(ModelErrorKind::ProviderUnavailable);
        report.transport_fault = Some(TransportFault::ConnectionRefused);
        assert_eq!(classify_probe(&report), "network");
        report.transport_fault = None;
        report.error_status = Some(503);
        assert_eq!(classify_probe(&report), "server");
        report.error_kind = Some(ModelErrorKind::InvalidRequest);
        report.error_status = Some(404);
        assert_eq!(classify_probe(&report), "model_not_found");
        // A message that LOOKS like a classification changes nothing.
        report.error_kind = Some(ModelErrorKind::Transport);
        report.error = Some("auth 404 rate limit".into());
        report.error_status = None;
        assert_eq!(classify_probe(&report), "network");
    }
}
