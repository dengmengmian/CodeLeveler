//! End-to-end proof of the model-facing search chain:
//! Tool Registry → `web_search` → SearchProvider → Host HTTP gate → provider
//! response → normalized `ToolOutput`.
//!
//! These use a real local HTTP server and the production registry. They do not
//! prove a live vendor is reachable; the real-API test is separate.

use std::sync::Arc;
use std::time::{Duration, Instant};

use leveler_execution::PermissionProfile;
use leveler_test_support::{MockResponse, MockServer};
use leveler_tools::search::{DeepSeekSearchProvider, TavilySearchProvider};
use leveler_tools::{Capabilities, CapabilityPacks, ToolContext, ToolRegistry, model_surface};
use serde_json::json;
use tokio_util::sync::CancellationToken;

const KEY: &str = "sk-test-search-key-do-not-leak";

fn environment() -> Arc<leveler_core::EnvSnapshot> {
    Arc::new(leveler_core::environment().clone())
}

fn capabilities(provider: Arc<dyn leveler_tools::search::SearchProvider>) -> Capabilities {
    Capabilities::in_process(environment()).with_search_provider(Some(provider))
}

fn registry(capabilities: &Capabilities) -> ToolRegistry {
    model_surface(
        CapabilityPacks {
            web_search: true,
            ..CapabilityPacks::NONE
        },
        capabilities,
    )
}

fn context(mode: PermissionProfile) -> ToolContext {
    ToolContext::without_workspace_with_environment(mode, environment())
}

fn deepseek_fixture() -> serde_json::Value {
    json!({
        "results": [
            {
                "rerank_score": 2.5,
                "title": "DeepSeek V4",
                "url": "https://example.test/deepseek-v4",
                "snippet": "Release notes for DeepSeek V4.",
                "summary": "A longer summary that must not reach the model.",
                "content": "",
                "site_name": "Example",
                "site_icon": "https://example.test/favicon.ico",
                "date": "2026-10-01T00:00:00+00:00",
                "authority": 4
            },
            {
                "title": "Second",
                "url": "https://example.test/second",
                "snippet": "",
                "date": null,
                "authority": -1
            }
        ]
    })
}

#[tokio::test]
async fn deepseek_chain_hits_the_configurable_search_endpoint_and_normalizes_sources() {
    let server =
        MockServer::start_one(MockResponse::json_ok(&deepseek_fixture().to_string())).await;
    let provider = DeepSeekSearchProvider::new(
        KEY,
        Some(&format!("{}/v1/tools", server.base_url())),
        5,
        Duration::from_secs(5),
    )
    .unwrap();
    let caps = capabilities(Arc::new(provider));
    let registry = registry(&caps);

    let output = registry
        .execute(
            "web_search",
            json!({"query": "DeepSeek V4", "count": 2}),
            context(PermissionProfile::FullAccess),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert!(!output.is_error, "{}", output.content);
    assert!(
        output
            .content
            .contains("1. DeepSeek V4 (2026-10-01T00:00:00+00:00)")
    );
    assert!(output.content.contains("https://example.test/deepseek-v4"));
    assert!(output.content.contains("Release notes for DeepSeek V4."));
    assert!(
        !output.content.contains("longer summary"),
        "{}",
        output.content
    );
    assert_eq!(output.metadata["provider"], "deepseek");
    assert_eq!(output.metadata["truncated"], false);
    assert_eq!(
        output.metadata["sources"][0]["published_at"],
        "2026-10-01T00:00:00+00:00"
    );

    let heads = server.request_heads().await;
    assert_eq!(
        heads.len(),
        1,
        "one direct search request, no model request"
    );
    assert!(
        heads[0].starts_with("POST /v1/tools/search HTTP/1.1"),
        "{}",
        heads[0]
    );
    assert!(
        heads[0]
            .to_ascii_lowercase()
            .contains(&format!("authorization: bearer {KEY}").to_ascii_lowercase()),
        "{}",
        heads[0]
    );
    let bodies = server.request_bodies().await;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&bodies[0]).unwrap(),
        json!({"query": "DeepSeek V4", "count": 2, "need_content": false})
    );
}

#[tokio::test]
async fn tavily_chain_keeps_the_existing_model_contract() {
    let server = MockServer::start_one(MockResponse::json_ok(
        &json!({
            "results": [
                {"title": "Tokio", "url": "https://tokio.rs/", "content": "An async runtime.", "score": 0.9}
            ]
        })
        .to_string(),
    ))
    .await;
    let provider = TavilySearchProvider::new(
        "tvly-test-key",
        Some(&server.base_url()),
        5,
        Duration::from_secs(5),
    )
    .unwrap();
    let caps = capabilities(Arc::new(provider));
    let registry = registry(&caps);

    let output = registry
        .execute(
            "web_search",
            json!({"query": "tokio", "count": 1}),
            context(PermissionProfile::FullAccess),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert!(!output.is_error, "{}", output.content);
    assert!(output.content.contains("https://tokio.rs/"));
    assert_eq!(output.metadata["provider"], "tavily");
    assert!(!output.content.contains("0.9"), "{}", output.content);

    let heads = server.request_heads().await;
    assert!(
        heads[0].starts_with("POST /search HTTP/1.1"),
        "{}",
        heads[0]
    );
    let bodies = server.request_bodies().await;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&bodies[0]).unwrap(),
        json!({"query": "tokio", "search_depth": "basic", "max_results": 1})
    );
}

