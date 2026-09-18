//! Runtime P2A lifecycle invariants over the real local transport:
//!
//! - Scenario C: a client disconnect never cancels a running task; only an
//!   explicit `CancelCurrentTurn` does, and exactly once.
//! - Scenario F: a client subscribed to session A never receives session B's
//!   scoped events.
//!
//! Scenario B (session survives its first client and carries to the next) is
//! locked by `cross_client_relay.rs`; these tests add the cancellation and
//! isolation halves with a model endpoint that holds requests open, so the
//! turn is genuinely *running* when the client goes away.

#![cfg(unix)]

use std::sync::Arc;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    ClientCommand, InteractiveRuntimeClient, PermissionProfile as WirePermissionProfile,
    RuntimeEvent,
};
use leveler_execution::PermissionProfile;
use leveler_local_transport::{
    CreateSessionRequest, LocalRuntimeService, LocalSocketRuntimeClient, LocalSocketServer,
    LocalWaiters,
};
use leveler_model::ModelRef;
use leveler_project::Layout;
use tokio_util::sync::CancellationToken;

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

/// A model endpoint that accepts connections and then simply holds them open.
/// Any turn against it stays "running" until cancelled — exactly the state a
/// disconnect-vs-cancel test needs.
async fn hold_open_model_endpoint() -> (String, CancellationToken) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let stop = CancellationToken::new();
    let server_stop = stop.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = server_stop.cancelled() => return,
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { return };
                    let conn_stop = server_stop.clone();
                    tokio::spawn(async move {
                        // Hold the request open; never answer.
                        conn_stop.cancelled().await;
                        drop(stream);
                    });
                }
            }
        }
    });
    (format!("http://{addr}"), stop)
}

struct Harness {
    _tmp: tempfile::TempDir,
    runtime: Arc<InProcessRuntimeClient>,
    socket: std::path::PathBuf,
}

async fn harness(base_url: &str) -> Harness {
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), base_url);
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
    let socket = tmp.path().join("daemon.sock");
    Harness {
        _tmp: tmp,
        runtime,
        socket,
    }
}

/// Retry until the session reports a busy main turn (admission refuses a
/// second submit). Admission happens synchronously inside `send`, so one
/// attempt is normally enough; the loop only absorbs scheduler jitter.
async fn assert_turn_running(runtime: &InProcessRuntimeClient, session: &leveler_core::SessionId) {
    for _ in 0..20 {
        let result = runtime
            .send(ClientCommand::SubmitMessage {
                session_id: session.clone(),
                content: "probe: should be refused while busy".to_string(),
                attachments: vec![],
            })
            .await;
        match result {
            Err(e) if e.to_string().contains("active turn") => return,
            // Not busy yet (or the probe won admission — which would itself
            // start a turn against the held-open endpoint, keeping the
            // session busy for the next probe).
            _ => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
        }
    }
    panic!("the session never reported a running turn");
}

