//! RCP-B: a goal stops owing work when the run that drove it says so — not
//! when the function that drove it returns.
//!
//! The engine already produces a structured terminal truth for every run:
//! `TaskOutcome`. It distinguishes a task that finished from one that ran out
//! of budget with work still owed, and it uses that distinction itself. The
//! app then settled the durable goal unconditionally the moment `engine.run`
//! returned, which meant the one case the whole goal ledger exists for — a run
//! killed by its own round budget — was recorded as owing nothing.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use leveler_app::Application;
use leveler_execution::{AutoApprove, PermissionProfile};
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_storage::{GoalState, GoalStore};
use leveler_test_support::{MockResponse, MockServer};

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
        "choices": [{
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": id,
                    "function": { "name": name, "arguments": arguments.to_string() }
                }]
            }
        }]
    })
    .to_string()
}

fn finish_frame(reason: &str) -> String {
    serde_json::json!({"choices": [{"delta": {}, "finish_reason": reason}]}).to_string()
}

fn text_frame(text: &str) -> String {
    serde_json::json!({"choices": [{"delta": {"content": text}, "finish_reason": "stop"}]})
        .to_string()
}

fn isolate_global_config() {
    use std::sync::OnceLock;
    static EMPTY_HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    let dir = EMPTY_HOME.get_or_init(|| tempfile::tempdir().unwrap());
    unsafe {
        std::env::set_var("LEVELER_HOME", dir.path());
    }
}

fn write_config(root: &std::path::Path, base_url: &str) {
    isolate_global_config();
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
    app: Application,
    session: leveler_core::SessionId,
    _tmp: tempfile::TempDir,
    _server: MockServer,
}

async fn fixture(responses: Vec<MockResponse>) -> Fixture {
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
    let app = Application::assemble(layout).unwrap();
    let session = app
        .create_session(&ModelRef::new("mock", "m"), "keep working")
        .await
        .unwrap();
    Fixture {
        app,
        session,
        _tmp: tmp,
        _server: server,
    }
}

/// A tool-calling round that never resolves the goal, repeated enough times to
/// outlast any small round budget.
fn endless_work(n: usize) -> Vec<MockResponse> {
    (0..n)
        .map(|i| {
            sse(vec![
                tool_call_frame(
                    &format!("c{i}"),
                    "read_file",
                    serde_json::json!({"path": "src/lib.rs"}),
                ),
                finish_frame("tool_calls"),
            ])
        })
        .collect()
}

/// RCP-B red. A goal whose run was stopped by its round budget still owes
/// work, and a restart must be able to find it.
#[tokio::test]
async fn a_budget_stopped_goal_is_not_settled_and_stays_discoverable() {
    let f = fixture(endless_work(12)).await;

    let outcome = f
        .app
        .run_in_session_bounded(
            &f.session,
            &ModelRef::new("mock", "m"),
            PermissionProfile::Assisted,
            "keep working",
            Arc::new(AutoApprove),
            false,
            &mut |_| {},
            CancellationToken::new(),
            2,
            None,
        )
        .await
        .expect("a budget stop is a normal return, not an error");

    assert!(
        matches!(
            outcome.stop_reason,
            leveler_agent::StopReason::TurnLimitReached
                | leveler_agent::StopReason::BudgetExhausted
        ),
        "the fixture must actually hit the budget: {outcome:?}"
    );

    let db = f.app.open_database().await.unwrap();
    let owed = db.unfinished().await.unwrap();
    assert_eq!(
        owed.len(),
        1,
        "a goal stopped at its round budget still owes work; found {owed:?}"
    );
    assert_eq!(owed[0].state, GoalState::Running);
    assert_eq!(owed[0].objective, "keep working");
    assert!(
        owed[0].windows_run >= 1,
        "the windows it consumed must be on the record, not lost with the \
         process that ran them: {:?}",
        owed[0]
    );
}

/// The other half: a goal that actually finished must settle, or every
/// completed run would be reported forever as unfinished work.
#[tokio::test]
async fn a_completed_goal_settles() {
    let f = fixture(vec![
        sse(vec![
            tool_call_frame(
                "g1",
                "update_goal",
                serde_json::json!({"status": "complete", "summary": "nothing was needed"}),
            ),
            finish_frame("tool_calls"),
        ]),
        sse(vec![text_frame("done")]),
    ])
    .await;

    f.app
        .run_in_session_bounded(
            &f.session,
            &ModelRef::new("mock", "m"),
            PermissionProfile::Assisted,
            "keep working",
            Arc::new(AutoApprove),
            false,
            &mut |_| {},
            CancellationToken::new(),
            8,
            None,
        )
        .await
        .expect("a resolved goal is a normal return");

    let db = f.app.open_database().await.unwrap();
    assert!(
        db.unfinished().await.unwrap().is_empty(),
        "a goal the run resolved owes nothing further"
    );
}

/// RCP-B. Resuming the same objective continues the goal that was left owed —
/// it does not open a second one beside it.
///
/// Two records for one intent would make the discovery surface lie in both
/// directions at once: it would report work that is already being done, and
/// the count of windows spent on the problem would be split across records
/// that each know only their own half.
#[tokio::test]
async fn resuming_a_budget_stopped_goal_continues_the_same_record() {
    let mut responses = endless_work(6);
    // The second invocation resolves it.
    responses.push(sse(vec![
        tool_call_frame(
            "g1",
            "update_goal",
            serde_json::json!({"status": "complete", "summary": "finished on the second pass"}),
        ),
        finish_frame("tool_calls"),
    ]));
    responses.push(sse(vec![text_frame("done")]));
    let f = fixture(responses).await;

    async fn run(
        f: &Fixture,
        rounds: u32,
    ) -> Result<leveler_agent::AgentOutcome, leveler_app::AppError> {
        f.app
            .run_in_session_bounded(
                &f.session,
                &ModelRef::new("mock", "m"),
                PermissionProfile::Assisted,
                "keep working",
                Arc::new(AutoApprove),
                false,
                &mut |_| {},
                CancellationToken::new(),
                rounds,
                None,
            )
            .await
    }

    run(&f, 2).await.expect("first window stops at its budget");
    let db = f.app.open_database().await.unwrap();
    let owed = db.unfinished().await.unwrap();
    assert_eq!(owed.len(), 1, "one goal is owed after the budget stop");
    let goal = owed[0].id.clone();
    let windows_after_first = owed[0].windows_run;
    assert!(windows_after_first >= 1);

    run(&f, 8).await.expect("the second window finishes it");

    let db = f.app.open_database().await.unwrap();
    let all = db.for_task(&owed[0].task_id).await.unwrap();
    assert_eq!(
        all.len(),
        1,
        "resuming the same objective must continue the goal it left owed, not \
         open a second record for the same intent: {all:?}"
    );
    let settled = db.get(&goal).await.unwrap().expect("the goal still exists");
    assert_eq!(
        settled.state,
        GoalState::Settled,
        "and the run that finished it settles that same record"
    );
    assert!(
        settled.windows_run > windows_after_first,
        "the windows the second invocation spent are added to the record, not \
         lost: {windows_after_first} -> {}",
        settled.windows_run
    );
}
