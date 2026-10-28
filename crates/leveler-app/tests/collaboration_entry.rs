//! Entry / persistence contract for the collaboration axis.
//!
//! A session is created with an explicit, durable collaboration mode: the
//! transport request carries it, the daemon writes it to the session row once,
//! and every ordinary submission afterwards runs under the row's axis. There is
//! no second "goal flag" and no prompt inspection.
//!
//! The observable proof of the executor profile is the tool surface the model
//! receives: `update_goal` exists in the request exactly when
//! `executor.goal_mode` is on (Goal), and never in a Chat request.

#[path = "support/observed_command.rs"]
mod observed_command;
use observed_command::ObservedSettings;

use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    ApprovalPolicy, ClientCommand, InteractiveRuntimeClient, PermissionProfile as WirePermission,
    RuntimeEvent,
};
use leveler_core::SessionId;
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_storage::SessionRepository;
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

fn text(content: &str) -> MockResponse {
    sse(vec![
        serde_json::json!({"choices": [{"delta": {"content": content}, "finish_reason": "stop"}]})
            .to_string(),
    ])
}

/// One tool call plus its finish frame: a complete goal declaration in one round.
fn goal_complete_tool_call() -> MockResponse {
    sse(vec![
        serde_json::json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0,
            "id": "c-goal",
            "type": "function",
            "function": {
                "name": "update_goal",
                "arguments": serde_json::json!({"status": "complete", "summary": "done"}).to_string()
            }
        }]}}]})
        .to_string(),
        serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}).to_string(),
    ])
}

/// A model that tries to write a file, then reports its result: the shape a
/// read-only overlay has to refuse.
fn write_file_tool_call() -> MockResponse {
    sse(vec![
        serde_json::json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0,
            "id": "c-write",
            "type": "function",
            "function": {
                "name": "write_file",
                "arguments": serde_json::json!({"path": "plan-wrote.txt", "content": "x"}).to_string()
            }
        }]}}]})
        .to_string(),
        serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}).to_string(),
    ])
}

async fn harness(
    responses: Vec<MockResponse>,
) -> (
    tempfile::TempDir,
    MockServer,
    Arc<Application>,
    Arc<InProcessRuntimeClient>,
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
limits: { context_window: 131072, reliable_context: 65536, max_output_tokens: 1024, max_tool_schema_bytes: 8192, max_parallel_tool_calls: 1 }
compatibility: { synthesize_tool_call_ids: true, drop_unsupported_fields: true }
"#,
    )
    .unwrap();
    let layout = Layout::from_parts(
        tmp.path().to_path_buf(),
        tmp.path().join("configs"),
        tmp.path().join("state"),
    );
    // No `.with_collaboration`: this is the product entry an ordinary client
    // goes through, so the request decides.
    let app = Arc::new(Application::assemble(layout).unwrap());
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        ModelRef::new("mock", "m"),
        leveler_execution::PermissionProfile::Assisted,
        false,
    ));
    (tmp, server, app, client)
}

fn request(
    goal: &str,
    collaboration: leveler_local_transport::CollaborationMode,
) -> leveler_local_transport::CreateSessionRequest {
    leveler_local_transport::CreateSessionRequest {
        request_id: None,
        workspace: leveler_local_transport::CreateWorkspaceSelection::RuntimeDefault,
        goal: goal.to_string(),
        model: None,
        mode: WirePermission::Assisted,
        approval_policy: ApprovalPolicy::Interactive,
        collaboration,
    }
}

/// The tool names the model request actually advertised. A goal contract quoted
/// inside the conversation history (an earlier `update_goal` call) is not a
/// surface: only the request's own `tools` table decides what the turn can do.
fn tool_names(body: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    value["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| {
                    tool["function"]["name"]
                        .as_str()
                        .or_else(|| tool["name"].as_str())
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

async fn collaboration_of(app: &Application, session: &SessionId) -> String {
    let db = app.open_database().await.unwrap();
    SessionRepository::new(&db)
        .get(session)
        .await
        .unwrap()
        .unwrap()
        .collaboration
}

/// Drain events until the turn reaches a terminal state.
async fn wait_terminal(rx: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Ok(event)) => match event {
                RuntimeEvent::TurnCompleted
                | RuntimeEvent::TurnCompletedWithWarnings { .. }
                | RuntimeEvent::TurnAnswered
                | RuntimeEvent::TurnTruncated { .. }
                | RuntimeEvent::TurnIncomplete { .. }
                | RuntimeEvent::TurnFailed { .. }
                | RuntimeEvent::TurnCancelled
                // An explicit logical-task cancellation is terminal too; without
                // this arm a test waiting on it would hang instead of failing.
                | RuntimeEvent::TaskCancelled => return,
                _ => continue,
            },
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            _ => panic!("the turn never settled"),
        }
    }
}