/// Scenario C: TUI disconnect leaves the task running; an explicit cancel
/// from a *different* client cancels it exactly once.
#[tokio::test]
async fn client_disconnect_does_not_cancel_and_explicit_cancel_fires_once() {
    let (base_url, model_stop) = hold_open_model_endpoint().await;
    let h = harness(&base_url).await;

    // Client 1 over the real socket transport.
    let server = LocalSocketServer::bind(&h.socket, h.runtime.clone())
        .await
        .unwrap();
    let shutdown = CancellationToken::new();
    let serve_task = tokio::spawn(server.serve(shutdown.clone()));
    let client1 = LocalSocketRuntimeClient::connect(&h.socket).await.unwrap();

    let bootstrap = client1
        .create_session(CreateSessionRequest {
            approval_policy: leveler_client_protocol::ApprovalPolicy::Interactive,
            goal: "long task".to_string(),
            model: None,
            mode: WirePermissionProfile::Assisted,
        })
        .await
        .unwrap();
    let session = bootstrap.session.id.clone();
    let mut events = h.runtime.subscribe_session(&session);

    client1
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "work forever".to_string(),
            attachments: vec![],
        })
        .await
        .unwrap();
    assert_turn_running(&h.runtime, &session).await;

    // ── the first client vanishes entirely ───────────────────────────────
    drop(client1);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // The task is still running: admission still refuses a new main turn.
    assert_turn_running(&h.runtime, &session).await;

    // ── a second client cancels explicitly ───────────────────────────────
    let client2 = LocalSocketRuntimeClient::connect(&h.socket).await.unwrap();
    client2
        .send(ClientCommand::CancelCurrentTurn {
            session_id: session.clone(),
        })
        .await
        .unwrap();

    // Exactly one cancellation reaches the canonical stream.
    let mut cancelled = 0;
    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(5), events.recv()).await {
            Ok(Ok(RuntimeEvent::TurnCancelled)) => {
                cancelled += 1;
                // Keep draining briefly: a second TurnCancelled would mean
                // the disconnect ALSO produced one.
                let extra = tokio::time::timeout(std::time::Duration::from_millis(700), async {
                    loop {
                        if let Ok(RuntimeEvent::TurnCancelled) = events.recv().await {
                            return;
                        }
                    }
                })
                .await;
                assert!(extra.is_err(), "cancellation must happen exactly once");
                break;
            }
            Ok(Ok(_)) => continue,
            Ok(Err(e)) => panic!("event stream closed early: {e}"),
            Err(_) => panic!("explicit cancel never produced TurnCancelled"),
        }
    }
    assert_eq!(cancelled, 1);

    // The session accepts new work after the explicit cancel.
    for _ in 0..40 {
        let result = h
            .runtime
            .send(ClientCommand::SubmitMessage {
                session_id: session.clone(),
                content: "next task".to_string(),
                attachments: vec![],
            })
            .await;
        if result.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    model_stop.cancel();
    shutdown.cancel();
    let _ = serve_task.await;
}

/// Scenario F: a socket subscription scoped to session A receives nothing
/// when all the traffic happens in session B.
#[tokio::test]
async fn session_scoped_subscription_never_sees_another_sessions_events() {
    let h = harness("http://127.0.0.1:9").await; // unreachable: turns fail fast

    let server = LocalSocketServer::bind(&h.socket, h.runtime.clone())
        .await
        .unwrap();
    let shutdown = CancellationToken::new();
    let serve_task = tokio::spawn(server.serve(shutdown.clone()));
    let client = LocalSocketRuntimeClient::connect(&h.socket).await.unwrap();

    let session_a = client
        .create_session(CreateSessionRequest {
            approval_policy: leveler_client_protocol::ApprovalPolicy::Interactive,
            goal: "session A".to_string(),
            model: None,
            mode: WirePermissionProfile::Assisted,
        })
        .await
        .unwrap()
        .session
        .id;
    let session_b = client
        .create_session(CreateSessionRequest {
            approval_policy: leveler_client_protocol::ApprovalPolicy::Interactive,
            goal: "session B".to_string(),
            model: None,
            mode: WirePermissionProfile::Assisted,
        })
        .await
        .unwrap()
        .session
        .id;

    let mut a_events = client.subscribe_session(&session_a);
    // Drain anything pending from A's own creation before B acts.
    while a_events.try_recv().is_ok() {}

    let mut b_events = h.runtime.subscribe_session(&session_b);
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session_b.clone(),
            content: "SECRET_FOR_B_ONLY".to_string(),
            attachments: vec![],
        })
        .await
        .unwrap();
    // Dispatch is the activity. An unreachable model retries with backoff
    // before it emits TurnFailed, so a terminal event is far away; wait only
    // for the submit to become visible and let the test cancel the turn.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match b_events.recv().await {
                Ok(RuntimeEvent::UserMessageAdded { .. }) => return,
                Ok(_) => continue,
                Err(_) => panic!("B's stream closed"),
            }
        }
    })
    .await
    .expect("B's submit is visible on B's stream");
    h.runtime
        .send(ClientCommand::CancelCurrentTurn {
            session_id: session_b.clone(),
        })
        .await
        .unwrap();
    for _ in 0..400 {
        if !h.runtime.has_live_turn(&session_b) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    // Give the transport a moment to (incorrectly) forward anything to A.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let mut leaked = Vec::new();
    while let Ok(event) = a_events.try_recv() {
        leaked.push(event);
    }
    assert!(
        leaked.is_empty(),
        "session A's subscription must stay silent during B's turn, got: {leaked:?}"
    );

    shutdown.cancel();
    let _ = serve_task.await;
}

