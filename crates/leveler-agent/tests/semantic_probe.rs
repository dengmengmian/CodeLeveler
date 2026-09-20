//! Real-model dogfood probe for semantic memory-candidate extraction.
//!
//! Ignored by default: it makes network calls to a real provider. It exists so
//! the extraction contract can be measured against a real model — paraphrase
//! coverage, evidence faithfulness, latency, token cost and failure behavior —
//! instead of asserted from a double.
//!
//! Run it with the provider under test, e.g.:
//!
//! ```text
//! SEMANTIC_PROBE_BASE_URL=https://taotoken.net/api/v1 \
//! SEMANTIC_PROBE_API_KEY=$TAOTOKEN_API_KEY \
//! SEMANTIC_PROBE_MODEL=deepseek-flash \
//!   cargo test -p leveler-agent --test semantic_probe -- --ignored --nocapture
//! ```

use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use leveler_agent::{AgentEvent, Executor, ModelSemanticExtractor, NoopSink};
use leveler_execution::{AutoApprove, PermissionProfile, Workspace};
use leveler_memory::{
    MemoryCandidate, MemoryStore, parse_semantic_candidates, validate_semantic_candidates,
};
use leveler_model::{
    ContentPart, ModelCapabilities, ModelError, ModelEventStream, ModelLimits, ModelPricing,
    ModelProfile, ModelRef, ModelRequest, ModelResponse, ModelRuntime, ProtocolKind,
    ReasoningConfig, ReasoningEffort, ReasoningStyle,
};
use leveler_provider::{ModelConfigFile, ProviderConfig, ProviderRegistry, RegistryInputs};
use leveler_tools::ToolContext;

/// The probe sentences, in order. Each is a natural paraphrase, deliberately
/// avoiding the deterministic fast path's templates.
const PROBE: &[(&str, &str)] = &[
    ("A", "这个项目后面模型就 Pro 吧，别老切来切去了。"),
    ("B", "Windows 这块还是得保留，只是最近先把 macOS 收完。"),
    ("C", "保护分支后面别直接推了，都走正常流程。"),
    ("D", "今天 full test 先别跑。"),
    ("E", "Go 这条线就不继续了，后面的东西按 Rust 来。"),
    ("F", "模型还是改回 Flash 吧。"),
];

fn model_profile(model_id: &str) -> ModelProfile {
    ModelProfile {
        id: model_id.to_string(),
        provider: "probe".to_string(),
        model_id: model_id.to_string(),
        protocol: ProtocolKind::OpenAiChat,
        capabilities: ModelCapabilities {
            streaming: true,
            tool_calling: true,
            parallel_tool_calls: true,
            structured_output: false,
            reasoning: true,
            vision: false,
        },
        limits: ModelLimits {
            context_window: 1_048_576,
            reliable_context: 786_432,
            max_output_tokens: 65_536,
            max_tool_schema_bytes: 32_768,
            max_parallel_tool_calls: 1,
            max_tool_output_bytes: None,
        },
        context_quality: None,
        reasoning: ReasoningConfig {
            style: ReasoningStyle::ThinkingFlag,
            supported_efforts: vec![
                ReasoningEffort::Low,
                ReasoningEffort::High,
                ReasoningEffort::Max,
            ],
            default_effort: Some(ReasoningEffort::Low),
        },
        compatibility: Default::default(),
        instructions: None,
        pricing: Some(ModelPricing {
            input_usd_per_mtok: 0.1389,
            output_usd_per_mtok: 0.2778,
            cached_input_usd_per_mtok: None,
        }),
    }
}

