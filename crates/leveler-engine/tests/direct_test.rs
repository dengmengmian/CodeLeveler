//! End-to-end DirectStrategy tests (plan B3): a scripted model runtime drives
//! the engine and every side of persistence is asserted — turns, turn-stamped
//! messages, the append-only event log, and the terminal outcome column.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use leveler_agent::{AutoClarify, StopReason};
use leveler_core::{RequestId, ToolCallId};
use leveler_engine::{
    EngineEvent, ExecutionKind, ExecutorFactory, TaskEngine, TaskOutcome, TaskSpec,
};
use leveler_execution::{AutoApprove, PermissionProfile, Workspace};
use leveler_model::{
    ContentPart, FinishReason, Message, ModelError, ModelEventStream, ModelProfile, ModelRef,
    ModelRequest, ModelResponse, ModelRuntime, Role, TokenUsage, ToolCall,
};
use leveler_storage::{
    Database, EventRepository, MessageRepository, SessionRepository, TurnRepository,
};
use leveler_tools::{ToolContext, default_registry};
use leveler_verifier::{CheckKind, VerificationCommand, VerificationPlan};

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
        use leveler_model::ModelEvent;
        let response = self.generate(request, cancellation).await?;
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
                    events.push(Ok(ModelEvent::ToolCallCompleted { call: call.clone() }));
                }
                _ => {}
            }
        }
        events.push(Ok(ModelEvent::MessageCompleted {
            finish_reason: response.finish_reason,
        }));
        Ok(Box::pin(futures::stream::iter(events)))
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

fn patch_then_resolve() -> Vec<ModelResponse> {
    vec![
        tool_call(
            "c1",
            "apply_patch",
            serde_json::json!({
                "patch": "*** Begin Patch\n*** Update File: src/lib.rs\n pub fn old() {}\n+pub fn added() {}\n*** End Patch"
            }),
        ),
        tool_call(
            "g1",
            "update_goal",
            serde_json::json!({"status": "complete", "summary": "added the function"}),
        ),
    ]
}

/// Understand JSON with a required AC that greps the patch fixture (`pub fn added`).
fn understand_met_required_ac() -> ModelResponse {
    text(
        r#"{"goal":"add a function","task_type":"feature","constraints":[],
        "acceptance_criteria":[{"id":"AC-1","description":"added() exists",
        "verification_hint":"grep -q 'pub fn added' src/lib.rs","required":true}],
        "out_of_scope":[],"risk":"low","uncertainties":[]}"#,
    )
}

/// Goal turn + understand that proves required acceptance (impl-class Verified path).
fn patch_resolve_and_proven_ac() -> Vec<ModelResponse> {
    let mut v = patch_then_resolve();
    v.push(understand_met_required_ac());
    v
}

struct Harness {
    engine: TaskEngine,
    dir: tempfile::TempDir,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

async fn harness(responses: Vec<ModelResponse>) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/lib.rs"), "pub fn old() {}\n").unwrap();
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
    let runtime = Arc::new(MockRuntime::new(responses));
    let requests = runtime.requests.clone();
    let engine = TaskEngine {
        db: Database::connect_in_memory().await.unwrap(),
        factory: ExecutorFactory {
            runtime,
            registry: Arc::new(default_registry()),
            tool_context,
            model: ModelRef::new("mock", "m"),
            commit_co_author: true,
            overrides: None,
            work_profile: leveler_agent::WorkProfile::Balanced,
            memory_index: String::new(),
            permission_rules: leveler_execution::PermissionRuleSet::default(),
            permission_rules_path: None,
            hook_runner: leveler_execution::HookRunner::empty(std::path::PathBuf::from(".")),
            grants_state_dir: None,
        },
        approver: Arc::new(AutoApprove),
        clarifier: Arc::new(AutoClarify),
    };
    Harness {
        engine,
        dir,
        requests,
    }
}

