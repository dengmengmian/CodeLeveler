//! MA-RT — Multi-Agent Restart Truth Hardening gates.
//!
//! The durable record (`SubAgentStarted` without `SubAgentFinished`) is the
//! truth about a child; process-local state is not. These tests seed the
//! exact wreckage an unclean process exit leaves behind and prove the next
//! window reconciles it truthfully: a ghost Worker becomes blocking debt that
//! denies Verified, a durably finished child is never re-classified as lost,
//! and a re-delivered settlement happens exactly once.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use leveler_agent::{AutoClarify, WorkProfile};
use leveler_core::{RequestId, SessionId, ToolCallId, TurnId};
use leveler_engine::{
    EngineEvent, EventLog, ExecutionKind, ExecutorFactory, TaskEngine, TaskOutcome, TaskSpec,
};
use leveler_execution::{
    ApprovalDecision, ApprovalRequest, Approver, PermissionProfile, Workspace,
};
use leveler_model::{
    ContentPart, FinishReason, Message, ModelError, ModelEventStream, ModelProfile, ModelRef,
    ModelRequest, ModelResponse, ModelRuntime, Role, TokenUsage, ToolCall,
};
use leveler_storage::{Database, EventRepository, TurnRepository};
use leveler_tools::{ToolContext, default_registry};
use leveler_verifier::{CheckKind, VerificationCommand, VerificationPlan};

// ── mock model ───────────────────────────────────────────────────────────────

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
        request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        // Completion Reconciliation Gate calls are answered out of band so
        // scripted FIFOs and request-count assertions stay about the loop.
        if let Some(reply) = leveler_test_support::reconcile_autopilot(&request) {
            return Ok(reply);
        }
        self.responses.lock().unwrap().pop_front().ok_or_else(|| {
            ModelError::new(leveler_model::ModelErrorKind::Other, "no more responses")
        })
    }

    async fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
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
            }
        }))
        .unwrap())
    }
}

struct AutoApprove;

#[async_trait]
impl Approver for AutoApprove {
    async fn decide(&self, _request: &ApprovalRequest) -> ApprovalDecision {
        ApprovalDecision::ApproveOnce
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

fn patch_call() -> ModelResponse {
    tool_call(
        "c1",
        "apply_patch",
        serde_json::json!({
            "patch": "*** Begin Patch\n*** Update File: src/lib.rs\n pub fn old() {}\n+pub fn added() {}\n*** End Patch"
        }),
    )
}

fn complete_call(id: &str) -> ModelResponse {
    tool_call(
        id,
        "update_goal",
        serde_json::json!({"status": "complete", "summary": "added the function"}),
    )
}

/// Understand JSON with a required AC that greps the patch fixture.
fn understand_met_required_ac() -> ModelResponse {
    let hint = if cfg!(windows) {
        "findstr \\\"pub fn added\\\" src\\\\lib.rs".to_string()
    } else {
        "grep -q 'pub fn added' src/lib.rs".to_string()
    };
    text(&format!(
        r#"{{"goal":"add a function","task_type":"feature","constraints":[],
        "acceptance_criteria":[{{"id":"AC-1","description":"added() exists",
        "verification_hint":"{hint}","required":true}}],
        "out_of_scope":[],"risk":"low","uncertainties":[]}}"#
    ))
}

// ── harness ──────────────────────────────────────────────────────────────────

fn engine_on(db: &Database, dir: &Path, responses: Vec<ModelResponse>) -> TaskEngine {
    let workspace = Workspace::new(dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    TaskEngine {
        stores: leveler_storage::EngineStores::from_database(db),
        runtime_id: leveler_core::RuntimeId::new("rt-test"),
        factory: ExecutorFactory {
            runtime: Arc::new(MockRuntime::new(responses)),
            registry: Arc::new(default_registry()),
            tool_context,
            model: ModelRef::new("mock", "m"),
            commit_co_author: true,
            overrides: None,
            work_profile: WorkProfile::Balanced,
            memory_index: String::new(),
            permission_rules: leveler_execution::PermissionRuleSet::default(),
            permission_rules_path: None,
            hook_runner: leveler_execution::HookRunner::empty(std::path::PathBuf::from(".")),
            grants_state_dir: None,
            steering: None,
            allow_delegation: true,
            independent_review: leveler_engine::IndependentReviewPolicy::Auto,
            completion_judge_model: None,
        },
        approver: Arc::new(AutoApprove),
        clarifier: Arc::new(AutoClarify),
        supervisor: None,
    }
}

fn workspace_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/lib.rs"), "pub fn old() {}\n").unwrap();
    dir
}

