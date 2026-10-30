//! Where does an enabled capability live?
//!
//! `capability(action="enable")` answers "tools available on the following
//! request". Two things must then be true at once:
//!
//! 1. the following request really carries the tool (`spawn_agent`), and
//! 2. the state the model is told about (`capability(action="status")`,
//!    `exposed`) agrees with the tool list of the request it just received.
//!
//! A real TUI dogfood run reported (1) then, on later turns, (2) as false —
//! "exposed": false for a capability the runtime had accepted an enable for.
//! These tests lock both halves from the provider request itself, across the
//! turn boundary where the state used to be dropped.

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
    tmp: tempfile::TempDir,
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
        model,
        PermissionProfile::FullAccess,
        false,
    ));
    let session = app
        .create_session(&ModelRef::new("mock", "m"), "capability scope")
        .await
        .unwrap();
    Fixture {
        app,
        client,
        session,
        server,
        tmp,
    }
}

/// Reopen the same workspace as a new process would: a fresh `Application`
/// over the same state directory (the provider server survives on purpose so
/// the request log stays one story).
async fn reopen(f: &Fixture) -> (Arc<Application>, Arc<InProcessRuntimeClient>) {
    let layout = Layout::from_parts(
        f.tmp.path().to_path_buf(),
        f.tmp.path().join("configs"),
        f.tmp.path().join("state"),
    );
    let app = Arc::new(
        Application::assemble(layout)
            .unwrap()
            .with_collaboration(leveler_agent::CollaborationMode::Chat),
    );
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        ModelRef::new("mock", "m"),
        PermissionProfile::FullAccess,
        false,
    ));
    (app, client)
}

/// Wait for the turn row AND the runtime's admit lease. The lease is released
/// after the row stops running, so returning earlier makes the next `send`
/// fail with "already has an active turn" — a harness race, not a product
/// fact, and it would hide what this file is about.
async fn settled(
    app: &Application,
    client: &InProcessRuntimeClient,
    session: &leveler_core::SessionId,
    min_turns: usize,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let done = {
            let db = app.open_database().await.unwrap();
            let turns = TurnRepository::new(&db).list(session).await.unwrap();
            assert!(
                tokio::time::Instant::now() < deadline,
                "the turn did not settle in time: {turns:?}"
            );
            turns.len() >= min_turns && turns.iter().all(|turn| turn.status != "running")
        };
        if done && !client.has_live_turn(session) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn submit(
    app: &Arc<Application>,
    client: &Arc<InProcessRuntimeClient>,
    session: &leveler_core::SessionId,
    content: &str,
    min_turns: usize,
) {
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: content.to_string(),
            attachments: vec![],
        })
        .await
        .unwrap();
    settled(app, client, session, min_turns).await;
}

fn request_tools(body: &str) -> Vec<String> {
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

/// The `exposed` flag the model itself read from the `capability` tool result
/// carried by a request body.
fn exposed_in(body: &str, id: &str) -> Option<bool> {
    let request: serde_json::Value = serde_json::from_str(body).ok()?;
    let content = request
        .get("messages")?
        .as_array()?
        .iter()
        .rev()
        .filter_map(|message| message.get("content"))
        .filter_map(|content| content.as_str())
        .find(|content| content.contains("\"exposed\"") && content.contains(id))?;
    let parsed: serde_json::Value = serde_json::from_str(content).ok()?;
    parsed
        .as_array()?
        .iter()
        .find(|entry| entry.get("id").and_then(|v| v.as_str()) == Some(id))?
        .get("exposed")?
        .as_bool()
}

/// Turn 1 enables delegation; turn 2 is a new request the model makes after
/// that turn ended. The tool must still be there, because nothing told the
/// model — or the user — that the enable expired with the turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_enabled_capability_survives_the_turn_boundary() {
    let f = fixture(vec![
        // Turn 1: enable, then answer.
        sse(vec![
            tool_call_frame(
                "c-enable",
                "capability",
                serde_json::json!({"action": "enable", "id": "multi_agent"}),
            ),
            finish_frame("tool_calls"),
        ]),
        sse(vec![answer_frame("已启用。"), finish_frame("stop")]),
        // Turn 2: answer (the assertion reads this request's tool list).
        sse(vec![answer_frame("好的。"), finish_frame("stop")]),
    ])
    .await;

    submit(&f.app, &f.client, &f.session, "启用委派能力", 1).await;
    submit(&f.app, &f.client, &f.session, "现在委派一个子任务", 2).await;

    let bodies = f.server.request_bodies().await;
    let enable_request = bodies
        .iter()
        .position(|body| body.contains("c-enable"))
        .expect("the enable round reached the model");
    let following = bodies
        .get(enable_request)
        .expect("the request carrying the enable result");
    assert!(
        request_tools(following).contains(&"spawn_agent".to_string()),
        "the following request must carry the enabled tool: {:?}",
        request_tools(following)
    );
    let turn_two = bodies.last().expect("turn two reached the model");
    assert!(
        request_tools(turn_two).contains(&"spawn_agent".to_string()),
        "an enabled capability must not silently expire at the turn boundary: {:?}",
        request_tools(turn_two)
    );
}

