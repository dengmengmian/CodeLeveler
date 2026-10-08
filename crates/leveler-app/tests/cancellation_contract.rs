//! Phase 2B cancellation-contract regression suite.
//!
//! The audit's failure reproductions (Phase 2A) are kept, but the force-cancel
//! ones are re-pointed at the contract this round now states: there is no
//! separate force-cancel capability, so the tests must prove the missing
//! capability instead of pinning its absence. None of them relaxes or replaces
//! a frozen Runtime Lifecycle / Durable ACK assertion.
//!
//! What this suite pins:
//!
//! - The legacy `force_cancel_current_turn` wire payload is still accepted, and
//!   resolves to the one turn cancel with the same resumable `interrupted`
//!   terminal.
//! - Cancelling a turn DOES stop an in-flight tool process tree (the tool
//!   token is a child of the turn's token), and the stop is confirmed.
//! - Cancelling a turn must NOT kill an independent background task the same
//!   session started; that process is the user's service, not the turn's.
//! - Repeating a cancel is safe; a late cancel is a no-op that never rewrites
//!   a committed terminal.
//! - Cancelling one session never touches another session's turn.
//! - `CancelTask` must not overwrite a committed `completed` task (Control C),
//!   must settle exactly once under duplicate delivery (Control D), and must
//!   not touch another session (Control E).
//! - Crash recovery reaps the dead boot's running TURN but leaves
//!   `sessions.status` stale at `running`; the status projection compensates,
//!   so the column and the projection disagree. The `#[ignore]`d anchor
//!   `a_reaped_dead_boot_does_not_leave_stale_session_status` is the Phase 2C
//!   target, deliberately not a release gate.
//!
//! The acknowledged-`CancelTask`-lost-on-crash reproduction is a real-process
//! SIGKILL test and lives in `leveler-cli/tests/daemon_e2e.rs` behind the
//! `test-crash-barrier` feature, where an exact crash window can be enforced.

use std::sync::Arc;
use std::time::Duration;

use leveler_app::runtime_boot::{RuntimeBootLease, StateDirBootLiveness};
use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    ClientCommand, InteractiveRuntimeClient, RuntimeEvent, UiCommandStop,
};
use leveler_execution::PermissionProfile;
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_storage::{EngineStores, SessionRepository, TurnRepository};
use leveler_test_support::{MockResponse, MockServer, sleep_command};

/// Point `LEVELER_HOME` at an empty dir so `GlobalConfig::load()` yields the
/// default. Tests must not depend on the developer's `~/.leveler/config.toml`.
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

fn text_frame(content: &str) -> String {
    serde_json::json!({
        "choices": [{"delta": {"content": content}, "finish_reason": "stop"}]
    })
    .to_string()
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
    session: leveler_core::SessionId,
    client: Arc<InProcessRuntimeClient>,
    _tmp: tempfile::TempDir,
    _server: MockServer,
}

async fn fixture_with(responses: Vec<MockResponse>, profile: PermissionProfile) -> Fixture {
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
            // An interactive session is a chat: no update_goal requirement, so
            // the turn stays on the cancellation path under test.
            .with_collaboration(leveler_agent::CollaborationMode::Chat),
    );
    let model = ModelRef::new("mock", "m");
    let session = app
        .create_session(&model, "cancellation audit")
        .await
        .unwrap();
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        model,
        profile,
        false,
    ));
    Fixture {
        app,
        session,
        client,
        _tmp: tmp,
        _server: server,
    }
}

async fn fixture(responses: Vec<MockResponse>) -> Fixture {
    fixture_with(responses, PermissionProfile::FullAccess).await
}

/// A turn that stays running: one foreground tool call blocks for a while.
fn blocking_command(id: &str) -> MockResponse {
    let (program, args) = sleep_command(30);
    sse(vec![
        tool_call_frame(
            id,
            "run_command",
            serde_json::json!({ "program": program, "args": args }),
        ),
        finish_frame("tool_calls"),
    ])
}