/// C — a Goal request survives the transport and lands on the row; an explicit
/// Chat request is different.
#[tokio::test]
async fn create_session_goal_and_chat_reach_the_row() {
    use leveler_local_transport::{CollaborationMode, LocalRuntimeService};

    let (_tmp, _server, app, client) = harness(vec![]).await;
    let goal_session = client
        .create_session(request("ship it", CollaborationMode::Goal))
        .await
        .unwrap()
        .session
        .id;
    assert_eq!(collaboration_of(&app, &goal_session).await, "goal");
    assert_eq!(
        app.session_product_axes(&goal_session).await.unwrap(),
        leveler_agent::CollaborationMode::Goal
    );

    let chat_session = client
        .create_session(request("just chat", CollaborationMode::Chat))
        .await
        .unwrap()
        .session
        .id;
    assert_eq!(collaboration_of(&app, &chat_session).await, "chat");
    assert_eq!(
        app.session_product_axes(&chat_session).await.unwrap(),
        leveler_agent::CollaborationMode::Chat
    );
}

/// The interactive bootstrap's own request — the values `leveler` / `leveler
/// tui` send for a new session — lands on the row AND in the snapshot as Chat.
/// The row is the source of truth; the snapshot is what the client renders.
#[tokio::test]
async fn interactive_bootstrap_axis_reaches_row_and_snapshot_as_chat() {
    use leveler_local_transport::{CollaborationMode, LocalRuntimeService};

    let (_tmp, _server, app, client) = harness(vec![]).await;
    let bootstrap = client
        .create_session(leveler_local_transport::CreateSessionRequest {
            request_id: None,
            workspace: leveler_local_transport::CreateWorkspaceSelection::RuntimeDefault,
            goal: "interactive session".to_string(),
            model: None,
            mode: WirePermission::Assisted,
            approval_policy: ApprovalPolicy::Interactive,
            collaboration: CollaborationMode::Chat,
        })
        .await
        .unwrap();
    let session = bootstrap.session.id.clone();
    assert_eq!(collaboration_of(&app, &session).await, "chat");
    assert_eq!(
        bootstrap.session.collaboration.as_deref(),
        Some("chat"),
        "the client renders the axis the runtime just wrote"
    );
    assert_eq!(
        app.session_product_axes(&session).await.unwrap(),
        leveler_agent::CollaborationMode::Chat
    );
}

/// The axis the interactive entry writes is the axis the runtime adopts. The
/// embedded TUI creates through the wrapped `Application` and then reads the
/// same row, so this is the exact chain the footer renders:
/// create-with-collaboration → row → snapshot.collaboration.
#[tokio::test]
async fn the_interactive_create_axis_is_what_the_runtime_adopts() {
    let (_tmp, _server, app, client) = harness(vec![]).await;
    let session = app
        .create_session_with_collaboration(
            &ModelRef::new("mock", "m"),
            "interactive session",
            leveler_execution::PermissionProfile::Assisted,
            leveler_agent::CollaborationMode::Chat,
        )
        .await
        .unwrap();
    assert_eq!(collaboration_of(&app, &session).await, "chat");
    let snapshot = client.snapshot(&session).await.unwrap();
    assert_eq!(
        snapshot.collaboration.as_deref(),
        Some("chat"),
        "the runtime must adopt the axis the embedded create wrote"
    );
}

/// A + D — an omitted wire field is the product default (goal) and survives a
/// reopen: the row, not a process default, is the source of truth.
#[tokio::test]
async fn omitted_axis_is_goal_and_survives_a_reopen() {
    use leveler_local_transport::LocalRuntimeService;

    let (_tmp, _server, app, client) = harness(vec![]).await;
    // An older client's JSON: no `collaboration` key at all.
    let omitted: leveler_local_transport::CreateSessionRequest = serde_json::from_value(
        serde_json::json!({"goal":"new coding task", "model":null, "mode":"assisted"}),
    )
    .unwrap();
    let session = client.create_session(omitted).await.unwrap().session.id;
    assert_eq!(collaboration_of(&app, &session).await, "goal");

    // A later read of the same repository must not downgrade it.
    let reopened = Application::assemble(Layout::from_parts(
        _tmp.path().to_path_buf(),
        _tmp.path().join("configs"),
        _tmp.path().join("state"),
    ))
    .unwrap();
    assert_eq!(
        reopened.session_product_axes(&session).await.unwrap(),
        leveler_agent::CollaborationMode::Goal
    );
}