#[tokio::test]
async fn factory_reasoning_override_reaches_every_model_request() {
    let mut h = harness(vec![tool_call(
        "g1",
        "update_goal",
        serde_json::json!({"status": "complete", "summary": "done"}),
    )])
    .await;
    h.engine.factory.overrides = Some(leveler_engine::ExecutionOverrides {
        reasoning_effort: Some(leveler_model::ReasoningEffort::High),
        ..leveler_engine::ExecutionOverrides::default()
    });
    let spec = spec(&h, VerificationPlan::default());
    let session = h.engine.create_task(&spec).await.unwrap();
    h.engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    let requests = h.requests.lock().unwrap();
    assert!(!requests.is_empty());
    assert!(
        requests.iter().all(|request| {
            request.reasoning_effort == Some(leveler_model::ReasoningEffort::High)
        })
    );
}

fn spec(h: &Harness, plan: VerificationPlan) -> TaskSpec {
    TaskSpec {
        repository: h.dir.path().to_path_buf(),
        goal: "add a function".to_string(),
        mode: PermissionProfile::Assisted,
        sandbox: false,
        kind: ExecutionKind::Direct,
        continuation: leveler_agent::ContinuationPolicy::UntilTerminal,
        limits: leveler_agent::StepLimits::default(),
        verification: plan,
    }
}

fn gate(name: &str, program: &str) -> VerificationPlan {
    VerificationPlan {
        commands: vec![VerificationCommand {
            name: name.into(),
            program: program.into(),
            args: vec![],
            kind: CheckKind::Test,
            gating: true,
            timeout_seconds: 30,
        }],
    }
}

