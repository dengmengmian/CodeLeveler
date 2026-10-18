//! `web_search` — search the web for up-to-date information the repository and
//! the model's training data may not cover.
//!
//! The tool owns the model-facing contract (`query` + `count`) and nothing
//! about a backend. A [`SearchProvider`] owns the request shape, credentials
//! and response mapping; the composition root selects it, so the conversation
//! model and the search backend are independent facts.
//!
//! An OPTIONAL capability: the composition root answers "is a provider
//! configured" once, and an unconfigured host never registers this tool at all.
//! What reaches `execute` therefore already carries a usable provider.
//!
//! Runtime failures are handled as mechanical facts: a timeout, an unreachable
//! host, an HTTP error or an unparsable body all return an `is_error` result.
//! The tool does not retry, does not fall back to another provider, does not
//! start a model request, and does not advise.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::RiskLevel;

use crate::search::{
    SearchNetwork, SearchProvider, SearchResult, SearchSource, TavilySearchProvider,
};
use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

const LEGACY_TIMEOUT: Duration = Duration::from_secs(10);
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

/// The web search tool, constructed with the provider the composition root
/// selected. The provider holds its own credential; the tool never sees it and
/// therefore cannot log or echo it.
pub struct WebSearchTool {
    provider: Arc<dyn SearchProvider>,
}

impl WebSearchTool {
    /// Legacy constructor: a Tavily key with the built-in endpoint and timeout.
    pub fn new(api_key: impl Into<String>) -> Self {
        let provider = TavilySearchProvider::new(api_key, None, MAX_COUNT, LEGACY_TIMEOUT)
            .expect("a non-empty key is the only precondition");
        Self::with_provider(Arc::new(provider))
    }

    /// Build the tool over an already-selected provider.
    pub fn with_provider(provider: Arc<dyn SearchProvider>) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn requires_workspace(&self) -> bool {
        false
    }
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

    async fn network_resource(
        &self,
        _input: &serde_json::Value,
    ) -> Result<Option<leveler_execution::NetworkResource>, String> {
        crate::network::resolve_target(self.provider.endpoint())
            .await
            .map(Some)
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: Input = super::parse_input(self.name(), input)?;

        // Socket mediation also protects direct tool calls outside admission.
        let network_scope = context.policy.network_scope();
        let unrestricted = context.policy.unrestricted_execution();
        if !unrestricted && matches!(network_scope, leveler_execution::NetworkScope::None) {
            return Ok(ToolOutput::error(
                "web_search 不可用:当前模式/沙箱已禁用网络。",
            ));
        }
        let query = input.query.trim().to_string();
        if query.is_empty() {
            return Ok(ToolOutput::error("web_search 需要非空 query。"));
        }
        let count = input.count.unwrap_or(DEFAULT_COUNT).clamp(1, MAX_COUNT);

        let network = SearchNetwork {
            scope: &network_scope,
            unrestricted,
        };
        // Race the request against cancellation so a killed turn returns promptly.
        let result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return Ok(ToolOutput::error("web_search 已取消。"));
            }
            r = self.provider.search(&query, count, network, &cancellation) => r,
        };

        match result {
            Ok(result) if result.sources.is_empty() => {
                Ok(ToolOutput::ok(format!("web_search「{query}」:无结果。")))
            }
            Ok(result) => {
                let content = format_results(&result);
                let metadata = serde_json::json!({
                    "provider": self.provider.id(),
                    "sources": result.sources,
                    "truncated": result.truncated,
                });
                Ok(ToolOutput::ok(content).with_metadata(metadata))
            }
            // A mechanical fact about this request, and nothing more: what the
            // model does about it is the model's call.
            Err(reason) => Ok(ToolOutput::error(format!("web_search 失败:{reason}。"))),
        }
    }
}

/// Render the normalized result as `N. title (date)\n   url\n   snippet`.
///
/// Provider-neutral by construction: the model sees titles, URLs and snippets,
/// never a provider's response envelope or quality scores.
fn format_results(result: &SearchResult) -> String {
    let mut out = String::new();
    for (i, source) in result.sources.iter().enumerate() {
        let title = display_title(source);
        match &source.published_at {
            Some(date) => out.push_str(&format!("{}. {title} ({date})\n", i + 1)),
            None => out.push_str(&format!("{}. {title}\n", i + 1)),
        }
        out.push_str(&format!("   {}\n", source.url));
        if !source.snippet.is_empty() {
            out.push_str(&format!("   {}\n", source.snippet));
        }
        out.push('\n');
    }
    if result.truncated {
        out.push_str(&format!(
            "(结果已截断至 {} 条,可细化查询获取更多。)\n",
            result.sources.len()
        ));
    }
    out
}

fn display_title(source: &SearchSource) -> &str {
    if source.title.is_empty() {
        &source.url
    } else {
        &source.title
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(title: &str, url: &str, snippet: &str, date: Option<&str>) -> SearchSource {
        SearchSource {
            title: title.into(),
            url: url.into(),
            snippet: snippet.into(),
            published_at: date.map(str::to_string),
        }
    }

    fn fixture() -> SearchResult {
        SearchResult {
            sources: vec![
                source(
                    "Tokio CancellationToken",
                    "https://docs.rs/tokio-util/latest/",
                    "Cancellation tokens provide...",
                    None,
                ),
                source(
                    "Tokio Documentation",
                    "https://tokio.rs/",
                    "An asynchronous runtime.",
                    Some("2026-02-02"),
                ),
                source("Third", "https://example.invalid/", "third snippet", None),
            ],
            truncated: false,
        }
    }

    #[test]
    fn formats_results_from_title_url_snippet_and_date() {
        let out = format_results(&fixture());
        assert!(out.contains("1. Tokio CancellationToken"));
        assert!(out.contains("https://docs.rs/tokio-util/latest/"));
        assert!(out.contains("Cancellation tokens provide..."));
        assert!(out.contains("2. Tokio Documentation (2026-02-02)"));
    }

    #[test]
    fn provider_internal_fields_never_reach_the_model() {
        let out = format_results(&fixture());
        for leaked in [
            "score",
            "0.98",
            "response_time",
            "rerank_score",
            "authority",
        ] {
            assert!(!out.contains(leaked), "{leaked} leaked into the output");
        }
    }

    #[test]
    fn an_empty_title_falls_back_to_the_url() {
        let result = SearchResult {
            sources: vec![source("", "https://a.test", "", None)],
            truncated: false,
        };
        let out = format_results(&result);
        assert!(out.contains("1. https://a.test"), "{out}");
    }

    #[test]
    fn truncation_is_stated_once() {
        let mut result = fixture();
        result.truncated = true;
        let out = format_results(&result);
        assert_eq!(out.matches("结果已截断").count(), 1, "{out}");
        assert!(out.contains("3 条"), "{out}");
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