fn gated_spec(dir: &Path) -> TaskSpec {
    let (program, args) = if cfg!(windows) {
        (
            "cmd".to_string(),
            vec!["/c".to_string(), "exit 0".to_string()],
        )
    } else {
        ("true".to_string(), Vec::new())
    };
    TaskSpec {
        runtime: leveler_engine::RuntimeTaskSpec {
            goal: "add a function".to_string(),
            kind: ExecutionKind::Direct,
            continuation: leveler_agent::ContinuationPolicy::UntilTerminal,
            limits: leveler_agent::StepLimits::default(),
        },
        coding: leveler_engine::CodingTaskSpec {
            repository: dir.to_path_buf(),
            mode: PermissionProfile::Assisted,
            sandbox: false,
            verification: VerificationPlan {
                commands: vec![VerificationCommand {
                    name: "ok".into(),
                    program,
                    args,
                    kind: CheckKind::Test,
                    gating: true,
                    timeout_seconds: 30,
                    scope_policy: Default::default(),
                }],
            },
            base_commit: None,
        },
    }
}

/// Seed the wreckage of a dead window: a turn that was running when the
/// process died, with a durably started child that never finished.
async fn seed_ghost_child(
    db: &Database,
    session: &SessionId,
    id: &str,
    nickname: &str,
    role: &str,
) -> TurnId {
    let turn = TurnRepository::new(db)
        .start(session, "user", None, leveler_core::now())
        .await
        .unwrap();
    let turn_id = TurnId::new(turn.id);
    let log = EventLog::new(db, session.clone());
    log.append(
        Some(&turn_id),
        EngineEvent::SubAgentStarted {
            id: id.to_string(),
            nickname: nickname.to_string(),
            role: role.to_string(),
            task: "fix the parser module".to_string(),
            profile_id: None,
            profile_role: None,
            capabilities: Vec::new(),
        },
        &mut |_| {},
    )
    .await
    .unwrap();
    turn_id
}

async fn event_rows(db: &Database, session: &SessionId) -> Vec<(Option<String>, EngineEvent)> {
    EventRepository::new(db)
        .load(session)
        .await
        .unwrap()
        .iter()
        .map(|row| {
            (
                row.turn_id.clone(),
                EngineEvent::from_payload(&row.payload).unwrap(),
            )
        })
        .collect()
}

/// The last persisted evidence ledger, replayed from durable events only.
fn last_ledger(
    events: &[(Option<String>, EngineEvent)],
) -> Option<leveler_lifecycle::EvidenceLedger> {
    events.iter().rev().find_map(|(_, e)| match e {
        EngineEvent::EvidenceLedgerUpdated { ledger } => Some(ledger.clone()),
        _ => None,
    })
}

// ── MA-RT-2: restart ghost completion truth ──────────────────────────────────

/// Control anchor: without a ghost this exact script legitimately reaches
/// Verified. The treatment test below differs by ONE seeded fact.
#[tokio::test]
async fn control_the_same_script_reaches_verified_without_a_ghost() {
    let dir = workspace_dir();
    let db = Database::connect_in_memory().await.unwrap();
    let engine = engine_on(
        &db,
        dir.path(),
        vec![
            patch_call(),
            complete_call("g1"),
            understand_met_required_ac(),
        ],
    );
    let spec = gated_spec(dir.path());
    let session = engine.create_task(&spec).await.unwrap();
    let report = engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(report.outcome, TaskOutcome::Verified);
}