#[tokio::test]
async fn direct_run_persists_turns_messages_events_and_outcome() {
    // Impl-class Verified requires proven Met required AC (not empty fallback).
    let h = harness(patch_resolve_and_proven_ac()).await;
    let spec = spec(&h, gate("ok", "true"));
    let session = h.engine.create_task(&spec).await.unwrap();

    let mut seen: Vec<EngineEvent> = Vec::new();
    let report = h
        .engine
        .run(
            &session,
            &spec,
            &mut |e| seen.push(e),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(report.outcome, TaskOutcome::Verified);
    assert_eq!(report.modified_files, vec!["src/lib.rs".to_string()]);

    // Session row: execution config + terminal outcome.
    let (mode, sandbox, kind, outcome) = SessionRepository::new(&h.engine.db)
        .execution(&session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (mode.as_str(), sandbox, kind.as_str(), outcome),
        ("assisted", false, "direct", Some(TaskOutcome::Verified))
    );

    // One user turn, completed, owning the transcript messages.
    let turns = TurnRepository::new(&h.engine.db)
        .list(&session)
        .await
        .unwrap();
    assert_eq!(turns.len(), 1);
    assert_eq!(
        (turns[0].kind.as_str(), turns[0].status.as_str()),
        ("user", "completed")
    );
    assert!(turns[0].finished_at.is_some());
    let turn_id = leveler_core::TurnId::new(turns[0].id.clone());
    let turn_messages = MessageRepository::new(&h.engine.db)
        .load_for_turn(&session, &turn_id)
        .await
        .unwrap();
    assert!(
        !turn_messages.is_empty(),
        "the transcript must be stamped with the turn id"
    );

    // The event log: ordered, persisted, and shaped as expected.
    let rows = EventRepository::new(&h.engine.db)
        .load(&session)
        .await
        .unwrap();
    let types: Vec<&str> = rows.iter().map(|r| r.event_type.as_str()).collect();
    assert_eq!(types.first(), Some(&"task_started"));
    assert_eq!(types.last(), Some(&"task_finished"));
    for expected in [
        "turn_started",
        "tool_call_started",
        "tool_call_finished",
        "turn_finished",
        "verification_started",
        "verification_check",
        "verification_finished",
    ] {
        assert!(types.contains(&expected), "missing {expected} in {types:?}");
    }
    let sequences: Vec<i64> = rows.iter().map(|r| r.sequence).collect();
    assert_eq!(
        sequences,
        (1..=rows.len() as i64).collect::<Vec<_>>(),
        "sequences must be gapless"
    );

    // The observer saw the same terminal event (persist-before-forward held).
    assert!(seen.iter().any(|e| matches!(
        e,
        EngineEvent::TaskFinished {
            outcome: TaskOutcome::Verified,
            ..
        }
    )));
}

#[tokio::test]
async fn no_gates_means_completed_unverified() {
    let h = harness(patch_then_resolve()).await;
    let spec = spec(&h, VerificationPlan::default());
    let session = h.engine.create_task(&spec).await.unwrap();
    let report = h
        .engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(report.outcome, TaskOutcome::CompletedUnverified);
    let (_, _, _, outcome) = SessionRepository::new(&h.engine.db)
        .execution(&session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome, Some(TaskOutcome::CompletedUnverified));
}

/// K19: pure Q&A (no mutations) with a green gate plan must stay
/// CompletedUnverified — never claim Verified just because the repo is healthy.
#[tokio::test]
async fn pure_qa_with_green_gates_is_completed_unverified() {
    let h = harness(vec![tool_call(
        "g1",
        "update_goal",
        serde_json::json!({"status": "complete", "summary": "auth uses JWT sessions"}),
    )])
    .await;
    let mut s = spec(&h, gate("ok", "true"));
    s.goal = "explain how auth works".to_string();
    let session = h.engine.create_task(&s).await.unwrap();
    let report = h
        .engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(report.outcome, TaskOutcome::CompletedUnverified);
    assert!(
        report.modified_files.is_empty(),
        "Q&A must not leave mutations: {:?}",
        report.modified_files
    );
    assert!(
        report.verification.is_none(),
        "K19 early-exit skips verify when there is no mutation"
    );
    assert!(!report.outcome.is_success());
}

/// Implementation-class Direct task with real edits and all-green gates → Verified
/// via shared `finalize_task_outcome` (needs_mutation + has_mutation).
#[tokio::test]
async fn impl_with_mutations_and_green_gates_is_verified() {
    let h = harness(patch_resolve_and_proven_ac()).await;
    // Goal contains "add" → task_looks_like_implementation; patch mutates src/lib.rs.
    let s = spec(&h, gate("ok", "true"));
    assert!(
        s.goal.to_lowercase().contains("add"),
        "fixture goal must look like implementation"
    );
    let session = h.engine.create_task(&s).await.unwrap();
    let report = h
        .engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(report.outcome, TaskOutcome::Verified);
    assert!(
        !report.modified_files.is_empty(),
        "impl path requires observed mutation"
    );
    assert!(report.verification.is_some());
    let ledger = report
        .acceptance
        .expect("Direct emits acceptance when Verified");
    assert!(
        ledger.has_proven_required_met(),
        "impl Verified requires Met required AC"
    );
    assert!(report.outcome.is_success());
}

/// rust-h3 class: green gates + mutation + empty/unproven AC → not Verified.
/// Content-only edits cannot false-Verified via mutation-derived path checks
/// (file still exists → no MUT-DEL synthesis).
#[tokio::test]
async fn impl_green_gates_with_unproven_acceptance_is_completed_unverified() {
    // No understand response → fallback optional AC → no proven required Met.
    let h = harness(patch_then_resolve()).await;
    let s = spec(&h, gate("ok", "true"));
    let session = h.engine.create_task(&s).await.unwrap();
    let report = h
        .engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        report.outcome,
        TaskOutcome::CompletedUnverified,
        "implementation-class without proven required AC must not Verified"
    );
    assert!(!report.outcome.is_success());
    if let Some(ledger) = &report.acceptance {
        assert!(
            !ledger.has_proven_required_met(),
            "edit-only mutation must not synthesize proven AC: {ledger:?}"
        );
    }
}

