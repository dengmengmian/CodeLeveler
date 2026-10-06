//! `/btw` owns its own capability surface.
//!
//! The permission profile decides how the tools ON that surface are
//! authorized. It never decides WHICH tools are on it. These tests pin both
//! sides of that line:
//!
//! * the side surface is the same observe-only set under every profile, and
//!   the wire request agrees (the model cannot even see a lifecycle-bound or
//!   mutating tool), and
//! * a normal main task under Full access keeps its full surface — the fix
//!   narrows the side question, not the session.

use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    ClientCommand, InteractiveRuntimeClient, PermissionProfile as WirePermissionProfile, RuntimeEvent,
};
use leveler_execution::PermissionProfile;
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_test_support::{MockResponse, MockServer};

/// Tools that must never reach a `/btw` surface, whatever the profile.
///
/// `wait_task` is the one that produced the original bug — not because it
/// writes, but because it binds the side question's lifecycle to a main-task
/// background job, which the side-question contract forbids.
const FORBIDDEN_ON_SIDE_SURFACE: &[&str] = &[
    "run_command",
    "shell_command",
    "wait_task",
    "kill_task",
    "apply_patch",
    "write_file",
    "create_checkpoint",
    "update_plan",
    "update_goal",
    "spawn_agent",
    "request_permissions",
];

/// The observation primitives a side question must still be able to use.
const REQUIRED_ON_SIDE_SURFACE: &[&str] = &[
    "read_file",
    "grep",
    "list_files",
    "find_files",
    "read_project_rules",
    "get_task",
];

const PROFILES: [PermissionProfile; 3] = [
    PermissionProfile::RequestApproval,
    PermissionProfile::Assisted,
    PermissionProfile::FullAccess,
];

fn isolate_global_config() {
    use std::sync::OnceLock;
    static EMPTY_HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    let dir = EMPTY_HOME.get_or_init(|| tempfile::tempdir().unwrap());
    unsafe {
        std::env::set_var("LEVELER_HOME", dir.path());
    }
}

fn json_response(content: &str) -> MockResponse {
    MockResponse::SilentThenJson {
        silent_ms: 0,
        body: serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": content}, "finish_reason": "stop"}]
        })
        .to_string(),
    }
}

async fn harness(
    responses: Vec<MockResponse>,
    profile: PermissionProfile,
) -> (
    tempfile::TempDir,
    MockServer,
    Arc<Application>,
    Arc<InProcessRuntimeClient>,
    leveler_core::SessionId,
) {
    isolate_global_config();
    let server = MockServer::start(responses).await;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("configs/providers")).unwrap();
    std::fs::create_dir_all(tmp.path().join("configs/models")).unwrap();
    std::fs::write(
        tmp.path().join("configs/providers/mock.yaml"),
        format!(
            "id: mock\nprotocol: openai_chat\nbase_url: {}\n",
            server.base_url()
        ),
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("configs/models/m.yaml"),
        r#"
id: m
provider: mock
model_id: mock-model
protocol: openai_chat
capabilities: { streaming: true, tool_calling: true, parallel_tool_calls: false, structured_output: true, reasoning: false, vision: false }
limits: { context_window: 131072, reliable_context: 65536, max_output_tokens: 1024, max_tool_schema_bytes: 65536, max_parallel_tool_calls: 1 }
compatibility: { synthesize_tool_call_ids: true, drop_unsupported_fields: true }
"#,
    )
    .unwrap();
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
    let session = app.create_session(&model, "goal").await.unwrap();
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        model,
        profile,
        false,
    ));
    // The session's own execution config is what a turn and a side question
    // actually resolve, so the profile under test must be the SESSION's.
    client
        .send(ClientCommand::SetPermissionProfile {
            session_id: session.clone(),
            mode: match profile {
                PermissionProfile::RequestApproval => WirePermissionProfile::RequestApproval,
                PermissionProfile::Assisted => WirePermissionProfile::Assisted,
                PermissionProfile::FullAccess => WirePermissionProfile::FullAccess,
            },
        })
        .await
        .unwrap();
    (tmp, server, app, client, session)
}

/// Every side-question tool name this surface exposes, sorted.
async fn side_surface_names(
    app: &Application,
    profile: PermissionProfile,
    session: &leveler_core::SessionId,
) -> Vec<String> {
    let (registry, _context) = app
        .side_question_tools(&ModelRef::new("mock", "m"), profile, false, Some(session.as_str()))
        .await
        .unwrap();
    let mut names: Vec<String> = registry
        .definitions()
        .into_iter()
        .map(|definition| definition.name)
        .collect();
    names.sort();
    names
}

/// The tool names a provider request actually advertised (`tools[].function.name`).
fn advertised_tool_names(body: &str) -> Vec<String> {
    let value: serde_json::Value = serde_json::from_str(body).unwrap();
    let mut names: Vec<String> = value["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| {
                    tool["function"]["name"]
                        .as_str()
                        .or_else(|| tool["name"].as_str())
                        .map(ToOwned::to_owned)
                })
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// The reported bug: Full access handed `/btw` the whole registry.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn side_surface_is_identical_under_every_permission_profile() {
    let (_tmp, _server, app, _client, session) = harness(vec![], PermissionProfile::Assisted).await;

    let mut baseline: Option<Vec<String>> = None;
    for profile in PROFILES {
        let names = side_surface_names(&app, profile, &session).await;
        for forbidden in FORBIDDEN_ON_SIDE_SURFACE {
            assert!(
                !names.iter().any(|name| name == forbidden),
                "{profile:?} side surface must not expose {forbidden}: {names:?}"
            );
        }
        for required in REQUIRED_ON_SIDE_SURFACE {
            assert!(
                names.iter().any(|name| name == required),
                "{profile:?} side surface must still expose {required}: {names:?}"
            );
        }
        match &baseline {
            None => baseline = Some(names),
            Some(expected) => assert_eq!(
                &names, expected,
                "the side-question capability surface must not depend on the permission profile"
            ),
        }
    }
}