/// MA_RT_GHOST_WORKER_INCOMPLETE + MA_RT_GHOST_WORKER_BLOCKING_DEBT +
/// MA_RT_GHOST_PREVENTS_VERIFIED + MA_RT_GHOST_FINISH_TURN_ATTRIBUTION —
/// a durably started Worker whose activation died must be reconciled into an
/// incomplete terminal plus open blocking debt, and the parent must not reach
/// Verified past it.
#[tokio::test]
async fn a_ghost_worker_from_a_dead_window_denies_verified_and_leaves_debt() {
    let dir = workspace_dir();
    let db = Database::connect_in_memory().await.unwrap();
    let mut script = vec![patch_call(), complete_call("g1")];
    // After the refusal the drive keeps the round loop and closeout going;
    // pad with honest stop replies — none of them can prove the AC, so any
    // Verified here could only come from ignoring the debt.
    for _ in 0..8 {
        script.push(text(
            "the completion was refused over the lost worker; stopping",
        ));
    }
    let engine = engine_on(&db, dir.path(), script);
    let spec = gated_spec(dir.path());
    let session = engine.create_task(&spec).await.unwrap();
    let origin_turn = seed_ghost_child(&db, &session, "agent-1", "wren", "worker").await;

    let report = engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    assert_ne!(
        report.outcome,
        TaskOutcome::Verified,
        "a lost Worker is unresolved original-goal debt; Verified past it is a false claim"
    );

    let events = event_rows(&db, &session).await;
    let ghost_finish = events
        .iter()
        .find(|(_, e)| matches!(e, EngineEvent::SubAgentFinished { id, .. } if id == "agent-1"))
        .expect("the ghost must be settled with a durable terminal, not left running");
    assert_eq!(
        ghost_finish.0.as_deref(),
        Some(origin_turn.as_str()),
        "the synthetic terminal must be attributed to the turn the child started in"
    );
    let (_, EngineEvent::SubAgentFinished { ok, .. }) = ghost_finish else {
        unreachable!()
    };
    assert!(!ok, "a child that never reported cannot be recorded as ok");

    let refused = events.iter().any(|(_, e)| {
        matches!(
            e,
            EngineEvent::ToolCallFinished {
                name,
                is_error: true,
                preview,
                ..
            } if name == "update_goal" && preview.contains("blocking")
        )
    });
    assert!(
        refused,
        "the completion attempt must be durably refused over the blocking debt"
    );

    let ledger = last_ledger(&events).expect("reconciliation must persist the debt ledger");
    let debt: Vec<_> = ledger
        .findings
        .iter()
        .filter(|f| f.source_child == "agent-1" && f.open_blocking())
        .collect();
    assert_eq!(
        debt.len(),
        1,
        "exactly one open blocking finding must represent the lost Worker's unfinished scope"
    );
}

/// Truth must converge, not dead-end: the parent settles the ghost debt with
/// resolve_finding (it finished the work itself) and Verified is legitimate
/// again.
#[tokio::test]
async fn resolving_the_ghost_debt_restores_verified() {
    let dir = workspace_dir();
    let db = Database::connect_in_memory().await.unwrap();
    let engine = engine_on(
        &db,
        dir.path(),
        vec![
            patch_call(),
            tool_call(
                "r1",
                "resolve_finding",
                serde_json::json!({
                    "id": "f-1",
                    "resolution": "rejected",
                    "reason": "the worker was lost in a restart; I completed the parser work myself"
                }),
            ),
            complete_call("g1"),
            understand_met_required_ac(),
        ],
    );
    let spec = gated_spec(dir.path());
    let session = engine.create_task(&spec).await.unwrap();
    seed_ghost_child(&db, &session, "agent-1", "wren", "worker").await;

    let report = engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        report.outcome,
        TaskOutcome::Verified,
        "settled debt must not keep blocking: the gate is truth, not punishment"
    );
}

// ── MA-RT-3: settlement / restart truth ──────────────────────────────────────

async fn append_event(db: &Database, session: &SessionId, turn: Option<&TurnId>, e: EngineEvent) {
    EventLog::new(db, session.clone())
        .append(turn, e, &mut |_| {})
        .await
        .unwrap();
}