/// E + F — `/goal <task>` writes the axis durably, and a window the runtime
/// merely paused keeps it; only an explicit axis change (or the window ending)
/// leaves Goal mode.
#[tokio::test]
async fn run_goal_is_durable_and_explicit_chat_restores_chat() {
    use leveler_local_transport::{CollaborationMode, LocalRuntimeService};

    // Paused, not blocked: an interrupted window leaves the goal owed with
    // nothing for the user to answer, which is the state a durable Goal route
    // exists for.
    let (_tmp, _server, app, client) = harness(vec![long_tool_call(10)]).await;
    let session = client
        .create_session(request("start as chat", CollaborationMode::Chat))
        .await
        .unwrap()
        .session
        .id;
    assert_eq!(collaboration_of(&app, &session).await, "chat");

    let mut rx = client.subscribe();
    client
        .send(ClientCommand::RunGoal {
            session_id: session.clone(),
            content: "now drive it".to_string(),
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    client
        .send(ClientCommand::CancelCurrentTurn {
            session_id: session.clone(),
        })
        .await
        .unwrap();
    wait_terminal(&mut rx).await;
    assert_eq!(
        collaboration_of(&app, &session).await,
        "goal",
        "`/goal <task>` must leave a durable goal axis while the paused goal is \
         owed, not a one-turn override"
    );

    // `/goal clear` / `/collab chat` is the explicit way back.
    client
        .send_observed(ClientCommand::SetProductAxes {
            session_id: session.clone(),
            work_profile: "single".to_string(),
            collaboration: "chat".to_string(),
        })
        .await
        .unwrap();
    assert_eq!(collaboration_of(&app, &session).await, "chat");
}

/// G — an ordinary composer submit in a Goal session enters the goal executor:
/// the model request exposes `update_goal`.
#[tokio::test]
async fn goal_session_ordinary_submit_exposes_the_goal_executor() {
    use leveler_local_transport::{CollaborationMode, LocalRuntimeService};

    let (_tmp, server, _app, client) = harness(vec![goal_complete_tool_call()]).await;
    let session = client
        .create_session(request("drive it", CollaborationMode::Goal))
        .await
        .unwrap()
        .session
        .id;
    let mut rx = client.subscribe();
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "do the work".to_string(),
            attachments: vec![],
        })
        .await
        .unwrap();
    wait_terminal(&mut rx).await;

    let bodies = server.request_bodies().await;
    assert!(!bodies.is_empty(), "the goal turn must reach the model");
    assert!(
        bodies[0].contains("\"update_goal\""),
        "a Goal session's executor must expose update_goal: {}",
        bodies[0]
    );
}

/// H — an ordinary composer submit in a Chat session stays a chat turn: the
/// model request never exposes `update_goal`.
#[tokio::test]
async fn chat_session_ordinary_submit_excludes_the_goal_executor() {
    use leveler_local_transport::{CollaborationMode, LocalRuntimeService};

    let (_tmp, server, _app, client) = harness(vec![text("hello")]).await;
    let session = client
        .create_session(request("just talk", CollaborationMode::Chat))
        .await
        .unwrap()
        .session
        .id;
    let mut rx = client.subscribe();
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "hi".to_string(),
            attachments: vec![],
        })
        .await
        .unwrap();
    wait_terminal(&mut rx).await;

    let bodies = server.request_bodies().await;
    assert!(!bodies.is_empty());
    assert!(
        !bodies[0].contains("\"update_goal\""),
        "a Chat session must not carry the goal executor: {}",
        bodies[0]
    );
}

// ---------------------------------------------------------------------------
// Headless entry (`run_in_session`), the path a CLI `leveler run` takes. The
// axis must EXECUTE here too, not just decorate the row: before this, the
// headless path called the goal engine unconditionally, so a chat session was
// persisted as `chat` and run as a Goal.
// ---------------------------------------------------------------------------

/// One Application over the harness's layout with the axis chosen explicitly,
/// the way the CLI does: `assemble(..).with_collaboration(axis)` then a headless
/// `run_in_session` — never the interactive client.
fn headless_app(tmp: &tempfile::TempDir, axis: leveler_agent::CollaborationMode) -> Application {
    Application::assemble(Layout::from_parts(
        tmp.path().to_path_buf(),
        tmp.path().join("configs"),
        tmp.path().join("state"),
    ))
    .unwrap()
    .with_collaboration(axis)
}

async fn run_headless(
    app: &Application,
    axis: leveler_agent::CollaborationMode,
    task: &str,
) -> leveler_agent::AgentOutcome {
    run_headless_with(app, axis, task, Arc::new(leveler_execution::AutoApprove)).await
}