/// The same fact as the model reads it: a `capability(status)` call on the
/// turn after the enable must not report `exposed: false` for a capability the
/// runtime accepted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_status_the_model_reads_never_contradicts_the_tool_list() {
    let f = fixture(vec![
        sse(vec![
            tool_call_frame(
                "c-enable",
                "capability",
                serde_json::json!({"action": "enable", "id": "multi_agent"}),
            ),
            finish_frame("tool_calls"),
        ]),
        sse(vec![answer_frame("已启用。"), finish_frame("stop")]),
        // Turn 2, request 1: ask for status. Request 2 carries the result.
        sse(vec![
            tool_call_frame(
                "c-status",
                "capability",
                serde_json::json!({"action": "status"}),
            ),
            finish_frame("tool_calls"),
        ]),
        sse(vec![answer_frame("查过了。"), finish_frame("stop")]),
    ])
    .await;

    submit(&f.app, &f.client, &f.session, "启用委派能力", 1).await;
    submit(&f.app, &f.client, &f.session, "看一下现在的能力状态", 2).await;

    let bodies = f.server.request_bodies().await;
    let after_status = bodies
        .iter()
        .find(|body| body.contains("c-status"))
        .expect("the status round reached the model");
    let exposed =
        exposed_in(after_status, "multi_agent").expect("the status result is visible to the model");
    assert!(
        exposed,
        "the model must not be told a capability it enabled is not exposed: {after_status}"
    );
}

/// Reopening the session in a new process (the `--session` resume path, with
/// or without a SIGKILL: the durable state is what a new boot reads) must keep
/// the same answer: an enable in the resumed process reaches the next request
/// and keeps reaching later requests.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_enable_in_a_reopened_session_reaches_the_next_request() {
    let f = fixture(vec![
        // Turn 1 (first process): an ordinary answer.
        sse(vec![answer_frame("好的。"), finish_frame("stop")]),
        // Turn 2 (reopened process): enable, then answer.
        sse(vec![
            tool_call_frame(
                "c-enable",
                "capability",
                serde_json::json!({"action": "enable", "id": "multi_agent"}),
            ),
            finish_frame("tool_calls"),
        ]),
        sse(vec![answer_frame("已启用。"), finish_frame("stop")]),
        // Turn 3 (same reopened process): the delegation request.
        sse(vec![answer_frame("好的。"), finish_frame("stop")]),
    ])
    .await;

    submit(&f.app, &f.client, &f.session, "你好", 1).await;

    let reopened = reopen(&f).await;
    submit(&reopened.0, &reopened.1, &f.session, "启用委派能力", 2).await;
    submit(
        &reopened.0,
        &reopened.1,
        &f.session,
        "现在委派一个子任务",
        3,
    )
    .await;

    let bodies = f.server.request_bodies().await;
    let enable_request = bodies
        .iter()
        .position(|body| body.contains("c-enable"))
        .expect("the enable round reached the model");
    assert!(
        request_tools(&bodies[enable_request]).contains(&"spawn_agent".to_string()),
        "the request after the enable must carry the tool"
    );
    let turn_three = bodies.last().expect("turn three reached the model");
    assert!(
        request_tools(turn_three).contains(&"spawn_agent".to_string()),
        "a resumed session must not lose an enable at the next turn: {:?}",
        request_tools(turn_three)
    );
}