#[tokio::test]
#[ignore = "real provider; requires SEMANTIC_PROBE_* / provider API key"]
async fn real_model_semantic_extraction_probe() {
    let Ok(api_key) = std::env::var("SEMANTIC_PROBE_API_KEY")
        .or_else(|_| std::env::var("TAOTOKEN_API_KEY"))
        .or_else(|_| std::env::var("DEEPSEEK_API_KEY"))
    else {
        eprintln!("SKIP: no SEMANTIC_PROBE_API_KEY / TAOTOKEN_API_KEY / DEEPSEEK_API_KEY");
        return;
    };
    let base_url = std::env::var("SEMANTIC_PROBE_BASE_URL")
        .unwrap_or_else(|_| "https://api.deepseek.com".to_string());
    let model_id =
        std::env::var("SEMANTIC_PROBE_MODEL").unwrap_or_else(|_| "deepseek-chat".to_string());

    let provider = ProviderConfig {
        id: "probe".to_string(),
        protocol: ProtocolKind::OpenAiChat,
        base_url,
        api_key_env: String::new(),
        api_key: Some(api_key.clone()),
        headers: Default::default(),
        timeouts: Default::default(),
        retry: Default::default(),
    };
    let registry = ProviderRegistry::build(RegistryInputs {
        providers: vec![(provider, Some(api_key))],
        models: vec![ModelConfigFile {
            profile: model_profile(&model_id),
            policy: None,
        }],
    })
    .expect("registry builds");
    let runtime: Arc<dyn leveler_model::ModelRuntime> = Arc::new(registry);
    let model = ModelRef::new("probe", &model_id);
    let extractor =
        ModelSemanticExtractor::new(runtime.clone(), model).with_timeout(Duration::from_secs(60));

    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::open(dir.path()).unwrap();

    let mut total_in = 0u64;
    let mut total_out = 0u64;
    let mut total_ms = 0u128;
    let mut hits = 0usize;
    let mut failures = 0usize;
    let mut timeouts = 0usize;

    for (label, sentence) in PROBE {
        println!("\n========== {label}: {sentence}");
        let started = Instant::now();
        let request = extractor.build_request(sentence).expect("non-empty");
        let response = match tokio::time::timeout(
            Duration::from_secs(60),
            runtime.generate(request, tokio_util::sync::CancellationToken::new()),
        )
        .await
        {
            Err(_) => {
                timeouts += 1;
                failures += 1;
                println!("TIMEOUT");
                continue;
            }
            Ok(Err(error)) => {
                failures += 1;
                println!("MODEL ERROR: {error}");
                continue;
            }
            Ok(Ok(response)) => response,
        };
        let elapsed = started.elapsed();
        total_ms += elapsed.as_millis();
        total_in += response.usage.input_tokens;
        total_out += response.usage.output_tokens;

        let text = response.message.text_content();
        let parsed = match parse_semantic_candidates(&text) {
            Ok(candidates) => candidates,
            Err(error) => {
                failures += 1;
                println!("PARSE ERROR: {error}\nraw: {text}");
                continue;
            }
        };
        println!(
            "latency={}ms in_tokens={} out_tokens={} raw_candidates={}",
            elapsed.as_millis(),
            response.usage.input_tokens,
            response.usage.output_tokens,
            parsed.len()
        );
        for candidate in &parsed {
            println!(
                "  candidate fact={:?} subject={:?} durability={:?} authority={:?} hint={:?}\n    evidence={:?}",
                candidate.fact,
                candidate.subject,
                candidate.durability,
                candidate.authority,
                candidate.operation_hint,
                candidate.evidence_span
            );
        }
        let validated = validate_semantic_candidates(&parsed, sentence);
        for (index, rejection) in &validated.rejected {
            println!(
                "  REJECTED[{index}] code={} reason={}",
                rejection.code(),
                rejection.reason()
            );
        }
        if validated.accepted.is_empty() {
            println!("  no durable candidate");
        } else {
            hits += 1;
        }
        for memory in &validated.accepted {
            let applied = commit(&store, memory);
            println!(
                "  COMMITTED fact={:?} key={:?} -> {applied:?}",
                memory.body, memory.semantic_key
            );
        }
    }

    println!("\n===== STORE (effective active) =====");
    for entry in store.effective_active().unwrap() {
        println!(
            "  active key={:?} body={:?} authority={:?}",
            entry.key,
            entry.body,
            entry.authority()
        );
    }
    println!("===== HISTORY =====");
    for entry in store.list_archived().unwrap() {
        println!("  archived {:?} status={:?}", entry.body, entry.status);
    }

    let turns = PROBE.len();
    let cost_usd =
        (total_in as f64 / 1_000_000.0) * 0.1389 + (total_out as f64 / 1_000_000.0) * 0.2778;
    println!("\n===== METRICS =====");
    println!("turns={turns}");
    println!("extra_model_call_per_turn=1");
    println!("extra_request_rate=1.0 (one extraction per eligible turn)");
    println!("hit_rate={hits}/{turns}");
    println!("timeout_rate={timeouts}/{turns}");
    println!("failure_rate={failures}/{turns}");
    println!(
        "total_extraction_ms={total_ms} mean_ms={}",
        total_ms / turns as u128
    );
    println!("total_input_tokens={total_in} total_output_tokens={total_out}");
    println!(
        "cost_per_100_turns_usd={:.6}",
        cost_usd / turns as f64 * 100.0
    );
}

