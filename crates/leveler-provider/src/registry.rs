//! The provider registry and the concrete [`ModelRuntime`] built on top of it.
//!
//! The registry owns one `reqwest::Client` and one [`ProtocolAdapter`] per
//! provider, plus the resolved profile for every configured model. The
//! agent only ever sees the model-agnostic [`ModelRuntime`] surface.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use leveler_core::RequestId;
use leveler_model::{
    ModelError, ModelErrorKind, ModelEvent, ModelEventStream, ModelProfile, ModelRef, ModelRequest,
    ModelResponse, ModelRuntime, ProtocolAdapter, ProtocolContext, ProtocolKind,
};
use leveler_protocol::{AnthropicMessagesAdapter, OpenAiChatAdapter};

use crate::catalog::ModelConfigFile;
use crate::config::ProviderConfig;
use crate::transport::{response_to_byte_stream, send_with_retry};

/// A fully-wired provider: config, resolved API key, HTTP client, adapter.
struct Provider {
    config: ProviderConfig,
    api_key: Option<String>,
    client: reqwest::Client,
    adapter: Arc<dyn ProtocolAdapter>,
}

/// A resolved model: its capability profile.
#[derive(Clone)]
struct ModelEntry {
    profile: ModelProfile,
}

/// Errors building the registry.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("provider `{0}` uses protocol {1:?} which is not supported in this build")]
    UnsupportedProtocol(String, ProtocolKind),
    #[error("failed to build HTTP client for provider `{provider}`: {source}")]
    Client {
        provider: String,
        source: reqwest::Error,
    },
    #[error("invalid header `{0}` in provider config")]
    InvalidHeader(String),
    #[error(
        "model `{model}` has reliable_context ({reliable}) >= context_window ({window}); \
         the compaction threshold must leave headroom below the hard window or requests \
         can exceed it and fail"
    )]
    InvalidLimits {
        model: String,
        reliable: u32,
        window: u32,
    },
}

/// Inputs to assemble a registry. API keys are resolved by the caller (the app
/// composition root) so this crate stays free of stray env reads.
pub struct RegistryInputs {
    pub providers: Vec<(ProviderConfig, Option<String>)>,
    pub models: Vec<ModelConfigFile>,
}

/// The model-agnostic runtime the rest of the system depends on.
pub struct ProviderRegistry {
    providers: HashMap<String, Provider>,
    models: HashMap<(String, String), ModelEntry>,
}

impl ProviderRegistry {
    /// Build a registry from configuration.
    pub fn build(inputs: RegistryInputs) -> Result<Self, RegistryError> {
        let mut providers = HashMap::new();
        for (config, api_key) in inputs.providers {
            let adapter = adapter_for(&config)?;
            let client = build_client(&config)?;
            providers.insert(
                config.id.clone(),
                Provider {
                    config,
                    api_key,
                    client,
                    adapter,
                },
            );
        }

        let mut models = HashMap::new();
        for model in inputs.models {
            let profile = model.profile;
            // The no-overflow guarantee relies on the compaction threshold sitting
            // below the hard window (auto-compaction fires at reliable_context, but
            // only after a request is sent). Make that invariant explicit so a
            // misconfigured profile fails at startup instead of 400-ing mid-run.
            validate_limits(&profile.id, &profile.limits)?;
            models.insert(
                (profile.provider.clone(), profile.id.clone()),
                ModelEntry { profile },
            );
        }

        Ok(Self { providers, models })
    }

    /// List all configured model references.
    pub fn model_refs(&self) -> Vec<ModelRef> {
        self.models
            .keys()
            .map(|(provider, id)| ModelRef::new(provider.clone(), id.clone()))
            .collect()
    }

    fn provider(&self, id: &str) -> Result<&Provider, ModelError> {
        self.providers.get(id).ok_or_else(|| {
            ModelError::new(
                ModelErrorKind::InvalidRequest,
                format!("unknown provider `{id}`"),
            )
        })
    }

    fn entry(&self, model: &ModelRef) -> Result<&ModelEntry, ModelError> {
        self.models
            .get(&(model.provider.clone(), model.model.clone()))
            .ok_or_else(|| {
                ModelError::new(
                    ModelErrorKind::InvalidRequest,
                    format!("unknown model `{model}`"),
                )
            })
    }