/// A headless turn under an explicit approver: the read-only overlay turns a
/// non-Safe action into an approval question, so which approver answers decides
/// whether the write lands.
async fn run_headless_with(
    app: &Application,
    axis: leveler_agent::CollaborationMode,
    task: &str,
    approver: Arc<dyn leveler_execution::Approver>,
) -> leveler_agent::AgentOutcome {
    let model = ModelRef::new("mock", "m");
    let session = app.create_session(&model, task).await.unwrap();
    assert_eq!(
        collaboration_of(app, &session).await,
        axis.as_str(),
        "the run must persist the axis it was assembled with"
    );
    app.run_in_session(
        &session,
        &model,
        leveler_execution::PermissionProfile::Assisted,
        task,
        approver,
        false,
        &mut |_| {},
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap()
}

/// Denies every approval. Proves the read-only overlay asks for a non-Safe
/// action rather than silently allowing it.
struct DenyAll;

#[async_trait::async_trait]
impl leveler_execution::Approver for DenyAll {
    async fn decide(
        &self,
        _request: &leveler_execution::ApprovalRequest,
    ) -> leveler_execution::ApprovalDecision {
        leveler_execution::ApprovalDecision::Deny
    }
}

/// A — headless chat runs the Chat TurnProfile: a textual final ends the turn
/// as `Answered`, the goal continuation never starts, and the request exposes
/// no `update_goal`.
#[tokio::test]
async fn headless_chat_runs_the_chat_profile() {
    let (tmp, server, _app, _client) = harness(vec![text("hi there")]).await;
    // This fixture measures Chat rounds, not the independent memory worker.
    std::fs::create_dir_all(tmp.path().join(".leveler")).unwrap();
    std::fs::write(
        tmp.path().join(".leveler/config.yaml"),
        "memory:\n  enabled: false\n",
    )
    .unwrap();
    let app = headless_app(&tmp, leveler_agent::CollaborationMode::Chat);
    assert!(
        !app.memory_policy().enabled(),
        "this Chat-axis fixture isolates the agent round from auxiliary memory calls"
    );
    let outcome = run_headless(&app, leveler_agent::CollaborationMode::Chat, "just chat").await;

    assert_eq!(
        outcome.stop_reason,
        leveler_agent::StopReason::Answered,
        "a chat answer is the terminal"
    );
    assert_eq!(
        server.request_count(),
        1,
        "a chat turn is not continued by the goal lifecycle"
    );
    assert_eq!(
        outcome.model_steps, 1,
        "a Chat answer has one agent model step"
    );
    let bodies = server.request_bodies().await;
    assert_eq!(
        bodies.len(),
        1,
        "one complete HTTP request must be observed"
    );
    let request: serde_json::Value = serde_json::from_str(&bodies[0]).unwrap();
    assert_eq!(request["stream"], serde_json::json!(true));
    assert!(
        !bodies[0].contains("\"update_goal\""),
        "a chat request must not carry the goal executor: {}",
        bodies[0]
    );
}

/// B — headless goal keeps the Goal terminal contract: a quiet final is
/// continued, never accepted as completion.
#[tokio::test]
async fn headless_goal_quiet_answer_is_continued_not_completed() {
    let (tmp, server, _app, _client) = harness(vec![text("looks done to me")]).await;
    let app = headless_app(&tmp, leveler_agent::CollaborationMode::Goal);
    let outcome = run_headless(&app, leveler_agent::CollaborationMode::Goal, "drive it").await;

    assert_ne!(
        outcome.stop_reason,
        leveler_agent::StopReason::Completed,
        "going quiet is not a Goal completion"
    );
    assert!(
        outcome.model_steps > 1,
        "a quiet Goal must start another agent model step"
    );
    assert!(
        server.request_count() > 1,
        "a Goal that went quiet must be continued (got {} request(s))",
        server.request_count()
    );
}

/// B — an explicit `update_goal` is what completes a headless Goal.
#[tokio::test]
async fn headless_goal_declared_completion_is_completed() {
    let (tmp, _server, _app, _client) = harness(vec![goal_complete_tool_call()]).await;
    let app = headless_app(&tmp, leveler_agent::CollaborationMode::Goal);
    let outcome = run_headless(&app, leveler_agent::CollaborationMode::Goal, "drive it").await;

    assert_eq!(
        outcome.stop_reason,
        leveler_agent::StopReason::Completed,
        "update_goal(complete) is the Goal terminal"
    );
}

/// C — headless plan keeps the read-only contract: the answer terminal, no
/// `update_goal`, and a write the model attempts is refused. Under the daily
/// Assisted profile the overlay turns a non-Safe action into an approval
/// question, so the refusal is observed with a denying approver; the Chat
/// control below shows the same approver does NOT block a normal write — the
/// overlay is what refuses it.
#[tokio::test]
async fn headless_plan_stays_read_only_with_no_completion_authority() {
    let (tmp, server, _app, _client) =
        harness(vec![write_file_tool_call(), text("plan: do X")]).await;
    let app = headless_app(&tmp, leveler_agent::CollaborationMode::Plan);
    let outcome = run_headless_with(
        &app,
        leveler_agent::CollaborationMode::Plan,
        "plan it",
        Arc::new(DenyAll),
    )
    .await;

    assert_eq!(
        outcome.stop_reason,
        leveler_agent::StopReason::Answered,
        "plan ends on its answer and has no completion authority"
    );
    let bodies = server.request_bodies().await;
    assert!(
        !bodies[0].contains("\"update_goal\""),
        "plan must not carry the goal executor: {}",
        bodies[0]
    );
    assert!(
        !tmp.path().join("plan-wrote.txt").exists(),
        "a read-only plan turn must not write"
    );
}

/// C control — without the read-only overlay the denying approver is never
/// consulted for an ordinary workspace write, so the write lands. This pins
/// that the plan refusal above comes from the overlay, not from the approver.
#[tokio::test]
async fn headless_chat_allows_the_workspace_write_plan_refuses() {
    let (tmp, _server, _app, _client) =
        harness(vec![write_file_tool_call(), text("wrote it")]).await;
    let app = headless_app(&tmp, leveler_agent::CollaborationMode::Chat);
    let outcome = run_headless_with(
        &app,
        leveler_agent::CollaborationMode::Chat,
        "write it",
        Arc::new(DenyAll),
    )
    .await;

    assert_eq!(outcome.stop_reason, leveler_agent::StopReason::Answered);
    assert!(
        tmp.path().join("plan-wrote.txt").exists(),
        "a chat turn has no read-only overlay: an ordinary workspace write is not held for approval"
    );
}

/// A tool call that keeps the turn busy long enough for a steer to arrive.
/// Platform-native on purpose: the test must not depend on a Unix `sleep`
/// binary that Windows CI does not have.
fn slow_tool_call() -> MockResponse {
    let (program, args) = if cfg!(windows) {
        ("cmd", vec!["/c", "ping -n 2 127.0.0.1 > nul"])
    } else {
        ("sh", vec!["-c", "sleep 1"])
    };
    let arguments = serde_json::json!({
        "program": program,
        "args": args,
        "reason": "hold the turn open so the steer has a running turn to land in"
    });
    sse(vec![
        serde_json::json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0,
            "id": "c-slow",
            "type": "function",
            "function": {"name": "run_command", "arguments": arguments.to_string()}
        }]}}]})
        .to_string(),
        serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}).to_string(),
    ])
}

