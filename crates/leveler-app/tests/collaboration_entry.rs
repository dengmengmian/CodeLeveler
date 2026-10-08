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
                | RuntimeEvent::TurnCancelled => return,
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

/// E + F — `/goal <task>` writes the axis durably; only an explicit axis change
/// returns the session to chat.
#[tokio::test]
async fn run_goal_is_durable_and_explicit_chat_restores_chat() {
    use leveler_local_transport::{CollaborationMode, LocalRuntimeService};

    let (_tmp, _server, app, client) = harness(vec![goal_complete_tool_call()]).await;
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
    wait_terminal(&mut rx).await;
    assert_eq!(
        collaboration_of(&app, &session).await,
        "goal",
        "`/goal <task>` must leave a durable goal axis, not a one-turn override"
    );

    // `/goal clear` / `/collab chat` is the explicit way back.
    client
        .send(ClientCommand::SetProductAxes {
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
    let app = headless_app(&tmp, leveler_agent::CollaborationMode::Chat);
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
    let bodies = server.request_bodies().await;
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