/// Retiring a runtime is not killing it.
///
/// The client asks; the runtime decides when. An idle one leaves at once, and
/// the process token it was handed is how it actually ends — replacing a
/// binary on disk cannot do that, which is the whole reason this exists.
#[tokio::test]
async fn an_idle_runtime_retires_itself_when_asked() {
    let (base_url, _model_stop) = hold_open_model_endpoint().await;
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), &base_url);
    let layout = Layout::from_parts(
        tmp.path().to_path_buf(),
        tmp.path().join("configs"),
        tmp.path().join("state"),
    );
    let app = Arc::new(Application::assemble(layout).unwrap());
    let token = tokio_util::sync::CancellationToken::new();
    let runtime = Arc::new(
        InProcessRuntimeClient::new(
            app,
            ModelRef::new("mock", "m"),
            PermissionProfile::Assisted,
            false,
        )
        .with_process_shutdown(token.clone()),
    );

    let before = runtime.runtime_info().await.unwrap();
    assert!(
        before.health.accepting_work,
        "a healthy idle runtime takes work"
    );

    runtime
        .send(leveler_client_protocol::ClientCommand::ShutdownWhenIdle {
            reason: leveler_client_protocol::RestartReason::BuildMismatch,
        })
        .await
        .unwrap();

    // Admissions close first: without that, arriving turns could keep the
    // runtime alive indefinitely and the replacement would never happen.
    let after = runtime.runtime_info().await.unwrap();
    assert!(
        !after.health.accepting_work,
        "a retiring runtime stops taking new work"
    );
    assert!(after.health.shutting_down);

    // Nothing is running, so the drain is immediate and the process is asked
    // to end.
    let retiring = runtime.runtime_info().await.unwrap().health;
    assert!(retiring.quiescent(), "an idle runtime is quiescent");
    assert_eq!(
        retiring.retiring_reason,
        Some(leveler_client_protocol::RestartReason::BuildMismatch),
        "a handover request records why it is retiring"
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), token.cancelled())
        .await
        .expect("an idle runtime retires promptly");
}

/// The identity a runtime reports is its own build, not merely its version —
/// the distinction the stale-daemon incident turned on.
#[tokio::test]
async fn a_runtime_reports_the_build_it_is() {
    let (base_url, _model_stop) = hold_open_model_endpoint().await;
    let h = harness(&base_url).await;
    let info = h.runtime.runtime_info().await.unwrap();
    assert!(
        info.build.is_known(),
        "a runtime that cannot say which build it is cannot be verified"
    );
    assert_eq!(info.build, leveler_core::BuildIdentity::current());
    assert!(
        info.build.matches(&leveler_core::BuildIdentity::current())
            || leveler_core::BuildIdentity::current().dirty,
        "a clean runtime matches the client that built it"
    );
}