fn padded(mut script: Vec<ModelResponse>) -> Vec<ModelResponse> {
    for _ in 0..14 {
        script.push(text("stopping"));
    }
    script
}

fn transcript(payloads: &[String]) -> Vec<Message> {
    payloads
        .iter()
        .map(|p| serde_json::from_str(p).unwrap())
        .collect()
}

/// MA_RT_TERMINAL_FIRST_WINS + MA_RT_FINISHED_NOT_RECLASSIFIED_LOST +
/// MA_RT_LOST_NOTE_TRUTH + MA_RT_C10 + MA_RT_C11 — a child whose terminal fact
/// IS durable but whose settlement the dead window may not have consumed is
/// re-delivered, exactly once, and never described as lost; restart must not
/// downgrade a terminal fact.
#[tokio::test]
async fn a_durably_finished_child_is_redelivered_not_reclassified_as_lost() {
    let dir = workspace_dir();
    let db = Database::connect_in_memory().await.unwrap();
    let engine = engine_on(
        &db,
        dir.path(),
        padded(vec![text(
            "integrating the re-delivered explorer result; nothing further",
        )]),
    );
    let spec = gated_spec(dir.path());
    let session = engine.create_task(&spec).await.unwrap();

    // The wreckage: the child durably finished, but the crash landed between
    // that terminal fact and the ProgressUpdated that would have cleared the
    // outstanding record (C10's exact window).
    let turn = TurnRepository::new(&db)
        .start(&session, "user", None, leveler_core::now())
        .await
        .unwrap();
    let t1 = TurnId::new(turn.id);
    append_event(
        &db,
        &session,
        Some(&t1),
        EngineEvent::SubAgentStarted {
            id: "agent-1".into(),
            nickname: "finch".into(),
            role: "explorer".into(),
            task: "map the parser".into(),
            profile_id: None,
            profile_role: None,
            capabilities: Vec::new(),
        },
    )
    .await;
    append_event(
        &db,
        &session,
        Some(&t1),
        EngineEvent::SubAgentFinished {
            id: "agent-1".into(),
            nickname: "finch".into(),
            ok: true,
            contribution: None,
            summary: "explored the parser: three modules, no defects".into(),
        },
    )
    .await;
    let stale = leveler_lifecycle::ProgressLedger {
        outstanding_children: vec!["agent-1|finch|explorer|".to_string()],
        ..Default::default()
    };
    append_event(
        &db,
        &session,
        Some(&t1),
        EngineEvent::ProgressUpdated { ledger: stale },
    )
    .await;

    engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    let events = event_rows(&db, &session).await;
    let finishes: Vec<_> = events
        .iter()
        .filter(|(_, e)| matches!(e, EngineEvent::SubAgentFinished { id, .. } if id == "agent-1"))
        .collect();
    assert_eq!(
        finishes.len(),
        1,
        "first terminal fact wins: reconciliation must not write a second, contradictory \
         terminal for a finished child"
    );
    if let Some(ledger) = last_ledger(&events) {
        assert!(
            !ledger
                .findings
                .iter()
                .any(|f| f.source_child == "agent-1" && f.open_blocking()),
            "a finished child is not debt"
        );
    }

    let messages = transcript(
        &leveler_storage::MessageRepository::new(&db)
            .load(&session)
            .await
            .unwrap(),
    );
    let texts: Vec<String> = messages
        .iter()
        .flat_map(|m| {
            m.content.iter().filter_map(|p| match p {
                ContentPart::Text { text } => Some(text.clone()),
                _ => None,
            })
        })
        .collect();
    assert!(
        !texts.iter().any(|t| t.contains("Delegations lost")),
        "a durably finished child must never be reported as lost"
    );
    let redelivered: Vec<_> = texts
        .iter()
        .filter(|t| t.contains("re-delivered after restart"))
        .collect();
    assert_eq!(
        redelivered.len(),
        1,
        "the recorded settlement must be re-delivered exactly once"
    );
    assert!(
        redelivered[0].contains("finch") && redelivered[0].contains("no defects"),
        "the re-delivery must carry the recorded outcome: {}",
        redelivered[0]
    );
    let last_progress = events
        .iter()
        .rev()
        .find_map(|(_, e)| match e {
            EngineEvent::ProgressUpdated { ledger } => Some(ledger.clone()),
            _ => None,
        })
        .expect("the pruned outstanding record must be persisted (the consumed mark)");
    assert!(
        last_progress.outstanding_children.is_empty(),
        "consuming the re-delivery must durably clear the outstanding record"
    );

    // C11: a second window sees the consumed mark and re-delivers nothing.
    let engine2 = engine_on(&db, dir.path(), padded(vec![text("nothing left")]));
    engine2
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();
    let texts2: Vec<String> = transcript(
        &leveler_storage::MessageRepository::new(&db)
            .load(&session)
            .await
            .unwrap(),
    )
    .iter()
    .flat_map(|m| {
        m.content.iter().filter_map(|p| match p {
            ContentPart::Text { text } => Some(text.clone()),
            _ => None,
        })
    })
    .collect();
    assert_eq!(
        texts2
            .iter()
            .filter(|t| t.contains("re-delivered after restart"))
            .count(),
        1,
        "re-delivery is once per settlement, not once per window"
    );
    let finishes2 = event_rows(&db, &session)
        .await
        .iter()
        .filter(|(_, e)| matches!(e, EngineEvent::SubAgentFinished { id, .. } if id == "agent-1"))
        .count();
    assert_eq!(finishes2, 1, "still exactly one terminal fact");
}

