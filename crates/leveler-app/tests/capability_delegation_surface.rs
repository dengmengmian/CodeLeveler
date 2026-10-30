//! The delegation tool surface across turns.
//!
//! `spawn_agent` is behind capability disclosure: it is not in the first
//! request, and a `capability enable multi_agent` call must put it in the NEXT
//! request. A real TUI dogfood run reported the opposite — the enable came back
//! "tools available on the following request" and the tool never appeared — so
//! this locks the surface against the model requests the runtime actually built.

use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{ClientCommand, InteractiveRuntimeClient};
use leveler_execution::PermissionProfile;
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_storage::TurnRepository;
use leveler_test_support::{MockResponse, MockServer};

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

fn answer_frame(text: &str) -> String {
    serde_json::json!({"choices": [{"delta": {"content": text}}]}).to_string()
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
  context_window: 131072
  reliable_context: 65536
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
    client: Arc<InProcessRuntimeClient>,
    session: leveler_core::SessionId,
    server: MockServer,
    _tmp: tempfile::TempDir,
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
    let app = Arc::new(
        Application::assemble(layout)
            .unwrap()
            .with_collaboration(leveler_agent::CollaborationMode::Chat),
    );
    let model = ModelRef::new("mock", "m");
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        model.clone(),
        PermissionProfile::FullAccess,
        false,
    ));
    let session = app
        .create_session(&model, "optional capabilities")
        .await
        .unwrap();
    Fixture {
        app,
        client,
        session,
        server,
        _tmp: tmp,
    }
}

