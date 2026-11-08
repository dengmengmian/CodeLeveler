//! Real-process crash injection for the Phase 2B matrix.
//!
//! Each scenario runs the production engine (real SQLite, real turn runner,
//! real revision/recovery code) in a CHILD process of this test binary, with a
//! scripted provider and one closed-set failpoint. The failpoint is compiled
//! into debug builds only and requires an env var naming both the point and a
//! marker file, so a normal run can never reach it. The child writes the
//! marker, then sends itself SIGKILL — an actual process death with no
//! destructor, no flush and no recovery in that process.
//!
//! The parent then proves the cut from the marker + `WaitStatus`, opens the
//! same SQLite database, and runs the real recovery. Side effects are counted
//! through a file the probe tool appends to, never through a tool return value.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use leveler_agent::coding::{CodingRuntime, ExecutorFactory, TaskSpec};
use leveler_agent::{AutoClarify, StepLimits};
use leveler_core::SessionId;
use leveler_engine::{EngineBoot, EngineEvent, ExecutionKind};
use leveler_execution::{AutoApprove, PermissionProfile, Workspace};
use leveler_lifecycle::StopReason;
use leveler_model::{
    ContentPart, FinishReason, Message, ModelError, ModelEventStream, ModelProfile, ModelRef,
    ModelRequest, ModelResponse, ModelRuntime, Role, TokenUsage, ToolCall,
};
use leveler_storage::{
    Database, EngineStores, EventRepository, MessageRepository, ModelRequestRepository,
    SessionRecord, SessionRepository,
};
use leveler_tools::{RiskLevel, Tool, ToolContext, ToolError, ToolOutput};

const ANSWER: &str = "Chat answer delivered once.";
const SECOND: &str = "Second answer generated because no delivery was durable.";

// ── scripted provider ───────────────────────────────────────────────────────

#[derive(Clone)]
struct Runtime {
    responses: Arc<Mutex<VecDeque<ModelResponse>>>,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

impl Runtime {
    fn new(responses: Vec<ModelResponse>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(VecDeque::from(responses))),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

fn profile() -> ModelProfile {
    serde_json::from_value(serde_json::json!({
        "id": "m", "provider": "mock", "model_id": "m", "protocol": "openai_chat",
        "capabilities": {
            "streaming": true, "tool_calling": true, "parallel_tool_calls": false,
            "structured_output": false, "reasoning": false, "vision": false
        },
        "limits": {
            "context_window": 131072, "reliable_context": 65536,
            "max_output_tokens": 8192, "max_tool_schema_bytes": 32768,
            "max_parallel_tool_calls": 1
        }
    }))
    .unwrap()
}

#[async_trait]
impl ModelRuntime for Runtime {
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
        let response = self.generate(request, cancellation).await?;
        Ok(leveler_model::stream_from_response(response))
    }

    async fn profile(&self, _model: &ModelRef) -> Result<ModelProfile, ModelError> {
        Ok(profile())
    }
}

fn text(value: &str) -> ModelResponse {
    ModelResponse {
        request_id: leveler_core::RequestId::generate(),
        message: Message::text(Role::Assistant, value),
        finish_reason: FinishReason::Stop,
        usage: TokenUsage::default(),
    }
}

fn probe_call(id: &str) -> ModelResponse {
    ModelResponse {
        request_id: leveler_core::RequestId::generate(),
        message: Message {
            origin: None,
            role: Role::Assistant,
            content: vec![ContentPart::ToolCall {
                call: ToolCall {
                    id: leveler_core::ToolCallId::new(id),
                    name: "crash_probe".to_string(),
                    arguments: serde_json::json!({"note": "once"}),
                },
            }],
            route: None,
        },
        finish_reason: FinishReason::ToolCalls,
        usage: TokenUsage::default(),
    }
}

fn declared_answer() -> ModelResponse {
    ModelResponse {
        request_id: leveler_core::RequestId::generate(),
        message: Message {
            origin: None,
            role: Role::Assistant,
            content: vec![
                ContentPart::Text {
                    text: ANSWER.to_string(),
                },
                ContentPart::ToolCall {
                    call: ToolCall {
                        id: leveler_core::ToolCallId::new("g1"),
                        name: "update_goal".to_string(),
                        arguments: serde_json::json!({
                            "status": "complete",
                            "summary": "Goal complete.",
                            "deliver_answer": true,
                        }),
                    },
                },
            ],
            route: None,
        },
        finish_reason: FinishReason::ToolCalls,
        usage: TokenUsage::default(),
    }
}

// ── side-effect probe tool ──────────────────────────────────────────────────

/// Appends one line to the file named by `CL_CRASH_SIDE_EFFECT`, then reaches
/// the `during_tool_side_effect` failpoint. Counting is done by the parent
/// reading that file — never by this tool's return value.
struct CrashProbe;

#[async_trait]
impl Tool for CrashProbe {
    fn name(&self) -> &str {
        "crash_probe"
    }