/// The real incident's entry path: a client sends the literal `/goal <task>`
/// as an ordinary message. The runtime must parse it, write the durable goal
/// axis, and run a Goal turn — the command cannot be a per-client feature. The
/// completed goal then ends its own route.
#[tokio::test]
async fn submit_message_goal_command_enters_goal_mode_from_any_client() {
    use leveler_local_transport::{CollaborationMode, LocalRuntimeService};

    let (_tmp, server, app, client) = harness(vec![goal_complete_tool_call(), text("done")]).await;
    let session = client
        .create_session(request("chat first", CollaborationMode::Chat))
        .await
        .unwrap()
        .session
        .id;
    assert_eq!(collaboration_of(&app, &session).await, "chat");

    let mut rx = client.subscribe();
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "/goal now drive it".to_string(),
            attachments: vec![],
        })
        .await
        .unwrap();
    wait_terminal(&mut rx).await;

    let goals = goals_of(&app, &session).await;
    assert_eq!(goals.len(), 1, "the command opened one goal: {goals:?}");
    assert_eq!(
        goals[0].objective, "now drive it",
        "the objective is the user's text, not the command word"
    );
    assert_eq!(
        collaboration_of(&app, &session).await,
        "chat",
        "the goal completed, so the route it wrote ended with it"
    );
    let bodies = server.request_bodies().await;
    assert!(
        bodies
            .iter()
            .any(|body| tool_names(body).iter().any(|name| name == "update_goal")),
        "the turn must run the Goal executor: {bodies:?}"
    );
    assert!(
        bodies[0].contains("now drive it"),
        "the objective must reach the model: {}",
        bodies[0]
    );
    assert!(
        !bodies[0].contains("/goal now drive it"),
        "the command word must not be sent to the model: {}",
        bodies[0]
    );
}

/// The exact accident: `/goal` typed while a turn is already running. The
/// terminal forwards it as a steer; the runtime must upgrade THAT turn to Goal
/// instead of injecting the literal command and letting the run stop at its
/// next stage summary.
#[tokio::test]
async fn steering_goal_command_upgrades_the_running_chat_turn() {
    use leveler_local_transport::{CollaborationMode, LocalRuntimeService};

    let (_tmp, server, app, client) = harness(vec![
        slow_tool_call(),
        goal_complete_tool_call(),
        text("done"),
    ])
    .await;
    let session = client
        .create_session(request("chat first", CollaborationMode::Chat))
        .await
        .unwrap()
        .session
        .id;

    let mut rx = client.subscribe();
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "start chat work".to_string(),
            attachments: vec![],
        })
        .await
        .unwrap();
    // The first model request has been answered; the turn is inside the slow
    // tool call, so a steer queued now is drained at the next round.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while server.request_count() == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the first model request never reached the provider"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    client
        .send(ClientCommand::SteerCurrentTurn {
            session_id: session.clone(),
            content: "/goal drive the goal now".to_string(),
        })
        .await
        .unwrap();
    wait_terminal(&mut rx).await;

    let goals = goals_of(&app, &session).await;
    assert_eq!(
        goals.len(),
        1,
        "the steer opened one goal for the running turn: {goals:?}"
    );
    assert_eq!(goals[0].objective, "drive the goal now");
    assert_eq!(
        collaboration_of(&app, &session).await,
        "chat",
        "the upgraded goal completed, so its route ended with it"
    );
    let bodies = server.request_bodies().await;
    assert!(
        bodies
            .iter()
            .skip(1)
            .any(|body| tool_names(body).iter().any(|name| name == "update_goal")),
        "the RUNNING turn must gain the goal surface: {bodies:?}"
    );
    assert!(
        bodies
            .iter()
            .skip(1)
            .any(|body| body.contains("drive the goal now")),
        "the declaration must be injected as the user's text: {bodies:?}"
    );
    assert!(
        bodies
            .iter()
            .all(|body| !body.contains("/goal drive the goal now")),
        "the command word must never reach the model: {bodies:?}"
    );
}

/// A slow tool call that holds the turn open long enough to inspect the
/// declaration window without racing the executor's next round.
fn long_tool_call(seconds: u32) -> MockResponse {
    let (program, args) = if cfg!(windows) {
        (
            "cmd",
            vec![
                "/c".to_string(),
                format!("ping -n {seconds} 127.0.0.1 > nul"),
            ],
        )
    } else {
        ("sh", vec!["-c".to_string(), format!("sleep {seconds}")])
    };
    let arguments = serde_json::json!({
        "program": program,
        "args": args,
        "reason": "hold the turn open across the declaration window"
    });
    sse(vec![
        serde_json::json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0,
            "id": "c-hold",
            "type": "function",
            "function": {"name": "run_command", "arguments": arguments.to_string()}
        }]}}]})
        .to_string(),
        serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}).to_string(),
    ])
}