    fn context(&self, provider: &Provider, profile: &ModelProfile) -> ProtocolContext {
        ProtocolContext {
            base_url: provider.config.base_url.clone(),
            model_id: profile.model_id.clone(),
            api_key: provider.api_key.clone(),
            extra_headers: Vec::new(),
            reasoning: profile.reasoning,
            parallel_tool_calls: profile.capabilities.parallel_tool_calls,
            supports_temperature: profile.compatibility.supports_temperature,
        }
    }

    fn endpoint(base_url: &str, path: &str) -> String {
        format!("{}{}", base_url.trim_end_matches('/'), path)
    }
}

#[async_trait]
impl ModelRuntime for ProviderRegistry {
    async fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        let provider = self.provider(&request.model.provider)?;
        let entry = self.entry(&request.model)?;
        let mut context = self.context(provider, &entry.profile);

        let encoded = provider
            .adapter
            .encode_request(&request, &context, true)
            .map_err(|e| ModelError::new(ModelErrorKind::InvalidRequest, e.to_string()))?;
        // Apply protocol-supplied headers (e.g. Anthropic's `x-api-key` /
        // `anthropic-version`); the transport only reads `context.extra_headers`.
        context
            .extra_headers
            .extend(encoded.headers.iter().cloned());
        let url = Self::endpoint(&context.base_url, &encoded.path);

        let response = send_with_retry(
            &provider.client,
            &url,
            &encoded.body,
            &context,
            &provider.config.retry,
            None, // streaming relies on the client's idle read timeout
            &cancellation,
        )
        .await?;

        let byte_stream = response_to_byte_stream(response);
        let decoded = provider
            .adapter
            .decode_stream(byte_stream, &context)
            .map_err(|e| ModelError::new(ModelErrorKind::Decode, e.to_string()))?;

        // Prepend the MessageStarted event (the protocol layer has no request id).
        let request_id = request.request_id.clone();
        let started =
            futures::stream::once(async move { Ok(ModelEvent::MessageStarted { request_id }) });
        Ok(Box::pin(started.chain(decoded)))
    }

    async fn generate(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        let provider = self.provider(&request.model.provider)?;
        let entry = self.entry(&request.model)?;
        let mut context = self.context(provider, &entry.profile);

        let encoded = provider
            .adapter
            .encode_request(&request, &context, false)
            .map_err(|e| ModelError::new(ModelErrorKind::InvalidRequest, e.to_string()))?;
        // Apply protocol-supplied headers (see `stream`); the transport only
        // reads `context.extra_headers`.
        context
            .extra_headers
            .extend(encoded.headers.iter().cloned());
        let url = Self::endpoint(&context.base_url, &encoded.path);

        let response = send_with_retry(
            &provider.client,
            &url,
            &encoded.body,
            &context,
            &provider.config.retry,
            Some(Duration::from_secs(
                provider.config.timeouts.request_seconds,
            )),
            &cancellation,
        )
        .await?;

        let body = response
            .bytes()
            .await
            .map_err(|e| crate::transport::map_reqwest_error(&e))?;

        let mut decoded = provider
            .adapter
            .decode_response(&body, &context)
            .map_err(|e| ModelError::new(ModelErrorKind::Decode, e.to_string()))?;
        decoded.request_id = RequestId::new(request.request_id.into_inner());
        Ok(decoded)
    }

    async fn profile(&self, model: &ModelRef) -> Result<ModelProfile, ModelError> {
        Ok(self.entry(model)?.profile.clone())
    }
}

/// Select the protocol adapter for a provider.
fn adapter_for(config: &ProviderConfig) -> Result<Arc<dyn ProtocolAdapter>, RegistryError> {
    match config.protocol {
        ProtocolKind::OpenAiChat => Ok(Arc::new(OpenAiChatAdapter::new())),
        ProtocolKind::AnthropicMessages => Ok(Arc::new(AnthropicMessagesAdapter::new())),
        other => Err(RegistryError::UnsupportedProtocol(config.id.clone(), other)),
    }
}

