//! Runtime evidence that the durable-memory lifecycle closes.
//!
//! These tests drive the REAL executor loop with a capturing model runtime:
//! the memory is formed by the runtime, persisted to the real file store, and
//! recalled into a real `ModelRequest`. The provider is a test double at the
//! network boundary only — every step between the user sentence and the
//! provider request is production code.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use leveler_agent::{AgentEvent, Executor, NoopSink};
use leveler_core::RequestId;
use leveler_execution::{AutoApprove, PermissionProfile, Workspace};
use leveler_memory::{MemoryAuthority, MemoryStore, parse_durable_fact};
use leveler_model::{
    ContentPart, FinishReason, Message, ModelError, ModelEventStream, ModelProfile, ModelRef,
    ModelRequest, ModelResponse, ModelRuntime, Role, TokenUsage,
};
use leveler_tools::ToolContext;

/// Replays scripted responses and records every request the executor builds.
struct CaptureRuntime {
    responses: Mutex<VecDeque<ModelResponse>>,
    requests: Mutex<Vec<ModelRequest>>,
}

impl CaptureRuntime {
    fn new(responses: Vec<ModelResponse>) -> Self {
        Self {
            responses: Mutex::new(VecDeque::from(responses)),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl ModelRuntime for CaptureRuntime {
    async fn generate(
        &self,
        _request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        unreachable!("the executor uses streaming")
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
            if let ContentPart::Text { text } = part {
                events.push(Ok(ModelEvent::TextDelta {
                    delta: text.clone(),
                }));
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

/// An executor with memory exposed and pointed at `mem`; everything else is the
/// production default surface.
fn executor(
    dir: &std::path::Path,
    mem: &std::path::Path,
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
    .with_memory_root(Some(mem.to_path_buf()))
    .with_approver(Arc::new(AutoApprove))
}

async fn run_turn(executor: &Executor, request: &str, events: &mut Vec<AgentEvent>) {
    executor
        .run(
            request,
            &mut |event| events.push(event),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .expect("turn completes");
}

fn workspace(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "leveler-memory-lifecycle-{name}-{}",
        std::process::id() as u64 * 41 + 3
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mem = dir.join("memory");
    (dir, mem)
}

fn provider_saw(runtime: &CaptureRuntime, needle: &str) -> bool {
    runtime.requests().iter().any(|request| {
        request.messages.iter().any(|message| {
            message
                .content
                .iter()
                .any(|part| matches!(part, ContentPart::Text { text } if text.contains(needle)))
        })
    })
}

/// M1: a normal sentence forms active memory with no `/remember` and no accept.
#[tokio::test]
async fn explicit_user_rule_becomes_active_memory_autonomously() {
    let (dir, mem) = workspace("create");
    let runtime = Arc::new(CaptureRuntime::new(vec![assistant_text("好")]));
    let executor = executor(&dir, &mem, runtime.clone());
    let mut events = Vec::new();
    run_turn(
        &executor,
        "以后这个项目的发布探针代号固定为 ORANGE-7319",
        &mut events,
    )
    .await;

    let store = MemoryStore::open(&mem).unwrap();
    let active = store.effective_active().unwrap();
    assert_eq!(active.len(), 1, "exactly one durable fact");
    assert_eq!(active[0].authority(), MemoryAuthority::ExplicitUser);
    assert!(active[0].body.contains("ORANGE-7319"));
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::MemoryChanged { operation, .. } if operation == "created"
        )),
        "a created event must be emitted: {events:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// M8: an unrelated turn over an empty store recalls nothing and emits nothing.
#[tokio::test]
async fn a_turn_with_no_memory_emits_no_recall_event() {
    let (dir, mem) = workspace("no-recall");
    let runtime = Arc::new(CaptureRuntime::new(vec![assistant_text("ok")]));
    let executor = executor(&dir, &mem, runtime.clone());
    let mut events = Vec::new();
    run_turn(&executor, "帮我看一下这个组件", &mut events).await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::MemoryRecalled { .. })),
        "a 0-hit search is not a recall: {events:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// M2/M8: a later run recalls the fact into the real provider request and emits
/// the structured recall event. The request is the evidence, not the store.
#[tokio::test]
async fn a_later_run_recalls_the_fact_into_the_provider_request() {
    let (dir, mem) = workspace("recall");
    let runtime_a = Arc::new(CaptureRuntime::new(vec![assistant_text("记住了")]));
    let executor_a = executor(&dir, &mem, runtime_a.clone());
    let mut events_a = Vec::new();
    run_turn(
        &executor_a,
        "以后这个项目的发布探针代号固定为 ORANGE-7319",
        &mut events_a,
    )
    .await;

    // A NEW executor over the same store — a new session reading durable memory.
    let runtime_b = Arc::new(CaptureRuntime::new(vec![assistant_text("ORANGE-7319")]));
    let executor_b = executor(&dir, &mem, runtime_b.clone());
    let mut events_b = Vec::new();
    run_turn(
        &executor_b,
        "我们之前给发布探针定的代号是什么？",
        &mut events_b,
    )
    .await;

    assert!(
        provider_saw(&runtime_b, "ORANGE-7319"),
        "the provider request must carry the recalled body"
    );
    assert!(
        events_b.iter().any(|e| matches!(
            e,
            AgentEvent::MemoryRecalled { count, .. } if *count >= 1
        )),
        "a real recall must emit an event: {events_b:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// M3/M4/M9: a correction converges to one current truth, keeps history, and
/// emits the supersede event; recall then returns only the new value.
#[tokio::test]
async fn a_correction_supersedes_the_old_fact_and_recall_follows() {
    let (dir, mem) = workspace("update");
    let store = MemoryStore::open(&mem).unwrap();
    let first = parse_durable_fact("以后这个项目的发布探针代号固定为 ORANGE-7319").unwrap();
    let created = store.commit_candidate(&first).unwrap();
    let old_id = created.entry.unwrap().id;

    let runtime_c = Arc::new(CaptureRuntime::new(vec![assistant_text("好")]));
    let executor_c = executor(&dir, &mem, runtime_c.clone());
    let mut events_c = Vec::new();
    run_turn(&executor_c, "以后发布探针代号改成 BLUE-4821", &mut events_c).await;

    let active = store.effective_active().unwrap();
    assert_eq!(active.len(), 1, "one current truth");
    assert!(active[0].body.contains("BLUE-4821"));
    assert_eq!(active[0].supersedes.as_deref(), Some(old_id.as_str()));
    assert!(
        events_c.iter().any(|e| matches!(
            e,
            AgentEvent::MemoryChanged { operation, .. } if operation == "superseded"
        )),
        "a supersede event must be emitted: {events_c:?}"
    );
    assert_eq!(store.list_archived().unwrap().len(), 1, "history preserved");

    // A later recall returns the new value and never the old one.
    let runtime_d = Arc::new(CaptureRuntime::new(vec![assistant_text("BLUE-4821")]));
    let executor_d = executor(&dir, &mem, runtime_d.clone());
    let mut events_d = Vec::new();
    run_turn(&executor_d, "发布探针代号是什么？", &mut events_d).await;
    assert!(provider_saw(&runtime_d, "BLUE-4821"), "new truth recalled");
    assert!(
        !provider_saw(&runtime_d, "ORANGE-7319"),
        "superseded truth must not be recalled"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