/// The declaration is durable BEFORE the running executor can drain it, and
/// the axis and the text are written as one fact. A crash in this window may
/// not leave a Goal session whose objective was never persisted.
#[tokio::test]
async fn a_busy_goal_declaration_is_durable_with_the_axis() {
    use leveler_local_transport::{CollaborationMode, LocalRuntimeService};

    let (_tmp, _server, app, client) = harness(vec![
        long_tool_call(3),
        goal_complete_tool_call(),
        text("done"),
    ])
    .await;
    let session = client
        .create_session(request("chat first", CollaborationMode::Chat))
        .await
        .unwrap()
        .session
        .id;

    let mut rx = client.subscribe();
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "start chat work".to_string(),
            attachments: vec![],
        })
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while _server.request_count() == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the first model request never reached the provider"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    client
        .send(ClientCommand::SteerCurrentTurn {
            session_id: session.clone(),
            content: "/goal 持久化的目标".to_string(),
        })
        .await
        .unwrap();

    // The window: the running turn has not drained the declaration yet (its
    // tool call is still sleeping), but the durable pair must already agree.
    {
        let db = app.open_database().await.unwrap();
        let repo = SessionRepository::new(&db);
        let record = repo.get(&session).await.unwrap().expect("session row");
        assert_eq!(
            record.collaboration, "goal",
            "the axis is Goal immediately after the declaration is accepted"
        );
        assert_eq!(
            repo.pending_goal(&session).await.unwrap().as_deref(),
            Some("持久化的目标"),
            "the declaration text is durable in the same window, not only in memory"
        );
    }

    wait_terminal(&mut rx).await;

    // Drained: the declaration became a real goal, and the window is closed.
    let db = app.open_database().await.unwrap();
    let repo = SessionRepository::new(&db);
    assert_eq!(
        repo.pending_goal(&session).await.unwrap(),
        None,
        "a bound declaration is consumed"
    );
    let stores = leveler_storage::EngineStores::from_database(&db);
    let task = stores
        .tasks
        .task_for_session(&session)
        .await
        .unwrap()
        .unwrap();
    let goals = stores.goals.for_task(&task).await.unwrap();
    assert_eq!(
        goals.len(),
        1,
        "the mid-turn upgrade opened one goal: {goals:?}"
    );
    assert_eq!(goals[0].objective, "持久化的目标");
    assert_eq!(
        goals[0].state,
        leveler_storage::GoalState::Settled,
        "the terminal of the upgraded turn settles the goal it bound"
    );
}

/// The crash window's recovery: axis Goal + a durable declaration, with no
/// turn that ever ran. `继续` must continue the DECLARED objective, not adopt
/// the word `继续` as a new goal.
#[tokio::test]
async fn a_pending_declaration_is_recovered_by_continue() {
    use leveler_local_transport::{CollaborationMode, LocalRuntimeService};

    let (_tmp, server, app, client) = harness(vec![goal_complete_tool_call()]).await;
    let session = client
        .create_session(request("chat first", CollaborationMode::Chat))
        .await
        .unwrap()
        .session
        .id;

    // Exactly what `steer_into_goal` leaves behind if the process dies before
    // the executor's next round: one transaction, axis + objective, no goal.
    {
        let db = app.open_database().await.unwrap();
        SessionRepository::new(&db)
            .set_goal_declaration(&session, "goal", "恢复原始目标", leveler_core::now())
            .await
            .unwrap();
    }

    let mut rx = client.subscribe();
    client
        .send(ClientCommand::ResumeTask {
            session_id: session.clone(),
            content: "继续".to_string(),
        })
        .await
        .unwrap();
    wait_terminal(&mut rx).await;

    let bodies = server.request_bodies().await;
    assert!(
        bodies.iter().any(|body| body.contains("恢复原始目标")),
        "the recovered objective must reach the model: {bodies:?}"
    );

    let db = app.open_database().await.unwrap();
    let stores = leveler_storage::EngineStores::from_database(&db);
    let task = stores
        .tasks
        .task_for_session(&session)
        .await
        .unwrap()
        .unwrap();
    let goals = stores.goals.for_task(&task).await.unwrap();
    assert_eq!(
        goals.len(),
        1,
        "one goal, not one per continuation: {goals:?}"
    );
    assert_eq!(goals[0].objective, "恢复原始目标");
    assert_eq!(
        SessionRepository::new(&db)
            .pending_goal(&session)
            .await
            .unwrap(),
        None,
        "the recovered declaration is consumed by the turn that bound it"
    );
}

