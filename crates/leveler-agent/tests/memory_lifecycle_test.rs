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
use leveler_memory::{
    CandidateKind, CandidateSource, MemoryAuthority, MemoryCandidate, MemoryStore,
};
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
        // Recalled memory is a control-context block, separate from the
        // transcript; a test reading only `messages` would miss it.
        request.control_context.text().contains(needle)
            || request.messages.iter().any(|message| {
                message
                    .content
                    .iter()
                    .any(|part| matches!(part, ContentPart::Text { text } if text.contains(needle)))
            })
    })
}

/// A structured durable fact: the model owns the subject, and the test
/// supplies it directly. No prose is parsed into identity.
fn structured_fact(subject: &str, body: &str) -> MemoryCandidate {
    let key = leveler_memory::semantic_key_of(subject);
    let mut candidate = MemoryCandidate::new(
        body,
        body,
        CandidateKind::Free,
        Some(key.clone()),
        CandidateSource::UserExplicit,
        vec![format!("subject:{subject}")],
    )
    .unwrap();
    candidate.semantic_key = Some(key);
    candidate.authority = MemoryAuthority::ExplicitUser;
    candidate
}

/// The executor no longer interprets natural language into memory. Meaning
/// belongs to the model extractor; the turn must not invent an identity from
/// the sentence, so an ordinary durable-looking statement writes nothing here.
#[tokio::test]
async fn a_natural_sentence_is_not_written_by_the_executor() {
    let (dir, mem) = workspace("create");
    let runtime = Arc::new(CaptureRuntime::new(vec![assistant_text("好")]));
    let executor_a = executor(&dir, &mem, runtime.clone());
    let mut events = Vec::new();
    run_turn(
        &executor_a,
        "以后 API 响应字段命名统一使用 snake_case",
        &mut events,
    )
    .await;

    let store = MemoryStore::open(&mem).unwrap();
    assert!(
        store.effective_active().unwrap().is_empty(),
        "the executor must not parse prose into a memory identity"
    );
    assert!(store.list_pending().unwrap().is_empty());
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::MemoryChanged { .. })),
        "no memory changed: {events:?}"
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