/// Build the HTTP client for a provider with its configured timeouts/headers.
/// The compaction threshold must leave headroom below the hard window, or a
/// request can be sent (compaction only fires afterward) that exceeds the window
/// and fails. A `reliable_context` of 0 means "disabled" and is allowed.
fn validate_limits(model: &str, limits: &leveler_model::ModelLimits) -> Result<(), RegistryError> {
    if limits.reliable_context > 0 && limits.reliable_context >= limits.context_window {
        return Err(RegistryError::InvalidLimits {
            model: model.to_string(),
            reliable: limits.reliable_context,
            window: limits.context_window,
        });
    }
    Ok(())
}

/// Whether `base_url`'s host is loopback (127.0.0.0/8, ::1, or "localhost").
///
/// A loopback endpoint must never be routed through an ambient HTTP proxy:
/// bypassing loopback is standard client behavior (curl and browsers do it) and
/// it keeps the local `MockServer` that the integration tests spin up
/// deterministic regardless of the shell's `http_proxy`/`all_proxy`. Remote
/// endpoints are unaffected and keep honoring the environment's proxy.
///
/// Unparseable URLs return `false` — leave proxy behavior unchanged rather than
/// guess.
fn base_url_is_loopback(base_url: &str) -> bool {
    let Some(host) = reqwest::Url::parse(base_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
    else {
        return false;
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    // `host_str()` wraps an IPv6 literal in brackets (`[::1]`); strip them
    // before parsing so `IpAddr::is_loopback` (127.0.0.0/8 and ::1) can decide.
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(&host);
    host.parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

fn build_client(config: &ProviderConfig) -> Result<reqwest::Client, RegistryError> {
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

    let mut headers = HeaderMap::new();
    for (name, value) in &config.headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| RegistryError::InvalidHeader(name.clone()))?;
        let value = HeaderValue::from_str(value)
            .map_err(|_| RegistryError::InvalidHeader(name.to_string()))?;
        headers.insert(name, value);
    }

    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(config.timeouts.connect_seconds))
        .read_timeout(Duration::from_secs(config.timeouts.idle_stream_seconds))
        .default_headers(headers);
    // Never proxy a loopback provider endpoint (see `base_url_is_loopback`).
    if base_url_is_loopback(&config.base_url) {
        builder = builder.no_proxy();
    }
    builder.build().map_err(|source| RegistryError::Client {
        provider: config.id.clone(),
        source,
    })
}

#[cfg(test)]
mod proxy_tests {
    use super::base_url_is_loopback;

    #[test]
    fn loopback_hosts_bypass_proxy() {
        assert!(base_url_is_loopback("http://127.0.0.1:8080/v1"));
        assert!(base_url_is_loopback("http://127.0.0.53:9")); // whole 127/8
        assert!(base_url_is_loopback("http://localhost:1234"));
        assert!(base_url_is_loopback("http://LocalHost")); // case-insensitive
        assert!(base_url_is_loopback("http://[::1]:1234/v1"));
    }

    #[test]
    fn remote_hosts_keep_proxy() {
        assert!(!base_url_is_loopback("https://api.deepseek.com"));
        assert!(!base_url_is_loopback("https://192.168.1.10:8080")); // LAN, not loopback
        assert!(!base_url_is_loopback("https://10.0.0.1"));
    }

    #[test]
    fn unparseable_leaves_proxy_unchanged() {
        assert!(!base_url_is_loopback(""));
        assert!(!base_url_is_loopback("not a url"));
    }
}

#[cfg(test)]
mod limits_tests {
    use super::*;
    use leveler_model::ModelLimits;

    fn limits(window: u32, reliable: u32) -> ModelLimits {
        ModelLimits {
            context_window: window,
            reliable_context: reliable,
            max_output_tokens: 8192,
            max_tool_schema_bytes: 32768,
            max_parallel_tool_calls: 1,
        }
    }

    #[test]
    fn reliable_below_window_is_ok() {
        assert!(validate_limits("m", &limits(128_000, 64_000)).is_ok());
    }

    #[test]
    fn disabled_reliable_context_is_ok() {
        assert!(validate_limits("m", &limits(128_000, 0)).is_ok());
    }

    #[test]
    fn reliable_at_or_above_window_is_rejected() {
        assert!(matches!(
            validate_limits("m", &limits(128_000, 128_000)),
            Err(RegistryError::InvalidLimits { .. })
        ));
        assert!(matches!(
            validate_limits("m", &limits(64_000, 65_000)),
            Err(RegistryError::InvalidLimits { .. })
        ));
    }
}
