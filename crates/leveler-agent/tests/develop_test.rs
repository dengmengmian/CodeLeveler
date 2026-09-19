//! End-to-end `/develop` tests: the Analyze → Coding → Verify → Review
//! workflow driven by a scripted model runtime.
//!
//! The mock runtime serves ONE queue to every executor in the run, so the
//! response order below is also the stage order — which is what lets these
//! tests assert that the stages ran at all, and in the right sequence.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use leveler_agent::AutoClarify;
use leveler_agent::coding::{CodingRuntime, ExecutorFactory, TaskSpec};
use leveler_core::{RequestId, ToolCallId};
use leveler_engine::{ExecutionKind, TaskEngine, TaskOutcome};
use leveler_execution::{AutoApprove, PermissionProfile, Workspace};
use leveler_model::{
    ContentPart, FinishReason, Message, ModelError, ModelEventStream, ModelProfile, ModelRef,
    ModelRequest, ModelResponse, ModelRuntime, Role, TokenUsage, ToolCall,
};
use leveler_storage::Database;
use leveler_tools::ToolContext;
use leveler_verifier::VerificationPlan;

/// The surface a real coding turn gets: the tool crate's composition plus the
/// harness controls THIS crate registers (`update_plan`). Production composes
/// the same two halves in `leveler-app`.
fn default_registry() -> leveler_tools::ToolRegistry {
    let mut registry = leveler_tools::default_registry();
    leveler_agent::register_harness_controls(&mut registry);
    registry
}

struct MockRuntime {
    responses: Mutex<VecDeque<ModelResponse>>,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

impl MockRuntime {
    fn new(responses: Vec<ModelResponse>) -> Self {
        Self {
            responses: Mutex::new(VecDeque::from(responses)),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl ModelRuntime for MockRuntime {
    async fn generate(
        &self,
        request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        self.requests.lock().unwrap().push(request);
        self.responses.lock().unwrap().pop_front().ok_or_else(|| {
            ModelError::new(leveler_model::ModelErrorKind::Other, "no more responses")
        })
    }

    async fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        // One shared definition of response→stream semantics (phase 6).
        let response = self.generate(request, cancellation).await?;
        Ok(leveler_model::stream_from_response(response))
    }

    async fn profile(&self, _model: &ModelRef) -> Result<ModelProfile, ModelError> {
        Ok(serde_json::from_value(serde_json::json!({
            "id": "m", "provider": "mock", "model_id": "m", "protocol": "openai_chat",
            "capabilities": {
                "streaming": true, "tool_calling": true, "parallel_tool_calls": true,
                "structured_output": false, "reasoning": false, "vision": false
            },
            "limits": {
                "context_window": 128000, "reliable_context": 64000,
                "max_output_tokens": 4096, "max_tool_schema_bytes": 65536,
                "max_parallel_tool_calls": 4
            },
            // Priced, so a run exercises the cost half of spend accounting
            // rather than leaving every row unpriced and every cost zero.
            "pricing": {
                "input_usd_per_mtok": 1.0,
                "output_usd_per_mtok": 4.0,
                "cached_input_usd_per_mtok": 0.1
            }
        }))
        .unwrap())
    }
}

fn tool_call(id: &str, name: &str, args: serde_json::Value) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message {
            role: Role::Assistant,
            content: vec![ContentPart::ToolCall {
                call: ToolCall {
                    id: ToolCallId::new(id),
                    name: name.to_string(),
                    arguments: args,
                },
            }],
        },
        finish_reason: FinishReason::ToolCalls,
        usage: TokenUsage::default(),
    }
}

fn text(value: &str) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message::text(Role::Assistant, value),
        finish_reason: FinishReason::Stop,
        usage: TokenUsage::default(),
    }
}