/// A turn ending is not the runtime going idle.
///
/// The incident that started all of this was a `cargo check` that outlived
/// its turn by half an hour. Retiring on "no turn running" would have ended
/// the process on top of it and thrown that work away, so a live background
/// task holds the handover open until it settles.
#[tokio::test]
async fn a_live_background_task_holds_the_handover_open() {
    let (base_url, _model_stop) = hold_open_model_endpoint().await;
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), &base_url);
    let layout = Layout::from_parts(
        tmp.path().to_path_buf(),
        tmp.path().join("configs"),
        tmp.path().join("state"),
    );
    let app = Arc::new(Application::assemble(layout).unwrap());
    let token = tokio_util::sync::CancellationToken::new();
    let runtime = Arc::new(
        InProcessRuntimeClient::new(
            app.clone(),
            ModelRef::new("mock", "m"),
            PermissionProfile::Assisted,
            false,
        )
        .with_process_shutdown(token.clone()),
    );

    // Long enough to outlive the retirement request, like a real build.
    let task_id = app
        .background_tasks()
        .spawn(
            leveler_execution::ProcessRequest::new(
                "sleep",
                vec!["30".to_string()],
                tmp.path().to_path_buf(),
            ),
            None,
        )
        .await
        .expect("background task starts");
    assert_eq!(app.background_tasks().alive_count().await, 1);

    runtime
        .send(leveler_client_protocol::ClientCommand::ShutdownWhenIdle {
            reason: leveler_client_protocol::RestartReason::BuildMismatch,
        })
        .await
        .unwrap();

    // No turn is running, so a turn-only idea of idleness would retire here.
    // The health a client reads must agree with the drain: `active_turns == 0`
    // is NOT quiescent while background work is alive.
    let health = runtime.runtime_info().await.unwrap().health;
    assert_eq!(health.active_turns, 0);
    assert_eq!(health.active_background_tasks, 1);
    assert!(
        !health.quiescent(),
        "a live background task is not idle, whatever the turn count says"
    );
    assert!(
        !health.accepting_work,
        "a retiring runtime reports that it takes no new work"
    );
    assert_eq!(
        health.retiring_reason,
        Some(leveler_client_protocol::RestartReason::BuildMismatch)
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(3), token.cancelled())
            .await
            .is_err(),
        "a runtime with live background work has not finished; it must not retire"
    );

    // A second retire request (two clients racing the same handover) is
    // harmless: the drain is already committed and still waits for the SAME
    // work, never cut short by the duplicate.
    runtime
        .send(leveler_client_protocol::ClientCommand::ShutdownWhenIdle {
            reason: leveler_client_protocol::RestartReason::ConfigChanged,
        })
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(500), token.cancelled())
            .await
            .is_err(),
        "a duplicate retire request must not end the drain early"
    );

    // The work settles, and only then does the handover proceed.
    app.background_tasks().kill(&task_id).await.ok();
    tokio::time::timeout(std::time::Duration::from_secs(10), token.cancelled())
        .await
        .expect("once nothing is left running, the runtime retires");
}

/// The client's idea of "idle" and the drain's are the SAME reading.
///
/// The defect this pins: `RuntimeHealth` used to expose only `active_turns`,
/// so a runtime with a live background task and no turn looked idle to any
/// client while the drain knew it was not. The counters and the derived
/// `quiescent` flag now come from one computation.
#[tokio::test]
async fn health_reports_the_same_quiescence_the_drain_waits_on() {
    let (base_url, _model_stop) = hold_open_model_endpoint().await;
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), &base_url);
    let layout = Layout::from_parts(
        tmp.path().to_path_buf(),
        tmp.path().join("configs"),
        tmp.path().join("state"),
    );
    let app = Arc::new(Application::assemble(layout).unwrap());
    let runtime = InProcessRuntimeClient::new(
        app.clone(),
        ModelRef::new("mock", "m"),
        PermissionProfile::Assisted,
        false,
    );

    let idle = runtime.runtime_info().await.unwrap().health;
    assert!(idle.quiescent(), "an idle runtime is quiescent");
    assert_eq!(idle.active_turns, 0);
    assert_eq!(idle.active_background_tasks, 0);
    assert!(idle.retiring_reason.is_none());

    let task = app
        .background_tasks()
        .spawn(
            leveler_execution::ProcessRequest::new(
                "sleep",
                vec!["30".to_string()],
                tmp.path().to_path_buf(),
            ),
            None,
        )
        .await
        .expect("background task starts");

    let busy = runtime.runtime_info().await.unwrap().health;
    assert_eq!(busy.active_turns, 0, "no turn is running");
    assert_eq!(busy.active_background_tasks, 1);
    assert!(
        !busy.quiescent(),
        "a background task alone keeps the runtime from being idle"
    );
    assert_eq!(
        busy.quiescent(),
        busy.active_turns == 0 && busy.active_background_tasks == 0,
        "quiescent must be derived from exactly the two counters"
    );

    app.background_tasks().kill(&task).await.ok();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if runtime.runtime_info().await.unwrap().health.quiescent() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the settled background task must make the runtime quiescent"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// A real registry spawn/settlement is projected onto the owning session's
