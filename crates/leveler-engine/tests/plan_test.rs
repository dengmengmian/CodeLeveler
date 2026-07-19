//! PlanStrategy tests (plan B5): the orchestrated pipeline scenarios ported
//! onto the engine, plus the assertion the legacy path could never make —
//! every node runs as a fully-persisted turn (no NoopSink anywhere).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use leveler_agent::AutoClarify;
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

fn text(t: &str) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message::text(Role::Assistant, t),
        finish_reason: FinishReason::Stop,
        usage: TokenUsage::default(),
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

struct Harness {
    engine: TaskEngine,
    dir: tempfile::TempDir,
}

async fn harness(responses: Vec<ModelResponse>) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/lib.rs"), "pub fn a() {}\n").unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let environment = Arc::new(leveler_core::EnvSnapshot::new(
        std::env::vars_os(),
        std::env::current_dir().unwrap_or_default(),
        std::env::temp_dir(),
    ));
    let tool_context =
        ToolContext::with_environment(workspace, PermissionProfile::Assisted, environment);
    let engine = TaskEngine {
        db: Database::connect_in_memory().await.unwrap(),
        factory: ExecutorFactory {
            runtime: Arc::new(MockRuntime::new(responses)),
            registry: Arc::new(default_registry()),
            tool_context,
            model: ModelRef::new("mock", "m"),
            commit_co_author: true,
            overrides: Some(leveler_engine::ExecutionOverrides {
                completion_evidence: Some(false),
                ..Default::default()
            }),
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
    Harness { engine, dir }
}

/// A gate that always passes. Unix fixtures use `true`; Windows runners have
/// no `true`/`sh` on PATH, so the gate goes through `cmd /c` there.
fn passing_gate(name: &str) -> VerificationCommand {
    let (program, args) = if cfg!(windows) {
        ("cmd", vec!["/c".to_string(), "exit 0".to_string()])
    } else {
        ("true", Vec::new())
    };
    VerificationCommand {
        name: name.into(),
        program: program.into(),
        args,
        kind: CheckKind::Test,
        gating: true,
        timeout_seconds: 30,
    }
}

/// A gate that passes only when `needle` appears in `file`.
fn grep_gate(name: &str, needle: &str, file: &str) -> VerificationCommand {
    let (program, args) = if cfg!(windows) {
        (
            "cmd",
            vec!["/c".to_string(), format!("findstr {needle} {file}")],
        )
    } else {
        (
            "sh",
            vec!["-c".to_string(), format!("grep -q {needle} {file}")],
        )
    };
    VerificationCommand {
        name: name.into(),
        program: program.into(),
        args,
        kind: CheckKind::Build,
        gating: true,
        timeout_seconds: 30,
    }
}

fn spec(h: &Harness, plan: VerificationPlan) -> TaskSpec {
    TaskSpec {
        repository: h.dir.path().to_path_buf(),
        goal: "add a function b".to_string(),
        mode: PermissionProfile::Assisted,
        sandbox: false,
        kind: ExecutionKind::Orchestrate,
        continuation: leveler_agent::ContinuationPolicy::UntilTerminal,
        limits: leveler_agent::StepLimits::default(),
        verification: plan,
    }
}

// Required AC with a real check command — empty/missing hints are Unverifiable
// and block Verified (K2). `grep` is non-trivial so acceptance can Met.
const REQUIREMENT_JSON: &str = r#"{"goal":"add function b","task_type":"feature",
    "acceptance_criteria":[{"id":"AC-1","description":"b exists",
    "verification_hint":"grep -q 'pub fn b' src/lib.rs","required":true}]}"#;
const PLAN_JSON: &str = r#"{"nodes":[{"id":"n1","kind":"edit",
    "description":"add pub fn b to src/lib.rs","allowed_paths":["src/lib.rs"]}]}"#;
const PATCH: &str =
    "*** Begin Patch\n*** Update File: src/lib.rs\n pub fn a() {}\n+pub fn b() {}\n*** End Patch";