    fn description(&self) -> &str {
        "Test-only synthetic side effect."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"note": {"type": "string"}},
            "additionalProperties": false
        })
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::WorkspaceWrite
    }

    fn mutates_files(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        _input: serde_json::Value,
        _context: ToolContext,
        _cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let path = std::env::var("CL_CRASH_SIDE_EFFECT")
            .map_err(|_| ToolError::Io("CL_CRASH_SIDE_EFFECT is not set".into()))?;
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|error| ToolError::Io(format!("open side-effect file: {error}")))?;
        writeln!(file, "side-effect")
            .map_err(|error| ToolError::Io(format!("write side-effect file: {error}")))?;
        file.sync_all()
            .map_err(|error| ToolError::Io(format!("sync side-effect file: {error}")))?;
        leveler_core::test_crash::point("during_tool_side_effect");
        Ok(ToolOutput::ok("probe recorded one side effect"))
    }
}

fn registry() -> leveler_tools::ToolRegistry {
    let mut registry = leveler_tools::default_registry();
    leveler_agent::register_harness_controls(&mut registry);
    registry.register(Arc::new(CrashProbe));
    registry
}

// ── fixture ─────────────────────────────────────────────────────────────────

fn coding_runtime(db: &Database, root: &Path, script: Vec<ModelResponse>) -> CodingRuntime {
    let stores = EngineStores::from_database(db);
    CodingRuntime {
        engine: leveler_engine::TaskEngine {
            stores,
            runtime_id: leveler_core::RuntimeId::new("crash-harness"),
            boot: EngineBoot {
                id: leveler_core::BootId::generate(),
                liveness: Arc::new(leveler_test_support::TestBoots::new()),
            },
        },
        factory: ExecutorFactory {
            tool_discovery: None,
            resource_grants: Arc::new(leveler_storage::MemoryResourceGrantStore::default()),
            runtime: Arc::new(Runtime::new(script)),
            registry: Arc::new(registry()),
            tool_context: ToolContext::new(
                Workspace::new(root).unwrap(),
                PermissionProfile::Assisted,
            ),
            model: ModelRef::new("mock", "m"),
            commit_co_author: true,
            overrides: None,
            background_tasks: Arc::new(leveler_execution::BackgroundTaskRegistry::new()),
            permission_rules: leveler_execution::PermissionRuleSet::default(),
            permission_rules_path: None,
            hook_runner: leveler_execution::HookRunner::empty(".".into()),
            steering: None,
            allow_delegation: true,
            allow_host_input: true,
            capabilities: None,
            independent_review: leveler_agent::coding::IndependentReviewPolicy::Off,
        },
        approver: Arc::new(AutoApprove),
        clarifier: Arc::new(AutoClarify),
        task_cancel: Arc::new(AtomicBool::new(false)),
    }
}

fn spec(root: &Path) -> TaskSpec {
    TaskSpec {
        runtime: leveler_agent::coding::RuntimeTaskSpec {
            goal: "run the crash matrix task".to_string(),
            kind: ExecutionKind::Direct,
            continuation: leveler_agent::ContinuationPolicy::bounded(6),
            limits: StepLimits::default(),
        },
        coding: leveler_agent::coding::CodingTaskSpec {
            repository: Some(root.into()),
            mode: PermissionProfile::Assisted,
            sandbox: false,
        },
    }
}