/// A direct command is stored verbatim and is visible to a later, separate
/// session. The command does not parse an identity out of its prose, so the
/// entry is a plain record with no key.
#[tokio::test]
async fn a_direct_command_is_stored_and_recalled_in_a_later_session() {
    let (dir, mem) = workspace("direct-then-recall");
    let runtime_a = Arc::new(CaptureRuntime::new(vec![assistant_text("好")]));
    let executor_a = executor(&dir, &mem, runtime_a);
    let mut events_a = Vec::new();
    run_turn(&executor_a, "记住：终端输出保持紧凑", &mut events_a).await;

    let store = MemoryStore::open(&mem).unwrap();
    let active = store.effective_active().unwrap();
    assert_eq!(active.len(), 1);
    assert!(active[0].body.contains("终端输出保持紧凑"));
    assert!(active[0].key.is_none(), "no identity is parsed from prose");
    assert!(store.list_pending().unwrap().is_empty());

    let runtime_b = Arc::new(CaptureRuntime::new(vec![assistant_text("好")]));
    let executor_b = executor(&dir, &mem, runtime_b.clone());
    let mut events_b = Vec::new();
    run_turn(&executor_b, "终端输出应该怎么处理？", &mut events_b).await;
    assert!(
        provider_saw(&runtime_b, "终端输出保持紧凑"),
        "a new session must recall the direct write"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A structured correction — the shape the model extractor produces — replaces
/// the old truth through the lifecycle, and a later session recalls only the
/// new value. No prose is compared to decide the relation.
#[tokio::test]
async fn a_structured_correction_replaces_the_old_recall_truth() {
    let (dir, mem) = workspace("natural-update");
    let store = MemoryStore::open(&mem).unwrap();
    store
        .commit_candidate(&structured_fact("default_model", "默认模型：Pro"))
        .unwrap();
    let outcome = store
        .commit_candidate(&structured_fact("default_model", "默认模型：Flash"))
        .unwrap();
    assert_eq!(
        outcome.operation,
        leveler_memory::AppliedOperation::Superseded
    );

    let runtime_c = Arc::new(CaptureRuntime::new(vec![assistant_text("Flash")]));
    let executor_c = executor(&dir, &mem, runtime_c.clone());
    let mut events_c = Vec::new();
    run_turn(&executor_c, "默认模型是什么？", &mut events_c).await;
    assert!(provider_saw(&runtime_c, "Flash"));
    assert!(
        !provider_saw(&runtime_c, "默认模型：Pro"),
        "the superseded value must not be recalled"
    );
    assert_eq!(store.effective_active().unwrap().len(), 1);
    assert_eq!(store.list_archived().unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A candidate with no structured identity is a proposal, never current truth.
#[tokio::test]
async fn an_identityless_candidate_does_not_overwrite_current_truth() {
    let (dir, mem) = workspace("ambiguous-update");
    let store = MemoryStore::open(&mem).unwrap();
    store
        .commit_candidate(&structured_fact("default_model", "默认模型：Pro"))
        .unwrap();

    let proposed = store
        .propose(
            MemoryCandidate::new(
                "默认模型",
                "以后用 Flash 吧",
                CandidateKind::Free,
                None,
                CandidateSource::SystemInferred,
                vec![],
            )
            .unwrap(),
        )
        .unwrap();
    assert!(
        matches!(proposed, leveler_memory::ProposeOutcome::Pending(_)),
        "{proposed:?}"
    );

    assert!(store.effective_active().unwrap()[0].body.contains("Pro"));
    assert_eq!(store.list_archived().unwrap().len(), 0);
    assert_eq!(
        store.list_pending().unwrap().len(),
        1,
        "an uncertain identity waits for a person"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// M8: an unrelated turn over an empty store recalls no ids. A zero-hit search
/// is still reported as a result (count 0), because the status surface must be
/// able to tell "nothing matched" from "memory was never consulted".
#[tokio::test]
async fn a_turn_with_no_memory_reports_a_zero_hit_recall() {
    let (dir, mem) = workspace("no-recall");
    let runtime = Arc::new(CaptureRuntime::new(vec![assistant_text("ok")]));
    let executor = executor(&dir, &mem, runtime.clone());
    let mut events = Vec::new();
    run_turn(&executor, "帮我看一下这个组件", &mut events).await;
    let recalls: Vec<(usize, Vec<String>, usize)> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::MemoryRecalled {
                count,
                ids,
                experience,
            } => Some((*count, ids.clone(), *experience)),
            _ => None,
        })
        .collect();
    assert_eq!(
        recalls,
        vec![(0, Vec::new(), 0)],
        "a 0-hit search is a zero-count result, not a recalled memory: {events:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// M2/M8: a later run recalls the fact into the real provider request and emits
/// the structured recall event. The request is the evidence, not the store.
#[tokio::test]
async fn a_later_run_recalls_the_fact_into_the_provider_request() {
    let (dir, mem) = workspace("recall");
    let store = MemoryStore::open(&mem).unwrap();
    let fact = structured_fact(
        "release_probe_codename",
        "以后这个项目的发布探针代号固定为 ORANGE-7319",
    );
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

/// A direct command is a plain create: it does not guess a relation to a
/// structured memory, so the two coexist and nothing is silently replaced.
#[tokio::test]
async fn a_direct_command_does_not_guess_a_relation_to_structured_memory() {
    let (dir, mem) = workspace("update");
    let store = MemoryStore::open(&mem).unwrap();
    let first = structured_fact(
        "release_probe_codename",
        "以后这个项目的发布探针代号固定为 ORANGE-7319",
    );
    store.commit_candidate(&first).unwrap();

    let runtime_c = Arc::new(CaptureRuntime::new(vec![assistant_text("好")]));
    let executor_c = executor(&dir, &mem, runtime_c.clone());
    let mut events_c = Vec::new();
    run_turn(
        &executor_c,
        "记住：以后发布探针代号改成 BLUE-4821",
        &mut events_c,
    )
    .await;

    assert!(store.list_pending().unwrap().is_empty());
    let active = store.effective_active().unwrap();
    assert_eq!(active.len(), 2, "the command is its own plain record");
    assert!(
        active
            .iter()
            .any(|entry| entry.body.contains("ORANGE-7319"))
    );
    let direct = active
        .iter()
        .find(|entry| entry.body.contains("BLUE-4821"))
        .expect("the command was stored");
    assert!(
        direct.key.is_none(),
        "no identity is parsed from the command"
    );
    assert!(store.list_archived().unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}