/// MA_RT_C9 + MA_RT_FINDING_ADOPTION_IDEMPOTENT — a ghost whose findings were
/// already durably adopted keeps them: the synthetic terminal carries a
/// projection over them instead of contradicting them, the original finding is
/// not duplicated, and reconciling twice changes nothing.
#[tokio::test]
async fn a_ghost_with_adopted_findings_keeps_them_in_its_terminal() {
    let dir = workspace_dir();
    let db = Database::connect_in_memory().await.unwrap();
    let engine = engine_on(&db, dir.path(), padded(vec![text("noted; stopping")]));
    let spec = gated_spec(dir.path());
    let session = engine.create_task(&spec).await.unwrap();

    let origin = seed_ghost_child(&db, &session, "agent-1", "wren", "worker").await;
    let mut ledger = leveler_lifecycle::EvidenceLedger::default();
    ledger.findings.push(leveler_lifecycle::FindingRecord {
        id: "f-9".into(),
        source_child: "agent-1".into(),
        role: "worker".into(),
        kind: leveler_lifecycle::FindingKind::Observation,
        summary: "parser drops trailing comments".into(),
        file: None,
        symbol: None,
        blocking: false,
        state: leveler_lifecycle::FindingState::Acknowledged,
        resolution_reason: None,
    });
    append_event(
        &db,
        &session,
        Some(&origin),
        EngineEvent::EvidenceLedgerUpdated { ledger },
    )
    .await;

    engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    let events = event_rows(&db, &session).await;
    let (
        _,
        EngineEvent::SubAgentFinished {
            contribution,
            summary,
            ..
        },
    ) = events
        .iter()
        .find(|(_, e)| matches!(e, EngineEvent::SubAgentFinished { id, .. } if id == "agent-1"))
        .expect("the ghost must be settled")
    else {
        unreachable!()
    };
    let projection = contribution
        .as_ref()
        .expect("durable findings must surface in the terminal projection, not vanish");
    assert!(
        projection.findings_total >= 1,
        "the projection must count the preserved finding"
    );
    assert!(
        summary.contains("remain adopted"),
        "the terminal must say the earlier findings still stand: {summary}"
    );

    let ledger = last_ledger(&events).unwrap();
    assert_eq!(
        ledger
            .findings
            .iter()
            .filter(|f| f.id == "f-9" || f.summary.contains("trailing comments"))
            .count(),
        1,
        "the adopted finding must survive exactly once — neither erased nor duplicated"
    );
    assert_eq!(
        ledger
            .findings
            .iter()
            .filter(|f| f.source_child == "agent-1" && f.open_blocking())
            .count(),
        1,
        "the lost Worker's debt joins the preserved finding"
    );

    // Reconcile again (second window): nothing changes.
    let engine2 = engine_on(&db, dir.path(), padded(Vec::new()));
    engine2
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();
    let events = event_rows(&db, &session).await;
    assert_eq!(
        events
            .iter()
            .filter(
                |(_, e)| matches!(e, EngineEvent::SubAgentFinished { id, .. } if id == "agent-1")
            )
            .count(),
        1,
        "reconciliation is idempotent: one terminal per child"
    );
    let ledger = last_ledger(&events).unwrap();
    assert_eq!(
        ledger
            .findings
            .iter()
            .filter(|f| f.source_child == "agent-1" && f.open_blocking())
            .count(),
        1,
        "reconciliation is idempotent: one debt finding per lost Worker"
    );
}

