//! Reaper authority: a runtime may interrupt only a running turn it can prove
//! orphaned. Several hosts share one repository state — and so one RuntimeId —
//! while each is its own live boot.

use std::sync::Arc;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{ClientCommand, ClientError, InteractiveRuntimeClient};
use leveler_core::SessionId;
use leveler_execution::PermissionProfile;
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_storage::{MessageRepository, OwnershipStore, TaskOwner, TaskStore, TurnRepository};

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
        format!(
            "id: mock\nprotocol: openai_chat\nbase_url: {base_url}\n\
             timeouts:\n  connect_seconds: 5\n  request_seconds: 300\n  \
             idle_stream_seconds: 300\nretry:\n  max_attempts: 1\n  \
             initial_backoff_ms: 10\n  max_backoff_ms: 10\n"
        ),
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

/// A model endpoint that accepts and never answers, so a turn stays running.
async fn hold_open_model_endpoint() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let _stream = stream;
                std::future::pending::<()>().await;
            });
        }
    });
    format!("http://{addr}")
}

fn layout(root: &std::path::Path) -> Layout {
    Layout::from_parts(root.to_path_buf(), root.join("configs"), root.join("state"))
}

async fn turn_statuses(app: &Application, session: &SessionId) -> Vec<String> {
    let db = app.open_database().await.unwrap();
    TurnRepository::new(&db)
        .list(session)
        .await
        .unwrap()
        .into_iter()
        .map(|turn| turn.status)
        .collect()
}

/// Wait until `needle` is readable from the session's transcript.
///
/// The `running` turn row and the user's message row are two writes by the
/// SPAWNED turn, not by the `SubmitMessage` that returned: accepting the command
/// only stages the turn. A read that straddles the second write sees a session
/// whose transcript is still empty — ubuntu CI run 35239415660 read the sibling
/// snapshot in exactly that window — so the test has to observe the row before
/// asserting on it.
async fn wait_for_message(app: &Application, session: &SessionId, needle: &str) {
    let db = app.open_database().await.unwrap();
    let repo = MessageRepository::new(&db);
    for _ in 0..200 {
        let payloads = repo.load(session).await.unwrap();
        if payloads.iter().any(|p| p.contains(needle)) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("{needle:?} never reached the transcript");
}

/// Boot A runs a turn. Boot B — another host on the same repository, with the
/// same RuntimeId — starts up and creates a session, as `leveler run` or an
/// embedded TUI does. A is alive, so its turn must still be running.
#[tokio::test]
async fn a_starting_sibling_does_not_interrupt_a_live_boots_running_turn() {
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), &hold_open_model_endpoint().await);
    let model = ModelRef::new("mock", "m");

    let app_a = Arc::new(Application::assemble(layout(tmp.path())).unwrap());
    let session = app_a.create_session(&model, "live work").await.unwrap();
    let client_a = InProcessRuntimeClient::new_with_options(
        app_a.clone(),
        model.clone(),
        PermissionProfile::Assisted,
        false,
        false,
    )
    .with_durable_wire_ack();
    client_a
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "keep working".to_string(),
            attachments: vec![],
        })
        .await
        .expect("turn accepted");
    assert_eq!(turn_statuses(&app_a, &session).await, ["running"]);

    let app_b = Application::assemble(layout(tmp.path())).unwrap();
    assert_eq!(app_a.runtime_id().unwrap(), app_b.runtime_id().unwrap());
    app_b.create_session(&model, "sibling start").await.unwrap();

    assert_eq!(
        turn_statuses(&app_b, &session).await,
        ["running"],
        "a live boot's turn is not an orphan"
    );
}

async fn task_owner(app: &Application, session: &SessionId) -> TaskOwner {
    let db = app.open_database().await.unwrap();
    let task = TaskStore::task_for_session(&db, session)
        .await
        .unwrap()
        .expect("the running session has a task");
    OwnershipStore::current(&db, &task).await.unwrap().unwrap()
}

/// Boot A with one genuinely running turn, and a second boot B on the same
/// repository — the same RuntimeId — with its own client.
struct LiveSibling {
    _tmp: tempfile::TempDir,
    app_a: Arc<Application>,
    client_a: InProcessRuntimeClient,
    app_b: Arc<Application>,
    client_b: InProcessRuntimeClient,
    session: SessionId,
    owner_before: TaskOwner,
}

