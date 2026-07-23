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
        // findstr parses `/` in the file argument as option switches.
        (
            "cmd",
            vec![
                "/c".to_string(),
                format!("findstr {needle} {}", file.replace('/', "\\")),
            ],
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
        base_commit: None,
    }
}

// Required AC with a real check command — empty/missing hints are Unverifiable
// and block Verified (K2). The grep is non-trivial so acceptance can Met; the
// hint runs through the platform's shell (`sh -c` / `cmd /c`).
fn requirement_json() -> String {
    let hint = grep_hint("pub fn b", "src/lib.rs");
    format!(
        r#"{{"goal":"add function b","task_type":"feature",
    "acceptance_criteria":[{{"id":"AC-1","description":"b exists",
    "verification_hint":"{hint}","required":true}}]}}"#
    )
}

/// `grep`-style acceptance hint for the platform's shell (`sh -c` on Unix,
/// `cmd /c` on Windows), already JSON-escaped for the understand fixture.
fn grep_hint(needle: &str, file: &str) -> String {
    if cfg!(windows) {
        // findstr parses `/` in the file argument as option switches, and the
        // JSON fixture escapes both the quotes and the path backslashes.
        format!("findstr \\\"{needle}\\\" {}", file.replace('/', "\\\\"))
    } else {
        format!("grep -q '{needle}' {file}")
    }
}
const PLAN_JSON: &str = r#"{"nodes":[{"id":"n1","kind":"edit",
    "description":"add pub fn b to src/lib.rs","allowed_paths":["src/lib.rs"]}]}"#;
const PATCH: &str =
    "*** Begin Patch\n*** Update File: src/lib.rs\n pub fn a() {}\n+pub fn b() {}\n*** End Patch";