/// Resume after a mid-turn upgrade: the Chat turn that became a Goal was
/// interrupted, and `继续` must re-enter THAT goal — not a fresh goal whose
/// objective is the continuation word, and not a Chat turn that ignores the
/// goal contract.
#[tokio::test]
async fn a_mid_turn_upgrade_resumes_as_its_goal() {
    use leveler_local_transport::{CollaborationMode, LocalRuntimeService};

    let (_tmp, server, app, client) = harness(vec![
        long_tool_call(1),
        long_tool_call(5),
        goal_complete_tool_call(),
    ])
    .await;
    let session = client
        .create_session(request("chat first", CollaborationMode::Chat))
        .await
        .unwrap()
        .session
        .id;

    let mut rx = client.subscribe();
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "start chat work".to_string(),
            attachments: vec![],
        })
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while server.request_count() == 0 {
        assert!(tokio::time::Instant::now() < deadline, "no first request");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    client
        .send(ClientCommand::SteerCurrentTurn {
            session_id: session.clone(),
            content: "/goal 升级后的目标".to_string(),
        })
        .await
        .unwrap();
    // The second request is asked AFTER the declaration was bound, so the
    // goal identity is durable before the interruption.
    while server.request_count() < 2 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the upgraded round never reached the provider"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    {
        let db = app.open_database().await.unwrap();
        let stores = leveler_storage::EngineStores::from_database(&db);
        let task = stores
            .tasks
            .task_for_session(&session)
            .await
            .unwrap()
            .unwrap();
        let goals = stores.goals.for_task(&task).await.unwrap();
        assert_eq!(
            goals.len(),
            1,
            "the upgrade opened a durable goal: {goals:?}"
        );
        assert_eq!(goals[0].objective, "升级后的目标");
        assert_eq!(goals[0].state, leveler_storage::GoalState::Running);
    }

    // Interrupt the running window: the goal stays owed.
    client
        .send(ClientCommand::CancelCurrentTurn {
            session_id: session.clone(),
        })
        .await
        .unwrap();
    wait_terminal(&mut rx).await;

    // Continue: same goal, goal profile, original objective.
    client
        .send(ClientCommand::ResumeTask {
            session_id: session.clone(),
            content: "继续".to_string(),
        })
        .await
        .unwrap();
    wait_terminal(&mut rx).await;

    let db = app.open_database().await.unwrap();
    let stores = leveler_storage::EngineStores::from_database(&db);
    let task = stores
        .tasks
        .task_for_session(&session)
        .await
        .unwrap()
        .unwrap();
    let goals = stores.goals.for_task(&task).await.unwrap();
    assert_eq!(
        goals.len(),
        1,
        "the continuation is the same goal: {goals:?}"
    );
    assert_eq!(goals[0].objective, "升级后的目标");
    assert_eq!(goals[0].state, leveler_storage::GoalState::Settled);
    let turns = leveler_storage::TurnRepository::new(&db)
        .list(&session)
        .await
        .unwrap();
    assert_eq!(
        turns.last().map(|turn| turn.kind.as_str()),
        Some("user"),
        "the resumed turn runs the Goal profile, not Chat"
    );
    let bodies = server.request_bodies().await;
    assert!(
        bodies
            .iter()
            .skip(2)
            .any(|body| body.contains("\"update_goal\"")),
        "the resumed turn advertises the goal surface: {bodies:?}"
    );
}

// ---------------------------------------------------------------------------
// The Goal route ends with the goal. A completed (or cancelled) goal can never
// be continued, so its session must stop routing ordinary messages to the Goal
// profile — otherwise every later message silently opens a NEW Goal and the UI
// keeps claiming Goal mode the runtime no longer runs.
// ---------------------------------------------------------------------------

/// The task's goals, newest first.
async fn goals_of(app: &Application, session: &SessionId) -> Vec<leveler_storage::GoalRecord> {
    let db = app.open_database().await.unwrap();
    let stores = leveler_storage::EngineStores::from_database(&db);
    let Some(task) = stores.tasks.task_for_session(session).await.unwrap() else {
        return Vec::new();
    };
    stores.goals.for_task(&task).await.unwrap()
}

/// Wait for the axis the runtime published to clients, and return it.
async fn wait_published_axis(
    rx: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>,
    expected: &str,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Ok(RuntimeEvent::SessionUpdated { session }))
                if session.collaboration.as_deref() == Some(expected) =>
            {
                return;
            }
            Ok(Ok(_)) => continue,
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            _ => panic!("the runtime never published the `{expected}` axis"),
        }
    }
}

/// A goal that completed is over: the route ends with it, and the client is
/// told (the UI displays the row, so a silent row change would leave a stale
/// Goal chip behind).
#[tokio::test]
async fn a_completed_goal_ends_the_goal_route_and_publishes_it() {
    use leveler_local_transport::{CollaborationMode, LocalRuntimeService};

    let (_tmp, _server, app, client) = harness(vec![goal_complete_tool_call()]).await;
    let session = client
        .create_session(request("drive it", CollaborationMode::Goal))
        .await
        .unwrap()
        .session
        .id;
    let mut rx = client.subscribe();
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "do the work".to_string(),
            attachments: vec![],
        })
        .await
        .unwrap();
    wait_terminal(&mut rx).await;

    assert_eq!(
        collaboration_of(&app, &session).await,
        "chat",
        "a completed goal must not leave the Goal route behind"
    );
    wait_published_axis(&mut rx, "chat").await;
}