/// Delete a workspace file; understand fails (no response) → mutation-derived
/// `test ! -e` proves absence → Verified despite optional fallback AC.
#[tokio::test]
async fn delete_file_with_green_gates_and_no_understand_is_verified() {
    let responses = vec![
        tool_call(
            "c1",
            "apply_patch",
            serde_json::json!({
                "patch": "*** Begin Patch\n*** Delete File: quicksort.py\n*** End Patch"
            }),
        ),
        tool_call(
            "g1",
            "update_goal",
            serde_json::json!({"status": "complete", "summary": "deleted quicksort.py"}),
        ),
        // no understand response → fallback + mutation-derived AC
    ];
    let h = harness(responses).await;
    std::fs::write(h.dir.path().join("quicksort.py"), "def qs(): pass\n").unwrap();
    let mut s = spec(&h, gate("ok", "true"));
    s.goal = "delete quicksort.py".to_string();
    let session = h.engine.create_task(&s).await.unwrap();
    let report = h
        .engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    assert!(
        !std::path::Path::new(&h.dir.path().join("quicksort.py")).exists(),
        "file must be gone on disk"
    );
    assert_eq!(
        report.outcome,
        TaskOutcome::Verified,
        "delete + green gates + MUT-DEL Met must Verified; got {:?}",
        report.outcome
    );
    assert!(
        report
            .modified_files
            .iter()
            .any(|p| p.contains("quicksort.py")),
        "modified_files should track delete: {:?}",
        report.modified_files
    );
    let ledger = report
        .acceptance
        .as_ref()
        .expect("acceptance ledger when health Verified");
    assert!(
        ledger.has_proven_required_met(),
        "mutation-derived required Met expected: {ledger:?}"
    );
    assert!(
        ledger
            .items
            .iter()
            .any(|i| i.id.starts_with("MUT-DEL-") && i.required),
        "expected MUT-DEL item: {ledger:?}"
    );
    assert!(report.outcome.is_success());
}

/// Direct extracts acceptance via understand and evaluates it: required Unmet
/// downgrades Verified → CompletedUnverified (same finalize as Orchestrate).
#[tokio::test]
async fn direct_required_unmet_acceptance_blocks_verified() {
    let mut responses = patch_then_resolve();
    // After goal completes + gates pass, conclude_direct calls understand.
    responses.push(text(
        r#"{"goal":"add a function","task_type":"feature","constraints":[],
        "acceptance_criteria":[{"id":"AC-1","description":"must contain NEVER_MARKER",
        "verification_hint":"grep -q NEVER_MARKER src/lib.rs","required":true}],
        "out_of_scope":[],"risk":"low","uncertainties":[]}"#,
    ));
    let h = harness(responses).await;
    let s = spec(&h, gate("ok", "true"));
    let session = h.engine.create_task(&s).await.unwrap();
    let report = h
        .engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(
        report.outcome,
        TaskOutcome::CompletedUnverified,
        "required Unmet acceptance must block Verified on Direct"
    );
    let ledger = report
        .acceptance
        .as_ref()
        .expect("Direct must emit acceptance ledger when health is Verified");
    assert!(!ledger.all_required_met());
}

/// Direct with required Met acceptance + green gates stays Verified.
#[tokio::test]
async fn direct_required_met_acceptance_allows_verified() {
    let mut responses = patch_then_resolve();
    responses.push(text(
        r#"{"goal":"add a function","task_type":"feature","constraints":[],
        "acceptance_criteria":[{"id":"AC-1","description":"added() exists",
        "verification_hint":"grep -q 'pub fn added' src/lib.rs","required":true}],
        "out_of_scope":[],"risk":"low","uncertainties":[]}"#,
    ));
    let h = harness(responses).await;
    let s = spec(&h, gate("ok", "true"));
    let session = h.engine.create_task(&s).await.unwrap();
    let report = h
        .engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(report.outcome, TaskOutcome::Verified);
    let ledger = report.acceptance.expect("ledger present");
    assert!(ledger.all_required_met());
}

#[tokio::test]
async fn top_level_goal_runs_until_terminal_past_the_old_model_round_budget() {
    let h = harness(vec![
        tool_call("c1", "list_files", serde_json::json!({"path": "."})),
        tool_call("c2", "list_files", serde_json::json!({"path": "src"})),
        tool_call("c3", "read_file", serde_json::json!({"path": "src/lib.rs"})),
        tool_call(
            "g1",
            "update_goal",
            serde_json::json!({"status": "complete", "summary": "inspection complete"}),
        ),
    ])
    .await;
    let spec = spec(&h, VerificationPlan::default());
    let session = h.engine.create_task(&spec).await.unwrap();

    let report = h
        .engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(report.stop_reason, StopReason::Completed);
    assert_eq!(report.rounds, 4);
}

