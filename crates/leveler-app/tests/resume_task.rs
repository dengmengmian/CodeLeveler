//! `继续` is a logical-task continuation, not a fresh chat turn.
//!
//! When a task window ends early (an interruption, or a budget boundary), the
//! session still owns its task, goal and plan. The next continuation must
//! re-enter through the runtime's resume path so that identity is preserved:
//! same task, same goal, plan seeded before the model can re-declare one, and
//! an amendment added on top of the original objective rather than replacing
//! it.

use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{ClientCommand, InteractiveRuntimeClient};
use leveler_execution::{AutoApprove, PermissionProfile};
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_storage::{EngineStores, EventRepository, GoalState, TurnRepository};
use leveler_test_support::{MockResponse, MockServer};
use tokio_util::sync::CancellationToken;

fn isolate_global_config() {
    use std::sync::OnceLock;
    static EMPTY_HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    let dir = EMPTY_HOME.get_or_init(|| tempfile::tempdir().unwrap());
    unsafe {
        std::env::set_var("LEVELER_HOME", dir.path());
    }
}

fn sse(frames: Vec<String>) -> MockResponse {
    let mut body = String::new();
    for frame in frames {
        body.push_str("data: ");
        body.push_str(&frame);
        body.push_str("\n\n");
    }
    body.push_str("data: [DONE]\n\n");
    MockResponse::Sse { body }
}

fn tool_call_frame(id: &str, name: &str, arguments: serde_json::Value) -> String {
    serde_json::json!({
        "choices": [{"delta": {"tool_calls": [{
            "index": 0, "id": id,
            "function": {"name": name, "arguments": arguments.to_string()}
        }]}}]
    })
    .to_string()
}

fn finish_frame(reason: &str) -> String {
    serde_json::json!({"choices": [{"delta": {}, "finish_reason": reason}]}).to_string()
}

fn write_config(root: &std::path::Path, base_url: &str) {
    std::fs::create_dir_all(root.join("configs/providers")).unwrap();
    std::fs::create_dir_all(root.join("configs/models")).unwrap();
    std::fs::write(
        root.join("configs/providers/mock.yaml"),
        format!("id: mock\nprotocol: openai_chat\nbase_url: {base_url}\n"),
    )
    .unwrap();
    std::fs::write(
        root.join("configs/models/m.yaml"),
        r#"
id: m
provider: mock
model_id: mock-model
protocol: openai_chat
capabilities:
  streaming: true
  tool_calling: true
  parallel_tool_calls: false
  structured_output: true
  reasoning: false
  vision: false
limits:
  context_window: 8192
  reliable_context: 4096
  max_output_tokens: 1024
  max_tool_schema_bytes: 8192
  max_parallel_tool_calls: 1
compatibility:
  synthesize_tool_call_ids: true
  drop_unsupported_fields: true
"#,
    )
    .unwrap();
}

struct Fixture {
    app: Arc<Application>,
    session: leveler_core::SessionId,
    _tmp: tempfile::TempDir,
    _server: MockServer,
}

async fn fixture(responses: Vec<MockResponse>) -> Fixture {
    isolate_global_config();
    let server = MockServer::start(responses).await;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(tmp.path().join("src/lib.rs"), "pub fn old() {}\n").unwrap();
    write_config(tmp.path(), &server.base_url());
    let layout = Layout::from_parts(
        tmp.path().to_path_buf(),
        tmp.path().join("configs"),
        tmp.path().join("state"),
    );
    let app = Arc::new(Application::assemble(layout).unwrap());
    let session = app
        .create_session(&ModelRef::new("mock", "m"), "finish the inventory work")
        .await
        .unwrap();
    Fixture {
        app,
        session,
        _tmp: tmp,
        _server: server,
    }
}

/// Round 1 declares a two-step plan and leaves step 1 in progress; round 2 does
/// real work. A bounded run then stops at its ceiling with the plan still open.
fn interrupted_work() -> Vec<MockResponse> {
    vec![
        sse(vec![
            tool_call_frame(
                "c-plan",
                "update_plan",
                serde_json::json!({"plan": [
                    {"step": "implement the inventory core", "status": "in_progress"},
                    {"step": "run the tests", "status": "pending"}
                ]}),
            ),
            finish_frame("tool_calls"),
        ]),
        sse(vec![
            tool_call_frame(
                "c-read",
                "read_file",
                serde_json::json!({"path": "src/lib.rs"}),
            ),
            finish_frame("tool_calls"),
        ]),
        sse(vec![
            tool_call_frame(
                "c-final-plan",
                "update_plan",
                serde_json::json!({"plan": [
                    {"step": "implement the inventory core", "status": "completed"},
                    {"step": "run the tests", "status": "pending"}
                ]}),
            ),
            finish_frame("tool_calls"),
        ]),
        sse(vec![
            tool_call_frame(
                "c-goal",
                "update_goal",
                serde_json::json!({"status": "complete", "summary": "continued to the end"}),
            ),
            finish_frame("tool_calls"),
        ]),
    ]
}

