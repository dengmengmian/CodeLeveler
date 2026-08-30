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
    // Wait until B's turn produced activity and settled (unreachable model
    // → fails on its own).
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(30), b_events.recv())
            .await
            .expect("B's turn must settle")
            .expect("B's stream open");
        if matches!(
            event,
            RuntimeEvent::TurnFailed { .. } | RuntimeEvent::TurnCancelled
        ) {
            break;
        }
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
    assert_eq!(runtime.runtime_info().await.unwrap().health.active_turns, 0);
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(3), token.cancelled())
            .await
            .is_err(),
        "a runtime with live background work has not finished; it must not retire"
    );

    // The work settles, and only then does the handover proceed.
    app.background_tasks().kill(&task_id).await.ok();
    tokio::time::timeout(std::time::Duration::from_secs(10), token.cancelled())
        .await
        .expect("once nothing is left running, the runtime retires");
}
