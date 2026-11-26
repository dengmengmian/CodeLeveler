//! Ask a provider which models a key can actually reach.
//!
//! Model ids and context windows move faster than a release, and which ones an
//! account can use depends on the plan behind the key — so a built-in table is
//! guesswork (see [`crate::presets`], which deliberately promises only the
//! endpoint and the protocol). This asks the provider instead.
//!
//! Both wire protocols expose the same shape at `GET {base_url}/models`:
//! `{"data": [{"id": "..."}, ...]}`. Providers that do not implement it (custom
//! gateways, local runtimes) return an error the caller reports as "could not
//! list" rather than pretending the account has no models.

use leveler_model::ProtocolKind;

use crate::config::{ConfigError, ProviderConfig, resolve_api_key};

/// Why a listing could not be produced.
///
/// Separate from [`ConfigError`]: "this gateway has no /models endpoint" is not
/// a configuration mistake, and callers present the two very differently.
#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    #[error("{0}")]
    Config(#[from] ConfigError),
    #[error("could not reach {url}: {source}")]
    Transport { url: String, source: reqwest::Error },
    /// The endpoint answered, but not before the client's deadline.
    #[error("{url} did not answer in time")]
    Timeout { url: String },
    /// The endpoint rejected the credential (401/403). Distinct from a gateway
    /// that simply has no `/models` route: a connection test must say which
    /// one it observed.
    #[error("{url} rejected the credential ({status})")]
    Auth {
        url: String,
        status: reqwest::StatusCode,
    },
    #[error("{url} is rate limiting this key ({status})")]
    RateLimited {
        url: String,
        status: reqwest::StatusCode,
    },
    #[error("{url} failed with a server error ({status})")]
    Server {
        url: String,
        status: reqwest::StatusCode,
    },
    #[error("{url} returned {status} — this provider may not implement /models")]
    Unsupported {
        url: String,
        status: reqwest::StatusCode,
    },
    #[error("{url} returned a listing this client does not understand: {detail}")]
    Malformed { url: String, detail: String },
}

/// Query `{base_url}/models`.
///
/// Returns ids sorted for stable display. An empty list is a real answer (a key
/// with no entitlements) and is distinct from an error.
pub async fn list_remote_models(config: &ProviderConfig) -> Result<Vec<String>, DiscoveryError> {
    let key = resolve_api_key(config)?;
    let url = models_url(&config.base_url);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|source| DiscoveryError::Transport {
            url: url.clone(),
            source,
        })?;

    let mut request = client.get(&url);
    // Mirror the chat path's auth rules: an explicit `x-api-key` (Anthropic)
    // replaces bearer auth rather than being sent alongside it, and the
    // Messages protocol itself authenticates with that header pair instead of
    // a bearer token.
    let has_explicit_key = config
        .headers
        .keys()
        .any(|k| k.eq_ignore_ascii_case("x-api-key"));
    if let Some(key) = key.as_deref().filter(|_| !has_explicit_key) {
        if config.protocol == ProtocolKind::AnthropicMessages {
            request = request
                .header("x-api-key", key)
                .header("anthropic-version", "2023-06-01");
        } else {
            request = request.bearer_auth(key);
        }
    }
    for (k, v) in &config.headers {
        request = request.header(k, v);
    }

    let response = match request.send().await {
        Ok(response) => response,
        Err(source) if source.is_timeout() => return Err(DiscoveryError::Timeout { url }),
        Err(source) => return Err(DiscoveryError::Transport { url, source }),
    };
    let status = response.status();
    if !status.is_success() {
        return Err(match status.as_u16() {
            401 | 403 => DiscoveryError::Auth { url, status },
            429 => DiscoveryError::RateLimited { url, status },
            500..=599 => DiscoveryError::Server { url, status },
            _ => DiscoveryError::Unsupported { url, status },
        });
    }
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|e| DiscoveryError::Malformed {
            url: url.clone(),
            detail: e.to_string(),
        })?;
    // Missing data retains the existing empty-list contract. An explicit
    // non-array data field is malformed, rather than an empty entitlement list.
    if body.get("data").is_some_and(|data| !data.is_array()) {
        return Err(DiscoveryError::Malformed {
            url,
            detail: "expected data to be an array".into(),
        });
    }
    Ok(parse_model_ids(&body))
}

/// `{base_url}/models`, tolerating a base that already ends in `/`.
fn models_url(base_url: &str) -> String {
    format!("{}/models", base_url.trim_end_matches('/'))
}

/// Extract ids from an OpenAI-shaped listing, ignoring entries without one.
///
/// Kept separate from the request so the parsing — the part that actually
/// varies between gateways — is testable without a network.
fn parse_model_ids(body: &serde_json::Value) -> Vec<String> {
    let items = body
        .get("data")
        .and_then(|d| d.as_array())
        .cloned()
        .unwrap_or_default();
    let mut ids: Vec<String> = items
        .iter()
        .filter_map(|m| {
            m.get("id")
                .and_then(|i| i.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_come_back_sorted_and_deduplicated() {
        let body = serde_json::json!({"data": [
            {"id": "gpt-4o"}, {"id": "claude-3"}, {"id": "gpt-4o"}
        ]});
        assert_eq!(parse_model_ids(&body), vec!["claude-3", "gpt-4o"]);
    }

    /// Gateways vary; entries without a usable id are skipped rather than
    /// surfacing as blank rows in a picker.
    #[test]
    fn entries_without_an_id_are_skipped() {
        let body = serde_json::json!({"data": [
            {"id": "ok"}, {"name": "no-id"}, {"id": ""}, {"id": "   "}
        ]});
        assert_eq!(parse_model_ids(&body), vec!["ok"]);
    }

    /// A key with no entitlements is a real answer, not a parse failure.
    #[test]
    fn an_empty_listing_is_empty_not_an_error() {
        assert!(parse_model_ids(&serde_json::json!({"data": []})).is_empty());
        assert!(parse_model_ids(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn the_url_does_not_double_its_slash() {
        assert_eq!(
            models_url("https://api.x.com/v1"),
            "https://api.x.com/v1/models"
        );
        assert_eq!(
            models_url("https://api.x.com/v1/"),
            "https://api.x.com/v1/models"
        );
    }
}