fn commit(store: &MemoryStore, memory: &MemoryCandidate) -> String {
    match store.commit_candidate(memory) {
        Ok(outcome) => format!("{:?}", outcome.operation),
        Err(error) => format!("error: {error}"),
    }
}

// ── end-to-end: the real executor, the real extractor, the real store ───────

/// Delegates to the real registry and records every request, so "what did the
/// provider actually see" is evidence from a real run.
struct RecordingRuntime {
    inner: Arc<dyn ModelRuntime>,
    requests: Mutex<Vec<ModelRequest>>,
    /// Raw text of every extraction (non-streaming) response, in order.
    extractions: Mutex<Vec<String>>,
}

#[async_trait]
impl ModelRuntime for RecordingRuntime {
    async fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        self.requests.lock().unwrap().push(request.clone());
        self.inner.stream(request, cancellation).await
    }

    async fn generate(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        let response = self.inner.generate(request, cancellation).await?;
        self.extractions
            .lock()
            .unwrap()
            .push(response.message.text_content());
        Ok(response)
    }

    async fn profile(&self, model: &ModelRef) -> Result<ModelProfile, ModelError> {
        self.inner.profile(model).await
    }
}

/// Build a real provider from the probe environment, or `None` when unset.
fn real_registry() -> Option<(Arc<RecordingRuntime>, ModelRef)> {
    let api_key = std::env::var("SEMANTIC_PROBE_API_KEY")
        .or_else(|_| std::env::var("TAOTOKEN_API_KEY"))
        .or_else(|_| std::env::var("DEEPSEEK_API_KEY"))
        .ok()?;
    let base_url = std::env::var("SEMANTIC_PROBE_BASE_URL")
        .unwrap_or_else(|_| "https://api.deepseek.com".to_string());
    let model_id =
        std::env::var("SEMANTIC_PROBE_MODEL").unwrap_or_else(|_| "deepseek-chat".to_string());
    let provider = ProviderConfig {
        id: "probe".to_string(),
        protocol: ProtocolKind::OpenAiChat,
        base_url,
        api_key_env: String::new(),
        api_key: Some(api_key.clone()),
        headers: Default::default(),
        timeouts: Default::default(),
        retry: Default::default(),
    };
    let registry = ProviderRegistry::build(RegistryInputs {
        providers: vec![(provider, Some(api_key))],
        models: vec![ModelConfigFile {
            profile: model_profile(&model_id),
            policy: None,
        }],
    })
    .ok()?;
    let runtime: Arc<dyn ModelRuntime> = Arc::new(registry);
    let recording = Arc::new(RecordingRuntime {
        inner: runtime,
        requests: Mutex::new(Vec::new()),
        extractions: Mutex::new(Vec::new()),
    });
    Some((recording, ModelRef::new("probe", &model_id)))
}

fn real_executor(
    runtime: Arc<RecordingRuntime>,
    model: &ModelRef,
    dir: &std::path::Path,
    mem: &std::path::Path,
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
    // An EMPTY tool registry: this probe exercises the memory path, not the
    // coding tools, and it must never let a real model touch the machine.
    let extractor: Arc<dyn leveler_agent::SemanticExtractor> = Arc::new(
        ModelSemanticExtractor::new(runtime.clone(), model.clone())
            .with_timeout(Duration::from_secs(60)),
    );
    Executor::new(
        runtime,
        Arc::new(leveler_tools::ToolRegistry::new()),
        tool_context,
        model.clone(),
        2,
    )
    .with_memory_expose(true)
    .with_memory_root(Some(mem.to_path_buf()))
    .with_semantic_extractor(Some(extractor))
    .with_approver(Arc::new(AutoApprove))
}

