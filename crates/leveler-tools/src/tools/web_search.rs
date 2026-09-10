//! `web_search` — search the web (Tavily) for up-to-date information the
//! repository and the model's training data may not cover.
//!
//! An OPTIONAL capability, and the tool does not decide whether it exists: the
//! composition root answers "is a key configured" once
//! (`leveler_app::Application::capability_availability`), and an unconfigured
//! host never registers this tool at all. What reaches `execute` is therefore
//! already carrying the key it needs — there is no "not configured" branch
//! here, because that state cannot reach here.
//!
//! Runtime failures are a different thing entirely and stay handled: a timeout,
//! an unreachable host, an HTTP error or an unparsable body all return an
//! `is_error` result stating the mechanical fact. The model decides what to do
//! next; this tool does not retry, does not fall back to another capability,
//! and does not advise.

use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::RiskLevel;

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

const ENDPOINT: &str = "https://api.tavily.com/search";
const TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_COUNT: usize = 5;
const MAX_COUNT: usize = 10;

#[derive(Debug, Deserialize, JsonSchema)]
struct Input {
    /// What to search the web for.
    query: String,
    /// How many results to return (default 5, max 10).
    #[serde(default)]
    count: Option<usize>,
}

/// The web search tool, constructed with the key the composition root found.
///
/// The key is held, never logged, never echoed: it is absent from `Debug` (not
/// derived), from every error string, and from every `ToolOutput`.
pub struct WebSearchTool {
    api_key: String,
}

impl WebSearchTool {
    /// Build the tool over a Tavily API key.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
        }
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &'static str {
        "web_search"
    }

    fn description(&self) -> &'static str {
        "Search the web for up-to-date information (documentation, APIs, error \
         messages, current facts) that may lie outside the repository or your \
         training data. Returns the top results as title + URL + snippet."
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<Input>()
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Network
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: Input = super::parse_input(self.name(), input)?;

        // NOT a duplicate of Host admission. The ToolHost freezes
        // `network_allowed` into the resolved policy, but only the OS sandbox
        // enforces it, and that covers `run_command` children — not an
        // in-process `reqwest` call. For a network tool that dials directly
        // this check is the ONLY enforcement point, exactly as in `web_fetch`
        // and the browser tools.
        if context.policy.network_denied() {
            return Ok(ToolOutput::error(
                "web_search 不可用:当前模式/沙箱已禁用网络。",
            ));
        }
        let query = input.query.trim().to_string();
        if query.is_empty() {
            return Ok(ToolOutput::error("web_search 需要非空 query。"));
        }
        let count = input.count.unwrap_or(DEFAULT_COUNT).clamp(1, MAX_COUNT);

        // Race the request against cancellation so a killed turn returns promptly.
        let result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return Ok(ToolOutput::error("web_search 已取消。"));
            }
            r = search(&self.api_key, &query, count) => r,
        };

        match result {
            Ok(text) if !text.trim().is_empty() => Ok(ToolOutput::ok(text)),
            Ok(_) => Ok(ToolOutput::ok(format!("web_search「{query}」:无结果。"))),
            // A mechanical fact about this request, and nothing more: what the
            // model does about it is the model's call.
            Err(reason) => Ok(ToolOutput::error(format!("web_search 失败:{reason}。"))),
        }
    }
}

/// The request body. Only the two fields the tool contract needs — Tavily's
/// other parameters stay out until a real consumer asks for one.
fn request_body(query: &str, count: usize) -> serde_json::Value {
    serde_json::json!({
        "query": query,
        "search_depth": "basic",
        "max_results": count,
    })
}

/// One request, one answer. No retry, no fallback.
async fn search(api_key: &str, query: &str, count: usize) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .post(ENDPOINT)
        .bearer_auth(api_key)
        .json(&request_body(query, count))
        .send()
        .await
        .map_err(net_reason)?;

    let status = resp.status();
    if !status.is_success() {
        return Err(format!("HTTP {}", status.as_u16()));
    }
    let json: serde_json::Value = resp.json().await.map_err(|_| "响应无法解析".to_string())?;
    Ok(format_results(&json, count))
}