/// structured runtime stream. The UI does not parse tool output to invent
/// lifecycle events.
#[tokio::test]
async fn registry_background_lifecycle_reaches_the_session_event_stream() {
    let (base_url, _model_stop) = hold_open_model_endpoint().await;
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), &base_url);
    let layout = Layout::from_parts(
        tmp.path().to_path_buf(),
        tmp.path().join("configs"),
        tmp.path().join("state"),
    );
    let app = Arc::new(Application::assemble(layout).unwrap());
    let runtime = InProcessRuntimeClient::new(
        app.clone(),
        ModelRef::new("mock", "m"),
        PermissionProfile::Assisted,
        false,
    );
    let session_id = runtime
        .create_session(CreateSessionRequest {
            approval_policy: leveler_client_protocol::ApprovalPolicy::Interactive,
            goal: "background lifecycle".into(),
            model: None,
            mode: WirePermissionProfile::Assisted,
        })
        .await
        .expect("create session")
        .session
        .id;
    let mut events = runtime.subscribe_session(&session_id);

    let spawned_id = app
        .background_tasks()
        .spawn_owned(
            leveler_execution::ProcessRequest::new(
                "sleep",
                vec!["30".to_string()],
                tmp.path().to_path_buf(),
            ),
            None,
            Some(session_id.as_str()),
        )
        .await
        .expect("background task starts");
    let started = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
        .await
        .expect("start event arrives")
        .expect("stream open");
    assert!(matches!(
        started,
        RuntimeEvent::BackgroundTaskStarted { ref task_id, .. } if task_id == &spawned_id
    ));
    let active = runtime
        .snapshot(&session_id)
        .await
        .expect("active snapshot");
    assert_eq!(active.active_background_tasks.len(), 1);
    assert_eq!(active.active_background_tasks[0].task_id, spawned_id);

    app.background_tasks()
        .kill(&spawned_id)
        .await
        .expect("kill");
    let exited = loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
            .await
            .expect("exit event arrives")
            .expect("stream open");
        if matches!(event, RuntimeEvent::BackgroundTaskExited { .. }) {
            break event;
        }
    };
    assert!(matches!(
        exited,
        RuntimeEvent::BackgroundTaskExited { task_id: ref exited_id, ok: false, .. }
            if exited_id == &spawned_id
    ));
    let terminal = runtime
        .snapshot(&session_id)
        .await
        .expect("terminal snapshot");
    assert!(terminal.active_background_tasks.is_empty());
}

