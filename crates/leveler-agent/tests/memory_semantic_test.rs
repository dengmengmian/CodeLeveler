//! Runtime evidence that semantic memory-candidate extraction closes.
//!
//! These drive the REAL executor loop over the REAL file-backed memory store,
//! with the coding model replaced by a scripted double at the network boundary
//! and the semantic extractor replaced by a scripted double at the LLM
//! boundary. Everything between the user's sentence and the provider request
//! — input minimization, the deterministic validation gate, the commit
//! decision, the store, recall — is production code.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use leveler_agent::{AgentEvent, Executor, ExtractionError, NoopSink, SemanticExtractor};
use leveler_core::RequestId;
use leveler_execution::{AutoApprove, PermissionProfile, Workspace};
use leveler_memory::{
    CandidateDurability, CandidateScope, MemoryAuthority, MemoryStore, OperationHint,
    SemanticCandidate,
};
use leveler_model::{
    ContentPart, FinishReason, Message, ModelError, ModelErrorKind, ModelEventStream, ModelProfile,
    ModelRef, ModelRequest, ModelResponse, ModelRuntime, Role, TokenUsage,
};
use leveler_tools::ToolContext;

// ── the coding model double ────────────────────────────────────────────────

/// Replays scripted assistant responses and records every request the executor
/// builds, so "what did the provider actually see" is asserted, not assumed.
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
        unreachable!("the extractor has its own double; the coding model streams")
    }

    async fn stream(
        &self,
        request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        use leveler_model::ModelEvent;
        self.requests.lock().unwrap().push(request);
        let response =
            self.responses.lock().unwrap().pop_front().ok_or_else(|| {
                ModelError::new(ModelErrorKind::Other, "no more scripted responses")
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

// ── the semantic extractor double ──────────────────────────────────────────

/// Answers with candidates derived from the user's sentence by a script, and
/// records every input so input minimization is testable.
struct FakeExtractor {
    script: Box<dyn Fn(&str) -> Result<Vec<SemanticCandidate>, ExtractionError> + Send + Sync>,
    inputs: Mutex<Vec<String>>,
}

impl FakeExtractor {
    fn new(
        script: impl Fn(&str) -> Result<Vec<SemanticCandidate>, ExtractionError> + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            script: Box::new(script),
            inputs: Mutex::new(Vec::new()),
        })
    }

    fn inputs(&self) -> Vec<String> {
        self.inputs.lock().unwrap().clone()
    }
}

#[async_trait]
impl SemanticExtractor for FakeExtractor {
    async fn extract(
        &self,
        user_message: &str,
        _cancellation: &CancellationToken,
    ) -> Result<Vec<SemanticCandidate>, ExtractionError> {
        self.inputs.lock().unwrap().push(user_message.to_string());
        (self.script)(user_message)
    }
}

fn candidate(
    fact: &str,
    subject: &str,
    evidence: &str,
    durability: CandidateDurability,
    hint: OperationHint,
) -> SemanticCandidate {
    SemanticCandidate {
        fact: fact.to_string(),
        subject: subject.to_string(),
        value: None,
        scope: CandidateScope::Project,
        durability,
        authority: MemoryAuthority::ExplicitUser,
        operation_hint: hint,
        evidence_span: evidence.to_string(),
        confidence: None,
    }
}

// ── the real executor over the real store ──────────────────────────────────

fn executor(
    dir: &std::path::Path,
    mem: &std::path::Path,
    runtime: Arc<CaptureRuntime>,
    extractor: Option<Arc<dyn SemanticExtractor>>,
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
    .with_semantic_extractor(extractor)
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
        "leveler-memory-semantic-{name}-{}",
        std::process::id() as u64 * 67 + 11
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

fn created_events(events: &[AgentEvent]) -> usize {
    events
        .iter()
        .filter(
            |e| matches!(e, AgentEvent::MemoryChanged { operation, .. } if operation == "created"),
        )
        .count()
}

// ── SMCE-2 / SMCE-3: many atomic candidates, durable split from temporary ──

#[tokio::test]
async fn one_message_yields_several_atomic_candidates_and_a_durable_split() {
    let (dir, mem) = workspace("atomic");
    let extractor = FakeExtractor::new(|_| {
        Ok(vec![
            candidate(
                "项目新方向使用 Rust",
                "project.language",
                "后面就 Rust 了",
                CandidateDurability::Durable,
                OperationHint::Create,
            ),
            candidate(
                "Windows 支持是长期要求",
                "project.windows.support",
                "Windows 也不能丢",
                CandidateDurability::Durable,
                OperationHint::Create,
            ),
            candidate(
                "本阶段先完成 macOS",
                "project.current_priority",
                "这阶段先把 macOS 做完",
                CandidateDurability::Temporary,
                OperationHint::Temporary,
            ),
        ])
    });
    let runtime = Arc::new(CaptureRuntime::new(vec![assistant_text("好")]));
    let executor = executor(&dir, &mem, runtime, Some(extractor.clone()));
    let mut events = Vec::new();
    run_turn(
        &executor,
        "后面就 Rust 了，Windows 也不能丢，这阶段先把 macOS 做完。",
        &mut events,
    )
    .await;

    let store = MemoryStore::open(&mem).unwrap();
    let active = store.effective_active().unwrap();
    assert_eq!(
        active.len(),
        2,
        "two durable facts, temporary refused: {active:?}"
    );
    assert!(active.iter().any(|e| e.body.contains("Rust")));
    assert!(active.iter().any(|e| e.body.contains("Windows")));
    assert!(
        !active.iter().any(|e| e.body.contains("macOS")),
        "a temporary priority is not long-term project memory"
    );
    assert_eq!(created_events(&events), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

// ── SMCE-4 / SMCE-5: evidence is a hard gate ───────────────────────────────

#[tokio::test]
async fn a_candidate_with_hallucinated_evidence_is_refused() {
    let (dir, mem) = workspace("hallucinated");
    // The user asked about a login bug; the model invented a Rust preference.
    let extractor = FakeExtractor::new(|_| {
        Ok(vec![candidate(
            "用户长期偏好 Rust",
            "project.language",
            "用户长期偏好 Rust",
            CandidateDurability::Durable,
            OperationHint::Create,
        )])
    });
    let runtime = Arc::new(CaptureRuntime::new(vec![assistant_text("修好了")]));
    let executor = executor(&dir, &mem, runtime, Some(extractor));
    let mut events = Vec::new();
    run_turn(&executor, "今天帮我修一下登录 bug", &mut events).await;

    let store = MemoryStore::open(&mem).unwrap();
    assert!(
        store.effective_active().unwrap().is_empty(),
        "an invented evidence span must not become memory"
    );
    assert_eq!(created_events(&events), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

// ── SMCE-6: recalled memory can never be re-proposed ───────────────────────

#[tokio::test]
async fn the_extractor_never_sees_recalled_memory_or_the_assistant() {
    let (dir, mem) = workspace("no-loop");
    // A durable fact already exists in the store.
    let store = MemoryStore::open(&mem).unwrap();
    let seed =
        leveler_memory::parse_durable_fact("以后本项目平台支持固定为 DISTINCTIVE-RECALL-TOKEN-42")
            .unwrap();
    store.commit_candidate(&seed).unwrap();

    // The extractor is instructed to propose whatever it is shown, so if it
    // were shown the recalled memory it would re-commit it.
    let extractor = FakeExtractor::new(|message| {
        if message.contains("DISTINCTIVE-RECALL-TOKEN-42") {
            Ok(vec![candidate(
                "平台支持 DISTINCTIVE-RECALL-TOKEN-42",
                "project.windows.support",
                "DISTINCTIVE-RECALL-TOKEN-42",
                CandidateDurability::Durable,
                OperationHint::Create,
            )])
        } else {
            Ok(Vec::new())
        }
    });
    let runtime = Arc::new(CaptureRuntime::new(vec![assistant_text("好的")]));
    let executor = executor(&dir, &mem, runtime, Some(extractor.clone()));
    let mut events = Vec::new();
    run_turn(&executor, "继续。", &mut events).await;

    let inputs = extractor.inputs();
    assert_eq!(inputs.len(), 1, "the extractor ran once");
    assert_eq!(inputs[0], "继续。");
    assert!(
        !inputs[0].contains("DISTINCTIVE-RECALL-TOKEN-42"),
        "input minimization: recalled memory is never handed to the extractor"
    );
    assert_eq!(
        store.effective_active().unwrap().len(),
        1,
        "no self-reinforcement"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ── SMCE-7: natural-language update supersedes ─────────────────────────────

#[tokio::test]
async fn a_paraphrased_update_supersedes_the_active_memory() {
    let (dir, mem) = workspace("update");
    let extractor = FakeExtractor::new(|message| {
        if message.contains("Pro 吧") {
            Ok(vec![candidate(
                "默认模型是 Pro",
                "project.default_model",
                "模型就 Pro",
                CandidateDurability::Durable,
                OperationHint::Create,
            )])
        } else if message.contains("换回 Flash") {
            Ok(vec![candidate(
                "默认模型是 Flash",
                "project.default_model",
                "换回 Flash",
                CandidateDurability::Durable,
                OperationHint::Update,
            )])
        } else {
            Ok(Vec::new())
        }
    });
    let runtime = Arc::new(CaptureRuntime::new(vec![
        assistant_text("好"),
        assistant_text("好"),
    ]));
    let executor = executor(&dir, &mem, runtime, Some(extractor));
    let mut events_a = Vec::new();
    run_turn(&executor, "这个项目后面模型就 Pro 吧", &mut events_a).await;
    let mut events_b = Vec::new();
    run_turn(
        &executor,
        "模型还是换回 Flash 吧，Pro 先不用了",
        &mut events_b,
    )
    .await;

    let store = MemoryStore::open(&mem).unwrap();
    let active = store.effective_active().unwrap();
    assert_eq!(active.len(), 1, "one current truth");
    assert!(active[0].body.contains("Flash"));
    assert_eq!(
        store.list_archived().unwrap().len(),
        1,
        "Pro is history, not deleted"
    );
    assert!(
        events_b.iter().any(|e| matches!(
            e,
            AgentEvent::MemoryChanged { operation, .. } if operation == "superseded"
        )),
        "a supersede event must be emitted: {events_b:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ── SMCE-8: reaffirm does not duplicate ────────────────────────────────────

#[tokio::test]
async fn a_reaffirmed_fact_creates_no_duplicate_and_no_event() {
    let (dir, mem) = workspace("reaffirm");
    let extractor = FakeExtractor::new(|_| {
        Ok(vec![candidate(
            "默认模型是 Pro",
            "project.default_model",
            "还是 Pro",
            CandidateDurability::Durable,
            OperationHint::Reaffirm,
        )])
    });
    let runtime = Arc::new(CaptureRuntime::new(vec![
        assistant_text("好"),
        assistant_text("好"),
    ]));
    let executor = executor(&dir, &mem, runtime, Some(extractor));
    let mut first = Vec::new();
    run_turn(&executor, "默认还是 Pro", &mut first).await;
    let mut second = Vec::new();
    run_turn(&executor, "模型还是 Pro，别换了", &mut second).await;

    let store = MemoryStore::open(&mem).unwrap();
    assert_eq!(store.effective_active().unwrap().len(), 1);
    assert_eq!(created_events(&first), 1);
    assert_eq!(
        created_events(&second),
        0,
        "a reaffirmation is not a new memory: {second:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ── SMCE-9: temporary / speculative never auto-commits ─────────────────────

#[tokio::test]
async fn temporary_and_speculative_candidates_stay_out_of_long_term_memory() {
    let (dir, mem) = workspace("temporary");
    let extractor = FakeExtractor::new(|_| {
        Ok(vec![
            candidate(
                "今天先用 Flash 调试",
                "project.today_model",
                "今天先用 Flash",
                CandidateDurability::Temporary,
                OperationHint::Temporary,
            ),
            candidate(
                "也许以后会用 Rust",
                "project.language",
                "也许以后会用 Rust",
                CandidateDurability::Unknown,
                OperationHint::Unknown,
            ),
        ])
    });
    let runtime = Arc::new(CaptureRuntime::new(vec![assistant_text("好")]));
    let executor = executor(&dir, &mem, runtime, Some(extractor));
    let mut events = Vec::new();
    run_turn(
        &executor,
        "今天先用 Flash 吧，也许以后会用 Rust。",
        &mut events,
    )
    .await;

    let store = MemoryStore::open(&mem).unwrap();
    assert!(store.effective_active().unwrap().is_empty());
    assert_eq!(created_events(&events), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

// ── SMCE-10: extraction failure never fails the coding turn ────────────────

#[tokio::test]
async fn extraction_failure_does_not_fail_the_coding_turn() {
    let (dir, mem) = workspace("failure");
    let extractor = FakeExtractor::new(|_| Err(ExtractionError::Timeout));
    let runtime = Arc::new(CaptureRuntime::new(vec![assistant_text("任务完成")]));
    let executor = executor(&dir, &mem, runtime, Some(extractor));
    let mut events = Vec::new();
    run_turn(&executor, "看一下这个组件", &mut events).await;

    let store = MemoryStore::open(&mem).unwrap();
    assert!(store.effective_active().unwrap().is_empty());
    assert_eq!(created_events(&events), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

// ── fast path stays zero-cost: no extraction when it already answered ──────

#[tokio::test]
async fn the_fast_path_suppresses_the_extraction_call() {
    let (dir, mem) = workspace("fast-path");
    let extractor = FakeExtractor::new(|_| {
        Ok(vec![candidate(
            "默认模型是 flash",
            "project.default_model",
            "固定为 flash",
            CandidateDurability::Durable,
            OperationHint::Create,
        )])
    });
    let runtime = Arc::new(CaptureRuntime::new(vec![assistant_text("好")]));
    let executor = executor(&dir, &mem, runtime, Some(extractor.clone()));
    let mut events = Vec::new();
    run_turn(&executor, "以后本项目默认模型固定为 flash", &mut events).await;

    assert!(
        extractor.inputs().is_empty(),
        "the deterministic path handled it; the model must not be called"
    );
    let store = MemoryStore::open(&mem).unwrap();
    assert_eq!(store.effective_active().unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

// ── SMCE-12: cross-session create → recall → update → recall ──────────────

#[tokio::test]
async fn cross_session_create_recall_update_recall() {
    let (dir, mem) = workspace("cross-session");
    let script = |message: &str| -> Result<Vec<SemanticCandidate>, ExtractionError> {
        if message.contains("模型就 Pro") {
            Ok(vec![candidate(
                "默认模型是 Pro",
                "project.default_model",
                "模型就 Pro",
                CandidateDurability::Durable,
                OperationHint::Create,
            )])
        } else if message.contains("换回 Flash") {
            Ok(vec![candidate(
                "默认模型是 Flash",
                "project.default_model",
                "换回 Flash",
                CandidateDurability::Durable,
                OperationHint::Update,
            )])
        } else {
            Ok(Vec::new())
        }
    };

    // Session A — create.
    let runtime_a = Arc::new(CaptureRuntime::new(vec![assistant_text("好")]));
    let executor_a = executor(&dir, &mem, runtime_a, Some(FakeExtractor::new(script)));
    let mut events_a = Vec::new();
    run_turn(
        &executor_a,
        "这个项目后面模型就 Pro 吧，别再来回切了",
        &mut events_a,
    )
    .await;

    // Session B — recall into a real provider request.
    let runtime_b = Arc::new(CaptureRuntime::new(vec![assistant_text("Pro")]));
    let executor_b = executor(
        &dir,
        &mem,
        runtime_b.clone(),
        Some(FakeExtractor::new(|_| Ok(Vec::new()))),
    );
    let mut events_b = Vec::new();
    run_turn(&executor_b, "模型这块按之前定的来", &mut events_b).await;
    assert!(
        provider_saw(&runtime_b, "默认模型是 Pro"),
        "the recalled truth must reach the provider request"
    );
    assert!(events_b.iter().any(|e| matches!(
        e,
        AgentEvent::MemoryRecalled { count, .. } if *count >= 1
    )));

    // Session C — update.
    let runtime_c = Arc::new(CaptureRuntime::new(vec![assistant_text("好")]));
    let executor_c = executor(&dir, &mem, runtime_c, Some(FakeExtractor::new(script)));
    let mut events_c = Vec::new();
    run_turn(
        &executor_c,
        "模型还是换回 Flash 吧，Pro 先不用了",
        &mut events_c,
    )
    .await;

    // Session D — recall the new truth, never the old one.
    let runtime_d = Arc::new(CaptureRuntime::new(vec![assistant_text("Flash")]));
    let executor_d = executor(
        &dir,
        &mem,
        runtime_d.clone(),
        Some(FakeExtractor::new(|_| Ok(Vec::new()))),
    );
    let mut events_d = Vec::new();
    run_turn(&executor_d, "模型按之前最终定的那个走", &mut events_d).await;
    assert!(
        provider_saw(&runtime_d, "默认模型是 Flash"),
        "new truth recalled"
    );
    assert!(
        !provider_saw(&runtime_d, "默认模型是 Pro"),
        "superseded truth must not be recalled"
    );

    let store = MemoryStore::open(&mem).unwrap();
    assert_eq!(store.effective_active().unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}