fn scenario_script(scenario: &str) -> (Vec<ModelResponse>, bool) {
    let goal = scenario.starts_with("goal_");
    let script = match scenario {
        "chat_before_delivery"
        | "chat_after_delivery"
        | "chat_after_turn_terminal"
        | "chat_after_task_terminal" => vec![text(ANSWER)],
        "goal_after_delivery" => vec![declared_answer()],
        "tool_after_started" => vec![probe_call("p1"), text(ANSWER)],
        "tool_during_effect" => vec![probe_call("p1"), text(ANSWER)],
        "tool_after_finished" => vec![probe_call("p1"), text(ANSWER)],
        "tool_after_transcript" => vec![probe_call("p1"), text(ANSWER)],
        "tool_during_transcript" => vec![probe_call("p1"), text(ANSWER)],
        other => panic!("unknown scenario {other}"),
    };
    (script, goal)
}

async fn child_run(scenario: &str, db_path: &Path, root: &Path) {
    let (script, goal) = scenario_script(scenario);
    let db = Database::connect(db_path).await.unwrap();
    let record = SessionRecord::new(
        root.to_string_lossy().as_ref(),
        if goal { "goal" } else { "chat" },
        "mock/m",
        leveler_core::now(),
    );
    SessionRepository::new(&db).create(&record).await.unwrap();
    let session = SessionId::new(record.id);
    SessionRepository::new(&db)
        .set_turn_collaboration(
            &session,
            if goal { "goal" } else { "chat" },
            leveler_core::now(),
        )
        .await
        .unwrap();
    let coding = coding_runtime(&db, root, script);
    let spec = spec(root);
    if goal {
        let _ = coding
            .run(&session, &spec, &mut |_| {}, CancellationToken::new())
            .await;
    } else {
        let _ = coding
            .chat_with_origin(
                &session,
                &spec,
                vec![ContentPart::Text {
                    text: "run the crash matrix task".to_string(),
                }],
                leveler_model::TranscriptOrigin::UserInput,
                &mut |_| {},
                CancellationToken::new(),
            )
            .await;
    }
}

/// The child entry point. Inert in a normal test run (no `CL_CRASH_SCENARIO`),
/// and invoked by the parent through `--exact crash_child`.
#[test]
fn crash_child() {
    let Ok(scenario) = std::env::var("CL_CRASH_SCENARIO") else {
        return;
    };
    let db: PathBuf = std::env::var("CL_CRASH_DB").unwrap().into();
    let root: PathBuf = std::env::var("CL_CRASH_WORKSPACE").unwrap().into();
    std::fs::create_dir_all(&root).unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(child_run(&scenario, &db, &root));
}

// ── parent-side assertions ──────────────────────────────────────────────────

fn side_effects(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|text| text.lines().count())
        .unwrap_or(0)
}

async fn events(db: &Database, session: &SessionId) -> Vec<EngineEvent> {
    EventRepository::new(db)
        .load(session)
        .await
        .unwrap()
        .iter()
        .map(|row| EngineEvent::from_payload(&row.payload).unwrap())
        .collect()
}

async fn model_requests(db: &Database, session: &SessionId) -> usize {
    ModelRequestRepository::new(db)
        .load_for_session(session)
        .await
        .unwrap()
        .len()
}

fn count(events: &[EngineEvent], what: &str) -> usize {
    events
        .iter()
        .filter(|event| match what {
            "delivery" => matches!(event, EngineEvent::DeliveryCommitted { .. }),
            "turn" => matches!(event, EngineEvent::TurnFinished { .. }),
            "task" => matches!(event, EngineEvent::TaskFinished { .. }),
            _ => false,
        })
        .count()
}

async fn session_in(db: &Database) -> SessionId {
    let sessions = SessionRepository::new(db).list().await.unwrap();
    SessionId::new(sessions[0].id.clone())
}