#[tokio::test]
async fn active_goal_automatically_continues_in_a_new_persisted_turn_after_stall() {
    let h = harness(vec![
        text("still working 1"),
        text("still working 2"),
        text("still working 3"),
        text("still working 4"),
        tool_call(
            "g1",
            "update_goal",
            serde_json::json!({"status": "complete", "summary": "finished after continuation"}),
        ),
    ])
    .await;
    let spec = spec(&h, VerificationPlan::default());
    let session = h.engine.create_task(&spec).await.unwrap();

    let report = h
        .engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(report.outcome, TaskOutcome::CompletedUnverified);
    assert_eq!(report.stop_reason, StopReason::Completed);
    assert_eq!(report.rounds, 5);
    let turns = TurnRepository::new(&h.engine.db)
        .list(&session)
        .await
        .unwrap();
    assert_eq!(
        turns
            .iter()
            .map(|turn| (turn.kind.as_str(), turn.status.as_str()))
            .collect::<Vec<_>>(),
        vec![("user", "completed"), ("user", "completed")]
    );
}

#[tokio::test]
async fn bounded_eval_goal_still_stops_at_the_case_round_limit() {
    let h = harness(vec![
        tool_call("c1", "list_files", serde_json::json!({"path": "."})),
        tool_call("c2", "list_files", serde_json::json!({"path": "src"})),
        tool_call("c3", "read_file", serde_json::json!({"path": "src/lib.rs"})),
    ])
    .await;
    let mut spec = spec(&h, VerificationPlan::default());
    spec.continuation = leveler_agent::ContinuationPolicy::bounded(2);
    let session = h.engine.create_task(&spec).await.unwrap();

    let report = h
        .engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(report.outcome, TaskOutcome::BudgetLimited);
    assert_eq!(report.stop_reason, StopReason::BudgetExhausted);
    assert_eq!(report.rounds, 2);
}

#[tokio::test]
async fn failed_verification_repairs_once_then_fails() {
    // Goal turn (patch + resolve), one repair turn (resolve again), gate
    // always fails → Failed after the bounded repair.
    let mut responses = patch_then_resolve();
    responses.push(tool_call(
        "g2",
        "update_goal",
        serde_json::json!({"status": "complete", "summary": "repaired"}),
    ));
    let h = harness(responses).await;
    let spec = spec(&h, gate("bad", "false"));
    let session = h.engine.create_task(&spec).await.unwrap();

    let report = h
        .engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(report.outcome, TaskOutcome::Failed);
    assert!(
        !report.outcome.is_success(),
        "failed verification must never count as automation success"
    );

    let turns = TurnRepository::new(&h.engine.db)
        .list(&session)
        .await
        .unwrap();
    let kinds: Vec<&str> = turns.iter().map(|t| t.kind.as_str()).collect();
    assert_eq!(kinds, vec!["user", "repair"]);
    assert_eq!(
        turns[1].payload.as_deref(),
        Some(r#"{"attempt":1}"#),
        "the repair turn records its attempt"
    );

    let (_, _, _, outcome) = SessionRepository::new(&h.engine.db)
        .execution(&session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome, Some(TaskOutcome::Failed));
}

#[tokio::test]
async fn agent_failure_persists_terminal_task_and_turn_events() {
    // No scripted response makes the first model request fail inside the turn.
    // The query projections already become failed; the canonical log must carry
    // the same terminal facts so replay cannot disagree with those projections.
    let h = harness(Vec::new()).await;
    let spec = spec(&h, VerificationPlan::default());
    let session = h.engine.create_task(&spec).await.unwrap();

    let error = h
        .engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .expect_err("an exhausted model runtime must fail the task");
    assert!(matches!(error, leveler_engine::EngineError::Agent(_)));

    let (_, _, _, outcome) = SessionRepository::new(&h.engine.db)
        .execution(&session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome, Some(TaskOutcome::Failed));
    let turns = TurnRepository::new(&h.engine.db)
        .list(&session)
        .await
        .unwrap();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].status, "failed");

    let events = EventRepository::new(&h.engine.db)
        .load(&session)
        .await
        .unwrap()
        .into_iter()
        .map(|row| EngineEvent::from_payload(&row.payload).unwrap())
        .collect::<Vec<_>>();
    assert!(
        events.iter().any(|event| matches!(
            event,
            EngineEvent::TurnFinished {
                turn_id,
                outcome: leveler_engine::TurnOutcome::Failed,
                ..
            } if turn_id.as_str() == turns[0].id
        )),
        "a failed turn must have a canonical terminal event: {events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            EngineEvent::TaskFinished {
                outcome: TaskOutcome::Failed,
                ..
            }
        )),
        "a failed task must have a canonical terminal event: {events:?}"
    );
}

