//! Planning-pipeline tests. Execution scenarios moved to
//! `leveler-engine/tests/plan_test.rs` (plan B5/B8) — what remains of the
//! orchestrator is the read-only understand → localize → plan pipeline.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use leveler_core::RequestId;
use leveler_execution::{PermissionProfile, Workspace};
use leveler_model::{
    FinishReason, Message, ModelError, ModelEventStream, ModelProfile, ModelRef, ModelRequest,
    ModelResponse, ModelRuntime, Role, TokenUsage,
};
use leveler_orchestrator::{Orchestrator, OrchestratorEvent};
use leveler_tools::{ToolContext, default_registry};

struct MockRuntime {
    responses: Mutex<VecDeque<ModelResponse>>,
}

impl MockRuntime {
    fn new(responses: Vec<ModelResponse>) -> Self {
        Self {
            responses: Mutex::new(VecDeque::from(responses)),
        }
    }
}

#[async_trait]
impl ModelRuntime for MockRuntime {
    async fn generate(
        &self,
        _request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        self.responses.lock().unwrap().pop_front().ok_or_else(|| {
            ModelError::new(leveler_model::ModelErrorKind::Other, "no more responses")
        })
    }

    async fn stream(
        &self,
        _request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        unimplemented!("planning uses generate()")
    }

    async fn profile(&self, _model: &ModelRef) -> Result<ModelProfile, ModelError> {
        unimplemented!()
    }
}

fn text(t: &str) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message::text(Role::Assistant, t),
        finish_reason: FinishReason::Stop,
        usage: TokenUsage::default(),
    }
}

#[tokio::test]
async fn plan_only_understands_localizes_and_plans() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-planonly-{}",
        std::process::id() as u64 + 3
    ));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), "pub fn a() {}\n").unwrap();

    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::RequestApproval);
    let registry = Arc::new(default_registry());

    let requirement_json = r#"{"goal":"add function b","task_type":"feature",
        "acceptance_criteria":[{"id":"AC-1","description":"b exists"}]}"#;
    let plan_json = r#"{"nodes":[{"id":"n1","kind":"edit",
        "description":"add pub fn b to src/lib.rs","allowed_paths":["src/lib.rs"]}]}"#;
    let runtime = Arc::new(MockRuntime::new(vec![
        text(requirement_json),
        text(plan_json),
    ]));

    let orchestrator =
        Orchestrator::new(runtime, registry, tool_context, ModelRef::new("mock", "m"));

    let mut events = Vec::new();
    let (requirement, graph) = orchestrator
        .plan_only(
            "add a function b",
            &mut |e| events.push(e),
            &CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(requirement.goal, "add function b");
    assert_eq!(graph.nodes.len(), 1);
    assert_eq!(graph.nodes[0].allowed_paths, vec!["src/lib.rs"]);

    // All three planning events fired, in order.
    assert!(matches!(events[0], OrchestratorEvent::RequirementReady(_)));
    assert!(matches!(events[1], OrchestratorEvent::ContextReady { .. }));
    assert!(matches!(events[2], OrchestratorEvent::PlanReady(_)));

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn plan_only_falls_back_on_malformed_json() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-planfall-{}",
        std::process::id() as u64 + 7
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::RequestApproval);
    let registry = Arc::new(default_registry());

    // Both model calls return junk (and the retry inside request_json too):
    // the planner must fall back to a usable requirement + single edit node.
    let runtime = Arc::new(MockRuntime::new(vec![
        text("not json"),
        text("still not json"),
        text("nope"),
        text("nope again"),
    ]));

    let orchestrator =
        Orchestrator::new(runtime, registry, tool_context, ModelRef::new("mock", "m"));

    let (requirement, graph) = orchestrator
        .plan_only("do the thing", &mut |_| {}, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(requirement.goal, "do the thing");
    assert_eq!(graph.nodes.len(), 1, "fallback single edit node");
    assert_eq!(requirement.acceptance_criteria.len(), 1);
    assert!(
        !requirement.acceptance_criteria[0].required,
        "fallback AC must be optional so weak understand cannot block Verified"
    );
    assert!(
        requirement.acceptance_criteria[0]
            .verification_hint
            .is_none()
    );

    std::fs::remove_dir_all(&dir).ok();
}