#[tokio::test]
async fn orchestrated_run_persists_every_node_as_a_turn() {
    let h = harness(vec![
        text(REQUIREMENT_JSON), // understand
        text(PLAN_JSON),        // plan
        tool_call("c1", "apply_patch", serde_json::json!({ "patch": PATCH })), // node round 1
        text("Added function b."), // node round 2 (finish)
    ])
    .await;
    let spec = spec(
        &h,
        VerificationPlan {
            commands: vec![passing_gate("ok")],
        },
    );
    let session = h.engine.create_task(&spec).await.unwrap();

    let mut seen = Vec::new();
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
    assert!(report.modified_files.contains(&"src/lib.rs".to_string()));
    let content = std::fs::read_to_string(h.dir.path().join("src/lib.rs")).unwrap();
    assert_eq!(content, "pub fn a() {}\npub fn b() {}\n");

    // The anti-NoopSink assertion: the node ran as a persisted turn owning
    // its transcript messages.
    let turns = TurnRepository::new(&h.engine.db)
        .list(&session)
        .await
        .unwrap();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].kind, "node");
    assert_eq!(turns[0].status, "completed");
    assert_eq!(turns[0].payload.as_deref(), Some(r#"{"node_id":"n1"}"#));
    let messages = MessageRepository::new(&h.engine.db)
        .load_for_turn(&session, &leveler_core::TurnId::new(turns[0].id.clone()))
        .await
        .unwrap();
    assert!(
        !messages.is_empty(),
        "the node transcript must be persisted and turn-stamped"
    );

    // Strategy progress is in the event log (the legacy loop kept it in memory).
    let rows = EventRepository::new(&h.engine.db)
        .load(&session)
        .await
        .unwrap();
    let types: Vec<&str> = rows.iter().map(|r| r.event_type.as_str()).collect();
    for expected in [
        "requirement_ready",
        "phase_changed",
        "plan_ready",
        "node_started",
        "node_finished",
        "verification_finished",
        "task_finished",
    ] {
        assert!(types.contains(&expected), "missing {expected} in {types:?}");
    }

    let (_, _, kind, outcome) = SessionRepository::new(&h.engine.db)
        .execution(&session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (kind.as_str(), outcome),
        ("orchestrate", Some(TaskOutcome::Verified))
    );

    // Observers saw the strategy events too.
    assert!(
        seen.iter()
            .any(|e| matches!(e, EngineEvent::PlanReady { .. }))
    );
    assert!(seen.iter().any(|e| matches!(
        e,
        EngineEvent::NodeFinished { node_id, .. } if node_id == "n1"
    )));
}

#[tokio::test]
async fn orchestrated_eval_respects_the_case_wide_round_budget() {
    let h = harness(vec![
        text(REQUIREMENT_JSON),
        text(PLAN_JSON),
        tool_call("c1", "apply_patch", serde_json::json!({ "patch": PATCH })),
        text("must not be requested"),
    ])
    .await;
    let mut spec = spec(&h, VerificationPlan::default());
    spec.continuation = leveler_agent::ContinuationPolicy::bounded(1);
    let session = h.engine.create_task(&spec).await.unwrap();

    let report = h
        .engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(report.outcome, TaskOutcome::BudgetLimited);
    assert_eq!(
        report.stop_reason,
        leveler_agent::StopReason::BudgetExhausted
    );
    assert_eq!(report.rounds, 1);
}

#[tokio::test]
async fn unbounded_orchestrated_node_can_run_past_the_legacy_twenty_round_cap() {
    // Inspect node: Answered without mutation is a normal success (K15 only
    // fails Edit). Isolation keeps this test about round caps, not mutation.
    let inspect_plan = r#"{"nodes":[{"id":"n1","kind":"inspect",
        "description":"long inspect turn"}]}"#;
    let mut responses = vec![
        text(r#"{"goal":"inspect long","acceptance_criteria":[]}"#),
        text(inspect_plan),
    ];
    for round in 1..=21 {
        responses.push(tool_call(
            &format!("plan-{round}"),
            "update_plan",
            serde_json::json!({
                "explanation": format!("round {round}"),
                "plan": [{"step": format!("step {round}"), "status": "in_progress"}]
            }),
        ));
    }
    responses.push(text("Node work is complete."));

    let h = harness(responses).await;
    let mut s = spec(&h, VerificationPlan::default());
    s.goal = "inspect long".into();
    let session = h.engine.create_task(&s).await.unwrap();

    let report = h
        .engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(report.outcome, TaskOutcome::CompletedUnverified);
    assert_eq!(report.stop_reason, leveler_agent::StopReason::Completed);
    assert_eq!(report.rounds, 22);
}

