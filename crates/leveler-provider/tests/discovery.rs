//! Discovery fixtures use the real HTTP boundary without provider credentials.
use leveler_model::ProtocolKind;
use leveler_provider::{
    ProviderConfig, Timeouts,
    discovery::{DiscoveryError, list_remote_models},
};
use leveler_test_support::{MockResponse, MockServer};

fn config(server: &MockServer) -> ProviderConfig {
    ProviderConfig {
        id: "discovery-fixture".into(),
        protocol: ProtocolKind::OpenAiChat,
        base_url: server.base_url(),
        api_key_env: String::new(),
        api_key: None,
        headers: Default::default(),
        timeouts: Timeouts::default(),
    }
}

#[tokio::test]
async fn explicitly_non_array_data_is_malformed() {
    for data in ["null", "{}", "\"models\"", "1", "true"] {
        let body = format!("{{\"data\":{data}}}");
        let server = MockServer::start_one(MockResponse::json_ok(&body)).await;
        let cfg = config(&server);
        let result = list_remote_models(&cfg).await;
        assert!(
            matches!(result, Err(DiscoveryError::Malformed { .. })),
            "data={data}: {result:?}"
        );
        assert_eq!(server.request_count(), 1);
    }
}

#[tokio::test]
async fn missing_and_empty_data_keep_their_existing_empty_result() {
    for body in ["{}", "{\"data\":[]}"] {
        let server = MockServer::start_one(MockResponse::json_ok(body)).await;
        assert!(
            list_remote_models(&config(&server))
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(server.request_count(), 1);
    }
}

#[tokio::test]
async fn usable_ids_are_still_trimmed_sorted_and_deduplicated() {
    let server = MockServer::start_one(MockResponse::json_ok(
        r#"{"data":[{"id":" z "},{"id":"a"},{"id":"z"},{"id":""},{"id":null},{"id":3},{"name":"missing"},null]}"#,
    )).await;
    assert_eq!(
        list_remote_models(&config(&server)).await.unwrap(),
        vec!["a", "z"]
    );
    let heads = server.request_heads().await;
    assert!(heads[0].starts_with("GET /models HTTP/1.1"));
}

#[tokio::test]
async fn unauthorized_is_its_own_classification() {
    for code in [401, 403] {
        let server = MockServer::start_one(MockResponse::Status {
            code,
            body: r#"{"error":"unauthorized"}"#.into(),
        })
        .await;
        let result = list_remote_models(&config(&server)).await;
        assert!(
            matches!(result, Err(DiscoveryError::Auth { status, .. }) if status.as_u16() == code),
            "code {code}: {result:?}"
        );
        assert_eq!(server.request_count(), 1);
    }
}

#[tokio::test]
async fn a_missing_models_route_stays_unsupported() {
    let server = MockServer::start_one(MockResponse::Status {
        code: 404,
        body: r#"{"error":"not found"}"#.into(),
    })
    .await;
    let result = list_remote_models(&config(&server)).await;
    assert!(
        matches!(result, Err(DiscoveryError::Unsupported { status, .. }) if status.as_u16() == 404)
    );
}

#[tokio::test]
async fn rate_limits_and_server_failures_are_named() {
    for (code, rate_limited) in [(429, true), (500, false), (503, false)] {
        let server = MockServer::start_one(MockResponse::Status {
            code,
            body: "{}".into(),
        })
        .await;
        let result = list_remote_models(&config(&server)).await;
        if rate_limited {
            assert!(
                matches!(result, Err(DiscoveryError::RateLimited { .. })),
                "{result:?}"
            );
        } else {
            assert!(
                matches!(result, Err(DiscoveryError::Server { .. })),
                "{result:?}"
            );
        }
    }
}

#[tokio::test]
async fn the_messages_protocol_discovers_with_its_own_auth_header() {
    let server = MockServer::start_one(MockResponse::json_ok(r#"{"data":[]}"#)).await;
    let mut cfg = config(&server);
    cfg.protocol = ProtocolKind::AnthropicMessages;
    cfg.api_key = Some("fixture-anthropic".into());
    list_remote_models(&cfg).await.unwrap();
    let head = server.request_heads().await[0].to_lowercase();
    assert!(head.contains("x-api-key: fixture-anthropic"), "{head}");
    assert!(head.contains("anthropic-version: 2023-06-01"), "{head}");
    assert!(!head.contains("authorization:"), "{head}");
}

#[tokio::test]
async fn explicit_anthropic_header_still_replaces_bearer_auth() {
    let server = MockServer::start_one(MockResponse::json_ok(r#"{"data":[]}"#)).await;
    let mut cfg = config(&server);
    cfg.protocol = ProtocolKind::AnthropicMessages;
    cfg.api_key = Some("fixture-bearer".into());
    cfg.headers
        .insert("x-api-key".into(), "fixture-anthropic".into());
    cfg.headers
        .insert("anthropic-version".into(), "2023-06-01".into());
    list_remote_models(&cfg).await.unwrap();
    let heads = server.request_heads().await;
    let head = heads[0].to_lowercase();
    assert!(head.contains("x-api-key: fixture-anthropic"));
    assert!(head.contains("anthropic-version: 2023-06-01"));
    assert!(!head.contains("authorization:"));
}