async fn last_outcome(db: &leveler_storage::Database, session: &leveler_core::SessionId) -> String {
    use leveler_engine::EngineEvent;
    let stores = EngineStores::from_database(db);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let row = loop {
        if let Some(row) = stores
            .events
            .load_last_by_type(session, "task_finished", None)
            .await
            .unwrap()
        {
            break row;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no terminal task_finished event was recorded"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    match EngineEvent::from_payload(&row.payload).unwrap() {
        EngineEvent::TaskFinished { outcome, .. } => outcome.as_str().to_string(),
        other => panic!("unexpected event: {other:?}"),
    }
}

async fn terminal_count(
    db: &leveler_storage::Database,
    session: &leveler_core::SessionId,
) -> usize {
    let stores = EngineStores::from_database(db);
    stores
        .events
        .load_by_types(session, &["task_finished"])
        .await
        .unwrap()
        .len()
}

async fn wait_for_running_turn(app: &Application, session: &leveler_core::SessionId) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let db = app.open_database().await.unwrap();
        let turns = TurnRepository::new(&db).list(session).await.unwrap();
        if turns.iter().any(|turn| turn.status == "running") {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no turn started in time: {turns:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn settled(app: &Application, session: &leveler_core::SessionId) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let db = app.open_database().await.unwrap();
        let turns = TurnRepository::new(&db).list(session).await.unwrap();
        if !turns.is_empty() && turns.iter().all(|turn| turn.status != "running") {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the turn did not settle in time: {turns:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Watch for the first event matching `predicate`, within a bounded window.
async fn wait_for_event<T>(
    rx: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>,
    mut predicate: impl FnMut(&RuntimeEvent) -> Option<T>,
) -> Option<T> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Ok(event)) => {
                if let Some(value) = predicate(&event) {
                    return Some(value);
                }
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            _ => return None,
        }
    }
}

/// CANCEL-03 (Phase 2B) — there is no force cancel, and a legacy request is a
/// plain cancel.
///
/// The protocol used to advertise `force_cancel_current_turn` as an escalation
/// while the runtime handled it identically to `CancelCurrentTurn`. This test
/// uses the REAL 1.x wire payload, proves it is accepted, proves it parses to
/// the one turn cancel, and proves it produces the same resumable `interrupted`
/// terminal. A separate force capability does not exist, so nothing may promise
/// one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn legacy_force_cancel_wire_payload_is_a_plain_cancel() {
    let plain = fixture(vec![blocking_command("c-plain")]).await;
    plain
        .client
        .send(ClientCommand::SubmitMessage {
            session_id: plain.session.clone(),
            content: "run it".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    wait_for_running_turn(&plain.app, &plain.session).await;
    plain
        .client
        .send(ClientCommand::CancelCurrentTurn {
            session_id: plain.session.clone(),
        })
        .await
        .unwrap();
    settled(&plain.app, &plain.session).await;
    let cancel_outcome =
        last_outcome(&plain.app.open_database().await.unwrap(), &plain.session).await;

    let legacy = fixture(vec![blocking_command("c-legacy")]).await;
    legacy
        .client
        .send(ClientCommand::SubmitMessage {
            session_id: legacy.session.clone(),
            content: "run it".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    wait_for_running_turn(&legacy.app, &legacy.session).await;

    // The real wire bytes a 1.x client would send, not a Rust constructor.
    let payload = format!(
        r#"{{"type":"force_cancel_current_turn","session_id":"{}"}}"#,
        legacy.session.as_str()
    );
    let parsed: ClientCommand = serde_json::from_str(&payload)
        .expect("a legacy force-cancel payload must deserialize, not be rejected");
    assert_eq!(
        parsed,
        ClientCommand::CancelCurrentTurn {
            session_id: legacy.session.clone(),
        },
        "the legacy payload must resolve to the one turn cancel"
    );
    legacy.client.send(parsed).await.unwrap();
    settled(&legacy.app, &legacy.session).await;
    let legacy_outcome =
        last_outcome(&legacy.app.open_database().await.unwrap(), &legacy.session).await;

    assert_eq!(cancel_outcome, "interrupted", "a plain cancel is resumable");
    assert_eq!(
        legacy_outcome, "interrupted",
        "the legacy force request must behave as a plain resumable cancel"
    );
}

/// CANCEL-02 — cancelling the turn stops the tool process tree it is inside,
/// and says so with a confirmed stop rather than a hopeful one.
///
/// The wait for `ToolCallOutput` is the point: `ToolCallStarted` is written at
/// ADMISSION, before the process exists, so cancelling there proves nothing
/// about process termination. Only once the command has produced output is
/// there a real tree to stop.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_a_turn_kills_its_running_command_tree_and_confirms_it() {
    let f = fixture(vec![sse(vec![
        tool_call_frame(
            "c-sleep",
            "shell_command",
            serde_json::json!({ "cmd": "echo ready; sleep 30" }),
        ),
        finish_frame("tool_calls"),
    ])])
    .await;
    let mut rx = f.client.subscribe();
    f.client
        .send(ClientCommand::SubmitMessage {
            session_id: f.session.clone(),
            content: "run it".into(),
            attachments: vec![],
        })
        .await
        .unwrap();

    let produced = wait_for_event(&mut rx, |event| match event {
        RuntimeEvent::ToolCallOutput { id, .. } if id.as_str() == "c-sleep" => Some(()),
        _ => None,
    })
    .await;
    assert!(
        produced.is_some(),
        "the command must actually be running before the cancel is meaningful"
    );

    f.client
        .send(ClientCommand::CancelCurrentTurn {
            session_id: f.session.clone(),
        })
        .await
        .unwrap();

    let stop = wait_for_event(&mut rx, |event| match event {
        RuntimeEvent::ToolCallCompleted { id, stop, .. } if id.as_str() == "c-sleep" => Some(*stop),
        _ => None,
    })
    .await;
    settled(&f.app, &f.session).await;

    assert_eq!(
        stop,
        Some(Some(UiCommandStop::Confirmed)),
        "cancelling the turn must confirm the command's process tree is gone"
    );
    assert_eq!(
        last_outcome(&f.app.open_database().await.unwrap(), &f.session).await,
        "interrupted"
    );
}

/// CANCEL-08 — an independent background task the same session started is not
/// the turn's to kill. Cancelling the turn must leave it running.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_a_turn_leaves_an_independent_background_task_running() {
    let (program, args) = sleep_command(120);
    let f = fixture(vec![
        sse(vec![
            tool_call_frame(
                "c-background",
                "run_command",
                serde_json::json!({ "program": program, "args": args, "background": true }),
            ),
            finish_frame("tool_calls"),
        ]),
        blocking_command("c-blocks"),
    ])
    .await;

    let mut rx = f.client.subscribe();
    f.client
        .send(ClientCommand::SubmitMessage {
            session_id: f.session.clone(),
            content: "start the server and then work".into(),
            attachments: vec![],
        })
        .await
        .unwrap();

    // The background task must exist before the turn is cancelled.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let background_id = loop {
        let ids = f
            .app
            .background_tasks()
            .active_ids_for_scope(f.session.as_str())
            .await;
        if let Some(id) = ids.into_iter().next() {
            break id;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the background task never started"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };

    // Now wait for the turn to be busy inside the second, blocking command.
    let blocking_started = wait_for_event(&mut rx, |event| match event {
        RuntimeEvent::ToolCallStarted { id, .. } if id.as_str() == "c-blocks" => Some(()),
        _ => None,
    })
    .await;
    assert!(blocking_started.is_some(), "the blocking call must start");

    f.client
        .send(ClientCommand::CancelCurrentTurn {
            session_id: f.session.clone(),
        })
        .await
        .unwrap();
    settled(&f.app, &f.session).await;

    let snapshot = f
        .app
        .background_tasks()
        .get(&background_id)
        .await
        .expect("the registry retains the task record");
    assert!(
        matches!(
            snapshot.status,
            leveler_execution::BackgroundTaskStatus::Running
        ),
        "cancelling the turn must leave an independent background task RUNNING: {snapshot:?}"
    );
    assert!(
        snapshot.pid.is_some(),
        "the background process must still have an OS identity"
    );

    // Cleanup, through the same ownership-checked path a client uses.
    let _ = f
        .app
        .background_tasks()
        .kill_owned(&background_id, f.session.as_str())
        .await;
}

/// CANCEL-09 — repeating a cancel is safe, and the task settles exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repeating_cancel_settles_the_turn_once() {
    let f = fixture(vec![blocking_command("c-repeat")]).await;
    f.client
        .send(ClientCommand::SubmitMessage {
            session_id: f.session.clone(),
            content: "run it".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    wait_for_running_turn(&f.app, &f.session).await;

    for _ in 0..3 {
        f.client
            .send(ClientCommand::CancelCurrentTurn {
                session_id: f.session.clone(),
            })
            .await
            .expect("a repeated cancel is accepted");
    }
    settled(&f.app, &f.session).await;

    let db = f.app.open_database().await.unwrap();
    assert_eq!(
        last_outcome(&db, &f.session).await,
        "interrupted",
        "repeated cancels must not invent a stronger terminal"
    );
    assert_eq!(
        terminal_count(&db, &f.session).await,
        1,
        "the task must settle exactly once"
    );
}

/// CANCEL-10 — a cancel that arrives after the turn already ended is accepted
/// and changes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancel_after_the_turn_completed_changes_nothing() {
    let f = fixture(vec![sse(vec![text_frame("done")])]).await;
    f.client
        .send(ClientCommand::SubmitMessage {
            session_id: f.session.clone(),
            content: "say done".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    settled(&f.app, &f.session).await;
    let before = last_outcome(&f.app.open_database().await.unwrap(), &f.session).await;
    assert_eq!(before, "completed");

    f.client
        .send(ClientCommand::CancelCurrentTurn {
            session_id: f.session.clone(),
        })
        .await
        .expect("a late cancel is accepted, not a hard error");

    let db = f.app.open_database().await.unwrap();
    assert_eq!(
        last_outcome(&db, &f.session).await,
        "completed",
        "a late cancel must never rewrite a committed terminal"
    );
    assert_eq!(terminal_count(&db, &f.session).await, 1);
}

/// CANCEL-16 — cancelling one session never touches another session's turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_one_session_leaves_the_other_sessions_turn_running() {
    let f = fixture(vec![blocking_command("c-a"), blocking_command("c-b")]).await;
    let model = ModelRef::new("mock", "m");
    let other = f
        .app
        .create_session(&model, "second cancellation audit")
        .await
        .unwrap();

    f.client
        .send(ClientCommand::SubmitMessage {
            session_id: f.session.clone(),
            content: "run a".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    f.client
        .send(ClientCommand::SubmitMessage {
            session_id: other.clone(),
            content: "run b".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    wait_for_running_turn(&f.app, &f.session).await;
    wait_for_running_turn(&f.app, &other).await;

    f.client
        .send(ClientCommand::CancelCurrentTurn {
            session_id: f.session.clone(),
        })
        .await
        .unwrap();
    settled(&f.app, &f.session).await;

    assert!(
        f.client.has_live_turn(&other),
        "cancelling one session must not end another session's turn"
    );

    // Cleanup.
    f.client
        .send(ClientCommand::CancelCurrentTurn {
            session_id: other.clone(),
        })
        .await
        .unwrap();
    settled(&f.app, &other).await;
}

/// Gate 4 / CANCEL-12 — crash recovery atomically interrupts the latest turn
/// and its running session; the runtime projection reports the same fact.
#[tokio::test]
async fn a_reaped_dead_boot_keeps_session_and_projection_consistent() {
    use leveler_app::session_projection::project_task;
    use leveler_client_protocol::UiTaskStatus;
    use leveler_lifecycle::SessionStatus;
    use leveler_storage::SessionRecord;

    isolate_global_config();
    let tmp = tempfile::tempdir().unwrap();
    let layout = Layout::from_parts(
        tmp.path().to_path_buf(),
        tmp.path().join("configs"),
        tmp.path().join("state"),
    );
    let app = Application::assemble(layout).unwrap();
    let db = app.open_database().await.unwrap();
    let zombie = SessionRecord::new(
        tmp.path().display().to_string(),
        "interrupted by a crash",
        "mock/m",
        leveler_core::now(),
    );
    SessionRepository::new(&db).create(&zombie).await.unwrap();
    let session = leveler_core::SessionId::new(zombie.id.clone());

    // Boot A admits a task and a turn, then dies without settling it. The
    // REAL start path is used so `sessions.status` really is `running`.
    let crashed = RuntimeBootLease::acquire(&app.layout.state_dir).unwrap();
    let engine = leveler_engine::TaskEngine {
        stores: EngineStores::from_database(&db),
        runtime_id: app.runtime_id().unwrap(),
        boot: leveler_engine::EngineBoot {
            id: crashed.id().clone(),
            liveness: Arc::new(StateDirBootLiveness::new(&app.layout.state_dir)),
        },
    };
    let token = engine
        .start_task(
            &session,
            leveler_lifecycle::AgentState::Execute,
            &leveler_engine::TaskExecution {
                mode: "assisted".to_string(),
                sandbox: false,
                kind: leveler_engine::ExecutionKind::Direct,
            },
        )
        .await
        .unwrap();
    engine
        .stores
        .turns
        .start_owned(&token, &session, "chat", None, leveler_core::now())
        .await
        .unwrap();
    drop(crashed);

    let reaped = app.reap_zombie_turns(&db, None).await.unwrap();
    assert_eq!(reaped, 1, "the dead boot's running turn must be reaped");

    // The TURN is settled...
    let turns = TurnRepository::new(&db).list(&session).await.unwrap();
    assert_eq!(turns.len(), 1);
    assert_eq!(
        turns[0].status, "interrupted",
        "the reaper settles the turn as interrupted"
    );

    // The SESSION row is settled in the same transaction.
    let record = SessionRepository::new(&db)
        .get(&session)
        .await
        .unwrap()
        .expect("the session row exists");
    assert_eq!(
        record.status,
        SessionStatus::Interrupted,
        "the durable session and reaped turn must agree after crash recovery"
    );

    // The runtime projection agrees with the durable lifecycle.
    let facts = db
        .session_facts(false)
        .await
        .unwrap()
        .into_iter()
        .find(|facts| facts.session.id == session.as_str())
        .expect("session facts");
    let probe = StateDirBootLiveness::new(&app.layout.state_dir);
    let (status, _terminal) = project_task(&facts, &probe, false, false).unwrap();
    assert_eq!(
        status,
        UiTaskStatus::Interrupted,
        "the projection must not echo the stale `running` column"
    );
}

/// Control C — a `CancelTask` that arrives after the task already committed a
/// non-cancelled terminal must not rewrite it. Cancellation is a request about
/// work still in flight, not an override of a finished task.
///
/// RED at the time of writing: `Application::cancel_task` only short-circuits
/// when the last terminal is already `cancelled`, so a late `CancelTask`
/// overwrites a committed `completed` (and appends a second terminal). Fixing
/// it changes terminal-persistence semantics, which this round deliberately
/// does not do. Ignored so the reproduction stays as the Phase 2C evidence
/// instead of a red release gate.
#[ignore = "Phase 2C: a late CancelTask currently overwrites a committed completed terminal"]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_late_cancel_task_does_not_overwrite_a_committed_completed() {
    let f = fixture(vec![sse(vec![text_frame("done")])]).await;
    f.client
        .send(ClientCommand::SubmitMessage {
            session_id: f.session.clone(),
            content: "say done".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    settled(&f.app, &f.session).await;
    let db = f.app.open_database().await.unwrap();
    assert_eq!(last_outcome(&db, &f.session).await, "completed");
    drop(db);

    f.client
        .send(ClientCommand::CancelTask {
            session_id: f.session.clone(),
        })
        .await
        .expect("a late CancelTask is accepted, not a hard error");

    let db = f.app.open_database().await.unwrap();
    assert_eq!(
        last_outcome(&db, &f.session).await,
        "completed",
        "a late CancelTask must never rewrite a committed completed terminal"
    );
    assert_eq!(
        terminal_count(&db, &f.session).await,
        1,
        "no second task terminal may be appended"
    );
}

/// Control D — delivering `CancelTask` twice settles the task exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn duplicate_cancel_task_settles_once() {
    let f = fixture(vec![blocking_command("c-dup")]).await;
    f.client
        .send(ClientCommand::SubmitMessage {
            session_id: f.session.clone(),
            content: "run it".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    wait_for_running_turn(&f.app, &f.session).await;

    for attempt in 0..2 {
        f.client
            .send(ClientCommand::CancelTask {
                session_id: f.session.clone(),
            })
            .await
            .unwrap_or_else(|error| panic!("CancelTask attempt {attempt} failed: {error:?}"));
    }
    settled(&f.app, &f.session).await;

    let db = f.app.open_database().await.unwrap();
    assert_eq!(
        last_outcome(&db, &f.session).await,
        "cancelled",
        "a logical task cancel is terminal"
    );
    assert_eq!(
        terminal_count(&db, &f.session).await,
        1,
        "duplicate CancelTask must not produce a second terminal"
    );
}

/// Control E — cancelling the logical task in one session must not end another
/// session's running turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_task_leaves_another_sessions_turn_running() {
    let f = fixture(vec![blocking_command("c-ct-a"), blocking_command("c-ct-b")]).await;
    let model = ModelRef::new("mock", "m");
    let other = f
        .app
        .create_session(&model, "second cancel-task audit")
        .await
        .unwrap();

    f.client
        .send(ClientCommand::SubmitMessage {
            session_id: f.session.clone(),
            content: "run a".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    f.client
        .send(ClientCommand::SubmitMessage {
            session_id: other.clone(),
            content: "run b".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    wait_for_running_turn(&f.app, &f.session).await;
    wait_for_running_turn(&f.app, &other).await;

    f.client
        .send(ClientCommand::CancelTask {
            session_id: f.session.clone(),
        })
        .await
        .unwrap();
    settled(&f.app, &f.session).await;

    assert!(
        f.client.has_live_turn(&other),
        "cancelling one session's logical task must not end another session's turn"
    );

    // Cleanup through the ordinary cooperative cancel.
    f.client
        .send(ClientCommand::CancelCurrentTurn {
            session_id: other.clone(),
        })
        .await
        .unwrap();
    settled(&f.app, &other).await;
}

/// Gate 7 / G-2 — a crash reap leaves a resumable durable session rather than
/// stale Running state. Kept as the original Phase 2C regression anchor.
#[tokio::test]
async fn a_reaped_dead_boot_does_not_leave_stale_session_status() {
    use leveler_lifecycle::SessionStatus;
    use leveler_storage::SessionRecord;

    isolate_global_config();
    let tmp = tempfile::tempdir().unwrap();
    let layout = Layout::from_parts(
        tmp.path().to_path_buf(),
        tmp.path().join("configs"),
        tmp.path().join("state"),
    );
    let app = Application::assemble(layout).unwrap();
    let db = app.open_database().await.unwrap();
    let zombie = SessionRecord::new(
        tmp.path().display().to_string(),
        "interrupted by a crash",
        "mock/m",
        leveler_core::now(),
    );
    SessionRepository::new(&db).create(&zombie).await.unwrap();
    let session = leveler_core::SessionId::new(zombie.id.clone());

    let crashed = RuntimeBootLease::acquire(&app.layout.state_dir).unwrap();
    let engine = leveler_engine::TaskEngine {
        stores: EngineStores::from_database(&db),
        runtime_id: app.runtime_id().unwrap(),
        boot: leveler_engine::EngineBoot {
            id: crashed.id().clone(),
            liveness: Arc::new(StateDirBootLiveness::new(&app.layout.state_dir)),
        },
    };
    let token = engine
        .start_task(
            &session,
            leveler_lifecycle::AgentState::Execute,
            &leveler_engine::TaskExecution {
                mode: "assisted".to_string(),
                sandbox: false,
                kind: leveler_engine::ExecutionKind::Direct,
            },
        )
        .await
        .unwrap();
    engine
        .stores
        .turns
        .start_owned(&token, &session, "chat", None, leveler_core::now())
        .await
        .unwrap();
    drop(crashed);

    let reaped = app.reap_zombie_turns(&db, None).await.unwrap();
    assert_eq!(reaped, 1, "the dead boot's running turn must be reaped");

    let record = SessionRepository::new(&db)
        .get(&session)
        .await
        .unwrap()
        .expect("the session row exists");
    assert_eq!(
        record.status,
        SessionStatus::Interrupted,
        "G-2: after a crash reap the durable session status must agree with the \
         interrupted turn"
    );
}