/// A running turn holds the handover open — and with it every child agent,
/// because a child cannot outlive the turn that spawned it: `drive` drains
/// its background children on every exit path before returning, so a live
/// child is always a live turn. That is why retirement asks about turns and
/// background tasks and does not need a third counter for children.
#[tokio::test]
async fn a_running_turn_holds_the_handover_open() {
    let (base_url, model_stop) = hold_open_model_endpoint().await;
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), &base_url);
    let layout = Layout::from_parts(
        tmp.path().to_path_buf(),
        tmp.path().join("configs"),
        tmp.path().join("state"),
    );
    let app = Arc::new(Application::assemble(layout).unwrap());
    let token = tokio_util::sync::CancellationToken::new();
    let runtime = Arc::new(
        InProcessRuntimeClient::new(
            app,
            ModelRef::new("mock", "m"),
            PermissionProfile::Assisted,
            false,
        )
        .with_process_shutdown(token.clone()),
    );

    let bootstrap = runtime
        .create_session(CreateSessionRequest {
            approval_policy: leveler_client_protocol::ApprovalPolicy::Interactive,
            goal: "hold the turn open".to_string(),
            model: None,
            mode: WirePermissionProfile::Assisted,
        })
        .await
        .unwrap();
    let session = bootstrap.session.id.clone();
    runtime
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "work forever".to_string(),
            attachments: vec![],
        })
        .await
        .unwrap();
    assert_turn_running(&runtime, &session).await;

    // A second, idle session created before retirement: after retirement
    // starts, a turn submitted for it must be refused, not silently admitted.
    let other = runtime
        .create_session(CreateSessionRequest {
            approval_policy: leveler_client_protocol::ApprovalPolicy::Interactive,
            goal: "must not start during retirement".to_string(),
            model: None,
            mode: WirePermissionProfile::Assisted,
        })
        .await
        .unwrap()
        .session
        .id;

    runtime
        .send(leveler_client_protocol::ClientCommand::ShutdownWhenIdle {
            reason: leveler_client_protocol::RestartReason::BuildMismatch,
        })
        .await
        .unwrap();

    // The turn was already admitted, so it keeps running: retiring stops new
    // work, it does not cancel work in flight.
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(3), token.cancelled())
            .await
            .is_err(),
        "a runtime with a live turn has not finished; it must not retire"
    );
    assert!(runtime.runtime_info().await.unwrap().health.active_turns > 0);

    // And no new work gets in behind it, which is what stops the handover
    // from being postponed forever.
    assert!(
        !runtime.runtime_info().await.unwrap().health.accepting_work,
        "a retiring runtime does not take new work"
    );
    let refused = runtime
        .send(ClientCommand::SubmitMessage {
            session_id: other,
            content: "must not start".to_string(),
            attachments: vec![],
        })
        .await;
    assert!(
        refused
            .expect_err("a retiring runtime must refuse a new turn")
            .to_string()
            .contains("retiring"),
        "the refusal must say the runtime is retiring"
    );

    model_stop.cancel();
}

// ── Idle eviction: a daemon retires itself only when nobody is watching and
//    nothing is owed. Generation handover (Retiring) is a different path. ──

struct EvictionHarness {
    _tmp: tempfile::TempDir,
    app: Arc<Application>,
    runtime: Arc<InProcessRuntimeClient>,
    socket: std::path::PathBuf,
    token: CancellationToken,
    waiters: LocalWaiters,
    serve: tokio::task::JoinHandle<Result<(), leveler_local_transport::TransportError>>,
}

impl EvictionHarness {
    async fn shutdown(self) {
        self.token.cancel();
        let _ = self.serve.await;
    }
}

async fn eviction_harness(base_url: &str, timeout: std::time::Duration) -> EvictionHarness {
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), base_url);
    let layout = Layout::from_parts(
        tmp.path().to_path_buf(),
        tmp.path().join("configs"),
        tmp.path().join("state"),
    );
    let app = Arc::new(Application::assemble(layout).unwrap());
    let token = CancellationToken::new();
    let waiters = LocalWaiters::new();
    let runtime = Arc::new(
        InProcessRuntimeClient::new(
            app.clone(),
            ModelRef::new("mock", "m"),
            PermissionProfile::Assisted,
            false,
        )
        .with_process_shutdown(token.clone())
        .with_client_presence(waiters.clone()),
    );
    let socket = tmp.path().join("daemon.sock");
    let server = LocalSocketServer::bind_with_waiters(&socket, runtime.clone(), waiters.clone())
        .await
        .unwrap();
    let serve = tokio::spawn(server.serve(token.clone()));
    runtime.spawn_idle_eviction(timeout);
    EvictionHarness {
        _tmp: tmp,
        app,
        runtime,
        socket,
        token,
        waiters,
        serve,
    }
}

