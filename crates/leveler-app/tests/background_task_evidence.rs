//! A background command's REAL result must become a durable engine fact.
//!
//! The reported defect: a command that runs longer than 15 seconds is handed to
//! the background, its call reports only a START, and when it finally succeeds
//! nothing records that result. `update_goal(complete)` then refuses a citation
//! of a test that really ran and really passed, and the same is true after a
//! resume. These tests drive the real registry, the real session event log and
//! the real runtime bridge — no mocked lifecycle.
//!
//! Covered here:
//! - a natural zero exit is persisted with the STARTING CALL's id and the argv
//!   the registry actually ran,
//! - a natural failure is persisted as a failure (never as a pass),
//! - a stopped task is not persisted at all (a cancel is not a result),
//! - the fact does not depend on the model calling `wait_task`,
//! - a task started outside the runtime (no recorded call) writes nothing.
#![cfg(unix)]

use std::sync::Arc;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    InteractiveRuntimeClient, PermissionProfile as WirePermissionProfile, RuntimeEvent,
};
use leveler_execution::PermissionProfile;
use leveler_local_transport::{CreateSessionRequest, LocalRuntimeService};
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_storage::EventStore as _;

fn isolate_global_config() {
    use std::sync::OnceLock;
    static EMPTY_HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    let dir = EMPTY_HOME.get_or_init(|| tempfile::tempdir().unwrap());
    unsafe {
        std::env::set_var("LEVELER_HOME", dir.path());
    }
}

/// A model endpoint that accepts a connection and never answers. The lifecycle
/// under test is independent of the model; it only needs a session that exists.
async fn hold_open_model_endpoint() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut stream = stream;
                let (_reader, writer) = stream.split();
                // Hold the connection open until the test process ends.
                let _ = writer;
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            });
        }
    });
    format!("http://{addr}/v1")
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
    runtime: Arc<InProcessRuntimeClient>,
    session: leveler_core::SessionId,
    repo: std::path::PathBuf,
    _tmp: tempfile::TempDir,
}

async fn fixture() -> Fixture {
    let base_url = hold_open_model_endpoint().await;
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), &base_url);
    let layout = Layout::from_parts(
        tmp.path().to_path_buf(),
        tmp.path().join("configs"),
        tmp.path().join("state"),
    );
    let app = Arc::new(Application::assemble(layout).unwrap());
    let runtime = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        ModelRef::new("mock", "m"),
        PermissionProfile::Assisted,
        false,
    ));
    let session = runtime
        .create_session(CreateSessionRequest {
            request_id: None,
            collaboration: leveler_local_transport::CollaborationMode::Goal,
            workspace: leveler_local_transport::CreateWorkspaceSelection::RuntimeDefault,
            approval_policy: leveler_client_protocol::ApprovalPolicy::Interactive,
            goal: "background evidence".into(),
            model: None,
            mode: WirePermissionProfile::Assisted,
        })
        .await
        .expect("create session")
        .session
        .id;
    let repo = tmp.path().to_path_buf();
    Fixture {
        app,
        runtime,
        session,
        repo,
        _tmp: tmp,
    }
}

impl Fixture {
    /// Record the tool call that starts a background command, exactly as the
    /// engine does. The call's own `ToolCallFinished` is not needed: a start is
    /// what the model saw.
    async fn record_starting_call(&self, call_id: &str, program: &str, args: &[&str]) {
        let db = self.app.open_database().await.expect("db");
        let event = leveler_engine::EngineEvent::ToolCallStarted {
            call_id: call_id.into(),
            name: "run_command".into(),
            arguments: serde_json::json!({ "program": program, "args": args }).to_string(),
            parallel: false,
            risk: None,
            agent_id: None,
            model_step: Some(1),
        };
        let (event_type, payload) = event.to_row().unwrap();
        db.append(
            &self.session,
            None,
            &event_type,
            &payload,
            leveler_core::now(),
        )
        .await
        .expect("append start event");
    }

    async fn spawn(&self, call_id: Option<&str>, script: &str) -> String {
        self.app
            .background_tasks()
            .spawn_for_writer(
                leveler_execution::ProcessRequest::new(
                    "sh",
                    vec!["-c".into(), script.to_string()],
                    self.repo.clone(),
                ),
                None,
                Some(self.session.as_str()),
                "parent",
                leveler_execution::BackgroundTaskLifetime::Goal,
                call_id.map(str::to_string),
            )
            .await
            .expect("spawn")
    }