// ── MA-RT-2 across a real process boundary (file-backed, §46) ────────────────

/// MA_RT_RESTART_OUTSTANDING_RECONCILIATION — the same ghost truth must hold
/// when the second window is a genuinely fresh process image: new Database
/// handle over the same file, nothing in memory carried over.
#[tokio::test]
async fn ghost_reconciliation_survives_a_real_database_reopen() {
    let dir = workspace_dir();
    let state = tempfile::tempdir().unwrap();
    let db_path = state.path().join("leveler.db");
    let spec = gated_spec(dir.path());

    // Window one: the process that created the task and started the worker.
    let session = {
        let db = Database::connect(&db_path).await.unwrap();
        let engine = engine_on(&db, dir.path(), Vec::new());
        let session = engine.create_task(&spec).await.unwrap();
        seed_ghost_child(&db, &session, "agent-1", "wren", "worker").await;
        session
        // db dropped here — the "process" dies.
    };

    // Window two: a fresh connection, as after a daemon restart.
    let db = Database::connect(&db_path).await.unwrap();
    let mut script = vec![patch_call(), complete_call("g1")];
    for _ in 0..8 {
        script.push(text("refused over the lost worker; stopping"));
    }
    let engine = engine_on(&db, dir.path(), script);
    let report = engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    assert_ne!(
        report.outcome,
        TaskOutcome::Verified,
        "a restart must not launder a lost Worker into a verified closure"
    );
    let events = event_rows(&db, &session).await;
    assert!(
        events.iter().any(
            |(_, e)| matches!(e, EngineEvent::SubAgentFinished { id, ok: false, .. } if id == "agent-1")
        ),
        "the ghost must be durably settled after the reopen"
    );
    let ledger = last_ledger(&events).expect("debt must be persisted");
    assert_eq!(
        ledger
            .findings
            .iter()
            .filter(|f| f.source_child == "agent-1" && f.open_blocking())
            .count(),
        1,
        "the durable record alone must be enough to reconstruct the Worker debt"
    );
}

// ── MA-RT-1: the durable total cap across a restart ──────────────────────────

/// MA_RT_TOTAL_CAP_ACROSS_RESTART — a window that starts from a persisted
/// ledger with the quota consumed refuses the next spawn; the durable count
/// survives where the drive-local counter used to reset.
#[tokio::test]
async fn the_total_child_cap_survives_a_restart() {
    let dir = workspace_dir();
    let db = Database::connect_in_memory().await.unwrap();
    let engine = engine_on(
        &db,
        dir.path(),
        padded(vec![
            tool_call(
                "s1",
                "spawn_agent",
                serde_json::json!({
                    "task": "explore the parser",
                    "role": "explorer",
                    "run_in_background": false
                }),
            ),
            text("the spawn was refused; doing the work directly"),
        ]),
    );
    let spec = gated_spec(dir.path());
    let session = engine.create_task(&spec).await.unwrap();

    let turn = TurnRepository::new(&db)
        .start(&session, "user", None, leveler_core::now())
        .await
        .unwrap();
    let t1 = TurnId::new(turn.id);
    let consumed = leveler_lifecycle::ProgressLedger {
        children_spawned_total: 6,
        ..Default::default()
    };
    append_event(
        &db,
        &session,
        Some(&t1),
        EngineEvent::ProgressUpdated { ledger: consumed },
    )
    .await;

    engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    let events = event_rows(&db, &session).await;
    assert!(
        !events
            .iter()
            .any(|(_, e)| matches!(e, EngineEvent::SubAgentStarted { .. })),
        "the durable quota is spent: no seventh child may start"
    );
    assert!(
        events.iter().any(|(_, e)| matches!(
            e,
            EngineEvent::ToolCallFinished {
                name,
                is_error: true,
                preview,
                ..
            } if name == "spawn_agent" && preview.contains("limit reached")
        )),
        "the refusal must be explicit and durable, naming the limit"
    );
}

