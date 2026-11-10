//! Wire-level proof that project rules reach the provider request.
//!
//! The mock runtime tests prove what the executor *intended* to send. This
//! test drives the real `ProviderRegistry` and OpenAI Chat adapter against a
//! loopback HTTP server and asserts the bytes that actually left the process:
//! the rule sentinel appears in the system message, and the same rule text is
//! on the wire again in the next round of the same turn.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use leveler_agent::{Executor, NoopSink, StopReason};
use leveler_execution::{PermissionProfile, Workspace};
use leveler_model::{ModelCapabilities, ModelLimits, ModelProfile, ModelRef, ProtocolKind};
use leveler_provider::{
    ModelConfigFile, ProviderConfig, ProviderRegistry, RegistryInputs, Timeouts,
};
use leveler_test_support::{MockResponse, MockServer};
use leveler_tools::ToolContext;

/// Unique to this test: no shipped prompt or other fixture contains it, so a
/// match can only come from the workspace rule document.
const RULE_SENTINEL: &str =
    "WIRE-RULE-SENTINEL-4f8c2d: every answer must cite the project covenant.";

fn provider_config(base_url: String) -> ProviderConfig {
    ProviderConfig {
        id: "mock".into(),
        protocol: ProtocolKind::OpenAiChat,
        base_url,
        api_key_env: String::new(),
        api_key: None,
        headers: Default::default(),
        timeouts: Timeouts {
            connect_seconds: 5,
            request_seconds: 30,
            idle_stream_seconds: 10,
        },
    }
}

fn model_config() -> ModelConfigFile {
    ModelConfigFile {
        profile: ModelProfile {
            id: "m".into(),
            provider: "mock".into(),
            model_id: "wire-capture-model".into(),
            protocol: ProtocolKind::OpenAiChat,
            capabilities: ModelCapabilities {
                streaming: true,
                tool_calling: true,
                parallel_tool_calls: false,
                structured_output: false,
                reasoning: false,
                vision: false,
            },
            limits: ModelLimits {
                context_window: 131_072,
                reliable_context: 65_536,
                max_output_tokens: 4_096,
                max_tool_schema_bytes: 65_536,
                max_parallel_tool_calls: 1,
                max_tool_output_bytes: None,
            },
            context_quality: None,
            reasoning: Default::default(),
            compatibility: Default::default(),
            pricing: None,
            thinking: None,
        },
        policy: None,
    }
}

fn system_texts(body: &str) -> Vec<String> {
    let request: serde_json::Value = serde_json::from_str(body).expect("request JSON");
    request["messages"]
        .as_array()
        .expect("messages array")
        .iter()
        .filter(|message| message["role"] == "system")
        .filter_map(|message| message["content"].as_str().map(str::to_string))
        .collect()
}

#[tokio::test]
async fn project_rules_reach_the_wire_and_stay_identical_across_rounds() {
    let dir = tempfile::Builder::new()
        .prefix("leveler-wire-rules-")
        .tempdir()
        .unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("AGENTS.md"),
        format!("# Covenant\n{RULE_SENTINEL}\n"),
    )
    .unwrap();
    std::fs::write(dir.path().join("note.txt"), "note body\n").unwrap();

    // Round 1 asks for a file read; round 2 answers. Two real provider
    // requests in one turn, each captured by the loopback server.
    let server = MockServer::start(vec![
        MockResponse::sse(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read_file","arguments":"{\"path\":\"note.txt\"}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        ]),
        MockResponse::sse(&[
            r#"{"choices":[{"delta":{"content":"done"}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":2}}"#,
        ]),
    ])
    .await;

    let provider = Arc::new(
        ProviderRegistry::build(RegistryInputs {
            providers: vec![(provider_config(server.base_url()), None)],
            models: vec![model_config()],
        })
        .expect("build registry"),
    );

    let workspace = Workspace::new(dir.path()).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let executor = Executor::new(
        provider,
        Arc::new(leveler_tools::default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        5,
    );

    let outcome = executor
        .run(
            "read the note",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(outcome.stop_reason, StopReason::Answered);

    let bodies = server.request_bodies().await;
    assert_eq!(bodies.len(), 2, "one request per model round");

    let mut rule_system_texts = Vec::new();
    for body in &bodies {
        assert_eq!(
            body.matches(RULE_SENTINEL).count(),
            1,
            "the rule must appear exactly once on the wire: {body}"
        );
        let systems = system_texts(body);
        let with_rule: Vec<_> = systems
            .iter()
            .filter(|text| text.contains(RULE_SENTINEL))
            .collect();
        assert_eq!(
            with_rule.len(),
            1,
            "exactly one system message carries the rule: {systems:?}"
        );
        rule_system_texts.push(with_rule[0].clone());
    }
    assert_eq!(
        rule_system_texts[0], rule_system_texts[1],
        "the rule block must be byte-identical across rounds of one turn"
    );

    std::fs::remove_dir_all(dir.path()).ok();
}