struct Harness {
    engine: CodingRuntime,
    db: Database,
    dir: tempfile::TempDir,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

async fn harness(responses: Vec<ModelResponse>) -> Harness {
    harness_with(responses, PermissionProfile::Assisted, |_| {}).await
}

/// [`harness`], with the workspace profile and an extra setup pass over the
/// fresh workspace directory. Repository-operation fixtures need both: a real
/// git repo to move HEAD in, and the unrestricted profile that a `.git` write
/// only ever runs under.
async fn harness_with(
    responses: Vec<ModelResponse>,
    profile: PermissionProfile,
    setup: impl FnOnce(&std::path::Path),
) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/lib.rs"), "pub fn old() {}\n").unwrap();
    setup(dir.path());
    let workspace = Workspace::new(dir.path()).unwrap();
    let tool_context = ToolContext::with_environment(
        workspace,
        profile,
        Arc::new(leveler_core::EnvSnapshot::new(
            std::env::vars_os(),
            std::env::current_dir().unwrap_or_default(),
            std::env::temp_dir(),
        )),
    );
    let runtime = Arc::new(MockRuntime::new(responses));
    let requests = runtime.requests.clone();
    let db = Database::connect_in_memory().await.unwrap();
    let engine = CodingRuntime {
        engine: TaskEngine {
            stores: leveler_storage::EngineStores::from_database(&db),
            runtime_id: leveler_core::RuntimeId::new("rt-test"),
            boot: leveler_engine::EngineBoot {
                id: leveler_core::BootId::generate(),
                liveness: std::sync::Arc::new(leveler_test_support::TestBoots::new()),
            },
        },
        factory: ExecutorFactory {
            runtime,
            registry: Arc::new(default_registry()),
            tool_context,
            model: ModelRef::new("mock", "m"),
            commit_co_author: true,
            overrides: None,
            memory_catalog: String::new(),
            memory_expose: true,
            memory_root: None,
            background_tasks: std::sync::Arc::new(leveler_execution::BackgroundTaskRegistry::new()),
            permission_rules: leveler_execution::PermissionRuleSet::default(),
            permission_rules_path: None,
            hook_runner: leveler_execution::HookRunner::empty(std::path::PathBuf::from(".")),
            steering: None,
            allow_delegation: true,
            independent_review: leveler_agent::coding::IndependentReviewPolicy::Off,
            develop_model: None,
        },
        approver: Arc::new(AutoApprove),
        clarifier: Arc::new(AutoClarify),
        task_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    Harness {
        engine,
        db,
        dir,
        requests,
    }
}

fn spec(h: &Harness, goal: &str) -> TaskSpec {
    TaskSpec {
        runtime: leveler_agent::coding::RuntimeTaskSpec {
            goal: goal.to_string(),
            kind: ExecutionKind::Direct,
            continuation: leveler_agent::ContinuationPolicy::UntilTerminal,
            limits: leveler_agent::StepLimits::default(),
        },
        coding: leveler_agent::coding::CodingTaskSpec {
            repository: h.dir.path().to_path_buf(),
            mode: PermissionProfile::Assisted,
            sandbox: false,
            verification: VerificationPlan::default(),
            base_commit: None,
        },
    }
}

/// The two responses one Coding stage consumes: an edit, then a goal close.
fn coding_stage(marker: &str) -> Vec<ModelResponse> {
    vec![
        tool_call(
            "c1",
            "apply_patch",
            serde_json::json!({
                "patch": format!(
                    "*** Begin Patch\n*** Update File: src/lib.rs\n pub fn old() {{}}\n+pub fn {marker}() {{}}\n*** End Patch"
                )
            }),
        ),
        tool_call(
            "g1",
            "update_goal",
            serde_json::json!({"status": "complete", "summary": "done"}),
        ),
    ]
}

/// Every user-role text the run sent to a model, in order.
fn user_texts(requests: &Arc<Mutex<Vec<ModelRequest>>>) -> Vec<String> {
    requests
        .lock()
        .unwrap()
        .iter()
        .flat_map(|r| r.messages.iter())
        .filter(|m| m.role == Role::User)
        .map(|m| m.text_content())
        .collect()
}

/// The product promise of `/develop`, end to end: the code is read before it
/// is written, and read again after it is verified.
#[tokio::test]
async fn develop_runs_analyze_then_coding_then_review() {
    let mut responses = vec![text("WORK ORDER: add `added` to src/lib.rs")];
    responses.extend(coding_stage("added"));
    responses.push(text(
        "I read the change and it does what was asked.\nDECISION: PASS",
    ));
    let h = harness(responses).await;

    let spec = spec(&h, "add a function");
    let session = h.engine.create_task(&spec).await.unwrap();
    let report = h
        .engine
        .run_develop(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(report.outcome, TaskOutcome::Completed);
    assert!(
        report.completion_warnings.is_empty(),
        "a passed review leaves nothing to warn about: {:?}",
        report.completion_warnings
    );
    assert_eq!(report.modified_files, vec!["src/lib.rs".to_string()]);

    let texts = user_texts(&h.requests);
    assert!(
        texts
            .iter()
            .any(|t| t.contains("You are the Analyze stage")),
        "Analyze must run first: {texts:#?}"
    );
    assert!(
        texts.iter().any(|t| t.contains("You are the Review stage")),
        "Review must run: {texts:#?}"
    );
}

/// The work order is what makes Coding better informed. If it does not reach
/// the Coding agent, the whole workflow is theatre.
#[tokio::test]
async fn the_work_order_reaches_the_coding_agent_under_the_original_goal() {
    let mut responses = vec![text("WORK ORDER: the root cause is src/lib.rs:1")];
    responses.extend(coding_stage("added"));
    responses.push(text("DECISION: PASS"));
    let h = harness(responses).await;

    let spec = spec(&h, "add a function");
    let session = h.engine.create_task(&spec).await.unwrap();
    h.engine
        .run_develop(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    let coding_input = user_texts(&h.requests)
        .into_iter()
        .find(|t| t.contains("WORK ORDER: the root cause is src/lib.rs:1"))
        .expect("the work order must reach a turn");
    assert!(
        coding_input.starts_with("add a function"),
        "the user's goal stays verbatim above the work order: {coding_input}"
    );
}

/// REWORK sends the work back to Coding without re-analyzing.
#[tokio::test]
async fn a_rework_verdict_runs_coding_again() {
    let mut responses = vec![text("WORK ORDER: first attempt")];
    responses.extend(coding_stage("added"));
    responses.push(text("You missed the second call site.\nDECISION: REWORK"));
    responses.extend(coding_stage("added_again"));
    responses.push(text("Now it is right.\nDECISION: PASS"));
    let h = harness(responses).await;

    let spec = spec(&h, "add a function");
    let session = h.engine.create_task(&spec).await.unwrap();
    let report = h
        .engine
        .run_develop(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(report.outcome, TaskOutcome::Completed);
    assert!(report.completion_warnings.is_empty());
    let texts = user_texts(&h.requests);
    let analyses = texts
        .iter()
        .filter(|t| t.contains("You are the Analyze stage"))
        .count();
    assert_eq!(analyses, 1, "REWORK must not re-run Analyze: {texts:#?}");
    assert!(
        texts
            .iter()
            .any(|t| t.contains("You missed the second call site.")),
        "review's send-back is the next work order: {texts:#?}"
    );
}

/// The one promise that must not bend: spending the retry budget is not the
/// same as finishing the work.
#[tokio::test]
async fn a_workflow_that_never_passes_review_stops_and_says_so() {
    let mut responses = vec![text("WORK ORDER: attempt")];
    for _ in 0..4 {
        responses.extend(coding_stage("added"));
        responses.push(text("Still wrong.\nDECISION: REWORK"));
    }
    let h = harness(responses).await;

    let spec = spec(&h, "add a function");
    let session = h.engine.create_task(&spec).await.unwrap();
    let report = h
        .engine
        .run_develop(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    let warning = report
        .completion_warnings
        .iter()
        .find(|w| w.starts_with("develop:"))
        .expect("a workflow that never passed review must say so");
    assert!(
        warning.contains("stopped before Review passed"),
        "the terminal must not read as a pass: {warning}"
    );
}

/// A review that wrote prose instead of a verdict has accepted nothing.
#[tokio::test]
async fn an_undecided_review_is_not_a_pass() {
    let mut responses = vec![text("WORK ORDER: attempt")];
    responses.extend(coding_stage("added"));
    responses.push(text("This looks like it would pass the tests, probably."));
    let h = harness(responses).await;

    let spec = spec(&h, "add a function");
    let session = h.engine.create_task(&spec).await.unwrap();
    let report = h
        .engine
        .run_develop(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    assert!(
        report
            .completion_warnings
            .iter()
            .any(|w| w.contains("Review produced no verdict")),
        "an absent verdict must be reported, never guessed: {:?}",
        report.completion_warnings
    );
}

/// §0.1: an ordinary turn must not gain a stage, a model call, or a token.
#[tokio::test]
async fn an_ordinary_run_never_analyzes_or_reviews() {
    let h = harness(coding_stage("added")).await;
    let spec = spec(&h, "add a function");
    let session = h.engine.create_task(&spec).await.unwrap();
    h.engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    let texts = user_texts(&h.requests);
    assert!(
        !texts
            .iter()
            .any(|t| t.contains("You are the Analyze stage")
                || t.contains("You are the Review stage")),
        "the ordinary path must be untouched: {texts:#?}"
    );
}

/// §16: a configured model that cannot be resolved is an error, not a
/// fallback — and it fails the command that asked for it.
#[tokio::test]
async fn an_invalid_develop_model_refuses_the_command_without_starting_a_task() {
    let mut h = harness(vec![text("unused")]).await;
    h.engine.factory.develop_model = Some("gpt-5.6".to_string());

    let spec = spec(&h, "add a function");
    let session = h.engine.create_task(&spec).await.unwrap();
    let error = h
        .engine
        .run_develop(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .expect_err("a bare model name is not a provider/model reference");
    assert!(
        error.to_string().contains("gpt-5.6"),
        "the refusal names the value: {error}"
    );
    assert!(
        h.requests.lock().unwrap().is_empty(),
        "nothing may run under a model reference we could not resolve"
    );
}

/// §19: the workflow's whole bill. A stage whose spend is off the books makes
/// `/develop` look cheaper than it is, which is the one way a cost report can
/// actively mislead.
#[tokio::test]
async fn analyze_and_review_spend_lands_on_the_books() {
    let mut responses = vec![text("WORK ORDER: add `added`")];
    responses.extend(coding_stage("added"));
    responses.push(text("DECISION: PASS"));
    let h = harness(responses).await;

    let spec = spec(&h, "add a function");
    let session = h.engine.create_task(&spec).await.unwrap();
    h.engine
        .run_develop(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    let rows = leveler_storage::ModelRequestRepository::new(&h.db)
        .load_for_session(&session)
        .await
        .unwrap();
    let coding_rows = rows.iter().filter(|r| r.agent_id.is_none()).count();
    let analyze_rows = rows
        .iter()
        .filter(|r| {
            r.agent_id
                .as_deref()
                .is_some_and(|id| id.starts_with("analyze-"))
        })
        .count();
    let review_rows = rows
        .iter()
        .filter(|r| {
            r.agent_id
                .as_deref()
                .is_some_and(|id| id.starts_with("review-"))
        })
        .count();

    assert!(coding_rows >= 1, "coding's own rows: {rows:?}");
    assert!(analyze_rows >= 1, "Analyze's calls are unbilled: {rows:?}");
    assert!(review_rows >= 1, "Review's calls are unbilled: {rows:?}");
}

/// A pinned develop model reaches the reading stages and ONLY them: the code
/// is still written by the session's own model.
#[tokio::test]
async fn a_pinned_develop_model_applies_to_the_reading_stages_only() {
    let mut responses = vec![text("WORK ORDER: add `added`")];
    responses.extend(coding_stage("added"));
    responses.push(text("DECISION: PASS"));
    let mut h = harness(responses).await;
    h.engine.factory.develop_model = Some("mock/reader".to_string());

    let spec = spec(&h, "add a function");
    let session = h.engine.create_task(&spec).await.unwrap();
    h.engine
        .run_develop(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    let models: Vec<(Option<String>, String)> = leveler_storage::ModelRequestRepository::new(&h.db)
        .load_for_session(&session)
        .await
        .unwrap()
        .into_iter()
        .map(|r| (r.agent_id, r.model))
        .collect();
    for (agent_id, model) in &models {
        match agent_id.as_deref() {
            Some(id) if id.starts_with("analyze-") || id.starts_with("review-") => {
                assert_eq!(model, "reader", "a reading stage must use develop.model");
            }
            None => assert_eq!(model, "m", "coding stays on the session's model"),
            _ => {}
        }
    }
    assert!(
        models
            .iter()
            .any(|(id, _)| id.as_deref().is_some_and(|i| i.starts_with("analyze-"))),
        "no analyze rows at all: {models:?}"
    );
}

/// The reading stages cannot write, and it is the runtime that stops them —
/// not the wording of their brief. A prompt that says "do not edit" is a
/// request; an absent tool is a bound.
#[tokio::test]
async fn analyze_cannot_write_to_the_workspace() {
    let mut responses = vec![
        // Analyze reaches for an edit tool it must not have.
        tool_call(
            "a1",
            "apply_patch",
            serde_json::json!({
                "patch": "*** Begin Patch\n*** Update File: src/lib.rs\n pub fn old() {}\n+pub fn sneaky() {}\n*** End Patch"
            }),
        ),
        text("WORK ORDER: add `added` to src/lib.rs"),
    ];
    responses.extend(coding_stage("added"));
    responses.push(text("DECISION: PASS"));
    let h = harness(responses).await;

    let spec = spec(&h, "add a function");
    let session = h.engine.create_task(&spec).await.unwrap();
    h.engine
        .run_develop(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    let lib = std::fs::read_to_string(h.dir.path().join("src/lib.rs")).unwrap();
    assert!(
        !lib.contains("sneaky"),
        "Analyze wrote to the workspace; its read-only bound is not real:\n{lib}"
    );
    assert!(
        lib.contains("added"),
        "the Coding stage still writes normally:\n{lib}"
    );

    // The bound itself, not only its consequence: the mutating tools are not
    // on Analyze's surface at all. Asserting only "the file did not change"
    // would still pass if Analyze merely FAILED to write — a late-bound
    // writer that forgot to claim a scope looks identical from the file
    // system, and would be a silent widening of what this stage may do.
    let (analyze, coding) = stage_toolsets(&h.requests);
    for tool in WRITE_TOOLS {
        assert!(
            !analyze.contains(&tool.to_string()),
            "`{tool}` is on Analyze's tool surface: {analyze:?}"
        );
    }
    assert!(
        coding.contains(&"apply_patch".to_string()),
        "the Coding stage must keep its full surface: {coding:?}"
    );
}

/// Same bound on the other side of the workflow: Review judges the change, it
/// does not get to fix it.
#[tokio::test]
async fn review_cannot_write_to_the_workspace() {
    let mut responses = vec![text("WORK ORDER: add `added` to src/lib.rs")];
    responses.extend(coding_stage("added"));
    responses.push(tool_call(
        "r1",
        "apply_patch",
        serde_json::json!({
            "patch": "*** Begin Patch\n*** Update File: src/lib.rs\n pub fn old() {}\n+pub fn reviewer_fix() {}\n*** End Patch"
        }),
    ));
    responses.push(text("DECISION: PASS"));
    let h = harness(responses).await;

    let spec = spec(&h, "add a function");
    let session = h.engine.create_task(&spec).await.unwrap();
    h.engine
        .run_develop(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    let lib = std::fs::read_to_string(h.dir.path().join("src/lib.rs")).unwrap();
    assert!(
        !lib.contains("reviewer_fix"),
        "Review wrote to the workspace; its read-only bound is not real:\n{lib}"
    );
    let review = review_toolset(&h.requests);
    for tool in WRITE_TOOLS {
        assert!(
            !review.contains(&tool.to_string()),
            "`{tool}` is on Review's tool surface: {review:?}"
        );
    }
}

/// Tools that change the workspace. A reading stage must hold none of them.
const WRITE_TOOLS: &[&str] = &[
    "apply_patch",
    "write_file",
    "run_command",
    "shell_command",
    "delete_path",
];

/// The tool names offered to the first Analyze request and the first Coding
/// request, identified by the brief each one carries.
fn stage_toolsets(requests: &Arc<Mutex<Vec<ModelRequest>>>) -> (Vec<String>, Vec<String>) {
    let reqs = requests.lock().unwrap();
    let names =
        |r: &ModelRequest| -> Vec<String> { r.tools.iter().map(|t| t.name.clone()).collect() };
    let has = |r: &ModelRequest, needle: &str| {
        r.messages.iter().any(|m| m.text_content().contains(needle))
    };
    let analyze = reqs
        .iter()
        .find(|r| has(r, "You are the Analyze stage"))
        .map(&names)
        .expect("an Analyze request");
    let coding = reqs
        .iter()
        .find(|r| has(r, "The following work order was produced"))
        .map(&names)
        .expect("a Coding request");
    (analyze, coding)
}

fn review_toolset(requests: &Arc<Mutex<Vec<ModelRequest>>>) -> Vec<String> {
    let reqs = requests.lock().unwrap();
    reqs.iter()
        .find(|r| {
            r.messages
                .iter()
                .any(|m| m.text_content().contains("You are the Review stage"))
        })
        .map(|r| r.tools.iter().map(|t| t.name.clone()).collect())
        .expect("a Review request")
}

/// What the user sees a stage doing must be a description, not the prompt we
/// sent the model.
///
/// The child's `task` field is what the TUI prints beside the running stage.
/// Passing the whole brief put "You are the Analyze stage of a development
/// workflow. Another engineer — a full coding agent…" on screen for two
/// minutes: the instructions leaking out as status text.
#[tokio::test]
async fn a_running_stage_is_described_in_words_not_in_its_prompt() {
    let mut responses = vec![text("WORK ORDER: add `added`")];
    responses.extend(coding_stage("added"));
    responses.push(text("DECISION: PASS"));
    let h = harness(responses).await;

    let spec = spec(&h, "add a function");
    let session = h.engine.create_task(&spec).await.unwrap();
    let mut started: Vec<(String, String)> = Vec::new();
    h.engine
        .run_develop(
            &session,
            &spec,
            &mut |event| {
                if let leveler_engine::EngineEvent::SubAgentStarted { nickname, task, .. } = event {
                    started.push((nickname, task));
                }
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(
        started.len(),
        2,
        "analyze and review both announce: {started:?}"
    );
    for (nickname, task) in &started {
        assert!(
            !task.contains("You are the"),
            "`{nickname}` announced itself with its own prompt: {task:?}"
        );
        assert!(
            !task.contains("DECISION:"),
            "`{nickname}` leaked its answer format: {task:?}"
        );
        assert!(
            task.len() < 120,
            "`{nickname}` headline is a wall of text ({} bytes): {task:?}",
            task.len()
        );
        assert!(!task.trim().is_empty(), "`{nickname}` announced nothing");
    }
}
