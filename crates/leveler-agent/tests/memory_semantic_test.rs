//! Main-turn boundary tests for semantic memory.
//!
//! Semantic extraction is owned by the runtime background consolidator. The
//! executor may still run the deterministic fast path and recall, but it must
//! never issue an extraction model call or wait for one.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use leveler_agent::{AgentEvent, Executor, NoopSink};
use leveler_core::RequestId;
use leveler_execution::{AutoApprove, PermissionProfile, Workspace};
use leveler_memory::{
    MemoryLanes, MemoryRoots, MemoryService, RememberRequest, ServiceScope,
    v2::{Actor, MemoryScope, OpId, ScopeRoots, UserInvocationId},
};
use leveler_model::{
    ContentPart, FinishReason, Message, ModelError, ModelErrorKind, ModelEventStream, ModelProfile,
    ModelRef, ModelRequest, ModelResponse, ModelRuntime, Role, TokenUsage,
};
use leveler_tools::ToolContext;

struct CaptureRuntime {
    responses: Mutex<VecDeque<ModelResponse>>,
    requests: Mutex<Vec<ModelRequest>>,
    generate_calls: Mutex<usize>,
}

impl CaptureRuntime {
    fn new(responses: Vec<ModelResponse>) -> Self {
        Self {
            responses: Mutex::new(VecDeque::from(responses)),
            requests: Mutex::new(Vec::new()),
            generate_calls: Mutex::new(0),
        }
    }

    fn requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().unwrap().clone()
    }

    fn generate_calls(&self) -> usize {
        *self.generate_calls.lock().unwrap()
    }
}

#[async_trait]
impl ModelRuntime for CaptureRuntime {
    async fn generate(
        &self,
        _request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        *self.generate_calls.lock().unwrap() += 1;
        Err(ModelError::new(
            ModelErrorKind::Other,
            "main turn attempted a semantic extraction call",
        ))
    }

    async fn stream(
        &self,
        request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        use leveler_model::ModelEvent;
        self.requests.lock().unwrap().push(request);
        let response = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| ModelError::new(ModelErrorKind::Other, "no scripted response"))?;
        let mut events = vec![Ok(ModelEvent::MessageStarted {
            request_id: response.request_id.clone(),
        })];
        for part in response.message.content {
            if let ContentPart::Text { text } = part {
                events.push(Ok(ModelEvent::TextDelta { delta: text }));
            }
        }
        events.push(Ok(ModelEvent::MessageCompleted {
            finish_reason: response.finish_reason,
        }));
        Ok(Box::pin(futures::stream::iter(events)))
    }

    async fn profile(&self, _model: &ModelRef) -> Result<ModelProfile, ModelError> {
        unimplemented!()
    }
}

fn assistant_text(text: &str) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message::text(Role::Assistant, text),
        finish_reason: FinishReason::Stop,
        usage: TokenUsage::default(),
    }
}

/// The project-scope Markdown service this test's `memory` path holds.
fn memory_service(dir: &std::path::Path, memory: &std::path::Path) -> Arc<MemoryService> {
    Arc::new(MemoryService::new(MemoryRoots::new().with_project(
        ScopeRoots::new(
            MemoryScope::Project,
            memory.to_path_buf(),
            dir.join("state"),
        ),
    )))
}

fn active(service: &MemoryService) -> Vec<leveler_memory::MemoryView> {
    service
        .listing_for_user(ServiceScope::Project, MemoryLanes::ALL)
        .unwrap()
}