    /// Every durable background-result fact in this session's log.
    async fn result_events(&self) -> Vec<leveler_engine::EngineEvent> {
        let db = self.app.open_database().await.expect("db");
        db.load_by_types(&self.session, &["background_task_finished"])
            .await
            .expect("load")
            .into_iter()
            .filter_map(|row| leveler_engine::EngineEvent::from_payload(&row.payload).ok())
            .collect()
    }

    async fn wait_for_events(&self, events: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>) {
        loop {
            let event = tokio::time::timeout(std::time::Duration::from_secs(10), events.recv())
                .await
                .expect("lifecycle event")
                .expect("stream open");
            if matches!(event, RuntimeEvent::BackgroundTaskExited { .. }) {
                break;
            }
        }
    }
}

/// The long-running success: the result is durable WITHOUT any `wait_task`.
#[tokio::test]
async fn a_finished_background_command_becomes_durable_evidence() {
    let f = fixture().await;
    let mut events = f.runtime.subscribe_session(&f.session);
    f.record_starting_call("call-bg-ok", "cargo", &["test"])
        .await;

    let id = f
        .spawn(Some("call-bg-ok"), "printf 'running tests\\n'; exit 0")
        .await;
    f.wait_for_events(&mut events).await;

    let recorded = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let rows = f.result_events().await;
            if !rows.is_empty() {
                return rows;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the durable result is written");

    match &recorded[0] {
        leveler_engine::EngineEvent::BackgroundTaskFinished {
            call_id,
            task_id,
            program,
            args,
            is_error,
            exit_code,
        } => {
            assert_eq!(call_id, "call-bg-ok", "attributed to the starting call");
            assert_eq!(task_id, &id);
            assert_eq!(program, "sh");
            assert_eq!(args[1], "printf 'running tests\\n'; exit 0");
            assert!(!is_error, "a zero exit is a pass");
            assert_eq!(*exit_code, Some(0));
        }
        other => panic!("unexpected event {other:?}"),
    }
}

/// A real failure stays a failure: it can never be cited as a pass.
#[tokio::test]
async fn a_failed_background_command_is_recorded_as_a_failure() {
    let f = fixture().await;
    let mut events = f.runtime.subscribe_session(&f.session);
    f.record_starting_call("call-bg-fail", "cargo", &["test"])
        .await;
    f.spawn(Some("call-bg-fail"), "exit 3").await;
    f.wait_for_events(&mut events).await;

    let recorded = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let rows = f.result_events().await;
            if !rows.is_empty() {
                return rows;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the durable result is written");
    match &recorded[0] {
        leveler_engine::EngineEvent::BackgroundTaskFinished {
            is_error,
            exit_code,
            ..
        } => {
            assert!(is_error, "exit 3 is a failure");
            assert_eq!(*exit_code, Some(3));
        }
        other => panic!("unexpected event {other:?}"),
    }
}

/// A stopped task produced no result. Persisting one would let a cancel count
/// as evidence.
#[tokio::test]
async fn a_stopped_background_command_records_no_result() {
    let f = fixture().await;
    let mut events = f.runtime.subscribe_session(&f.session);
    f.record_starting_call("call-bg-stop", "cargo", &["test"])
        .await;
    let id = f.spawn(Some("call-bg-stop"), "sleep 30").await;
    // Wait for the start to be observable, then stop it.
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(10), events.recv())
            .await
            .expect("start event")
            .expect("stream open");
        if matches!(event, RuntimeEvent::BackgroundTaskStarted { .. }) {
            break;
        }
    }
    f.app.background_tasks().kill(&id).await.expect("kill");
    f.wait_for_events(&mut events).await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        f.result_events().await.is_empty(),
        "a cancel is not a result"
    );
}

/// A task with no recorded starting call (an internal service, a user shell)
/// has no call to attribute a result to, so nothing is written.
#[tokio::test]
async fn a_task_without_a_starting_call_records_no_result() {
    let f = fixture().await;
    let mut events = f.runtime.subscribe_session(&f.session);
    f.spawn(None, "exit 0").await;
    f.wait_for_events(&mut events).await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(f.result_events().await.is_empty(), "no call, no evidence");
}
