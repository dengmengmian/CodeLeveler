//! Search providers behind the model-facing `web_search` tool.
//!
//! The tool contract is provider-neutral: `query` + `count` in, a normalized
//! list of sources out. A provider owns one backend's request shape, auth and
//! response mapping; it never decides when to search, never starts a model
//! request, and never talks to a model provider. Selection happens in the
//! application composition root, so changing the conversation model cannot
//! change the search backend.

use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::NetworkScope;

/// Default DeepSeek Search API base, including the `/v1/tools` prefix.
/// `/search` is appended.
pub const DEEPSEEK_SEARCH_BASE_URL: &str = "https://api.deepseek.com/v1/tools";

/// Default Tavily API base. `/search` is appended.
pub const TAVILY_SEARCH_BASE_URL: &str = "https://api.tavily.com";

/// DeepSeek accepts 1–50 results per request.
pub const DEEPSEEK_MAX_RESULTS: usize = 50;

/// Hard cap on a search response body. Search answers are small; a body past
/// this is not a search result we can represent honestly.
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// Longest provider error detail kept in an error message.
const MAX_ERROR_DETAIL_CHARS: usize = 300;

/// Longest snippet kept per source. Search snippets are short (DeepSeek
/// documents roughly 100–200 characters); a provider that returns page text
/// must not flood the model's context through this field.
const MAX_SNIPPET_CHARS: usize = 1000;

/// The backend a search request is sent to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchProviderKind {
    DeepSeek,
    Tavily,
}

impl SearchProviderKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DeepSeek => "deepseek",
            Self::Tavily => "tavily",
        }
    }

    /// Parse the user-facing provider name.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "deepseek" => Some(Self::DeepSeek),
            "tavily" => Some(Self::Tavily),
            _ => None,
        }
    }
}

/// The network authority of one tool call, taken from the frozen
/// `ToolContext`. Providers pass it to the one Host HTTP gate; they never
/// construct their own client or widen the scope.
#[derive(Clone, Copy)]
pub struct SearchNetwork<'a> {
    pub scope: &'a NetworkScope,
    pub unrestricted: bool,
}

/// One normalizeable search source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SearchSource {
    pub title: String,
    pub url: String,
    pub snippet: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published_at: Option<String>,
}

/// The provider-neutral search result the tool renders and stores as metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SearchResult {
    pub sources: Vec<SearchSource>,
    pub truncated: bool,
}

/// A search failure carrying only mechanical facts. Provider credentials are
/// never part of this value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SearchError {
    #[error("搜索配置无效:{0}")]
    Config(String),
    #[error("当前权限未授予该网络目标")]
    Denied,
    #[error("请求超时")]
    Timeout,
    #[error("已取消")]
    Cancelled,
    #[error("搜索响应格式无效")]
    InvalidResponse,
    #[error("{message}")]
    Http { status: u16, message: String },
    #[error("请求失败:{0}")]
    Transport(String),
}

/// One search backend.
#[async_trait]
pub trait SearchProvider: Send + Sync {
    /// Stable id reported in tool metadata.
    fn id(&self) -> &'static str;

    /// The full endpoint this provider sends to. Used by admission to resolve
    /// destination evidence; it does not grant access.
    fn endpoint(&self) -> &str;

    /// Configured upper bound on returned sources.
    fn max_results(&self) -> usize;

    /// Run one search under the call's frozen network authority.
    async fn search(
        &self,
        query: &str,
        count: usize,
        network: SearchNetwork<'_>,
        cancellation: &CancellationToken,
    ) -> Result<SearchResult, SearchError>;
}

/// The official DeepSeek Search API: one direct HTTP request, no auxiliary
/// model turn.
pub struct DeepSeekSearchProvider {
    api_key: String,
    endpoint: String,
    max_results: usize,
    timeout: Duration,
}