#[tokio::test]
async fn cancellation_is_recorded_as_interrupted() {
    let h = harness(patch_then_resolve()).await;
    let spec = spec(&h, VerificationPlan::default());
    let session = h.engine.create_task(&spec).await.unwrap();

    let token = CancellationToken::new();
    token.cancel();
    let err = h
        .engine
        .run(&session, &spec, &mut |_| {}, token)
        .await
        .expect_err("a pre-cancelled run must not succeed");
    assert!(matches!(
        err,
        leveler_engine::EngineError::Agent(leveler_agent::AgentError::Cancelled)
    ));

    let (_, _, _, outcome) = SessionRepository::new(&h.engine.db)
        .execution(&session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome, Some(TaskOutcome::Interrupted));
    let turns = TurnRepository::new(&h.engine.db)
        .list(&session)
        .await
        .unwrap();
    assert_eq!(turns[0].status, "interrupted");
}

/// Kill -9 / unclean TUI exit can leave a permanent `running` turn. Starting a
/// new turn must reap that zombie before inserting the next row.
#[tokio::test]
async fn starting_a_turn_reaps_orphan_running_siblings() {
    let h = harness(patch_then_resolve()).await;
    let spec = spec(&h, VerificationPlan::default());
    let session = h.engine.create_task(&spec).await.unwrap();

    // Simulate a zombie left by process kill: status running, no finished_at.
    let zombie = TurnRepository::new(&h.engine.db)
        .start(&session, "chat", None, leveler_core::now())
        .await
        .unwrap();
    assert_eq!(zombie.status, "running");
    assert!(zombie.finished_at.is_none());

    let report = h
        .engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(report.outcome, TaskOutcome::CompletedUnverified);

    let turns = TurnRepository::new(&h.engine.db)
        .list(&session)
        .await
        .unwrap();
    assert!(
        turns.len() >= 2,
        "zombie + at least one new turn, got {}",
        turns.len()
    );
    let zombie_row = turns.iter().find(|t| t.id == zombie.id).unwrap();
    assert_eq!(
        zombie_row.status, "interrupted",
        "orphan running turn must be reaped before the next turn starts"
    );
    assert!(zombie_row.finished_at.is_some());
    assert!(
        turns.iter().any(|t| t.status == "completed"),
        "new turn must complete: {:?}",
        turns
            .iter()
            .map(|t| (t.kind.as_str(), t.status.as_str()))
            .collect::<Vec<_>>()
    );
    assert!(
        turns
            .iter()
            .all(|t| t.status != "running" || t.finished_at.is_some()),
        "no permanent running zombies should remain"
    );
    let events = EventRepository::new(&h.engine.db)
        .load(&session)
        .await
        .unwrap()
        .into_iter()
        .map(|row| EngineEvent::from_payload(&row.payload).unwrap())
        .collect::<Vec<_>>();
    assert!(
        events.iter().any(|event| matches!(
            event,
            EngineEvent::TurnFinished {
                turn_id,
                outcome: leveler_engine::TurnOutcome::Interrupted,
                ..
            } if turn_id.as_str() == zombie.id
        )),
        "reaping must leave a canonical interruption event: {events:?}"
    );
}