async fn settled(app: &Application, session: &leveler_core::SessionId, min_turns: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let db = app.open_database().await.unwrap();
        let turns = TurnRepository::new(&db).list(session).await.unwrap();
        if turns.len() >= min_turns && turns.iter().all(|turn| turn.status != "running") {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the turn did not settle in time: {turns:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The core closure: an interrupted task + `继续` re-enters resume, keeps the
/// task's goal, seeds its plan, and carries the amendment as an addition.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn continuation_resumes_the_logical_task_and_seeds_its_plan() {
    let f = fixture(interrupted_work()).await;
    let model = ModelRef::new("mock", "m");

    // A window that ends early: two rounds, plan still open.
    let report = f
        .app
        .run_in_session_bounded(
            &f.session,
            &model,
            PermissionProfile::Assisted,
            "finish the inventory work",
            Arc::new(AutoApprove),
            false,
            &mut |_| {},
            CancellationToken::new(),
            2,
            None,
        )
        .await
        .expect("a bounded stop is a normal return");
    assert!(
        matches!(
            report.stop_reason,
            leveler_agent::StopReason::TurnLimitReached
                | leveler_agent::StopReason::BudgetExhausted
        ),
        "the fixture must end at the budget boundary: {report:?}"
    );

    let db = f.app.open_database().await.unwrap();
    let stores = EngineStores::from_database(&db);
    let task = stores
        .tasks
        .task_for_session(&f.session)
        .await
        .unwrap()
        .unwrap();
    let running: Vec<_> = stores
        .goals
        .for_task(&task)
        .await
        .unwrap()
        .into_iter()
        .filter(|goal| goal.state == GoalState::Running)
        .collect();
    assert_eq!(running.len(), 1, "the interrupted task still owes its goal");
    let goal_id = running[0].id.clone();
    drop(db);

    // The user continues, with an amendment.
    let client = Arc::new(InProcessRuntimeClient::new(
        f.app.clone(),
        model,
        PermissionProfile::Assisted,
        false,
    ));
    let client: Arc<dyn InteractiveRuntimeClient> = client;
    client
        .send(ClientCommand::ResumeTask {
            session_id: f.session.clone(),
            content: "继续，但是先不要跑测试".to_string(),
        })
        .await
        .unwrap();
    settled(&f.app, &f.session, 2).await;

    let db = f.app.open_database().await.unwrap();
    let turns = TurnRepository::new(&db).list(&f.session).await.unwrap();
    assert_eq!(turns.len(), 2, "the continuation is one more turn");
    let resumed = &turns[1];
    assert_eq!(
        resumed.kind, "user",
        "the continuation must enter the resume path, not a fresh chat turn"
    );
    let payload = resumed.payload.as_deref().unwrap_or_default();
    assert!(
        payload.contains("但是先不要跑测试"),
        "the amendment must be the resumed turn's own input: {payload}"
    );

    // The plan is seeded before the model can re-declare one: the first
    // `plan_updated` of the resumed turn precedes its first tool call.
    let stores = EngineStores::from_database(&db);
    let rows = EventRepository::new(&db).load(&f.session).await.unwrap();
    let resumed_rows: Vec<_> = rows
        .iter()
        .filter(|row| row.turn_id.as_deref() == Some(resumed.id.as_str()))
        .collect();
    let first_plan = resumed_rows
        .iter()
        .find(|row| row.event_type == "plan_updated")
        .unwrap_or_else(|| panic!("the resumed turn must re-emit the seeded plan"));
    let step_count = first_plan.payload.matches("\"step\"").count();
    assert!(
        step_count >= 2,
        "the seeded plan must be the interrupted one (two steps), got: {}",
        first_plan.payload
    );
    let first_tool = resumed_rows
        .iter()
        .find(|row| row.event_type == "tool_call_started");
    if let Some(first_tool) = first_tool {
        assert!(
            first_plan.sequence < first_tool.sequence,
            "the plan must be restored before the model runs a tool: plan={} tool={}",
            first_plan.sequence,
            first_tool.sequence
        );
    }

    // The goal is the same one, and the successful continuation settles it.
    let task = stores
        .tasks
        .task_for_session(&f.session)
        .await
        .unwrap()
        .unwrap();
    let goals = stores.goals.for_task(&task).await.unwrap();
    assert_eq!(
        goals.len(),
        1,
        "no second goal was opened by the continuation"
    );
    assert_eq!(
        goals[0].id, goal_id,
        "the original goal identity is preserved"
    );
    assert_eq!(
        goals[0].state,
        GoalState::Settled,
        "a completed continuation settles the original goal"
    );

    // The terminal binds that goal; a chat fallback would have written null.
    let terminal = stores
        .events
        .load_last_by_type(&f.session, "task_finished", None)
        .await
        .unwrap()
        .expect("a terminal was written");
    assert!(
        terminal.payload.contains(goal_id.as_str()),
        "the task terminal must name the settled goal: {}",
        terminal.payload
    );
}

/// No resumable task: `继续` is an ordinary message, not a resume.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn continuation_without_a_resumable_task_is_an_ordinary_message() {
    let f = fixture(vec![sse(vec![
        serde_json::json!({"choices": [{"delta": {"content": "hello"}, "finish_reason": "stop"}]})
            .to_string(),
    ])])
    .await;
    let client: Arc<dyn InteractiveRuntimeClient> = Arc::new(InProcessRuntimeClient::new(
        f.app.clone(),
        ModelRef::new("mock", "m"),
        PermissionProfile::Assisted,
        false,
    ));
    client
        .send(ClientCommand::ResumeTask {
            session_id: f.session.clone(),
            content: "继续".to_string(),
        })
        .await
        .unwrap();
    settled(&f.app, &f.session, 1).await;

    let db = f.app.open_database().await.unwrap();
    let turns = TurnRepository::new(&db).list(&f.session).await.unwrap();
    assert_eq!(turns.len(), 1);
    assert_eq!(
        turns[0].kind, "chat",
        "with nothing to resume, `继续` stays an ordinary chat turn"
    );
}