impl DeepSeekSearchProvider {
    pub fn new(
        api_key: impl Into<String>,
        base_url: Option<&str>,
        max_results: usize,
        timeout: Duration,
    ) -> Result<Self, SearchError> {
        let api_key = api_key.into();
        if api_key.trim().is_empty() {
            return Err(SearchError::Config("缺少 DeepSeek API Key".into()));
        }
        if !(1..=DEEPSEEK_MAX_RESULTS).contains(&max_results) {
            return Err(SearchError::Config(format!(
                "max_results 必须在 1–{DEEPSEEK_MAX_RESULTS} 之间"
            )));
        }
        if timeout.is_zero() {
            return Err(SearchError::Config("timeout 必须大于 0".into()));
        }
        let endpoint = endpoint_from(base_url.unwrap_or(DEEPSEEK_SEARCH_BASE_URL))?;
        Ok(Self {
            api_key: api_key.trim().to_string(),
            endpoint,
            max_results,
            timeout,
        })
    }

    fn body(&self, query: &str, count: usize) -> serde_json::Value {
        serde_json::json!({
            "query": query,
            "count": count,
            "need_content": false,
        })
    }
}

#[async_trait]
impl SearchProvider for DeepSeekSearchProvider {
    fn id(&self) -> &'static str {
        SearchProviderKind::DeepSeek.as_str()
    }

    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    fn max_results(&self) -> usize {
        self.max_results
    }

    async fn search(
        &self,
        query: &str,
        count: usize,
        network: SearchNetwork<'_>,
        cancellation: &CancellationToken,
    ) -> Result<SearchResult, SearchError> {
        if cancellation.is_cancelled() {
            return Err(SearchError::Cancelled);
        }
        let requested = count;
        let count = count.clamp(1, DEEPSEEK_MAX_RESULTS).min(self.max_results);
        let body = self.body(query, count);
        let response = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(SearchError::Cancelled),
            response = crate::network::send(
                network.scope,
                network.unrestricted,
                &self.endpoint,
                reqwest::Method::POST,
                Some(&body),
                Some(&self.api_key),
                self.timeout,
            ) => map_transport(response, &self.api_key)?,
        };

        if cancellation.is_cancelled() {
            return Err(SearchError::Cancelled);
        }
        let status = response.status();
        if !status.is_success() {
            let message = redact(&error_detail(response).await, &self.api_key);
            return Err(http_error(status.as_u16(), message));
        }
        let bytes = read_body(response).await?;
        if cancellation.is_cancelled() {
            return Err(SearchError::Cancelled);
        }
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|_| SearchError::InvalidResponse)?;
        let mut result = map_deepseek_response(&value, count)?;
        // The configured provider cap may be below what the model asked for;
        // say so instead of returning fewer sources silently.
        result.truncated |= requested > count;
        Ok(result)
    }
}

/// Tavily search, kept as the legacy-compatible backend.
pub struct TavilySearchProvider {
    api_key: String,
    endpoint: String,
    max_results: usize,
    timeout: Duration,
}

impl TavilySearchProvider {
    pub fn new(
        api_key: impl Into<String>,
        base_url: Option<&str>,
        max_results: usize,
        timeout: Duration,
    ) -> Result<Self, SearchError> {
        let api_key = api_key.into();
        if api_key.trim().is_empty() {
            return Err(SearchError::Config("缺少 Tavily API Key".into()));
        }
        if max_results == 0 {
            return Err(SearchError::Config("max_results 必须大于 0".into()));
        }
        if timeout.is_zero() {
            return Err(SearchError::Config("timeout 必须大于 0".into()));
        }
        let endpoint = endpoint_from(base_url.unwrap_or(TAVILY_SEARCH_BASE_URL))?;
        Ok(Self {
            api_key: api_key.trim().to_string(),
            endpoint,
            max_results,
            timeout,
        })
    }

    fn body(&self, query: &str, count: usize) -> serde_json::Value {
        serde_json::json!({
            "query": query,
            "search_depth": "basic",
            "max_results": count,
        })
    }
}