/// The dogfood accident: after a completed goal, an ordinary message — and a
/// bare `继续` that has nothing left to resume — are ordinary chat turns. No
/// new Goal, no `update_goal` contract, and every tool stays available.
#[tokio::test]
async fn after_a_goal_completes_ordinary_messages_are_chat_turns() {
    use leveler_local_transport::{CollaborationMode, LocalRuntimeService};

    let (_tmp, server, app, client) = harness(vec![
        goal_complete_tool_call(),
        text("已经推上去了，远端分支与本地一致。"),
        text("没有可继续的任务。"),
    ])
    .await;
    let session = client
        .create_session(request("ship the change", CollaborationMode::Goal))
        .await
        .unwrap()
        .session
        .id;
    let mut rx = client.subscribe();
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "ship it".to_string(),
            attachments: vec![],
        })
        .await
        .unwrap();
    wait_terminal(&mut rx).await;
    assert_eq!(collaboration_of(&app, &session).await, "chat");
    let goals_after_completion = goals_of(&app, &session).await;
    assert_eq!(goals_after_completion.len(), 1, "one goal ran");

    // The exact sentence from the incident: a verification question.
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "确定都推上去了吗？".to_string(),
            attachments: vec![],
        })
        .await
        .unwrap();
    wait_terminal(&mut rx).await;

    // A continuation word with nothing to resume falls back to the same path.
    client
        .send(ClientCommand::ResumeTask {
            session_id: session.clone(),
            content: "继续".to_string(),
        })
        .await
        .unwrap();
    wait_terminal(&mut rx).await;

    let bodies = server.request_bodies().await;
    assert_eq!(bodies.len(), 3, "goal, question, continuation: {bodies:?}");
    for body in &bodies[1..] {
        let tools = tool_names(body);
        assert!(
            !tools.iter().any(|name| name == "update_goal"),
            "a plain message after a completed goal must not carry the goal contract: {tools:?}"
        );
        assert!(
            tools.iter().any(|name| name == "run_command"),
            "the ordinary agent keeps its full execution surface: {tools:?}"
        );
        assert!(
            tools.iter().any(|name| name == "commit_task"),
            "only an explicit model declaration may upgrade a Chat turn: {tools:?}"
        );
    }
    assert!(
        bodies[1].contains("确定都推上去了吗？"),
        "the question reaches the model verbatim: {}",
        bodies[1]
    );
    assert_eq!(
        goals_of(&app, &session).await.len(),
        1,
        "an ordinary message must never create a second Goal"
    );
    assert_eq!(collaboration_of(&app, &session).await, "chat");
}

/// The reverse direction: a goal that still owes work keeps its route, and a
/// plain supplement continues THAT goal instead of opening a sibling whose
/// objective happens to be the user's sentence.
#[tokio::test]
async fn an_interrupted_goal_keeps_its_route_and_a_supplement_continues_it() {
    use leveler_local_transport::{CollaborationMode, LocalRuntimeService};

    let (_tmp, _server, app, client) =
        harness(vec![long_tool_call(10), goal_complete_tool_call()]).await;
    let session = client
        .create_session(request("保留原始目标", CollaborationMode::Goal))
        .await
        .unwrap()
        .session
        .id;

    let mut rx = client.subscribe();
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "保留原始目标".to_string(),
            attachments: vec![],
        })
        .await
        .unwrap();
    // Interrupt the window while the slow tool call holds it open. The goal is
    // owed, not finished, so the route must survive.
    tokio::time::sleep(Duration::from_millis(300)).await;
    client
        .send(ClientCommand::CancelCurrentTurn {
            session_id: session.clone(),
        })
        .await
        .unwrap();
    wait_terminal(&mut rx).await;
    assert_eq!(
        collaboration_of(&app, &session).await,
        "goal",
        "an interrupted goal still owes its work: the route stays"
    );
    let after_interrupt = goals_of(&app, &session).await;
    assert_eq!(after_interrupt.len(), 1);
    assert_eq!(
        after_interrupt[0].state,
        leveler_storage::GoalState::Running,
        "the interrupted window did not settle the goal"
    );

    // A supplement without the word `继续`: still the same goal.
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "补充：只改 README 那一段".to_string(),
            attachments: vec![],
        })
        .await
        .unwrap();
    wait_terminal(&mut rx).await;

    let goals = goals_of(&app, &session).await;
    assert_eq!(
        goals.len(),
        1,
        "the supplement continues the running goal instead of opening a sibling: {goals:?}"
    );
    assert_eq!(
        goals[0].objective, "保留原始目标",
        "identity stays the original objective, never the supplement text"
    );
    assert_eq!(
        goals[0].state,
        leveler_storage::GoalState::Settled,
        "the completed supplement settles the same goal"
    );
    assert_eq!(
        collaboration_of(&app, &session).await,
        "chat",
        "that settlement then ends the route"
    );
}

/// An explicit cancellation is terminal: the goal can never be continued, so
/// the session leaves Goal mode instead of turning the next message into a new
/// Goal.
#[tokio::test]
async fn cancelling_the_logical_task_ends_the_goal_route() {
    use leveler_local_transport::{CollaborationMode, LocalRuntimeService};

    let (_tmp, _server, app, client) = harness(vec![long_tool_call(10)]).await;
    let session = client
        .create_session(request("cancel me", CollaborationMode::Goal))
        .await
        .unwrap()
        .session
        .id;
    let mut rx = client.subscribe();
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "cancel me".to_string(),
            attachments: vec![],
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    client
        .send(ClientCommand::CancelTask {
            session_id: session.clone(),
        })
        .await
        .unwrap();
    wait_terminal(&mut rx).await;

    assert_eq!(
        collaboration_of(&app, &session).await,
        "chat",
        "a cancelled goal is terminal: its session returns to Chat"
    );
    let goals = goals_of(&app, &session).await;
    assert_eq!(goals.len(), 1);
    assert_eq!(goals[0].state, leveler_storage::GoalState::Settled);
}