#[tokio::test]
async fn a_denied_network_scope_returns_before_any_request() {
    let server = MockServer::start_one(MockResponse::json_ok("{}")).await;
    let provider = DeepSeekSearchProvider::new(
        KEY,
        Some(&format!("{}/v1/tools", server.base_url())),
        5,
        Duration::from_secs(5),
    )
    .unwrap();
    let caps = capabilities(Arc::new(provider));
    let registry = registry(&caps);

    // Assisted with the run-level network denial: scope is None.
    let output = registry
        .execute(
            "web_search",
            json!({"query": "blocked"}),
            context(PermissionProfile::Assisted).with_sandbox(true),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert!(output.is_error);
    assert!(output.content.contains("已禁用网络"), "{}", output.content);
    assert_eq!(server.request_count(), 0, "no socket may be opened");
}

#[tokio::test]
async fn restricted_scope_refuses_a_public_endpoint_before_connecting() {
    let provider = DeepSeekSearchProvider::new(
        KEY,
        Some("http://192.0.2.1/v1/tools"),
        5,
        Duration::from_secs(2),
    )
    .unwrap();
    let caps = capabilities(Arc::new(provider));
    let registry = registry(&caps);

    // RequestApproval defaults to Loopback; TEST-NET-1 is not loopback.
    let output = registry
        .execute(
            "web_search",
            json!({"query": "public"}),
            context(PermissionProfile::RequestApproval),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert!(output.is_error);
    assert!(
        output.content.contains("当前权限未授予该网络目标"),
        "{}",
        output.content
    );
}

#[tokio::test]
async fn provider_status_errors_keep_the_status_and_never_print_the_key() {
    for (status, needle) in [
        (401, "鉴权失败"),
        (403, "访问被拒绝"),
        (429, "请求过于频繁"),
        (500, "搜索服务异常"),
    ] {
        let server = MockServer::start_one(MockResponse::Status {
            code: status,
            body: json!({"error": {"message": format!("upstream detail {KEY}")}}).to_string(),
        })
        .await;
        let provider = DeepSeekSearchProvider::new(
            KEY,
            Some(&format!("{}/v1/tools", server.base_url())),
            5,
            Duration::from_secs(2),
        )
        .unwrap();
        let caps = capabilities(Arc::new(provider));
        let registry = registry(&caps);

        let output = registry
            .execute(
                "web_search",
                json!({"query": "failure"}),
                context(PermissionProfile::FullAccess),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert!(
            output.is_error,
            "HTTP {status} must be a model-visible failure"
        );
        assert!(output.content.contains(needle), "{}", output.content);
        assert!(
            output.content.contains("upstream detail"),
            "{}",
            output.content
        );
        assert!(
            !output.content.contains(KEY),
            "the key leaked: {}",
            output.content
        );
        assert!(output.content.contains("[redacted]"), "{}", output.content);
        assert!(output.metadata.is_null());
    }
}

#[tokio::test]
async fn a_non_json_success_body_is_an_invalid_response() {
    let server = MockServer::start_one(MockResponse::Status {
        code: 200,
        body: "not json".into(),
    })
    .await;
    let provider = DeepSeekSearchProvider::new(
        KEY,
        Some(&format!("{}/v1/tools", server.base_url())),
        5,
        Duration::from_secs(2),
    )
    .unwrap();
    let caps = capabilities(Arc::new(provider));
    let registry = registry(&caps);

    let output = registry
        .execute(
            "web_search",
            json!({"query": "bad body"}),
            context(PermissionProfile::FullAccess),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert!(output.is_error);
    assert!(
        output.content.contains("搜索响应格式无效"),
        "{}",
        output.content
    );
}

#[tokio::test]
async fn the_configured_timeout_is_enforced() {
    let server = MockServer::start_one(MockResponse::NeverResponds).await;
    let provider = DeepSeekSearchProvider::new(
        KEY,
        Some(&format!("{}/v1/tools", server.base_url())),
        5,
        Duration::from_millis(200),
    )
    .unwrap();
    let caps = capabilities(Arc::new(provider));
    let registry = registry(&caps);

    let started = Instant::now();
    let output = registry
        .execute(
            "web_search",
            json!({"query": "slow"}),
            context(PermissionProfile::FullAccess),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert!(output.is_error);
    assert!(output.content.contains("请求超时"), "{}", output.content);
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[tokio::test]
async fn caller_cancellation_returns_promptly() {
    let server = MockServer::start_one(MockResponse::NeverResponds).await;
    let provider = DeepSeekSearchProvider::new(
        KEY,
        Some(&format!("{}/v1/tools", server.base_url())),
        5,
        Duration::from_secs(30),
    )
    .unwrap();
    let caps = capabilities(Arc::new(provider));
    let registry = registry(&caps);

    let cancellation = CancellationToken::new();
    let cancel = cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();
    });

    let started = Instant::now();
    let output = registry
        .execute(
            "web_search",
            json!({"query": "cancel me"}),
            context(PermissionProfile::FullAccess),
            cancellation,
        )
        .await
        .unwrap();

    assert!(output.is_error);
    assert!(output.content.contains("已取消"), "{}", output.content);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "cancellation must not wait for the 30s timeout"
    );
}

#[tokio::test]
async fn a_redirect_never_carries_the_search_credential_to_another_host() {
    let target =
        MockServer::start_one(MockResponse::json_ok(&deepseek_fixture().to_string())).await;
    let redirector = MockServer::start_one(MockResponse::StatusWithHeaders {
        code: 307,
        body: "{}".into(),
        headers: vec![(
            "Location".into(),
            format!("{}/v1/tools/search", target.base_url()),
        )],
    })
    .await;
    let provider = DeepSeekSearchProvider::new(
        KEY,
        Some(&format!("{}/v1/tools", redirector.base_url())),
        5,
        Duration::from_secs(2),
    )
    .unwrap();
    let caps = capabilities(Arc::new(provider));
    let registry = registry(&caps);

    let output = registry
        .execute(
            "web_search",
            json!({"query": "redirect"}),
            context(PermissionProfile::FullAccess),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert!(output.is_error, "{}", output.content);
    assert!(output.content.contains("307"), "{}", output.content);
    assert_eq!(redirector.request_count(), 1);
    assert_eq!(
        target.request_count(),
        0,
        "the credential must never reach the redirect target"
    );
}

#[tokio::test]
async fn a_configured_result_cap_is_reported_as_truncation() {
    let server =
        MockServer::start_one(MockResponse::json_ok(&deepseek_fixture().to_string())).await;
    // The model asks for 5; the deployment cap is 1.
    let provider = DeepSeekSearchProvider::new(
        KEY,
        Some(&format!("{}/v1/tools", server.base_url())),
        1,
        Duration::from_secs(2),
    )
    .unwrap();
    let caps = capabilities(Arc::new(provider));
    let registry = registry(&caps);

    let output = registry
        .execute(
            "web_search",
            json!({"query": "capped", "count": 5}),
            context(PermissionProfile::FullAccess),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert!(!output.is_error, "{}", output.content);
    assert_eq!(output.metadata["truncated"], true);
    assert!(output.content.contains("结果已截断"), "{}", output.content);
    let bodies = server.request_bodies().await;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&bodies[0]).unwrap()["count"],
        1,
        "the provider cap is the request bound"
    );
}

#[tokio::test]
async fn an_oversized_success_body_is_rejected() {
    let oversized = format!(
        "{{\"results\":[],\"padding\":\"{}\"}}",
        "x".repeat(1024 * 1024)
    );
    let server = MockServer::start_one(MockResponse::json_ok(&oversized)).await;
    let provider = DeepSeekSearchProvider::new(
        KEY,
        Some(&format!("{}/v1/tools", server.base_url())),
        5,
        Duration::from_secs(5),
    )
    .unwrap();
    let caps = capabilities(Arc::new(provider));
    let registry = registry(&caps);

    let output = registry
        .execute(
            "web_search",
            json!({"query": "huge"}),
            context(PermissionProfile::FullAccess),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert!(output.is_error);
    assert!(
        output.content.contains("搜索响应格式无效"),
        "{}",
        output.content
    );
}

#[tokio::test]
async fn an_empty_search_is_a_successful_empty_result() {
    let server = MockServer::start_one(MockResponse::json_ok(r#"{"results":[]}"#)).await;
    let provider = DeepSeekSearchProvider::new(
        KEY,
        Some(&format!("{}/v1/tools", server.base_url())),
        5,
        Duration::from_secs(2),
    )
    .unwrap();
    let caps = capabilities(Arc::new(provider));
    let registry = registry(&caps);

    let output = registry
        .execute(
            "web_search",
            json!({"query": "nothing"}),
            context(PermissionProfile::FullAccess),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert!(!output.is_error);
    assert!(output.content.contains("无结果"), "{}", output.content);
    assert!(
        output.metadata.is_null(),
        "an empty result carries no sources"
    );
}