async fn live_sibling() -> LiveSibling {
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), &hold_open_model_endpoint().await);
    let model = ModelRef::new("mock", "m");
    let client = |app: &Arc<Application>| {
        InProcessRuntimeClient::new_with_options(
            app.clone(),
            model.clone(),
            PermissionProfile::Assisted,
            false,
            false,
        )
        .with_durable_wire_ack()
    };
    let app_a = Arc::new(Application::assemble(layout(tmp.path())).unwrap());
    let session = app_a.create_session(&model, "live work").await.unwrap();
    let client_a = client(&app_a);
    client_a
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "keep working".to_string(),
            attachments: vec![],
        })
        .await
        .expect("turn accepted");
    assert_eq!(turn_statuses(&app_a, &session).await, ["running"]);
    // The user's row is written by the spawned turn, not by the command that
    // returned above: a sibling reading the snapshot before it lands sees an
    // empty transcript. Settle it once here so every test built on this
    // fixture compares a session that has stopped moving.
    wait_for_message(&app_a, &session, "keep working").await;
    let owner_before = task_owner(&app_a, &session).await;

    let app_b = Arc::new(Application::assemble(layout(tmp.path())).unwrap());
    assert_eq!(app_a.runtime_id().unwrap(), app_b.runtime_id().unwrap());
    let client_b = client(&app_b);
    LiveSibling {
        _tmp: tmp,
        app_a,
        client_a,
        app_b,
        client_b,
        session,
        owner_before,
    }
}

impl LiveSibling {
    /// A's turn is untouched: still running, still A's, same generation.
    async fn assert_a_untouched(&self, what: &str) {
        assert_eq!(
            turn_statuses(&self.app_b, &self.session).await,
            ["running"],
            "{what}: a live boot's turn must stay running, and no second turn may start"
        );
        assert_eq!(
            task_owner(&self.app_b, &self.session).await,
            self.owner_before,
            "{what}: a live boot's ownership must not be fenced"
        );
    }