// ── MA-RT-4: lifecycle turn attribution ──────────────────────────────────────

/// MA_RT_NORMAL_CHILD_TURN_ATTRIBUTION — a child spawned and settled in the
/// normal flow has both lifecycle events attributed to the turn that ran it.
#[tokio::test]
async fn a_normally_settled_child_is_attributed_to_its_turn() {
    let dir = workspace_dir();
    let db = Database::connect_in_memory().await.unwrap();
    let engine = engine_on(
        &db,
        dir.path(),
        padded(vec![
            tool_call(
                "s1",
                "spawn_agent",
                serde_json::json!({
                    "task": "explore the parser",
                    "role": "explorer",
                    "run_in_background": false
                }),
            ),
            text("child report: parser has three modules"),
            text("synthesis done; stopping"),
        ]),
    );
    let spec = gated_spec(dir.path());
    let session = engine.create_task(&spec).await.unwrap();
    engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    let events = event_rows(&db, &session).await;
    let started = events
        .iter()
        .find(|(_, e)| matches!(e, EngineEvent::SubAgentStarted { .. }))
        .expect("the explorer must have started");
    let finished = events
        .iter()
        .find(|(_, e)| matches!(e, EngineEvent::SubAgentFinished { .. }))
        .expect("the explorer must have settled");
    assert!(
        started.0.is_some(),
        "a normally spawned child's start belongs to the turn that ran it"
    );
    assert_eq!(
        started.0, finished.0,
        "start and terminal must be attributed to the same turn"
    );
}

/// MA_RT_REVIEWER_TURN_ATTRIBUTION — the harness-owned reviewer is not a turn
/// (no turn row exists for it), so `turn_id = NULL` on both lifecycle events
/// is the truthful attribution; the pair must agree, and the start must be
/// durable (it is appended and awaited before the reviewer executes).
#[tokio::test]
async fn reviewer_lifecycle_events_share_truthful_null_attribution() {
    let dir = workspace_dir();
    let db = Database::connect_in_memory().await.unwrap();
    let engine = engine_on(
        &db,
        dir.path(),
        padded(vec![
            tool_call(
                "c1",
                "apply_patch",
                serde_json::json!({
                    "patch": "*** Begin Patch\n*** Add File: src/auth.rs\n+pub fn login() {}\n*** End Patch"
                }),
            ),
            complete_call("g1"),
            // Reviewer child rounds.
            text("reviewed src/auth.rs: nothing to flag"),
            text("reviewed src/auth.rs: nothing to flag"),
        ]),
    );
    let spec = gated_spec(dir.path());
    let session = engine.create_task(&spec).await.unwrap();
    engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    let events = event_rows(&db, &session).await;
    let reviewer_started = events
        .iter()
        .find(|(_, e)| matches!(e, EngineEvent::SubAgentStarted { role, .. } if role == "reviewer"))
        .expect("the closure review must have launched a reviewer");
    let (_, EngineEvent::SubAgentStarted { id, .. }) = reviewer_started else {
        unreachable!()
    };
    let reviewer_finished = events
        .iter()
        .find(|(_, e)| matches!(e, EngineEvent::SubAgentFinished { id: fid, .. } if fid == id))
        .expect("the reviewer must settle durably");
    assert_eq!(
        reviewer_started.0, None,
        "the reviewer belongs to no turn; a fabricated turn id would be false provenance"
    );
    assert_eq!(
        reviewer_finished.0, None,
        "start and terminal must agree on the (null) attribution"
    );
}