#[tokio::test]
async fn interrupted_direct_task_resumes_from_the_persisted_transcript() {
    // Phase 1: interrupt immediately — the seed transcript persists, the
    // session ends `interrupted`.
    let h = harness(patch_then_resolve()).await;
    let spec = spec(&h, VerificationPlan::default());
    let session = h.engine.create_task(&spec).await.unwrap();
    let token = CancellationToken::new();
    token.cancel();
    let _ = h
        .engine
        .run(&session, &spec, &mut |_| {}, token)
        .await
        .expect_err("pre-cancelled");
    let before = MessageRepository::new(&h.engine.db)
        .load(&session)
        .await
        .unwrap();
    assert!(!before.is_empty(), "the seed must have been persisted");

    // Phase 2: resume on the same database with a fresh scripted runtime.
    let dir2 = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(dir2.path().join("src")).unwrap();
    std::fs::write(dir2.path().join("src/lib.rs"), "pub fn old() {}\n").unwrap();
    let workspace = Workspace::new(dir2.path()).unwrap();
    let engine2 = TaskEngine {
        db: h.engine.db.clone(),
        factory: ExecutorFactory {
            runtime: Arc::new(MockRuntime::new(patch_then_resolve())),
            registry: Arc::new(default_registry()),
            tool_context: ToolContext::with_environment(
                workspace,
                PermissionProfile::Assisted,
                Arc::new(leveler_core::EnvSnapshot::new(
                    std::env::vars_os(),
                    std::env::current_dir().unwrap_or_default(),
                    std::env::temp_dir(),
                )),
            ),
            model: ModelRef::new("mock", "m"),
            commit_co_author: true,
            overrides: None,
            work_profile: leveler_agent::WorkProfile::Balanced,
            memory_index: String::new(),
            permission_rules: leveler_execution::PermissionRuleSet::default(),
            permission_rules_path: None,
            hook_runner: leveler_execution::HookRunner::empty(std::path::PathBuf::from(".")),
            grants_state_dir: None,
        },
        approver: Arc::new(AutoApprove),
        clarifier: Arc::new(AutoClarify),
    };
    let spec2 = TaskSpec {
        repository: dir2.path().to_path_buf(),
        goal: "add a function".to_string(),
        mode: PermissionProfile::Assisted,
        sandbox: false,
        kind: ExecutionKind::Direct,
        continuation: leveler_agent::ContinuationPolicy::UntilTerminal,
        limits: leveler_agent::StepLimits::default(),
        verification: VerificationPlan::default(),
    };

    let report = engine2
        .resume(&session, &spec2, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(report.outcome, TaskOutcome::CompletedUnverified);

    // Two turns: the interrupted original and the completed resume.
    let turns = TurnRepository::new(&h.engine.db)
        .list(&session)
        .await
        .unwrap();
    let statuses: Vec<&str> = turns.iter().map(|t| t.status.as_str()).collect();
    assert_eq!(statuses, vec!["interrupted", "completed"]);
    let (_, _, _, outcome) = SessionRepository::new(&h.engine.db)
        .execution(&session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome, Some(TaskOutcome::CompletedUnverified));
}

#[tokio::test]
async fn resume_refuses_a_successfully_completed_session() {
    let h = harness(patch_then_resolve()).await;
    let spec = spec(&h, VerificationPlan::default());
    let session = h.engine.create_task(&spec).await.unwrap();
    h.engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    let err = h
        .engine
        .resume(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .expect_err("a finished session must not be re-driven");
    assert!(err.to_string().contains("already completed"), "{err}");
}

#[tokio::test]
async fn resume_refuses_a_kind_mismatch() {
    let h = harness(patch_then_resolve()).await;
    let mut spec = spec(&h, VerificationPlan::default());
    let session = h.engine.create_task(&spec).await.unwrap();
    spec.kind = ExecutionKind::Orchestrate;
    let err = h
        .engine
        .resume(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .expect_err("kind mismatch must be refused");
    assert!(err.to_string().contains("is `direct`"), "{err}");
}