    /// A still holds a current generation: its own cancel lands its own
    /// fenced terminal write, which releases the task at that same generation
    /// — no sibling moved it.
    async fn assert_a_still_writes(&self) {
        self.client_a
            .send(ClientCommand::CancelCurrentTurn {
                session_id: self.session.clone(),
            })
            .await
            .unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        let released = TaskOwner {
            runtime: None,
            boot: None,
            epoch: self.owner_before.epoch,
        };
        loop {
            let owner = task_owner(&self.app_a, &self.session).await;
            if owner == released {
                break;
            }
            assert_eq!(
                owner, self.owner_before,
                "boot A's generation was moved by someone else"
            );
            assert!(
                tokio::time::Instant::now() < deadline,
                "boot A could not settle its own turn: {:?}",
                turn_statuses(&self.app_a, &self.session).await
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(
            turn_statuses(&self.app_a, &self.session).await,
            ["interrupted"]
        );
    }
}

/// T5: a sibling boot cannot start a turn in a session a live boot is running.
#[tokio::test]
async fn a_sibling_cannot_start_a_turn_in_a_live_boots_session() {
    let fixture = live_sibling().await;
    let rejected = fixture
        .client_b
        .send(ClientCommand::SubmitMessage {
            session_id: fixture.session.clone(),
            content: "take over".to_string(),
            attachments: vec![],
        })
        .await;
    assert!(
        matches!(rejected, Err(ClientError::OwnershipConflict(_))),
        "the sibling's turn must be refused as an ownership conflict: {rejected:?}"
    );
    fixture.assert_a_untouched("sibling submit").await;
    fixture.assert_a_still_writes().await;
}

/// T6: cancelling from a sibling boot does not reach across the database into
/// a live boot's turn.
#[tokio::test]
async fn a_sibling_cancel_does_not_end_a_live_boots_turn() {
    let fixture = live_sibling().await;
    let rejected = fixture
        .client_b
        .send(ClientCommand::CancelCurrentTurn {
            session_id: fixture.session.clone(),
        })
        .await;
    assert!(
        matches!(rejected, Err(ClientError::OwnershipConflict(_))),
        "a cancel that cannot reach the owning boot must say so: {rejected:?}"
    );
    fixture.assert_a_untouched("sibling cancel").await;
    fixture.assert_a_still_writes().await;
}

/// T7: a sibling boot shutting down settles only its own work.
#[tokio::test]
async fn a_sibling_quit_does_not_reap_a_live_boots_turn() {
    let fixture = live_sibling().await;
    fixture.client_b.send(ClientCommand::Quit).await.unwrap();
    fixture.assert_a_untouched("sibling quit").await;
    fixture.assert_a_still_writes().await;
}

/// T11/T12: the running turn, and the task it runs under, are owned by the
/// same boot the application admits commands as — written with the turn, not
/// after it.
#[tokio::test]
async fn a_running_turn_belongs_to_the_applications_one_boot() {
    let fixture = live_sibling().await;
    let boot = fixture.app_a.boot_id().unwrap();
    let db = fixture.app_a.open_database().await.unwrap();
    let turns = TurnRepository::new(&db)
        .list(&fixture.session)
        .await
        .unwrap();
    assert_eq!(turns[0].status, "running");
    assert_eq!(turns[0].owner_boot_id.as_deref(), Some(boot.as_str()));
    assert_eq!(fixture.owner_before.boot, Some(boot));
    assert_ne!(
        fixture.app_b.boot_id().unwrap(),
        fixture.app_a.boot_id().unwrap()
    );
}

/// T4/T19: one application is one boot, however many callers start it at
/// once; the next process on the same state is the same runtime and a new
/// boot, and the first boot is dead once its process is gone.
#[tokio::test]
async fn one_application_is_one_boot_and_a_restart_is_another() {
    isolate_global_config();
    let tmp = tempfile::tempdir().unwrap();
    let first = Arc::new(Application::assemble(layout(tmp.path())).unwrap());
    let ids: Vec<_> = (0..8)
        .map(|_| {
            let app = first.clone();
            std::thread::spawn(move || app.boot_id().unwrap())
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    assert!(ids.iter().all(|id| id == &ids[0]), "{ids:?}");
    let state_dir = first.layout.state_dir.clone();
    assert_eq!(
        leveler_app::runtime_boot::boot_liveness(&state_dir, &ids[0]),
        leveler_core::BootLiveness::Alive
    );

    let restarted = Application::assemble(layout(tmp.path())).unwrap();
    assert_eq!(first.runtime_id().unwrap(), restarted.runtime_id().unwrap());
    assert_ne!(restarted.boot_id().unwrap(), ids[0]);

    drop(first);
    assert_eq!(
        leveler_app::runtime_boot::boot_liveness(&state_dir, &ids[0]),
        leveler_core::BootLiveness::Dead
    );
}

/// T13: ownership fences execution, not reading. A sibling still opens,
/// lists and reads a session a live boot is running.
#[tokio::test]
async fn a_sibling_still_reads_a_live_boots_session() {
    let fixture = live_sibling().await;
    let snapshot = fixture
        .client_b
        .snapshot(&fixture.session)
        .await
        .expect("a sibling can read the session");
    assert_eq!(snapshot.id, fixture.session);
    assert!(
        snapshot
            .messages
            .iter()
            .any(|m| m.text.contains("keep working")),
        "the sibling sees the live transcript"
    );
    fixture
        .client_b
        .send(ClientCommand::OpenSession {
            session_id: fixture.session.clone(),
        })
        .await
        .expect("a sibling can open the session");
    let db = fixture.app_b.open_database().await.unwrap();
    let listed = leveler_storage::SessionRepository::new(&db)
        .list()
        .await
        .unwrap();
    assert!(listed.iter().any(|s| s.id == fixture.session.as_str()));
    fixture.assert_a_untouched("sibling reads").await;
}

/// T8: a turn left running before boots were recorded blocks a new turn in
/// its session — refused as unknown ownership, never interrupted and run over.
#[tokio::test]
async fn a_running_turn_without_a_boot_refuses_a_new_turn() {
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), &hold_open_model_endpoint().await);
    let model = ModelRef::new("mock", "m");
    let app = Arc::new(Application::assemble(layout(tmp.path())).unwrap());
    let session = app.create_session(&model, "legacy").await.unwrap();
    let db = app.open_database().await.unwrap();
    TurnRepository::new(&db)
        .start(&session, "chat", None, leveler_core::now())
        .await
        .unwrap();
    let client = InProcessRuntimeClient::new_with_options(
        app.clone(),
        model,
        PermissionProfile::Assisted,
        false,
        false,
    )
    .with_durable_wire_ack();

    let rejected = client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "continue".to_string(),
            attachments: vec![],
        })
        .await;
    assert!(
        matches!(rejected, Err(ClientError::OwnershipConflict(_))),
        "{rejected:?}"
    );
    assert_eq!(turn_statuses(&app, &session).await, ["running"]);
}
