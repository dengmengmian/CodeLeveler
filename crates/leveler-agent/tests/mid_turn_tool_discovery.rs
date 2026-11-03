//! A tool catalog that becomes reachable while a turn is running must join
//! THAT turn's surface.
//!
//! One user request can run for half an hour. When the request itself is "start
//! the project", the external MCP server is by definition unreachable for the
//! first minutes of the turn that must then use it; "next turn" is not a
//! recovery path the user asked for. This pins the seam: the executor asks the
//! host once per model request, and a newly discovered tool is both advertised
//! and executable in the same turn.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use leveler_agent::{Executor, NoopSink, StopReason};
use leveler_core::{RequestId, ToolCallId};
use leveler_execution::{PermissionProfile, Workspace};
use leveler_model::{
    ContentPart, FinishReason, Message, ModelError, ModelEventStream, ModelProfile, ModelRef,
    ModelRequest, ModelResponse, ModelRuntime, Role, TokenUsage, ToolCall,
};
use leveler_tools::tool::ToolDiscovery;
use leveler_tools::{RiskLevel, Tool, ToolContext, ToolError, ToolOutput};

fn mock_profile(model: &ModelRef) -> ModelProfile {
    serde_json::from_value(serde_json::json!({
        "id": model.to_string(),
        "provider": model.provider,
        "model_id": model.model,
        "protocol": "openai_chat",
        "capabilities": {
            "streaming": true, "tool_calling": true, "parallel_tool_calls": false,
            "structured_output": false, "reasoning": false, "vision": false
        },
        "limits": {
            "context_window": 131072, "reliable_context": 65536,
            "max_output_tokens": 8192, "max_tool_schema_bytes": 32768,
            "max_parallel_tool_calls": 1
        },
        "reasoning": { "style": "none" }
    }))
    .expect("valid mock model profile")
}

/// A catalog that answers "nothing yet" until it has been asked `ready_after`
/// times, then offers its tools — an external server that came up during the
/// turn.
///
/// The turn asks once while composing its opening surface and once per round,
/// so `ready_after: 2` is "the server came up between the first and the second
/// request".
struct LateCatalog {
    asks: Mutex<u32>,
    ready_after: u32,
    tools: Vec<Arc<dyn Tool>>,
}

#[async_trait]
impl ToolDiscovery for LateCatalog {
    async fn discovered(&self) -> Vec<Arc<dyn Tool>> {
        let mut asks = self.asks.lock().unwrap();
        *asks += 1;
        if *asks <= self.ready_after {
            return Vec::new();
        }
        self.tools.clone()
    }
}

/// A catalog that is already up when the turn starts — the ordinary case of a
/// server started before the session.
struct ReadyCatalog {
    tools: Vec<Arc<dyn Tool>>,
}

#[async_trait]
impl ToolDiscovery for ReadyCatalog {
    async fn discovered(&self) -> Vec<Arc<dyn Tool>> {
        self.tools.clone()
    }
}

struct LateEcho {
    calls: Mutex<u32>,
}

#[async_trait]
impl Tool for LateEcho {
    fn name(&self) -> &str {
        "mcp__fixture__late_echo"
    }
    fn description(&self) -> &str {
        "echo from a catalog that came up mid-turn"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    fn risk(&self) -> RiskLevel {
        RiskLevel::Safe
    }
    async fn execute(
        &self,
        _input: serde_json::Value,
        _context: ToolContext,
        _cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        *self.calls.lock().unwrap() += 1;
        Ok(ToolOutput::ok("late-tool-ok"))
    }
}

struct ScriptedRuntime {
    responses: Mutex<VecDeque<ModelResponse>>,
    requests: Mutex<Vec<ModelRequest>>,
}

#[async_trait]
impl ModelRuntime for ScriptedRuntime {
    async fn generate(
        &self,
        request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        self.requests.lock().unwrap().push(request);
        Err(ModelError::new(
            leveler_model::ModelErrorKind::Other,
            "streaming only",
        ))
    }

    async fn stream(
        &self,
        request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        use leveler_model::ModelEvent;
        self.requests.lock().unwrap().push(request);
        let response = self.responses.lock().unwrap().pop_front().ok_or_else(|| {
            ModelError::new(leveler_model::ModelErrorKind::Other, "no more responses")
        })?;
        let mut events: Vec<Result<ModelEvent, ModelError>> = Vec::new();
        events.push(Ok(ModelEvent::MessageStarted {
            request_id: response.request_id.clone(),
        }));
        for part in &response.message.content {
            match part {
                ContentPart::Text { text } => events.push(Ok(ModelEvent::TextDelta {
                    delta: text.clone(),
                })),
                ContentPart::ToolCall { call } => {
                    events.push(Ok(ModelEvent::ToolCallCompleted { call: call.clone() }))
                }
                _ => {}
            }
        }
        events.push(Ok(ModelEvent::MessageCompleted {
            finish_reason: response.finish_reason,
        }));
        Ok(Box::pin(futures::stream::iter(events)))
    }

