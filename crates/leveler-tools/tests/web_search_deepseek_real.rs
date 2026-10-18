//! Real DeepSeek Search API acceptance.
//!
//! Ignored by default: it spends a real request against the public endpoint and
//! requires an official `DEEPSEEK_API_KEY`. Run explicitly:
//!
//! ```text
//! DEEPSEEK_API_KEY=... cargo test -p leveler-tools --test web_search_deepseek_real -- --ignored --nocapture
//! ```
//!
//! A key issued for a model gateway is not an official DeepSeek API key; the
//! public endpoint answers HTTP 401 for it. That distinction is recorded, not
//! papered over by falling back to another endpoint.

use std::sync::Arc;
use std::time::{Duration, Instant};

use leveler_execution::PermissionProfile;
use leveler_tools::search::DeepSeekSearchProvider;
use leveler_tools::{Capabilities, CapabilityPacks, ToolContext, model_surface};
use serde_json::json;
use tokio_util::sync::CancellationToken;

#[tokio::test]
#[ignore = "real API: needs an official DEEPSEEK_API_KEY"]
async fn real_deepseek_search_returns_citeable_sources() {
    let key = std::env::var("DEEPSEEK_API_KEY")
        .expect("DEEPSEEK_API_KEY must carry an official DeepSeek API key");
    let provider = DeepSeekSearchProvider::new(key, None, 5, Duration::from_secs(30)).unwrap();
    let capabilities = Capabilities::in_process(Arc::new(leveler_core::environment().clone()))
        .with_search_provider(Some(Arc::new(provider)));
    let registry = model_surface(
        CapabilityPacks {
            web_search: true,
            ..CapabilityPacks::NONE
        },
        &capabilities,
    );
    let context = ToolContext::without_workspace_with_environment(
        PermissionProfile::FullAccess,
        Arc::new(leveler_core::environment().clone()),
    );

    let output = registry
        .execute(
            "web_search",
            json!({"query": "DeepSeek V4 release", "count": 3}),
            context,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    println!(
        "content:\n{}\nmetadata:\n{}",
        output.content, output.metadata
    );
    assert!(!output.is_error, "real search failed: {}", output.content);
    let sources = output.metadata["sources"]
        .as_array()
        .expect("normalized sources metadata");
    assert!(!sources.is_empty(), "a real search returned no sources");
    assert!(sources.len() <= 3);
    for source in sources {
        let url = source["url"].as_str().expect("source URL");
        assert!(
            url.starts_with("http://") || url.starts_with("https://"),
            "{url}"
        );
    }
}

fn registry_with(provider: DeepSeekSearchProvider) -> leveler_tools::ToolRegistry {
    let capabilities = Capabilities::in_process(Arc::new(leveler_core::environment().clone()))
        .with_search_provider(Some(Arc::new(provider)));
    model_surface(
        CapabilityPacks {
            web_search: true,
            ..CapabilityPacks::NONE
        },
        &capabilities,
    )
}

fn full_access() -> ToolContext {
    ToolContext::without_workspace_with_environment(
        PermissionProfile::FullAccess,
        Arc::new(leveler_core::environment().clone()),
    )
}

#[tokio::test]
#[ignore = "real API: needs an official DEEPSEEK_API_KEY"]
async fn real_deepseek_search_timeout_is_reported() {
    let key = std::env::var("DEEPSEEK_API_KEY")
        .expect("DEEPSEEK_API_KEY must carry an official DeepSeek API key");
    // A 1 ms budget guarantees the live request cannot complete; the service
    // does not bill a timed-out request.
    let provider = DeepSeekSearchProvider::new(key, None, 5, Duration::from_millis(1)).unwrap();
    let registry = registry_with(provider);

    let output = registry
        .execute(
            "web_search",
            json!({"query": "DeepSeek V4", "count": 1}),
            full_access(),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert!(output.is_error);
    assert!(output.content.contains("请求超时"), "{}", output.content);
}

#[tokio::test]
#[ignore = "real API: needs an official DEEPSEEK_API_KEY"]
async fn real_deepseek_search_cancellation_is_prompt() {
    let key = std::env::var("DEEPSEEK_API_KEY")
        .expect("DEEPSEEK_API_KEY must carry an official DeepSeek API key");
    let provider = DeepSeekSearchProvider::new(key, None, 5, Duration::from_secs(30)).unwrap();
    let registry = registry_with(provider);
    let cancellation = CancellationToken::new();
    let cancel = cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel.cancel();
    });

    let started = Instant::now();
    let output = registry
        .execute(
            "web_search",
            json!({"query": "DeepSeek V4", "count": 1}),
            full_access(),
            cancellation,
        )
        .await
        .unwrap();

    assert!(output.is_error);
    assert!(output.content.contains("已取消"), "{}", output.content);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "cancellation must not wait for the 30 s timeout"
    );
}