async fn real_turn(executor: &Executor, request: &str, events: &mut Vec<AgentEvent>) {
    executor
        .run(
            request,
            &mut |event| events.push(event),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .expect("real turn completes");
}

/// SMCE-12 against a real model: create → recall → update → recall, driven by
/// the real executor over the real store. Ignored by default.
#[tokio::test]
#[ignore = "real provider; requires SEMANTIC_PROBE_* / provider API key"]
async fn real_model_cross_session_create_recall_update_recall() {
    let Some((runtime, model)) = real_registry() else {
        eprintln!("SKIP: no probe API key");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let mem = dir.path().join("memory");

    // Session A — create from a natural paraphrase, no templates.
    let executor_a = real_executor(runtime.clone(), &model, dir.path(), &mem);
    let mut _events_a = Vec::new();
    real_turn(
        &executor_a,
        "这个项目后面模型就 Pro 吧，别再来回切了",
        &mut _events_a,
    )
    .await;
    let store = MemoryStore::open(&mem).unwrap();
    let created = store.effective_active().unwrap();
    assert_eq!(created.len(), 1, "session A created exactly one memory");
    let created_body = created[0].body.clone();
    println!("session A created: {created_body:?}");
    println!(
        "session A raw extraction: {:?}",
        runtime.extractions.lock().unwrap().last()
    );

    // Session B — recall into a real provider request.
    let before_b = runtime.requests.lock().unwrap().len();
    let executor_b = real_executor(runtime.clone(), &model, dir.path(), &mem);
    let mut _events_b = Vec::new();
    real_turn(&executor_b, "模型这块按之前定的来", &mut _events_b).await;
    let saw_created_in_b = requests_since(&runtime, before_b, &created_body);
    println!("session B: recall saw created body = {saw_created_in_b}");

    // Session C — paraphrase update.
    let executor_c = real_executor(runtime.clone(), &model, dir.path(), &mem);
    let mut _events_c = Vec::new();
    real_turn(
        &executor_c,
        "模型还是换回 Flash 吧，Pro 先不用了",
        &mut _events_c,
    )
    .await;
    let current = store.effective_active().unwrap();
    assert_eq!(current.len(), 1, "session C left one current truth");
    let current_body = current[0].body.clone();
    let old_bodies: Vec<String> = store
        .list_archived()
        .unwrap()
        .into_iter()
        .map(|entry| entry.body)
        .collect();
    println!(
        "session C raw extraction: {:?}",
        runtime.extractions.lock().unwrap().last()
    );
    println!("session C current: {current_body:?}, history: {old_bodies:?}");

    // Session D — the new truth is recalled, the old one is not, and a
    // recall/defer statement creates nothing.
    let before_d = runtime.requests.lock().unwrap().len();
    let executor_d = real_executor(runtime.clone(), &model, dir.path(), &mem);
    let mut _events_d = Vec::new();
    real_turn(&executor_d, "模型按之前最终定的那个走", &mut _events_d).await;
    println!(
        "session D raw extraction: {:?}",
        runtime.extractions.lock().unwrap().last()
    );
    let saw_current_in_d = requests_since(&runtime, before_d, &current_body);
    let saw_old_in_d = old_bodies
        .iter()
        .any(|body| requests_since(&runtime, before_d, body));
    println!("session D: recall saw current = {saw_current_in_d}, saw superseded = {saw_old_in_d}");

    let final_active = store.effective_active().unwrap();
    for entry in &final_active {
        println!("final active: key={:?} body={:?}", entry.key, entry.body);
    }
    for entry in store.list_archived().unwrap() {
        println!("final history: {:?} {:?}", entry.body, entry.status);
    }
    assert!(saw_created_in_b, "session B must recall the created truth");
    assert!(saw_current_in_d, "session D must recall the updated truth");
    assert!(
        !saw_old_in_d,
        "session D must not recall the superseded truth"
    );
    assert_eq!(final_active.len(), 1);
    assert_eq!(
        final_active[0].body, current_body,
        "a recall/defer sentence must not rewrite memory"
    );
}

fn requests_since(runtime: &Arc<RecordingRuntime>, from: usize, needle: &str) -> bool {
    runtime.requests.lock().unwrap()[from..]
        .iter()
        .any(|request| {
            request.messages.iter().any(|message| {
                message
                    .content
                    .iter()
                    .any(|part| matches!(part, ContentPart::Text { text } if text.contains(needle)))
            })
        })
}