    async fn profile(&self, model: &ModelRef) -> Result<ModelProfile, ModelError> {
        Ok(mock_profile(model))
    }
}

fn tool_call(name: &str, args: serde_json::Value) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message {
            origin: None,
            role: Role::Assistant,
            content: vec![ContentPart::ToolCall {
                call: ToolCall {
                    id: ToolCallId::new(format!("call-{name}")),
                    name: name.to_string(),
                    arguments: args,
                },
            }],
            route: None,
        },
        finish_reason: FinishReason::ToolCalls,
        usage: TokenUsage::default(),
    }
}

fn text(text: &str) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message::text(Role::Assistant, text),
        finish_reason: FinishReason::Stop,
        usage: TokenUsage::default(),
    }
}

#[tokio::test]
async fn a_tool_discovered_mid_turn_is_advertised_and_runs_in_the_same_turn() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let tool_context = ToolContext::with_environment(
        workspace,
        PermissionProfile::Assisted,
        Arc::new(leveler_core::EnvSnapshot::new(
            std::env::vars_os(),
            std::env::current_dir().unwrap_or_default(),
            std::env::temp_dir(),
        )),
    );
    let mut registry = leveler_tools::default_registry();
    leveler_agent::register_harness_controls(&mut registry);

    let echo = Arc::new(LateEcho {
        calls: Mutex::new(0),
    });
    let catalog = Arc::new(LateCatalog {
        asks: Mutex::new(0),
        ready_after: 2,
        tools: vec![echo.clone()],
    });

    let runtime = Arc::new(ScriptedRuntime {
        responses: Mutex::new(VecDeque::from(vec![
            // Round 1: the catalog is still empty, so the turn can only use a
            // built-in tool.
            tool_call("read_file", serde_json::json!({"path": "a.txt"})),
            // Round 2: the catalog came up; the model calls the new tool.
            tool_call("mcp__fixture__late_echo", serde_json::json!({})),
            text("done"),
        ])),
        requests: Mutex::new(Vec::new()),
    });

    let executor = Executor::new(
        runtime.clone(),
        Arc::new(registry),
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .with_tool_discovery(catalog)
    .with_continuation_policy(leveler_agent::ContinuationPolicy::bounded(6));

    let outcome = executor
        .run(
            "read a.txt then echo",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(outcome.stop_reason, StopReason::Answered);
    assert_eq!(
        *echo.calls.lock().unwrap(),
        1,
        "the discovered tool must actually run in this turn"
    );

    let requests = runtime.requests.lock().unwrap();
    let first: Vec<&str> = requests[0].tools.iter().map(|t| t.name.as_str()).collect();
    let second: Vec<&str> = requests[1].tools.iter().map(|t| t.name.as_str()).collect();
    assert!(
        !first.contains(&"mcp__fixture__late_echo"),
        "nothing is advertised before the catalog answers"
    );
    assert!(
        second.contains(&"mcp__fixture__late_echo"),
        "the following request of the SAME turn must advertise it: {second:?}"
    );
    // The model cannot observe a mid-turn catalog from the conversation, so the
    // harness states it: this is the fact that lets it decide what to use.
    let noticed = requests[1].control_context.blocks.iter().any(|block| {
        block.name == "tool_surface_grew" && block.text.contains("mcp__fixture__late_echo")
    });
    assert!(
        noticed,
        "the request that gained tools must say so: {:?}",
        requests[1]
            .control_context
            .blocks
            .iter()
            .map(|block| block.name.clone())
            .collect::<Vec<_>>()
    );
}

/// A catalog that comes up under an optional pack must be announced even though
/// it does not change the visible tool table: the model can only enable what it
/// knows exists. Enabling it exposes the already-discovered tool in the SAME
/// turn, and the announcement is then retracted.
#[tokio::test]
async fn an_unexposed_discovered_pack_is_announced_then_usable_after_enable() {
    use leveler_agent::capability::{CapabilityDisclosure, CapabilityId};

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let tool_context = ToolContext::with_environment(
        workspace,
        PermissionProfile::Assisted,
        Arc::new(leveler_core::EnvSnapshot::new(
            std::env::vars_os(),
            std::env::current_dir().unwrap_or_default(),
            std::env::temp_dir(),
        )),
    );
    let mut registry = leveler_tools::default_registry();
    leveler_agent::register_harness_controls(&mut registry);

    let echo = Arc::new(LateEcho {
        calls: Mutex::new(0),
    });
    let catalog = Arc::new(LateCatalog {
        asks: Mutex::new(0),
        ready_after: 1,
        tools: vec![echo.clone()],
    });
    let disclosure = Arc::new(
        CapabilityDisclosure::new(
            vec![CapabilityId::ExternalTools],
            vec![CapabilityId::ExternalTools],
            vec![],
        )
        .unwrap(),
    );

    let runtime = Arc::new(ScriptedRuntime {
        responses: Mutex::new(VecDeque::from(vec![
            tool_call("read_file", serde_json::json!({"path": "a.txt"})),
            tool_call("capability", serde_json::json!({"action": "list"})),
            tool_call(
                "capability",
                serde_json::json!({"action": "enable", "id": "external_tools"}),
            ),
            tool_call("mcp__fixture__late_echo", serde_json::json!({})),
            text("done"),
        ])),
        requests: Mutex::new(Vec::new()),
    });

    let executor = Executor::new(
        runtime.clone(),
        Arc::new(registry),
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .with_capabilities(Some(disclosure))
    .with_tool_discovery(catalog)
    .with_continuation_policy(leveler_agent::ContinuationPolicy::bounded(8));

    let outcome = executor
        .run(
            "read a.txt, then use the catalog that came up",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(outcome.stop_reason, StopReason::Answered);
    assert_eq!(*echo.calls.lock().unwrap(), 1);

    let requests = runtime.requests.lock().unwrap();
    let names =
        |i: usize| -> Vec<String> { requests[i].tools.iter().map(|t| t.name.clone()).collect() };
    let notice = |i: usize| -> Option<String> {
        requests[i]
            .control_context
            .blocks
            .iter()
            .find(|block| block.name == "tool_surface_grew")
            .map(|block| block.text.clone())
    };

    assert!(
        !names(1)
            .iter()
            .any(|name| name == "mcp__fixture__late_echo"),
        "an unexposed pack must not advertise its tool"
    );
    let text = notice(1).expect("the model must be told the catalog came up");
    assert!(text.contains("external_tools"), "{text}");
    // The catalog came up, but the pack is NOT on the surface: a model told
    // "this is now your tool" acts on it, calls it, is refused, and retries
    // instead of enabling the pack. The notice says what is actually true.
    assert!(
        !text.contains("now part of your surface"),
        "an unexposed pack's tools are not on the surface: {text}"
    );
    assert!(
        text.contains("refused") || text.contains("not on your surface"),
        "the notice must say a call would be refused until the pack is enabled: {text}"
    );
    assert!(
        names(3)
            .iter()
            .any(|name| name == "mcp__fixture__late_echo"),
        "enabling the pack exposes the tool discovered earlier in this turn"
    );
    assert!(
        notice(3).is_none(),
        "the announcement is retracted once the pack is exposed"
    );
    drop(requests);
}

/// A pack whose tools are already in the host's registry, and which is
/// available, permitted, and NOT exposed, is invisible in the tool table. The
/// generic "capabilities are available on demand" line does not say which pack
/// exists or how much is behind it, so a model asked to test a connected
/// external surface has no host fact telling it that surface is there and
/// callable at all — it goes around the harness instead.
///
/// This pins the discovery fact: the request states which packs are on offer
/// and how many tools each would add. Naming the pack is not a plan; usage
/// stays the model's decision.
#[tokio::test]
async fn an_available_unexposed_pack_states_what_it_would_add() {
    use leveler_agent::capability::{CapabilityDisclosure, CapabilityId};

    struct Fixture(&'static str);

    #[async_trait]
    impl Tool for Fixture {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "external fixture tool"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        fn risk(&self) -> RiskLevel {
            RiskLevel::Safe
        }
        async fn execute(
            &self,
            _input: serde_json::Value,
            _context: ToolContext,
            _cancellation: CancellationToken,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::ok("fixture-ok"))
        }
    }

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let tool_context = ToolContext::with_environment(
        workspace,
        PermissionProfile::Assisted,
        Arc::new(leveler_core::EnvSnapshot::new(
            std::env::vars_os(),
            std::env::current_dir().unwrap_or_default(),
            std::env::temp_dir(),
        )),
    );
    let mut registry = leveler_tools::default_registry();
    leveler_agent::register_harness_controls(&mut registry);
    // The real path: an external server that is already up when the turn
    // starts. Its tools arrive through the host catalog, not the base registry,
    // and are unexposed until the model enables the pack.
    let catalog = Arc::new(ReadyCatalog {
        tools: vec![
            Arc::new(Fixture("mcp__fixture__alpha")),
            Arc::new(Fixture("mcp__fixture__beta")),
        ],
    });
    let disclosure = Arc::new(
        CapabilityDisclosure::new(
            vec![CapabilityId::ExternalTools],
            vec![CapabilityId::ExternalTools],
            vec![],
        )
        .unwrap(),
    );
    let runtime = Arc::new(ScriptedRuntime {
        responses: Mutex::new(VecDeque::from(vec![text("done")])),
        requests: Mutex::new(Vec::new()),
    });
    let executor = Executor::new(
        runtime.clone(),
        Arc::new(registry),
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .with_capabilities(Some(disclosure))
    .with_tool_discovery(catalog);

    let outcome = executor
        .run(
            "check the connected tools",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(outcome.stop_reason, StopReason::Answered);

    let requests = runtime.requests.lock().unwrap();
    let first = &requests[0];
    assert!(
        !first.tools.iter().any(|t| t.name.starts_with("mcp__")),
        "an unexposed pack must not advertise its tools"
    );
    let discovery = first
        .control_context
        .blocks
        .iter()
        .find(|block| block.name == "capability_discovery")
        .map(|block| block.text.clone())
        .unwrap_or_default();
    assert!(
        discovery.contains("external_tools"),
        "the request must name the pack that is on offer, not just say that some exist: {discovery}"
    );
    assert!(
        discovery.contains('2'),
        "the fact must say how many tools the pack would add: {discovery}"
    );
}
