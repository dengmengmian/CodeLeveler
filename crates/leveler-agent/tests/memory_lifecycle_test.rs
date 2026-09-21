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
use leveler_memory::{MemoryStore, parse_durable_fact};
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

/// A durable-looking sentence is still only a proposal. The user did not issue
/// a memory command, so the runtime must not turn it into active truth.
#[tokio::test]
async fn durable_user_rule_waits_for_acceptance() {
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
    assert!(store.effective_active().unwrap().is_empty());
    let pending = store.list_pending().unwrap();
    assert_eq!(pending.len(), 1, "exactly one candidate waits for consent");
    assert!(pending[0].body.contains("ORANGE-7319"));
    assert!(
        !events.iter().any(
            |e| matches!(e, AgentEvent::MemoryChanged { operation, .. } if operation == "created")
        ),
        "a proposal is not a durable-memory change: {events:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A command is itself consent, so the narrow natural-language command path
/// remains a direct durable write.
#[tokio::test]
async fn explicit_remember_command_writes_active_memory() {
    let (dir, mem) = workspace("direct-command");
    let runtime = Arc::new(CaptureRuntime::new(vec![assistant_text("好")]));
    let executor = executor(&dir, &mem, runtime);
    let mut events = Vec::new();
    run_turn(&executor, "记住：终端输出保持紧凑", &mut events).await;

    let store = MemoryStore::open(&mem).unwrap();
    let active = store.effective_active().unwrap();
    assert_eq!(active.len(), 1);
    assert!(active[0].body.contains("终端输出保持紧凑"));
    assert!(store.list_pending().unwrap().is_empty());
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::MemoryChanged { operation, .. } if operation == "created"
    )));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A direct save and a later correction share the same semantic identity. The
/// explicit command must not become a keyless island that keeps being recalled
/// after the user changes their mind.
#[tokio::test]
async fn direct_save_then_later_correction_recalls_only_the_new_truth() {
    let (dir, mem) = workspace("direct-then-correct");
    let runtime_a = Arc::new(CaptureRuntime::new(vec![assistant_text("好")]));
    let executor_a = executor(&dir, &mem, runtime_a);
    let mut events_a = Vec::new();
    run_turn(
        &executor_a,
        "记住：以后本项目默认模型固定为 Pro",
        &mut events_a,
    )
    .await;

    let runtime_b = Arc::new(CaptureRuntime::new(vec![assistant_text("已更新")]));
    let executor_b = executor(&dir, &mem, runtime_b);
    let mut events_b = Vec::new();
    run_turn(&executor_b, "以后本项目默认模型改为 Flash", &mut events_b).await;

    let store = MemoryStore::open(&mem).unwrap();
    let active = store.effective_active().unwrap();
    assert_eq!(active.len(), 1);
    assert!(active[0].body.contains("Flash"));
    assert!(store.list_pending().unwrap().is_empty());
    assert!(events_b.iter().any(|event| matches!(
        event,
        AgentEvent::MemoryChanged { operation, .. } if operation == "superseded"
    )));

    let runtime_c = Arc::new(CaptureRuntime::new(vec![assistant_text("Flash")]));
    let executor_c = executor(&dir, &mem, runtime_c.clone());
    let mut events_c = Vec::new();
    run_turn(&executor_c, "默认模型是什么？", &mut events_c).await;
    assert!(provider_saw(&runtime_c, "默认模型：Flash"));
    assert!(!provider_saw(&runtime_c, "默认模型：Pro"));
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
    let store = MemoryStore::open(&mem).unwrap();
    let fact = parse_durable_fact("以后这个项目的发布探针代号固定为 ORANGE-7319").unwrap();
    store.commit_candidate(&fact).unwrap();

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

/// An explicit correction of a known subject takes effect immediately: the
/// current turn must win over stale memory while the old value remains only as
/// non-recallable history.
#[tokio::test]
async fn an_explicit_correction_replaces_old_recall_truth() {
    let (dir, mem) = workspace("update");
    let store = MemoryStore::open(&mem).unwrap();
    let first = parse_durable_fact("以后这个项目的发布探针代号固定为 ORANGE-7319").unwrap();
    let created = store.commit_candidate(&first).unwrap();
    let old_id = created.entry.unwrap().id;

    let runtime_c = Arc::new(CaptureRuntime::new(vec![assistant_text("好")]));
    let executor_c = executor(&dir, &mem, runtime_c.clone());
    let mut events_c = Vec::new();
    run_turn(&executor_c, "以后发布探针代号改成 BLUE-4821", &mut events_c).await;

    assert!(store.list_pending().unwrap().is_empty());
    let active = store.effective_active().unwrap();
    assert_eq!(active.len(), 1, "one current truth");
    assert!(active[0].body.contains("BLUE-4821"));
    assert!(
        events_c.iter().any(|e| matches!(
            e,
            AgentEvent::MemoryChanged { operation, .. } if operation == "superseded"
        )),
        "the stale-memory correction must be explicit: {events_c:?}"
    );
    let history = store.list_archived().unwrap();
    assert_eq!(history.len(), 1, "history preserved");
    assert_eq!(history[0].id, old_id);

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