#[async_trait]
impl SearchProvider for TavilySearchProvider {
    fn id(&self) -> &'static str {
        SearchProviderKind::Tavily.as_str()
    }

    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    fn max_results(&self) -> usize {
        self.max_results
    }

    async fn search(
        &self,
        query: &str,
        count: usize,
        network: SearchNetwork<'_>,
        cancellation: &CancellationToken,
    ) -> Result<SearchResult, SearchError> {
        if cancellation.is_cancelled() {
            return Err(SearchError::Cancelled);
        }
        let requested = count;
        let count = count.clamp(1, 50).min(self.max_results);
        let body = self.body(query, count);
        let response = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(SearchError::Cancelled),
            response = crate::network::send(
                network.scope,
                network.unrestricted,
                &self.endpoint,
                reqwest::Method::POST,
                Some(&body),
                Some(&self.api_key),
                self.timeout,
            ) => map_transport(response, &self.api_key)?,
        };

        if cancellation.is_cancelled() {
            return Err(SearchError::Cancelled);
        }
        let status = response.status();
        if !status.is_success() {
            let message = redact(&error_detail(response).await, &self.api_key);
            return Err(http_error(status.as_u16(), message));
        }
        let bytes = read_body(response).await?;
        if cancellation.is_cancelled() {
            return Err(SearchError::Cancelled);
        }
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|_| SearchError::InvalidResponse)?;
        let mut result = map_tavily_response(&value, count)?;
        result.truncated |= requested > count;
        Ok(result)
    }
}

/// Build `base + "/search"`, rejecting a base that is not an absolute HTTP(S)
/// URL. The base deliberately never comes from a model provider's endpoint.
fn endpoint_from(base: &str) -> Result<String, SearchError> {
    let base = base.trim().trim_end_matches('/');
    let url = reqwest::Url::parse(base)
        .map_err(|_| SearchError::Config(format!("搜索 API 地址无效:`{base}`")))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(SearchError::Config(format!(
            "搜索 API 地址必须是 http(s) URL:`{base}`"
        )));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(SearchError::Config(format!(
            "搜索 API 地址不能包含查询或片段:`{base}`"
        )));
    }
    Ok(format!("{base}/search"))
}

/// Map a transport failure without collapsing permission denials into
/// ordinary reachability errors, and without retaining credentials.
fn map_transport(
    response: Result<reqwest::Response, String>,
    api_key: &str,
) -> Result<reqwest::Response, SearchError> {
    match response {
        Ok(response) => Ok(response),
        Err(reason) => {
            let reason = redact(&reason, api_key);
            if reason.contains("scope")
                || reason.contains("blocked private")
                || reason.contains("URL credentials")
            {
                Err(SearchError::Denied)
            } else if reason.contains("timed out") || reason.contains("timeout") {
                Err(SearchError::Timeout)
            } else {
                Err(SearchError::Transport(reason))
            }
        }
    }
}

/// Remove a credential from any text that could become model-visible. The
/// upstream is untrusted: a hostile or broken endpoint could echo the header.
fn redact(text: &str, secret: &str) -> String {
    if secret.is_empty() {
        text.to_string()
    } else {
        text.replace(secret, "[redacted]")
    }
}