fn spawn_child(scenario: &str, dir: &Path, point: &str) -> std::process::ExitStatus {
    let exe = std::env::current_exe().unwrap();
    let mut command = std::process::Command::new(exe);
    command
        .args(["--exact", "crash_child", "--nocapture"])
        .env("CL_CRASH_SCENARIO", scenario)
        .env("CL_CRASH_DB", dir.join("sessions.db"))
        .env("CL_CRASH_WORKSPACE", dir.join("workspace"))
        .env("CL_CRASH_SIDE_EFFECT", dir.join("side-effects.log"))
        .env("LEVELER_TEST_CRASH_POINT", point)
        .env("LEVELER_TEST_CRASH_MARKER", dir.join("crash.marker"))
        .env("LEVELER_HOME", dir.join("home"))
        .env("HOME", dir.join("home"));
    command.status().unwrap()
}

#[cfg(unix)]
fn killed_by_sigkill(status: std::process::ExitStatus) -> bool {
    use std::os::unix::process::ExitStatusExt;
    status.signal() == Some(9)
}

#[cfg(not(unix))]
fn killed_by_sigkill(status: std::process::ExitStatus) -> bool {
    let _ = status;
    false
}

async fn prepare(scenario: &str, point: &str) -> (tempfile::TempDir, SessionId, Database) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("workspace")).unwrap();
    std::fs::create_dir_all(dir.path().join("home")).unwrap();
    let status = spawn_child(scenario, dir.path(), point);
    assert!(
        killed_by_sigkill(status),
        "{scenario}: expected a real SIGKILL at {point}, got {status:?}"
    );
    let marker = std::fs::read_to_string(dir.path().join("crash.marker")).unwrap();
    assert!(
        marker.contains(point),
        "{scenario}: marker must prove the cut: {marker}"
    );
    let db = Database::connect(&dir.path().join("sessions.db"))
        .await
        .unwrap();
    let session = session_in(&db).await;
    (dir, session, db)
}

async fn resume(
    dir: &Path,
    db: &Database,
    session: &SessionId,
    script: Vec<ModelResponse>,
) -> Result<leveler_agent::coding::TaskReport, leveler_engine::EngineError> {
    let root = dir.join("workspace");
    let coding = coding_runtime(db, &root, script);
    let spec = spec(&root);
    coding
        .resume(session, &spec, &mut |_| {}, CancellationToken::new())
        .await
}

async fn acknowledge(db: &Database, session: &SessionId) {
    let root = PathBuf::from("/");
    let coding = coding_runtime(db, &root, Vec::new());
    coding.acknowledge_crash_window(session).await.unwrap();
}

// ── delivery scenarios ──────────────────────────────────────────────────────

#[tokio::test]
async fn chat_before_delivery_may_regenerate_but_never_fakes_the_delivery() {
    let (dir, session, db) = prepare("chat_before_delivery", "before_delivery_committed").await;
    let before = events(&db, &session).await;
    assert_eq!(count(&before, "delivery"), 0, "no delivery was durable");
    assert_eq!(count(&before, "turn"), 0);
    assert_eq!(count(&before, "task"), 0);
    let requests = model_requests(&db, &session).await;

    let report = resume(dir.path(), &db, &session, vec![text(SECOND)])
        .await
        .expect("an undelivered answer resumes");
    assert_eq!(report.final_text, SECOND);
    assert_eq!(model_requests(&db, &session).await, requests + 1);
    let after = events(&db, &session).await;
    assert_eq!(count(&after, "delivery"), 1);
    assert_eq!(
        count(&after, "turn"),
        2,
        "the reaped interrupted turn plus the completed regenerated turn"
    );
    assert_eq!(count(&after, "task"), 1);
}

#[tokio::test]
async fn chat_after_delivery_recovers_without_a_model_request() {
    let (dir, session, db) = prepare("chat_after_delivery", "after_delivery_durable").await;
    let before = events(&db, &session).await;
    assert_eq!(count(&before, "delivery"), 1);
    assert_eq!(count(&before, "turn"), 0);
    assert_eq!(count(&before, "task"), 0);
    let requests = model_requests(&db, &session).await;

    let report = resume(dir.path(), &db, &session, Vec::new())
        .await
        .expect("the durable Chat delivery resumes");
    assert_eq!(report.final_text, ANSWER);
    assert_eq!(
        model_requests(&db, &session).await,
        requests,
        "a durable delivery must not ask the model again"
    );
    let after = events(&db, &session).await;
    assert_eq!(count(&after, "delivery"), 1);
    assert_eq!(count(&after, "turn"), 1);
    assert_eq!(count(&after, "task"), 1);
}