#[tokio::test]
async fn verification_failure_triggers_a_persisted_repair_turn() {
    // First edit adds `b` but NOT the FIXED marker the gate requires; the
    // repair adds it (ported from the legacy pipeline test).
    let edit2 =
        "*** Begin Patch\n*** Update File: src/lib.rs\n pub fn b() {}\n+// FIXED\n*** End Patch";
    // Required AC proves FIXED marker (same check as the gate) so impl-class
    // Verified is allowed after repair.
    let h = harness(vec![
        text(
            r#"{"goal":"add b and marker","task_type":"feature",
            "acceptance_criteria":[{"id":"AC-1","description":"FIXED marker present",
            "verification_hint":"grep -q FIXED src/lib.rs","required":true}]}"#,
        ),
        text(r#"{"nodes":[{"id":"n1","kind":"edit","description":"add pub fn b"}]}"#),
        tool_call("c1", "apply_patch", serde_json::json!({"patch": PATCH})),
        text("added b"),
        tool_call("c2", "apply_patch", serde_json::json!({"patch": edit2})),
        text("added marker"),
    ])
    .await;
    let spec = spec(
        &h,
        VerificationPlan {
            commands: vec![grep_gate("marker", "FIXED", "src/lib.rs")],
        },
    );
    let session = h.engine.create_task(&spec).await.unwrap();

    let report = h
        .engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(report.outcome, TaskOutcome::Verified);
    let content = std::fs::read_to_string(h.dir.path().join("src/lib.rs")).unwrap();
    assert!(
        content.contains("FIXED"),
        "repair must add the marker: {content}"
    );

    // Node turn + repair turn, both persisted.
    let turns = TurnRepository::new(&h.engine.db)
        .list(&session)
        .await
        .unwrap();
    let kinds: Vec<&str> = turns.iter().map(|t| t.kind.as_str()).collect();
    assert_eq!(kinds, vec!["node", "repair"]);
    let rows = EventRepository::new(&h.engine.db)
        .load(&session)
        .await
        .unwrap();
    assert!(
        rows.iter().any(|r| r.event_type == "repair_started"),
        "the repair must be in the event log"
    );
}