/// Wait for the transport's RAII count to reach `want`.
async fn wait_for_clients(h: &EvictionHarness, want: usize) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while h.waiters.count() != want {
        assert!(
            std::time::Instant::now() < deadline,
            "attached clients never reached {want} (now {})",
            h.waiters.count()
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn an_attached_client_prevents_idle_eviction() {
    let (base_url, _model) = hold_open_model_endpoint().await;
    let h = eviction_harness(&base_url, std::time::Duration::from_millis(250)).await;
    let client = LocalSocketRuntimeClient::connect(&h.socket).await.unwrap();
    wait_for_clients(&h, 1).await;
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    assert!(
        !h.token.is_cancelled(),
        "a runtime somebody is watching must not evict itself"
    );
    drop(client);
    h.shutdown().await;
}

#[tokio::test]
async fn the_last_client_leaving_evicts_an_idle_runtime() {
    let (base_url, _model) = hold_open_model_endpoint().await;
    let h = eviction_harness(&base_url, std::time::Duration::from_millis(250)).await;
    let client = LocalSocketRuntimeClient::connect(&h.socket).await.unwrap();
    wait_for_clients(&h, 1).await;
    drop(client);
    wait_for_clients(&h, 0).await;
    tokio::time::timeout(std::time::Duration::from_secs(5), h.token.cancelled())
        .await
        .expect("an idle runtime with no clients retires after its timeout");
    let _ = h.serve.await;
}

#[tokio::test]
async fn active_work_blocks_idle_eviction() {
    let (base_url, model_stop) = hold_open_model_endpoint().await;
    let h = eviction_harness(&base_url, std::time::Duration::from_millis(250)).await;
    let client = LocalSocketRuntimeClient::connect(&h.socket).await.unwrap();
    wait_for_clients(&h, 1).await;
    let session = client
        .create_session(CreateSessionRequest {
            approval_policy: leveler_client_protocol::ApprovalPolicy::Interactive,
            goal: "hold the daemon".to_string(),
            model: None,
            mode: WirePermissionProfile::Assisted,
        })
        .await
        .unwrap()
        .session
        .id;
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "work forever".to_string(),
            attachments: vec![],
        })
        .await
        .unwrap();
    assert_turn_running(&h.runtime, &session).await;
    drop(client);
    wait_for_clients(&h, 0).await;
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    assert!(
        !h.token.is_cancelled(),
        "a running turn must keep the daemon alive with no clients attached"
    );
    h.runtime
        .send(ClientCommand::CancelCurrentTurn {
            session_id: session,
        })
        .await
        .ok();
    tokio::time::timeout(std::time::Duration::from_secs(5), h.token.cancelled())
        .await
        .expect("once the turn ends, the idle daemon retires");
    let _ = h.serve.await;
    model_stop.cancel();
}