/// Wait until the session's own goal records satisfy `done`.
///
/// The terminal transaction that settles a goal lands after the turn row stops
/// running, and the session keeps its active-turn slot until then.
async fn wait_for_goals(f: &Fixture, done: impl Fn(&[leveler_storage::GoalRecord]) -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let db = f.app.open_database().await.unwrap();
        let stores = leveler_storage::EngineStores::from_database(&db);
        let goals = match stores.tasks.task_for_session(&f.session).await.unwrap() {
            Some(task) => stores.goals.for_task(&task).await.unwrap(),
            None => Vec::new(),
        };
        if done(&goals) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the goal did not reach its terminal state: {goals:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Wait for the turn row AND the runtime's admit lease before the next send.
/// The lease is released after the row stops running, so returning earlier
/// makes the next command fail with "already has an active turn".
async fn settled(f: &Fixture, min_turns: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let done = {
            let db = f.app.open_database().await.unwrap();
            let turns = TurnRepository::new(&db).list(&f.session).await.unwrap();
            assert!(
                tokio::time::Instant::now() < deadline,
                "the turn did not settle in time: {turns:?}"
            );
            turns.len() >= min_turns && turns.iter().all(|turn| turn.status != "running")
        };
        if done && !f.client.has_live_turn(&f.session) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn tool_names(body: &str) -> Vec<String> {
    let Ok(request) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    request
        .get("tools")
        .and_then(|tools| tools.as_array())
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| {
                    tool.pointer("/function/name")
                        .or_else(|| tool.get("name"))
                        .and_then(|name| name.as_str())
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Enabling `multi_agent` in a LATER turn of an existing Chat session must put
/// `spawn_agent` in the next request — this is the surface a real TUI run
/// reported missing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_capability_enabled_in_a_later_turn_exposes_spawn_agent() {
    let f = fixture(vec![
        // Turn 1: an ordinary answer. No delegation is enabled yet.
        sse(vec![answer_frame("好的。"), finish_frame("stop")]),
        // Turn 2: the model enables delegation, then answers once more.
        sse(vec![
            tool_call_frame(
                "c-enable",
                "capability",
                serde_json::json!({"action": "enable", "id": "multi_agent"}),
            ),
            finish_frame("tool_calls"),
        ]),
        sse(vec![answer_frame("已启用。"), finish_frame("stop")]),
    ])
    .await;
    let mut events = f.client.subscribe_session(&f.session);

    f.client
        .send(ClientCommand::SubmitMessage {
            session_id: f.session.clone(),
            content: "先看看仓库。".to_string(),
            attachments: vec![],
        })
        .await
        .unwrap();
    settled(&f, 1).await;

    f.client
        .send(ClientCommand::SubmitMessage {
            session_id: f.session.clone(),
            content: "启用委派能力。".to_string(),
            attachments: vec![],
        })
        .await
        .unwrap();
    settled(&f, 2).await;
    while events.try_recv().is_ok() {}

    let bodies = f.server.request_bodies().await;
    // The first turn's request: delegation is not part of the initial surface.
    let before = bodies.first().expect("the first turn reached the model");
    assert!(
        !tool_names(before).contains(&"spawn_agent".to_string()),
        "delegation is not part of the first surface: {:?}",
        tool_names(before)
    );

    let after = bodies
        .iter()
        .find(|body| body.contains("c-enable"))
        .expect("the request carrying the enable result reached the model");
    assert!(
        tool_names(after).contains(&"spawn_agent".to_string()),
        "an enabled capability must be exposed on the following request: {:?}",
        tool_names(after)
    );
    // The user-facing truth of the same fact: the model may now delegate.
    assert!(
        tool_names(after).contains(&"commit_task".to_string()),
        "the goal surface stays: {:?}",
        tool_names(after)
    );
}

/// The same disclosure, in a Chat session that already owns an open goal.
/// This is the shape a real TUI run followed: the goal is bound, the next Chat
/// turn enables `multi_agent`, and the runtime answers "tools available on the
/// following request" — while the following request still had no
/// `spawn_agent`, because the enable lived in an engine that the turn boundary
/// dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_capability_enabled_in_a_chat_turn_with_an_open_goal_exposes_spawn_agent() {
    let f = fixture(vec![
        // Turn 1: commit the task, then answer. The goal stays owed.
        sse(vec![
            tool_call_frame(
                "c-commit",
                "commit_task",
                serde_json::json!({
                    "objective": "finish the inventory work",
                    "acceptance": ["inventory is finished"],
                }),
            ),
            finish_frame("tool_calls"),
        ]),
        sse(vec![answer_frame("已记录这个任务。"), finish_frame("stop")]),
        // Turn 2: enable delegation, then answer.
        sse(vec![
            tool_call_frame(
                "c-enable",
                "capability",
                serde_json::json!({"action": "enable", "id": "multi_agent"}),
            ),
            finish_frame("tool_calls"),
        ]),
        sse(vec![answer_frame("已启用。"), finish_frame("stop")]),
    ])
    .await;
    let mut events = f.client.subscribe_session(&f.session);

    // Turn 1 answers twice (the commit opens the goal, then the model
    // answers), turn 2 answers twice (enable, then the final answer).
    for (index, content) in ["把库存这件事变成任务。", "现在启用委派能力。"]
        .into_iter()
        .enumerate()
    {
        f.client
            .send(ClientCommand::SubmitMessage {
                session_id: f.session.clone(),
                content: content.to_string(),
                attachments: vec![],
            })
            .await
            .unwrap();
        settled(&f, index + 1).await;
        if index == 0 {
            wait_for_goals(&f, |goals| goals.len() == 1).await;
        }
        let db = f.app.open_database().await.unwrap();
        let turns = TurnRepository::new(&db).list(&f.session).await.unwrap();
        println!(
            "turn {index}: turns={:?} bodies={} events={:?}",
            turns
                .iter()
                .map(|t| (t.kind.clone(), t.status.clone()))
                .collect::<Vec<_>>(),
            f.server.request_bodies().await.len(),
            std::iter::from_fn(|| events.try_recv().ok())
                .filter(|event| format!("{event:?}").contains("Turn")
                    || format!("{event:?}").contains("Goal"))
                .map(|event| format!("{event:?}").chars().take(60).collect::<String>())
                .collect::<Vec<_>>()
        );
    }
    while events.try_recv().is_ok() {}

    let bodies = f.server.request_bodies().await;
    let after = bodies
        .iter()
        .find(|body| body.contains("c-enable"))
        .expect("the request carrying the enable result reached the model");
    assert!(
        tool_names(after).contains(&"spawn_agent".to_string()),
        "an enabled capability must be exposed on the following request, even when the \
         session already owns an open goal: {:?}",
        tool_names(after)
    );
}