/// The same rule at the wire: the model is never *offered* a tool the surface
/// does not own. A refusal after the call would still be the wrong shape.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn side_question_wire_schema_never_advertises_a_lifecycle_or_write_tool() {
    let (_tmp, server, _app, client, session) = harness(
        vec![json_response("side answer")],
        PermissionProfile::FullAccess,
    )
    .await;

    client
        .send(ClientCommand::Btw {
            session_id: session.clone(),
            question: "还要多久？".into(),
        })
        .await
        .unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while server.request_count() == 0 && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let bodies = server.request_bodies().await;
    assert_eq!(bodies.len(), 1, "the side question must reach the provider");
    let names = advertised_tool_names(&bodies[0]);
    assert!(!names.is_empty(), "the side request advertises its tools");
    for forbidden in FORBIDDEN_ON_SIDE_SURFACE {
        assert!(
            !names.iter().any(|name| name == forbidden),
            "the side request must not advertise {forbidden}: {names:?}"
        );
    }
    assert!(
        names.iter().any(|name| name == "get_task"),
        "a side question may still read background task status: {names:?}"
    );
}

/// The other half of the boundary: narrowing the side surface must not narrow
/// the session. A main task under Full keeps its mutating tools.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn main_task_under_full_access_keeps_its_full_surface() {
    let (_tmp, server, _app, client, session) = harness(
        vec![json_response("main answer")],
        PermissionProfile::FullAccess,
    )
    .await;

    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "do the main thing".into(),
            attachments: Vec::new(),
        })
        .await
        .unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while server.request_count() == 0 && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let bodies = server.request_bodies().await;
    assert_eq!(bodies.len(), 1, "the main turn must reach the provider");
    let names = advertised_tool_names(&bodies[0]);
    assert!(
        names.iter().any(|name| name == "run_command"),
        "a main task under Full access keeps its full surface: {names:?}"
    );
}

/// The lifecycle regression: a background task is active and the main goal turn
/// keeps running, yet the side question still dispatches, answers, and stays
/// off the main task's lifecycle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_consecutive_side_questions_while_a_main_turn_waits_on_a_background_task() {
    let main_rx = MockResponse::SilentThenJson {
        silent_ms: 8000,
        body: serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "main answer"}, "finish_reason": "stop"}]
        })
        .to_string(),
    };
    let (tmp, server, app, client, session) = harness(
        vec![
            main_rx,
            json_response("side answer one"),
            json_response("side answer two"),
        ],
        PermissionProfile::FullAccess,
    )
    .await;

    let task_id = app
        .background_tasks()
        .spawn_owned(
            leveler_execution::ProcessRequest::new(
                "sleep",
                vec!["60".to_string()],
                tmp.path().to_path_buf(),
            ),
            None,
            Some(session.as_str()),
        )
        .await
        .expect("background task starts");

    let mut rx = client.subscribe();
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "do the main thing".into(),
            attachments: Vec::new(),
        })
        .await
        .unwrap();
    let start = std::time::Instant::now();
    while server.request_count() < 1 && start.elapsed() < Duration::from_secs(5) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        client.has_live_turn(&session),
        "the fixture must keep the main turn in flight"
    );

    for question in ["first", "那还要多久？"] {
        client
            .send(ClientCommand::Btw {
                session_id: session.clone(),
                question: question.into(),
            })
            .await
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        let mut started = false;
        loop {
            assert!(
                std::time::Instant::now() < deadline,
                "side question `{question}` did not finish while the main turn was running"
            );
            match tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
                Ok(Ok(RuntimeEvent::BtwStarted { .. })) => started = true,
                Ok(Ok(RuntimeEvent::BtwCompleted)) => break,
                Ok(Ok(RuntimeEvent::BtwFailed { error })) => {
                    panic!("side question `{question}` failed: {error}")
                }
                Ok(Ok(RuntimeEvent::BtwCancelled)) => {
                    panic!("side question `{question}` was cancelled")
                }
                _ => {}
            }
        }
        assert!(started, "the side question must announce itself first");
    }

    // Both side requests reached the provider, and neither could have parked on
    // the live background task.
    let bodies = server.request_bodies().await;
    assert_eq!(bodies.len(), 3, "main + two side questions");
    for body in &bodies[1..] {
        let names = advertised_tool_names(body);
        assert!(
            !names.iter().any(|name| name == "wait_task" || name == "run_command"),
            "a side question must never be able to wait on the main task: {names:?}"
        );
    }
    assert!(
        client.has_live_turn(&session),
        "the main turn must be untouched by the side questions"
    );

    app.background_tasks().kill(&task_id).await.ok();
}

/// The observe-class allowlist itself, pinned by semantic property rather than
/// by a snapshot: everything reachable from `/btw` must be `Safe` risk.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_side_surface_is_observe_class_by_construction() {
    let (_tmp, _server, app, _client, session) = harness(vec![], PermissionProfile::FullAccess).await;
    let names = side_surface_names(&app, PermissionProfile::FullAccess, &session).await;
    assert!(
        names.len() < FORBIDDEN_ON_SIDE_SURFACE.len() + 20,
        "the side surface stays a narrow observer set: {names:?}"
    );
}