/// Read a successful body without buffering past the cap. Streaming matters:
/// `Response::bytes()` would buffer an arbitrary body before any length check.
async fn read_body(mut response: reqwest::Response) -> Result<Vec<u8>, SearchError> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| SearchError::Transport(e.to_string()))?
    {
        if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(SearchError::InvalidResponse);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Best-effort provider error text, bounded the same way. The body is
/// untrusted; no request header (and therefore no credential) is read here.
async fn error_detail(response: reqwest::Response) -> String {
    let Ok(bytes) = read_body_capped(response, 16 * 1024).await else {
        return String::new();
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return String::new();
    };
    let detail = value
        .get("error")
        .and_then(|error| {
            error
                .get("message")
                .and_then(serde_json::Value::as_str)
                .or_else(|| error.as_str())
        })
        .or_else(|| value.get("message").and_then(serde_json::Value::as_str))
        .or_else(|| value.get("detail").and_then(serde_json::Value::as_str))
        .unwrap_or("");
    let collapsed = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(MAX_ERROR_DETAIL_CHARS).collect()
}

/// Read a body no larger than `max` bytes.
async fn read_body_capped(
    mut response: reqwest::Response,
    max: usize,
) -> Result<Vec<u8>, SearchError> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| SearchError::Transport(e.to_string()))?
    {
        if body.len() + chunk.len() > max {
            return Err(SearchError::InvalidResponse);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Turn an HTTP status into a model-visible, actionable failure. The status is
/// always retained; auth never prints the key.
fn http_error(status: u16, detail: String) -> SearchError {
    let base = match status {
        401 => "鉴权失败(HTTP 401),请检查搜索 API Key".to_string(),
        403 => "访问被拒绝(HTTP 403)".to_string(),
        429 => "请求过于频繁(HTTP 429),请稍后重试".to_string(),
        500..=599 => format!("搜索服务异常(HTTP {status})"),
        _ => format!("请求被拒绝(HTTP {status})"),
    };
    let message = if detail.is_empty() {
        base
    } else {
        format!("{base}:{detail}")
    };
    SearchError::Http { status, message }
}

/// Project the DeepSeek Search API response. The documented envelope is
/// `{ "results": [{ title, url, snippet, date, ... }] }`; every other field
/// stays out of the model's context.
fn map_deepseek_response(
    value: &serde_json::Value,
    count: usize,
) -> Result<SearchResult, SearchError> {
    let items = value
        .get("results")
        .and_then(serde_json::Value::as_array)
        .ok_or(SearchError::InvalidResponse)?;
    let mut sources = Vec::with_capacity(items.len().min(count));
    for item in items {
        let object = item.as_object().ok_or(SearchError::InvalidResponse)?;
        let url = required_str(object.get("url"))?;
        if url.is_empty() {
            return Err(SearchError::InvalidResponse);
        }
        let title = required_str(object.get("title"))?;
        let snippet = required_str(object.get("snippet"))?;
        let published_at = optional_str(object.get("date"))?;
        push_unique(
            &mut sources,
            SearchSource {
                title: title.to_string(),
                url: url.to_string(),
                snippet: bounded_snippet(snippet),
                published_at,
            },
        );
    }
    Ok(finish(sources, count))
}

/// Project Tavily's response. `content` is its snippet field; an optional
/// `published_date` is kept when present.
fn map_tavily_response(
    value: &serde_json::Value,
    count: usize,
) -> Result<SearchResult, SearchError> {
    let items = value
        .get("results")
        .and_then(serde_json::Value::as_array)
        .ok_or(SearchError::InvalidResponse)?;
    let mut sources = Vec::with_capacity(items.len().min(count));
    for item in items {
        let object = item.as_object().ok_or(SearchError::InvalidResponse)?;
        let url = required_str(object.get("url"))?;
        if url.is_empty() {
            return Err(SearchError::InvalidResponse);
        }
        let title = required_str(object.get("title"))?;
        let snippet = required_str(object.get("content"))?;
        let published_at = optional_str(object.get("published_date"))?;
        push_unique(
            &mut sources,
            SearchSource {
                title: title.to_string(),
                url: url.to_string(),
                snippet: bounded_snippet(snippet),
                published_at,
            },
        );
    }
    Ok(finish(sources, count))
}

fn required_str(value: Option<&serde_json::Value>) -> Result<&str, SearchError> {
    value
        .and_then(serde_json::Value::as_str)
        .ok_or(SearchError::InvalidResponse)
}

/// Bound one snippet on a character boundary. An ellipsis marks the cut so the
/// model can tell a truncated excerpt from a complete one.
fn bounded_snippet(text: &str) -> String {
    if text.chars().count() <= MAX_SNIPPET_CHARS {
        return text.to_string();
    }
    let mut bounded: String = text.chars().take(MAX_SNIPPET_CHARS).collect();
    bounded.push('…');
    bounded
}

fn optional_str(value: Option<&serde_json::Value>) -> Result<Option<String>, SearchError> {
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(text)) => Ok((!text.is_empty()).then(|| text.clone())),
        Some(_) => Err(SearchError::InvalidResponse),
    }
}

/// Keep the first occurrence of each URL.
fn push_unique(sources: &mut Vec<SearchSource>, source: SearchSource) {
    if sources.iter().all(|seen| seen.url != source.url) {
        sources.push(source);
    }
}