#[tokio::test]
async fn a_background_task_blocks_idle_eviction() {
    let (base_url, _model) = hold_open_model_endpoint().await;
    let h = eviction_harness(&base_url, std::time::Duration::from_millis(250)).await;
    let client = LocalSocketRuntimeClient::connect(&h.socket).await.unwrap();
    wait_for_clients(&h, 1).await;
    let task = h
        .app
        .background_tasks()
        .spawn(
            leveler_execution::ProcessRequest::new(
                "sleep",
                vec!["30".to_string()],
                h._tmp.path().to_path_buf(),
            ),
            None,
        )
        .await
        .expect("background task starts");
    drop(client);
    wait_for_clients(&h, 0).await;
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    assert!(
        !h.token.is_cancelled(),
        "a background task outliving its turn must keep the daemon alive"
    );
    h.app.background_tasks().kill(&task).await.ok();
    tokio::time::timeout(std::time::Duration::from_secs(5), h.token.cancelled())
        .await
        .expect("once the background work settles, the idle daemon retires");
    let _ = h.serve.await;
}

#[tokio::test]
async fn a_reconnect_before_the_timeout_keeps_the_runtime() {
    let (base_url, _model) = hold_open_model_endpoint().await;
    let h = eviction_harness(&base_url, std::time::Duration::from_millis(600)).await;
    let first = LocalSocketRuntimeClient::connect(&h.socket).await.unwrap();
    wait_for_clients(&h, 1).await;
    drop(first);
    wait_for_clients(&h, 0).await;
    // Reconnect inside the window: the countdown must reset.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let second = LocalSocketRuntimeClient::connect(&h.socket).await.unwrap();
    wait_for_clients(&h, 1).await;
    tokio::time::sleep(std::time::Duration::from_millis(900)).await;
    assert!(
        !h.token.is_cancelled(),
        "a reconnect must stop the pending idle eviction"
    );
    drop(second);
    tokio::time::timeout(std::time::Duration::from_secs(5), h.token.cancelled())
        .await
        .expect("after the last client leaves, idleness starts again");
    let _ = h.serve.await;
}

#[tokio::test]
async fn a_second_attached_client_prevents_eviction() {
    let (base_url, _model) = hold_open_model_endpoint().await;
    let h = eviction_harness(&base_url, std::time::Duration::from_millis(250)).await;
    let a = LocalSocketRuntimeClient::connect(&h.socket).await.unwrap();
    let b = LocalSocketRuntimeClient::connect(&h.socket).await.unwrap();
    wait_for_clients(&h, 2).await;
    drop(a);
    wait_for_clients(&h, 1).await;
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    assert!(
        !h.token.is_cancelled(),
        "one remaining client must keep the daemon alive"
    );
    drop(b);
    tokio::time::timeout(std::time::Duration::from_secs(5), h.token.cancelled())
        .await
        .expect("the last client leaving starts the countdown");
    let _ = h.serve.await;
}

#[tokio::test]
async fn retiring_ignores_client_presence() {
    let (base_url, _model) = hold_open_model_endpoint().await;
    let h = eviction_harness(&base_url, std::time::Duration::from_secs(30)).await;
    let client = LocalSocketRuntimeClient::connect(&h.socket).await.unwrap();
    wait_for_clients(&h, 1).await;
    h.runtime
        .send(ClientCommand::ShutdownWhenIdle {
            reason: leveler_client_protocol::RestartReason::BuildMismatch,
        })
        .await
        .unwrap();
    // Retirement is a generation handover: an attached client does not hold it
    // open (only turns and background work do).
    tokio::time::timeout(std::time::Duration::from_secs(3), h.token.cancelled())
        .await
        .expect("a quiescent retiring runtime exits even while a client is attached");
    let _ = h.serve.await;
    drop(client);
}

#[tokio::test]
async fn explicit_quit_does_not_trigger_idle_eviction() {
    let (base_url, _model) = hold_open_model_endpoint().await;
    let h = eviction_harness(&base_url, std::time::Duration::from_millis(250)).await;
    let client = LocalSocketRuntimeClient::connect(&h.socket).await.unwrap();
    wait_for_clients(&h, 1).await;
    h.runtime.send(ClientCommand::Quit).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    assert!(
        !h.token.is_cancelled(),
        "Quit is handled by the host's shutdown path; idle eviction stands down"
    );
    drop(client);
    h.shutdown().await;
}