#[tokio::test]
async fn chat_after_turn_terminal_recovers_the_task_without_a_model_request() {
    let (dir, session, db) = prepare("chat_after_turn_terminal", "after_turn_terminal").await;
    let before = events(&db, &session).await;
    assert_eq!(count(&before, "delivery"), 1);
    assert_eq!(count(&before, "turn"), 1);
    assert_eq!(count(&before, "task"), 0);
    let requests = model_requests(&db, &session).await;

    let report = resume(dir.path(), &db, &session, Vec::new())
        .await
        .expect("the missing task terminal is mechanical");
    assert_eq!(report.final_text, ANSWER);
    assert_eq!(model_requests(&db, &session).await, requests);
    let after = events(&db, &session).await;
    assert_eq!(count(&after, "task"), 1);
    assert_eq!(
        count(&after, "turn"),
        1,
        "the turn terminal is not duplicated"
    );
}

#[tokio::test]
async fn chat_after_task_terminal_replays_the_answer_without_a_model_request() {
    let (dir, session, db) = prepare("chat_after_task_terminal", "after_task_terminal").await;
    let before = events(&db, &session).await;
    assert_eq!(count(&before, "delivery"), 1);
    assert_eq!(count(&before, "turn"), 1);
    assert_eq!(count(&before, "task"), 1);
    let requests = model_requests(&db, &session).await;

    // The client may have missed the final frame; a bare continuation gets the
    // recorded answer back, never a second model request and never a second
    // delivery row.
    let report = resume(dir.path(), &db, &session, Vec::new())
        .await
        .expect("the committed delivery is replayable");
    assert_eq!(report.final_text, ANSWER);
    assert_eq!(
        model_requests(&db, &session).await,
        requests,
        "a committed delivery must not re-drive the model"
    );
    let after = events(&db, &session).await;
    assert_eq!(count(&after, "delivery"), 1);
    assert_eq!(count(&after, "task"), 1);
}

#[tokio::test]
async fn goal_after_delivery_recovers_settlement_without_a_model_request() {
    let (dir, session, db) = prepare("goal_after_delivery", "after_delivery_durable").await;
    let before = events(&db, &session).await;
    assert_eq!(count(&before, "delivery"), 1);
    assert_eq!(count(&before, "task"), 0);
    let requests = model_requests(&db, &session).await;

    let report = resume(dir.path(), &db, &session, Vec::new())
        .await
        .expect("the goal delivery resumes");
    assert_eq!(report.final_text, ANSWER);
    assert_eq!(
        report.stop_reason,
        StopReason::Completed,
        "the recorded goal stop is kept"
    );
    assert_eq!(model_requests(&db, &session).await, requests);
    let after = events(&db, &session).await;
    assert_eq!(count(&after, "task"), 1);
    let stores = EngineStores::from_database(&db);
    let task = stores
        .tasks
        .task_for_session(&session)
        .await
        .unwrap()
        .unwrap();
    let goals = stores.goals.for_task(&task).await.unwrap();
    assert_eq!(goals[0].state, leveler_storage::GoalState::Settled);
    assert_eq!(
        goals[0].windows_run, 1,
        "windows_run is charged exactly once"
    );
}

// ── tool / transcript scenarios ─────────────────────────────────────────────

#[tokio::test]
async fn tool_started_but_not_executed_stops_without_a_side_effect() {
    let (dir, session, db) = prepare("tool_after_started", "after_tool_started_durable").await;
    let side = dir.path().join("side-effects.log");
    assert_eq!(side_effects(&side), 0, "the tool never ran");
    let error = resume(dir.path(), &db, &session, vec![text(ANSWER)])
        .await
        .expect_err("an unacknowledged started call must stop recovery");
    assert!(
        matches!(
            error,
            leveler_engine::EngineError::RecoveryConfirmationRequired { .. }
        ),
        "unexpected error: {error}"
    );
    assert_eq!(side_effects(&side), 0);
}