/// Explain a transport failure in plain terms (offline vs timeout vs other).
fn net_reason(e: reqwest::Error) -> String {
    if e.is_timeout() {
        "请求超时".to_string()
    } else if e.is_connect() {
        "无法连接".to_string()
    } else {
        e.to_string()
    }
}

/// Format the top `count` results as `N. title\n   url\n   snippet`.
///
/// Provider-neutral by construction: the model sees titles, URLs and snippets,
/// never Tavily's response envelope or its per-result `score`.
fn format_results(json: &serde_json::Value, count: usize) -> String {
    let Some(items) = json.get("results").and_then(|v| v.as_array()) else {
        return String::new();
    };
    let mut out = String::new();
    for (i, item) in items.iter().take(count).enumerate() {
        let title = item.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let url = item.get("url").and_then(|v| v.as_str()).unwrap_or("");
        let snippet = item.get("content").and_then(|v| v.as_str()).unwrap_or("");
        out.push_str(&format!("{}. {title}\n   {url}\n   {snippet}\n\n", i + 1));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed Tavily body, shaped as the API documents it.
    fn tavily_fixture() -> serde_json::Value {
        serde_json::json!({
            "query": "tokio cancellation",
            "results": [
                {
                    "title": "Tokio CancellationToken",
                    "url": "https://docs.rs/tokio-util/latest/",
                    "content": "Cancellation tokens provide...",
                    "score": 0.98
                },
                {
                    "title": "Tokio Documentation",
                    "url": "https://tokio.rs/",
                    "content": "An asynchronous runtime.",
                    "score": 0.91
                },
                {
                    "title": "Third",
                    "url": "https://example.invalid/",
                    "content": "third snippet",
                    "score": 0.4
                }
            ],
            "response_time": 1.2
        })
    }

    #[test]
    fn formats_tavily_results_from_title_url_content() {
        let out = format_results(&tavily_fixture(), 5);
        assert!(out.contains("1. Tokio CancellationToken"));
        assert!(out.contains("https://docs.rs/tokio-util/latest/"));
        assert!(out.contains("Cancellation tokens provide..."));
        assert!(out.contains("2. Tokio Documentation"));
    }

    #[test]
    fn provider_internal_fields_never_reach_the_model() {
        let out = format_results(&tavily_fixture(), 5);
        for leaked in ["score", "0.98", "response_time", "1.2"] {
            assert!(!out.contains(leaked), "{leaked} leaked into the output");
        }
    }

    #[test]
    fn count_caps_the_results() {
        let out = format_results(&tavily_fixture(), 2);
        assert!(out.contains("1. Tokio CancellationToken") && out.contains("2. Tokio"));
        assert!(!out.contains("3. Third"), "count caps the results");
    }

    #[test]
    fn missing_results_yields_empty() {
        assert_eq!(format_results(&serde_json::json!({}), 5), "");
    }

    /// `count` maps to Tavily's `max_results`; `search_depth` is fixed and
    /// nothing else is sent.
    #[test]
    fn request_body_carries_only_query_depth_and_max_results() {
        let body = request_body("rust tokio", 7);
        assert_eq!(body["query"], "rust tokio");
        assert_eq!(body["search_depth"], "basic");
        assert_eq!(body["max_results"], 7);
        let keys: Vec<&String> = body.as_object().unwrap().keys().collect();
        assert_eq!(keys.len(), 3, "no extra provider parameters: {keys:?}");
    }

    /// The model's contract is `query` + `count` — a provider swap must not
    /// reach the schema.
    #[test]
    fn tool_schema_exposes_only_query_and_count() {
        let schema = WebSearchTool::new("tvly-test-value").input_schema();
        let props = schema["properties"].as_object().expect("properties");
        let mut names: Vec<&str> = props.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(names, ["count", "query"]);
    }
}
