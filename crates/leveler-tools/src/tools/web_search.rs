//! `web_search` — search the web (Bing or Google Custom Search) for up-to-date
//! information the repository and the model's training data may not cover.
//!
//! Configured via environment variables (secrets stay out of the config file):
//!   - `LEVELER_SEARCH_API_KEY`   — required
//!   - `LEVELER_SEARCH_PROVIDER`  — "bing" (default) or "google"
//!   - `LEVELER_SEARCH_CX`        — Google Custom Search engine id (google only)
//!
//! Degrades gracefully: an unconfigured key, a denied-network mode, a timeout,
//! or an unreachable host all return an `is_error` result the model can read and
//! route around — they never abort the agent loop.

use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::RiskLevel;

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

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

pub struct WebSearchTool;

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

        if context.deny_network {
            return Ok(ToolOutput::error(
                "web_search 不可用:当前模式/沙箱已禁用网络。",
            ));
        }
        let query = input.query.trim().to_string();
        if query.is_empty() {
            return Ok(ToolOutput::error("web_search 需要非空 query。"));
        }
        let count = input.count.unwrap_or(DEFAULT_COUNT).clamp(1, MAX_COUNT);

        let key = match context.environment.var("LEVELER_SEARCH_API_KEY") {
            Some(k) if !k.trim().is_empty() => k,
            _ => {
                return Ok(ToolOutput::error(
                    "web_search 未配置:请设置环境变量 LEVELER_SEARCH_API_KEY \
                     (可选 LEVELER_SEARCH_PROVIDER=bing|google;google 还需 \
                     LEVELER_SEARCH_CX)。",
                ));
            }
        };
        let provider = context
            .environment
            .var("LEVELER_SEARCH_PROVIDER")
            .unwrap_or_else(|| "bing".to_string());
        let search_cx = context.environment.var("LEVELER_SEARCH_CX");

        // Race the request against cancellation so a killed turn returns promptly.
        let result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return Ok(ToolOutput::error("web_search 已取消。"));
            }
            r = run_search(&provider, &key, search_cx.as_deref(), &query, count) => r,
        };

        match result {
            Ok(text) if !text.trim().is_empty() => Ok(ToolOutput::ok(text)),
            Ok(_) => Ok(ToolOutput::ok(format!("web_search「{query}」:无结果。"))),
            // A failed search is a normal, recoverable outcome — hand the reason
            // back so the model continues without it, rather than aborting.
            Err(reason) => Ok(ToolOutput::error(format!(
                "web_search 不可用:{reason}。请基于已有知识继续,或改用其他工具。"
            ))),
        }
    }
}

/// Run the provider request and format the top results, or a human reason on
/// failure (network/timeout/HTTP/parse).
async fn run_search(
    provider: &str,
    key: &str,
    google_cx: Option<&str>,
    query: &str,
    count: usize,
) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;
    let num = count.to_string();

    let (json, google) = if provider.eq_ignore_ascii_case("google") {
        let cx = google_cx.ok_or_else(|| "google 需要 LEVELER_SEARCH_CX".to_string())?;
        let resp = client
            .get("https://www.googleapis.com/customsearch/v1")
            .query(&[
                ("key", key),
                ("cx", cx),
                ("q", query),
                ("num", num.as_str()),
            ])
            .send()
            .await
            .map_err(net_reason)?;
        (parse_ok(resp).await?, true)
    } else {
        let resp = client
            .get("https://api.bing.microsoft.com/v7.0/search")
            .header("Ocp-Apim-Subscription-Key", key)
            .query(&[("q", query), ("count", num.as_str()), ("mkt", "zh-CN")])
            .send()
            .await
            .map_err(net_reason)?;
        (parse_ok(resp).await?, false)
    };

    Ok(format_results(&json, google, count))
}

/// Turn a response into JSON, mapping a non-2xx status to a readable reason.
async fn parse_ok(resp: reqwest::Response) -> Result<serde_json::Value, String> {
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("HTTP {}", status.as_u16()));
    }
    resp.json::<serde_json::Value>()
        .await
        .map_err(|e| format!("解析响应失败:{e}"))
}

/// Explain a transport failure in plain terms (offline vs timeout vs other).
fn net_reason(e: reqwest::Error) -> String {
    if e.is_timeout() {
        "请求超时(网络可能不通)".to_string()
    } else if e.is_connect() {
        "无法连接(网络可能不通)".to_string()
    } else {
        e.to_string()
    }
}

/// Format the top `count` results as `N. title\n   url\n   snippet`.
fn format_results(json: &serde_json::Value, google: bool, count: usize) -> String {
    let items = if google {
        json.get("items").and_then(|v| v.as_array())
    } else {
        json.get("webPages")
            .and_then(|w| w.get("value"))
            .and_then(|v| v.as_array())
    };
    let Some(items) = items else {
        return String::new();
    };
    let (title_k, url_k) = if google {
        ("title", "link")
    } else {
        ("name", "url")
    };
    let mut out = String::new();
    for (i, item) in items.iter().take(count).enumerate() {
        let title = item.get(title_k).and_then(|v| v.as_str()).unwrap_or("");
        let url = item.get(url_k).and_then(|v| v.as_str()).unwrap_or("");
        let snippet = item.get("snippet").and_then(|v| v.as_str()).unwrap_or("");
        out.push_str(&format!("{}. {title}\n   {url}\n   {snippet}\n\n", i + 1));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_bing_results() {
        let json = serde_json::json!({
            "webPages": { "value": [
                { "name": "Rust", "url": "https://rust-lang.org", "snippet": "A language" },
                { "name": "Docs", "url": "https://docs.rs", "snippet": "crate docs" }
            ]}
        });
        let out = format_results(&json, false, 5);
        assert!(out.contains("1. Rust") && out.contains("https://rust-lang.org"));
        assert!(out.contains("2. Docs") && out.contains("crate docs"));
    }

    #[test]
    fn formats_google_results_and_respects_count() {
        let json = serde_json::json!({
            "items": [
                { "title": "A", "link": "https://a", "snippet": "sa" },
                { "title": "B", "link": "https://b", "snippet": "sb" },
                { "title": "C", "link": "https://c", "snippet": "sc" }
            ]
        });
        let out = format_results(&json, true, 2);
        assert!(out.contains("1. A") && out.contains("2. B"));
        assert!(!out.contains("3. C"), "count caps the results");
    }

    #[test]
    fn missing_results_yields_empty() {
        assert_eq!(format_results(&serde_json::json!({}), false, 5), "");
    }
}