#[tokio::test]
async fn tool_side_effect_without_finish_is_unknown_and_not_replayed() {
    let (dir, session, db) = prepare("tool_during_effect", "during_tool_side_effect").await;
    let side = dir.path().join("side-effects.log");
    assert_eq!(side_effects(&side), 1, "the side effect really happened");
    let error = resume(dir.path(), &db, &session, vec![text(ANSWER)])
        .await
        .expect_err("an unknown side effect must stop recovery");
    assert!(
        matches!(
            error,
            leveler_engine::EngineError::RecoveryConfirmationRequired { .. }
        ),
        "unexpected error: {error}"
    );
    acknowledge(&db, &session).await;
    let report = resume(dir.path(), &db, &session, vec![text(ANSWER)])
        .await
        .expect("after acknowledgement the turn resumes");
    assert_eq!(report.final_text, ANSWER);
    assert_eq!(
        side_effects(&side),
        1,
        "acknowledged recovery must not repeat the unknown side effect"
    );
}

#[tokio::test]
async fn finished_tool_without_transcript_is_reconstructed_and_not_replayed() {
    let (dir, session, db) = prepare(
        "tool_after_finished",
        "after_tool_finished_before_transcript",
    )
    .await;
    let side = dir.path().join("side-effects.log");
    assert_eq!(side_effects(&side), 1);
    let before = events(&db, &session).await;
    assert!(before.iter().any(
        |event| matches!(event, EngineEvent::ToolCallFinished { call_id, .. } if call_id == "p1")
    ));

    let report = resume(dir.path(), &db, &session, vec![text(ANSWER)])
        .await
        .expect("the finished tool result is reconstructed");
    assert_eq!(report.final_text, ANSWER);
    assert_eq!(
        side_effects(&side),
        1,
        "recovery must not execute the tool again"
    );
    let transcript = MessageRepository::new(&db).load(&session).await.unwrap();
    assert!(
        transcript.iter().any(|payload| {
            payload.contains("\"call_id\":\"p1\"") && payload.contains("recovery")
        }),
        "the recovered exchange must be in the transcript: {transcript:?}"
    );
}

/// T5: the process dies inside the transcript batch's transaction. The batch
/// is one COMMIT, so no partial assistant/tool exchange can survive; the
/// durable finished event lets recovery reconstruct it once.
#[tokio::test]
async fn a_kill_inside_the_transcript_transaction_leaves_no_partial_batch() {
    let (dir, session, db) = prepare("tool_during_transcript", "during_transcript_commit").await;
    let side = dir.path().join("side-effects.log");
    assert_eq!(side_effects(&side), 1);
    let transcript = MessageRepository::new(&db).load(&session).await.unwrap();
    assert!(
        !transcript
            .iter()
            .any(|payload| payload.contains("\"role\":\"tool\"")),
        "an uncommitted batch must leave no tool row: {transcript:?}"
    );
    let after_crash = events(&db, &session).await;
    assert!(
        after_crash
            .iter()
            .any(|event| matches!(event, EngineEvent::ToolCallFinished { call_id, .. } if call_id == "p1")),
        "the finished event is durable before the transcript commit"
    );

    let report = resume(dir.path(), &db, &session, vec![text(ANSWER)])
        .await
        .expect("the transaction cut is reconstructed");
    assert_eq!(report.final_text, ANSWER);
    assert_eq!(side_effects(&side), 1, "the tool is not repeated");
    let transcript = MessageRepository::new(&db).load(&session).await.unwrap();
    assert!(
        transcript
            .iter()
            .any(|payload| payload.contains("\"call_id\":\"p1\"") && payload.contains("recovery")),
        "the recovered exchange is written once: {transcript:?}"
    );
}

#[tokio::test]
async fn transcript_without_a_next_round_resumes_from_the_durable_result() {
    let (dir, session, db) = prepare(
        "tool_after_transcript",
        "after_transcript_before_next_round",
    )
    .await;
    let side = dir.path().join("side-effects.log");
    assert_eq!(side_effects(&side), 1);
    let report = resume(dir.path(), &db, &session, vec![text(ANSWER)])
        .await
        .expect("the turn resumes from the durable transcript");
    assert_eq!(report.final_text, ANSWER);
    assert_eq!(side_effects(&side), 1, "the tool is not repeated");
}