fn executor(
    dir: &std::path::Path,
    memory: &std::path::Path,
    runtime: Arc<CaptureRuntime>,
) -> Executor {
    let workspace = Workspace::new(dir).unwrap();
    let tool_context = ToolContext::with_environment(
        workspace,
        PermissionProfile::Assisted,
        Arc::new(leveler_core::EnvSnapshot::new(
            std::env::vars_os(),
            std::env::current_dir().unwrap_or_default(),
            std::env::temp_dir(),
        )),
    );
    Executor::new(
        runtime,
        Arc::new(leveler_tools::default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        6,
    )
    .with_memory_expose(true)
    .with_memory_service(Some(memory_service(dir, memory)))
    .with_approver(Arc::new(AutoApprove))
}

async fn run_turn(executor: &Executor, request: &str) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    executor
        .run(
            request,
            &mut |event| events.push(event),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .expect("turn completes");
    events
}

#[tokio::test]
async fn ordinary_main_turn_makes_zero_semantic_model_calls() {
    let dir = tempfile::tempdir().unwrap();
    let memory = dir.path().join("memory");
    let runtime = Arc::new(CaptureRuntime::new(vec![assistant_text("好")]));
    let executor = executor(dir.path(), &memory, runtime.clone());

    run_turn(&executor, "这个项目后面模型就 Pro 吧").await;

    assert_eq!(runtime.generate_calls(), 0);
    assert_eq!(runtime.requests().len(), 1, "only the coding call is made");
}

/// The removed deterministic parser must not come back. A durable-looking
/// sentence makes no semantic call and writes no memory in the main turn;
/// meaning is the background extractor's job.
#[tokio::test]
async fn a_durable_looking_sentence_waits_for_the_background_extractor() {
    let dir = tempfile::tempdir().unwrap();
    let memory = dir.path().join("memory");
    let runtime = Arc::new(CaptureRuntime::new(vec![assistant_text("好")]));
    let executor = executor(dir.path(), &memory, runtime.clone());

    let events = run_turn(&executor, "以后本项目默认模型固定为 flash").await;

    assert_eq!(runtime.generate_calls(), 0);
    let service = memory_service(dir.path(), &memory);
    assert!(
        active(&service).is_empty(),
        "the main turn must not parse prose into memory"
    );
    assert!(
        service
            .pending(
                &Actor::User {
                    invocation: UserInvocationId::mint("semantic-test")
                },
                ServiceScope::Project
            )
            .unwrap()
            .is_empty()
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::MemoryChanged { .. })),
        "no memory changed: {events:?}"
    );
}

#[tokio::test]
async fn recall_remains_on_the_main_turn_without_triggering_extraction() {
    let dir = tempfile::tempdir().unwrap();
    let memory = dir.path().join("memory");
    let service = memory_service(dir.path(), &memory);
    let mut request = RememberRequest::new(
        OpId::derived("fixture", "default-model"),
        ServiceScope::Project,
        "默认模型",
        "以后本项目默认模型固定为 flash",
        "note",
    );
    request.subject = Some("默认模型".to_string());
    let entry_id = service
        .remember(
            &Actor::User {
                invocation: UserInvocationId::mint("semantic-fixture"),
            },
            request,
        )
        .unwrap()
        .applied_id()
        .expect("the fact was written")
        .to_string();
    let runtime = Arc::new(CaptureRuntime::new(vec![assistant_text("flash")]));
    let executor = executor(dir.path(), &memory, runtime.clone());

    let events = run_turn(&executor, "模型按之前定的来").await;

    assert_eq!(runtime.generate_calls(), 0);
    let request = &runtime.requests()[0];
    // The main turn carries the recalled record in the uncached tail, with its
    // revision: applicability is the model's judgment and the full text is one
    // memory read away.
    let trailing = request.control_context.trailing_text();
    assert!(
        trailing.contains(&format!("[{entry_id} r1]")),
        "the recalled candidate must reach the provider request: {trailing}"
    );
    assert!(trailing.contains("默认模型"), "{trailing}");
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::MemoryRecalled { count, .. } if *count >= 1
    )));
}

#[tokio::test]
async fn coding_failure_is_returned_without_a_semantic_model_call() {
    let dir = tempfile::tempdir().unwrap();
    let memory = dir.path().join("memory");
    let runtime = Arc::new(CaptureRuntime::new(Vec::new()));
    let executor = executor(dir.path(), &memory, runtime.clone());

    let result = executor
        .run(
            "普通消息",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await;

    assert!(result.is_err(), "the coding failure must remain visible");
    assert_eq!(runtime.generate_calls(), 0);
    assert_eq!(
        runtime.requests().len(),
        1,
        "only the failed coding call ran"
    );
}