/// Apply the result bound after de-duplication.
fn finish(mut sources: Vec<SearchSource>, count: usize) -> SearchResult {
    let truncated = sources.len() > count;
    sources.truncate(count);
    SearchResult { sources, truncated }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deepseek_fixture() -> serde_json::Value {
        serde_json::json!({
            "results": [
                {
                    "rerank_score": 2.5,
                    "title": "A",
                    "url": "https://a.test",
                    "snippet": "excerpt for A",
                    "summary": "longer summary for A",
                    "content": "",
                    "site_name": "A News",
                    "site_icon": "https://a.test/favicon.ico",
                    "date": "2026-02-02T16:00:00+00:00",
                    "authority": 4
                },
                {
                    "rerank_score": 1.2,
                    "title": "B",
                    "url": "https://b.test",
                    "snippet": "",
                    "summary": "",
                    "content": "",
                    "site_name": null,
                    "site_icon": null,
                    "date": null,
                    "authority": -1
                }
            ]
        })
    }

    #[test]
    fn deepseek_maps_documented_fields_and_nullable_date() {
        let result = map_deepseek_response(&deepseek_fixture(), 5).unwrap();
        assert_eq!(
            result,
            SearchResult {
                sources: vec![
                    SearchSource {
                        title: "A".into(),
                        url: "https://a.test".into(),
                        snippet: "excerpt for A".into(),
                        published_at: Some("2026-02-02T16:00:00+00:00".into()),
                    },
                    SearchSource {
                        title: "B".into(),
                        url: "https://b.test".into(),
                        snippet: String::new(),
                        published_at: None,
                    },
                ],
                truncated: false,
            }
        );
    }

    #[test]
    fn empty_results_are_a_successful_empty_search() {
        let result = map_deepseek_response(&serde_json::json!({"results": []}), 5).unwrap();
        assert!(result.sources.is_empty());
        assert!(!result.truncated);
    }

    #[test]
    fn malformed_deepseek_bodies_are_rejected() {
        for body in [
            serde_json::json!(null),
            serde_json::json!({}),
            serde_json::json!({"results": {}}),
            serde_json::json!({"results": [null]}),
            serde_json::json!({"results": [{"url": ""}]}),
            serde_json::json!({"results": [{"url": "https://a.test", "title": 1}]}),
            serde_json::json!({"results": [{"url": "https://a.test", "title": "", "snippet": 1}]}),
            serde_json::json!({"results": [{"url": "https://a.test", "title": "", "snippet": "", "date": 1}]}),
        ] {
            assert_eq!(
                map_deepseek_response(&body, 5),
                Err(SearchError::InvalidResponse),
                "{body}"
            );
        }
    }

    #[test]
    fn duplicate_urls_keep_the_first_source() {
        let body = serde_json::json!({"results": [
            {"url": "https://a.test", "title": "first", "snippet": "", "date": null},
            {"url": "https://a.test", "title": "second", "snippet": "", "date": null},
            {"url": "https://b.test", "title": "B", "snippet": "", "date": null}
        ]});
        let result = map_deepseek_response(&body, 5).unwrap();
        assert_eq!(result.sources.len(), 2);
        assert_eq!(result.sources[0].title, "first");
        assert!(!result.truncated, "a duplicate is not a truncation");
    }

    #[test]
    fn the_result_bound_truncates_and_flags() {
        let body = serde_json::json!({"results": [
            {"url": "https://a.test", "title": "A", "snippet": "", "date": null},
            {"url": "https://b.test", "title": "B", "snippet": "", "date": null}
        ]});
        let result = map_deepseek_response(&body, 1).unwrap();
        assert_eq!(result.sources.len(), 1);
        assert!(result.truncated);
    }

    #[test]
    fn a_very_long_snippet_is_bounded_on_a_character_boundary() {
        let long = "问".repeat(MAX_SNIPPET_CHARS + 10);
        let body = serde_json::json!({"results": [
            {"url": "https://a.test", "title": "A", "snippet": long, "date": null}
        ]});
        let result = map_deepseek_response(&body, 5).unwrap();
        let snippet = &result.sources[0].snippet;
        assert_eq!(snippet.chars().count(), MAX_SNIPPET_CHARS + 1);
        assert!(snippet.ends_with('…'));
    }

    #[test]
    fn tavily_mapping_keeps_title_url_content_and_optional_date() {
        let body = serde_json::json!({"results": [
            {"title": "T", "url": "https://t.test", "content": "C", "score": 0.9},
            {"title": "D", "url": "https://d.test", "content": "D", "published_date": "2026-01-01"}
        ]});
        let result = map_tavily_response(&body, 5).unwrap();
        assert_eq!(result.sources[0].snippet, "C");
        assert_eq!(result.sources[0].published_at, None);
        assert_eq!(
            result.sources[1].published_at.as_deref(),
            Some("2026-01-01")
        );
        assert!(!serde_json::to_string(&result).unwrap().contains("score"));
    }

    #[test]
    fn malformed_tavily_bodies_are_rejected() {
        for body in [
            serde_json::json!({}),
            serde_json::json!({"results": [{"url": "https://t.test"}]}),
            serde_json::json!({"results": [{"url": "https://t.test", "title": "T"}]}),
        ] {
            assert_eq!(
                map_tavily_response(&body, 5),
                Err(SearchError::InvalidResponse),
                "{body}"
            );
        }
    }

    #[test]
    fn status_errors_keep_the_status_and_never_echo_a_key() {
        for (status, needle) in [
            (401, "401"),
            (403, "403"),
            (429, "429"),
            (500, "500"),
            (502, "502"),
            (400, "400"),
        ] {
            let error = http_error(status, "upstream says no".into());
            let text = error.to_string();
            assert!(text.contains(needle), "{text}");
            assert!(!text.contains("sk-secret"), "{text}");
        }
    }

    #[test]
    fn endpoints_are_derived_from_the_search_base_not_a_model_endpoint() {
        assert_eq!(
            endpoint_from(DEEPSEEK_SEARCH_BASE_URL).unwrap(),
            "https://api.deepseek.com/v1/tools/search"
        );
        assert_eq!(
            endpoint_from("https://search.example/v1/tools/").unwrap(),
            "https://search.example/v1/tools/search"
        );
        assert!(endpoint_from("not a url").is_err());
        assert!(endpoint_from("ftp://example.com/v1/tools").is_err());
        assert!(endpoint_from("https://example.com/v1/tools?x=1").is_err());
    }

    #[test]
    fn deepseek_request_body_contains_only_documented_fields() {
        let provider = DeepSeekSearchProvider::new(
            "ds-key",
            Some("https://search.example/v1/tools"),
            5,
            Duration::from_secs(15),
        )
        .unwrap();
        assert_eq!(
            provider.body("rust", 3),
            serde_json::json!({"query": "rust", "count": 3, "need_content": false})
        );
    }

    #[test]
    fn tavily_request_body_keeps_the_existing_shape() {
        let provider =
            TavilySearchProvider::new("tvly-key", None, 5, Duration::from_secs(15)).unwrap();
        assert_eq!(
            provider.body("rust", 3),
            serde_json::json!({"query": "rust", "search_depth": "basic", "max_results": 3})
        );
    }

    #[test]
    fn provider_construction_rejects_unusable_values() {
        assert!(DeepSeekSearchProvider::new("", None, 5, Duration::from_secs(1)).is_err());
        assert!(DeepSeekSearchProvider::new("k", None, 0, Duration::from_secs(1)).is_err());
        assert!(DeepSeekSearchProvider::new("k", None, 51, Duration::from_secs(1)).is_err());
        assert!(DeepSeekSearchProvider::new("k", None, 5, Duration::ZERO).is_err());
        assert!(
            TavilySearchProvider::new("k", Some("not a url"), 5, Duration::from_secs(1)).is_err()
        );
    }

    #[test]
    fn provider_kind_parses_the_config_spelling() {
        assert_eq!(
            SearchProviderKind::parse(" DeepSeek "),
            Some(SearchProviderKind::DeepSeek)
        );
        assert_eq!(
            SearchProviderKind::parse("tavily"),
            Some(SearchProviderKind::Tavily)
        );
        assert_eq!(SearchProviderKind::parse("openai"), None);
    }
}