/// A killed process (SIGKILL: no graceful shutdown) followed by reopening the
/// session with `--session`.
///
/// The in-memory activation of the dead process is gone — that is honest, and
/// the runtime must say so rather than pretend: the first request of the
/// resumed session carries no `spawn_agent`, and `capability(status)` reports
/// `exposed: false`. From there the session must behave like any other: an
/// enable reaches the following request AND the turn after it, so the model is
/// never told one thing while the provider sees another.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_killed_boot_resumes_honestly_and_a_new_enable_still_sticks() {
    let f = fixture(vec![
        // The dead process: enable, then answer.
        sse(vec![
            tool_call_frame(
                "c-enable",
                "capability",
                serde_json::json!({"action": "enable", "id": "multi_agent"}),
            ),
            finish_frame("tool_calls"),
        ]),
        sse(vec![answer_frame("已启用。"), finish_frame("stop")]),
        // The resumed process, turn 2: ask for the status, then answer. The
        // second request carries the status result this process produced.
        sse(vec![
            tool_call_frame(
                "c-status",
                "capability",
                serde_json::json!({"action": "status"}),
            ),
            finish_frame("tool_calls"),
        ]),
        sse(vec![answer_frame("状态是新的。"), finish_frame("stop")]),
        // Turn 3: enable again in the resumed process, then answer.
        sse(vec![
            tool_call_frame(
                "c-reenable",
                "capability",
                serde_json::json!({"action": "enable", "id": "multi_agent"}),
            ),
            finish_frame("tool_calls"),
        ]),
        sse(vec![answer_frame("已重新启用。"), finish_frame("stop")]),
        // Turn 4: the request that must still carry the tool.
        sse(vec![answer_frame("好的。"), finish_frame("stop")]),
    ])
    .await;

    submit(&f.app, &f.client, &f.session, "启用委派能力", 1).await;

    // SIGKILL: a turn row stays `running` under a boot that is now gone.
    let crashed = leveler_app::runtime_boot::RuntimeBootLease::acquire(&f.tmp.path().join("state"))
        .expect("a second boot lease, as a second process would hold");
    let db = f.app.open_database().await.unwrap();
    let engine = leveler_engine::TaskEngine {
        stores: leveler_storage::EngineStores::from_database(&db),
        runtime_id: f.app.runtime_id().unwrap(),
        boot: leveler_engine::EngineBoot {
            id: crashed.id().clone(),
            liveness: Arc::new(leveler_app::runtime_boot::StateDirBootLiveness::new(
                &f.tmp.path().join("state"),
            )),
        },
    };
    let token = engine.acquire_ownership(&f.session).await.unwrap();
    engine
        .stores
        .turns
        .start_owned(&token, &f.session, "chat", None, leveler_core::now())
        .await
        .unwrap();
    drop(token);
    drop(engine);
    drop(db);
    // The killed process's boot lease is released by the OS, not by cleanup.
    drop(crashed);

    // A new process: fresh Application over the same state, no live cells.
    let reopened = reopen(&f).await;
    // The `--session` path reaps the dead boot's turn before the first submit.
    let db = reopened.0.open_database().await.unwrap();
    reopened
        .0
        .reap_zombie_turns(&db, Some(&f.session))
        .await
        .unwrap();
    drop(db);

    submit(&reopened.0, &reopened.1, &f.session, "现在状态如何？", 2).await;
    submit(&reopened.0, &reopened.1, &f.session, "重新启用委派", 3).await;
    submit(&reopened.0, &reopened.1, &f.session, "委派一个子任务", 4).await;

    let bodies = f.server.request_bodies().await;
    let after_kill = bodies
        .iter()
        .position(|body| body.contains("c-status"))
        .expect("the resumed session reached the model");
    let first_resumed_request = &bodies[after_kill - 1];
    assert!(
        !request_tools(first_resumed_request).contains(&"spawn_agent".to_string()),
        "the killed process's in-memory activation must not survive as a tool: {:?}",
        request_tools(first_resumed_request)
    );
    assert_eq!(
        exposed_in(&bodies[after_kill], "multi_agent"),
        Some(false),
        "the resumed session must report the truth it is acting on"
    );

    let reenabled = bodies
        .iter()
        .position(|body| body.contains("c-reenable"))
        .expect("the re-enable round reached the model");
    assert!(
        request_tools(&bodies[reenabled]).contains(&"spawn_agent".to_string()),
        "an enable in the resumed process must reach the following request"
    );
    let last = bodies.last().expect("the last turn reached the model");
    assert!(
        request_tools(last).contains(&"spawn_agent".to_string()),
        "and the turn after that: {:?}",
        request_tools(last)
    );
}