#[tokio::test]
async fn orchestrated_run_persists_every_node_as_a_turn() {
    let h = harness(vec![
        text(&requirement_json()), // understand
        text(PLAN_JSON),           // plan
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
        text(&requirement_json()),
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
    let hint = grep_hint("FIXED", "src/lib.rs");
    let h = harness(vec![
        text(&format!(
            r#"{{"goal":"add b and marker","task_type":"feature",
            "acceptance_criteria":[{{"id":"AC-1","description":"FIXED marker present",
            "verification_hint":"{hint}","required":true}}]}}"#
        )),
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
        text(&requirement_json()),
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

/// Golden (K11): understand JSON failure → fallback optional AC, and empty
/// required hints are demoted rather than left permanently Unverifiable.
///
/// The ledger-shaping behaviour asserted here is unchanged. What changed is the
/// verdict: an unproven ledger no longer downgrades a green gate, so both paths
/// now finish Verified. A model that cannot restate its own goal as criteria
/// has said nothing about whether the code is correct — the gate has.
#[tokio::test]
async fn fallback_and_demoted_acceptance_do_not_block_a_green_gate() {
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
        TaskOutcome::Verified,
        "fallback-only AC must not downgrade a passing gate"
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
        TaskOutcome::Verified,
        "a demoted empty required AC must not downgrade a passing gate"
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

/// Wraps the scripted runtime and records every request it receives, so tests
/// can assert what the resumed node actually showed the model.
struct RecordingRuntime {
    inner: MockRuntime,
    requests: Mutex<Vec<ModelRequest>>,
}

#[async_trait]
impl ModelRuntime for RecordingRuntime {
    async fn generate(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        self.requests.lock().unwrap().push(request.clone());
        self.inner.generate(request, cancellation).await
    }

    async fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        self.requests.lock().unwrap().push(request.clone());
        self.inner.stream(request, cancellation).await
    }

    async fn profile(&self, model: &ModelRef) -> Result<ModelProfile, ModelError> {
        self.inner.profile(model).await
    }
}

/// Delays each `stream` call by the next queued duration (then no delay), so
/// tests can burn a node's wall-clock budget deterministically.
struct SlowRuntime {
    inner: MockRuntime,
    delays: Mutex<VecDeque<std::time::Duration>>,
}

#[async_trait]
impl ModelRuntime for SlowRuntime {
    async fn generate(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        let delay = self.delays.lock().unwrap().pop_front().unwrap_or_default();
        tokio::time::sleep(delay).await;
        self.inner.generate(request, cancellation).await
    }

    async fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        let delay = self.delays.lock().unwrap().pop_front().unwrap_or_default();
        tokio::time::sleep(delay).await;
        self.inner.stream(request, cancellation).await
    }

    async fn profile(&self, model: &ModelRef) -> Result<ModelProfile, ModelError> {
        self.inner.profile(model).await
    }
}

/// `harness` with a caller-supplied runtime instead of a plain MockRuntime.
async fn harness_with_runtime(runtime: Arc<dyn ModelRuntime>) -> Harness {
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
            runtime,
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

/// An in-flight node whose turn wrote a ContextSnapshot (mid-node compaction)
/// and then kept working must resume from snapshot + everything after it — the
/// snapshot is a compact BASE, never a replacement for the later rounds. If the
/// tail were dropped, the model would re-do work whose side effects (edits,
/// commands) already happened before the crash.
#[tokio::test]
async fn resume_of_in_flight_node_keeps_messages_appended_after_snapshot() {
    use leveler_core::TurnId;
    use leveler_engine::EventLog;

    // Phase 1: two-node plan; n1 completes; n2 starts (seed transcript
    // persists) and dies mid-node.
    let two_node_plan = r#"{"nodes":[
        {"id":"n1","kind":"edit","description":"add pub fn b"},
        {"id":"n2","kind":"edit","description":"add a FIXED marker","dependencies":["n1"]}]}"#;
    let h = harness(vec![
        text(&requirement_json()),
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
    h.engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .expect_err("n2 must die mid-run");

    // n2's failed turn is the in-flight one the resume will re-drive.
    let turns = TurnRepository::new(&h.engine.db)
        .list(&session)
        .await
        .unwrap();
    let n2_turn = turns
        .iter()
        .find(|t| t.payload.as_deref() == Some(r#"{"node_id":"n2"}"#) && t.status == "failed")
        .expect("n2's interrupted turn must be persisted");
    let n2_turn_id = TurnId::new(n2_turn.id.clone());

    // Simulate mid-node compaction followed by more work: a ContextSnapshot
    // for n2's turn (the compact base), then a raw message appended AFTER it.
    let log = EventLog::new(&h.engine.db, session.clone());
    log.append(
        Some(&n2_turn_id),
        EngineEvent::ContextSnapshot {
            messages: vec![Message::text(
                Role::User,
                "COMPACT_BASE_SUMMARY of the earlier n2 rounds",
            )],
        },
        &mut |_| {},
    )
    .await
    .unwrap();
    MessageRepository::new(&h.engine.db)
        .append_in_turn(
            &session,
            &n2_turn_id,
            &[serde_json::to_string(&Message::text(
                Role::User,
                "POST_SNAPSHOT_MARKER: the FIXED marker patch was already prepared",
            ))
            .unwrap()],
            leveler_core::now(),
        )
        .await
        .unwrap();

    // Phase 2: resume with a recording runtime scripted only with n2's
    // continuation.
    let edit2 =
        "*** Begin Patch\n*** Update File: src/lib.rs\n pub fn b() {}\n+// FIXED\n*** End Patch";
    let runtime = Arc::new(RecordingRuntime {
        inner: MockRuntime::new(vec![
            tool_call("c2", "apply_patch", serde_json::json!({ "patch": edit2 })),
            text("added marker"),
        ]),
        requests: Mutex::new(Vec::new()),
    });
    let engine2 = TaskEngine {
        db: h.engine.db.clone(),
        factory: ExecutorFactory {
            runtime: runtime.clone(),
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

    engine2
        .resume(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    let requests = runtime.requests.lock().unwrap();
    let first = requests
        .first()
        .expect("the resumed node must issue a model request");
    let joined: String = first
        .messages
        .iter()
        .map(|m| m.text_content())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("COMPACT_BASE_SUMMARY"),
        "resume must start from the compact snapshot base: {joined}"
    );
    assert!(
        joined.contains("POST_SNAPSHOT_MARKER"),
        "resume must keep the raw messages appended after the snapshot, \
         not wholesale-replace them with the snapshot: {joined}"
    );
}

/// A node's declared wall-clock budget (`StepBudget.max_duration`) must reach
/// the executor as a `StepLimits.max_duration`, so a node stuck on a slow
/// model/command cannot run unbounded under `UntilTerminal`. (Rounds caps are
/// deliberately retired — see the twenty-round test above — but the wall clock
/// is the backstop.)
#[tokio::test]
async fn node_wall_clock_budget_reaches_the_executor() {
    // A 1s node budget with a >1s first model round: the boundary check after
    // that round must stop the node instead of letting it run its script dry.
    // (0 means unlimited, like the legacy caps.)
    let plan = r#"{"nodes":[{"id":"n1","kind":"inspect",
        "description":"budgeted node",
        "budget":{"max_tool_rounds":20,"max_modified_files":8,
                  "max_commands":10,"max_repairs":2,"max_duration":1}}]}"#;
    let inner = MockRuntime::new(vec![
        text(r#"{"goal":"inspect","acceptance_criteria":[]}"#),
        text(plan),
        tool_call(
            "p1",
            "update_plan",
            serde_json::json!({
                "explanation": "slow round",
                "plan": [{"step": "inspect", "status": "in_progress"}]
            }),
        ),
        // Never legitimately reached: the budget expires during round 1.
        text("node ran to completion"),
        text("still going"),
    ]);
    // Delay only the node's first round (call #3) past the 1s budget.
    let runtime = Arc::new(SlowRuntime {
        inner,
        delays: Mutex::new(VecDeque::from(vec![
            std::time::Duration::ZERO,
            std::time::Duration::ZERO,
            std::time::Duration::from_millis(1200),
        ])),
    });
    let h = harness_with_runtime(runtime).await;
    let mut s = spec(&h, VerificationPlan::default());
    s.goal = "inspect".into();
    let session = h.engine.create_task(&s).await.unwrap();

    let report = h
        .engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(
        report.stop_reason,
        leveler_agent::StopReason::BudgetExhausted,
        "an exhausted node wall-clock budget must stop the node: {report:?}"
    );
}