#[tokio::test]
async fn interrupted_orchestrated_run_resumes_mid_graph() {
    // Phase 1: understand + plan (2 nodes) + node n1 completes; node n2's
    // turn starts (its seed transcript persists) and then the model dies.
    let two_node_plan = r#"{"nodes":[
        {"id":"n1","kind":"edit","description":"add pub fn b"},
        {"id":"n2","kind":"edit","description":"add a FIXED marker","dependencies":["n1"]}]}"#;
    let h = harness(vec![
        text(REQUIREMENT_JSON),
        text(two_node_plan),
        tool_call("c1", "apply_patch", serde_json::json!({ "patch": PATCH })),
        text("added b"),
        // n2's turn: the model errors out (no more responses) mid-node.
    ])
    .await;
    let spec = spec(
        &h,
        VerificationPlan {
            commands: vec![passing_gate("ok")],
        },
    );
    let session = h.engine.create_task(&spec).await.unwrap();
    let err = h
        .engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .expect_err("n2 must die mid-run");
    assert!(err.to_string().contains("no more responses"), "{err}");

    // Phase 2: a fresh runtime scripted ONLY with n2's continuation — if the
    // resume wrongly re-ran understand/plan/n1 it would consume these
    // responses and fail loudly.
    let edit2 =
        "*** Begin Patch\n*** Update File: src/lib.rs\n pub fn b() {}\n+// FIXED\n*** End Patch";
    let engine2 = TaskEngine {
        db: h.engine.db.clone(),
        factory: ExecutorFactory {
            runtime: Arc::new(MockRuntime::new(vec![
                tool_call("c2", "apply_patch", serde_json::json!({ "patch": edit2 })),
                text("added marker"),
            ])),
            registry: Arc::new(default_registry()),
            tool_context: h.engine.factory.tool_context.clone(),
            model: ModelRef::new("mock", "m"),
            commit_co_author: true,
            overrides: Some(leveler_engine::ExecutionOverrides {
                completion_evidence: Some(false),
                ..Default::default()
            }),
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

    let report = engine2
        .resume(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(report.outcome, TaskOutcome::Verified);
    assert!(
        report.modified_files.contains(&"src/lib.rs".to_string()),
        "n1's files from before the interruption are carried over"
    );
    let content = std::fs::read_to_string(h.dir.path().join("src/lib.rs")).unwrap();
    assert!(content.contains("pub fn b"), "n1's work survived");
    assert!(
        content.contains("FIXED"),
        "n2 finished after resume: {content}"
    );

    // Turn history: n1 completed, n2 failed (the interruption), n2 completed
    // (the resume).
    let turns = TurnRepository::new(&h.engine.db)
        .list(&session)
        .await
        .unwrap();
    let summary: Vec<(String, String)> = turns
        .iter()
        .map(|t| (t.payload.clone().unwrap_or_default(), t.status.clone()))
        .collect();
    assert_eq!(
        summary,
        vec![
            (r#"{"node_id":"n1"}"#.to_string(), "completed".to_string()),
            (r#"{"node_id":"n2"}"#.to_string(), "failed".to_string()),
            (r#"{"node_id":"n2"}"#.to_string(), "completed".to_string()),
        ]
    );

    let (_, _, _, outcome) = SessionRepository::new(&h.engine.db)
        .execution(&session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome, Some(TaskOutcome::Verified));
}

/// Golden (K11 + proven-AC): understand JSON failure → fallback optional AC.
/// For implementation-class (Edit + mutation) that is **not** proven required
/// Met → CompletedUnverified. Empty required hints are demoted so mutation-
/// derived deletes can prove; content edits still cannot Verified without a
/// real executable required command.
#[tokio::test]
async fn fallback_optional_ac_allows_verified_required_unverifiable_blocks() {
    let gate = VerificationPlan {
        commands: vec![passing_gate("ok")],
    };

    // --- Path A: understand fails twice → Requirement::fallback (optional).
    // Impl-class edit + green gates cannot Verified without proven required AC.
    let h_fallback = harness(vec![
        text("not json"),
        text("still not json"),
        text(PLAN_JSON),
        tool_call("c1", "apply_patch", serde_json::json!({ "patch": PATCH })),
        text("Added function b."),
    ])
    .await;
    let session = h_fallback
        .engine
        .create_task(&spec(&h_fallback, gate.clone()))
        .await
        .unwrap();
    let report = h_fallback
        .engine
        .run(
            &session,
            &spec(&h_fallback, gate.clone()),
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        report.outcome,
        TaskOutcome::CompletedUnverified,
        "impl-class + fallback-only AC must not Verified"
    );
    let ledger = report
        .acceptance
        .expect("orchestrate emits acceptance ledger");
    assert_eq!(ledger.items.len(), 1);
    assert!(!ledger.items[0].required);
    assert_eq!(
        ledger.items[0].status,
        leveler_verifier::AcceptanceStatus::Unverifiable
    );
    assert!(ledger.all_required_met());
    assert!(!ledger.has_proven_required_met());

    // --- Path B: required AC with no verification_hint → demoted (no executable
    // required left); content edit cannot synthesize MUT-DEL → still unproven.
    let req_no_hint = r#"{"goal":"add function b","task_type":"feature",
        "acceptance_criteria":[{"id":"AC-1","description":"b exists","required":true}]}"#;
    let h_block = harness(vec![
        text(req_no_hint),
        text(PLAN_JSON),
        tool_call("c1", "apply_patch", serde_json::json!({ "patch": PATCH })),
        text("Added function b."),
    ])
    .await;
    let session = h_block
        .engine
        .create_task(&spec(&h_block, gate.clone()))
        .await
        .unwrap();
    let report = h_block
        .engine
        .run(
            &session,
            &spec(&h_block, gate),
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        report.outcome,
        TaskOutcome::CompletedUnverified,
        "empty required AC on content edit must not Verified"
    );
    let ledger = report
        .acceptance
        .expect("orchestrate emits acceptance ledger");
    // Empty required is demoted so it does not permanently Unverifiable-block
    // when mutation-derived deletes could otherwise prove; content edit has no
    // MUT-DEL, so proven Met is still false.
    assert!(!ledger.has_required_unverifiable());
    assert!(ledger.all_required_met());
    assert!(!ledger.has_proven_required_met());
}

/// K15: Edit + Answered + empty mutation fails the node before verify
/// (Node turns use goal_mode=false, so Answered is the normal stop).
#[tokio::test]
async fn orchestrate_edit_answered_without_mutation_is_failed() {
    let gate = VerificationPlan {
        commands: vec![passing_gate("ok")],
    };
    let req = r#"{"goal":"explain how auth works","task_type":"docs",
        "acceptance_criteria":[]}"#;
    let plan = r#"{"nodes":[{"id":"n1","kind":"edit",
        "description":"touch auth docs","allowed_paths":["src/lib.rs"]}]}"#;
    let h = harness(vec![
        text(req),
        text(plan),
        text("Looks fine; no code change needed."),
    ])
    .await;
    let mut s = spec(&h, gate);
    s.goal = "explain how auth works".into();
    let session = h.engine.create_task(&s).await.unwrap();
    let report = h
        .engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();
    assert!(
        report.modified_files.is_empty(),
        "fixture must not mutate: {:?}",
        report.modified_files
    );
    assert_eq!(
        report.outcome,
        TaskOutcome::Failed,
        "Edit Answered with no modified_files must fail at node (K15)"
    );
    assert!(
        report.final_text.contains("edit_answered_without_mutation")
            || report.final_text.contains("no modified files"),
        "failure detail should name K15: {}",
        report.final_text
    );
    // Never reached verify — no acceptance ledger.
    assert!(report.acceptance.is_none());
    assert!(report.verification.is_none());
}

/// Read-only orchestrate graph (Inspect only) + non-impl goal may Verified
/// without mutation when gates are green (needs_mutation = false).
#[tokio::test]
async fn orchestrate_inspect_only_may_verify_without_mutation() {
    let gate = VerificationPlan {
        commands: vec![passing_gate("ok")],
    };
    let req = r#"{"goal":"explain how auth works","task_type":"docs",
        "acceptance_criteria":[]}"#;
    let plan = r#"{"nodes":[{"id":"n1","kind":"inspect",
        "description":"read auth module"}]}"#;
    let h = harness(vec![
        text(req),
        text(plan),
        text("Auth uses JWT in middleware."),
    ])
    .await;
    let mut s = spec(&h, gate);
    s.goal = "explain how auth works".into();
    let session = h.engine.create_task(&s).await.unwrap();
    let report = h
        .engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();
    assert!(report.modified_files.is_empty());
    assert_eq!(
        report.outcome,
        TaskOutcome::Verified,
        "inspect-only + non-impl goal should not require mutation"
    );
}
