//! M5: command delivery goes through the idempotency/versioning envelope.
//!
//! `InProcessRuntimeClient::deliver` must dedup at-least-once delivery by
//! `command_id` (a duplicate never re-dispatches its action) and reject a
//! command issued against a stale `expected_version` (optimistic concurrency).

use std::sync::Arc;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    ClientCommand, ClientError, CommandEnvelope, InteractiveRuntimeClient,
    PermissionProfile as WirePermissionProfile, RuntimeEvent,
};
use leveler_core::{CommandId, SessionId};
use leveler_execution::PermissionProfile;
use leveler_local_transport::{CreateSessionRequest, LocalRuntimeService};
#[cfg(unix)]
use leveler_local_transport::{LocalSocketRuntimeClient, LocalSocketServer};
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_storage::TurnRepository;
#[cfg(unix)]
use tokio_util::sync::CancellationToken;

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
  middleware: []
  synthesize_tool_call_ids: true
  drop_unsupported_fields: true
"#,
    )
    .unwrap();
}

async fn build_client() -> (
    tempfile::TempDir,
    Arc<Application>,
    Arc<InProcessRuntimeClient>,
    SessionId,
) {
    let tmp = tempfile::tempdir().unwrap();
    // An unreachable base_url is fine: these tests observe the synchronous
    // dispatch effect (a UserMessageAdded event), not the background turn.
    write_config(tmp.path(), "http://127.0.0.1:9");
    let layout = Layout {
        repo_root: tmp.path().to_path_buf(),
        config_dir: tmp.path().join("configs"),
        state_dir: tmp.path().join("state"),
    };
    let app = Arc::new(Application::assemble(layout).unwrap());
    let model = ModelRef::new("mock", "m");
    let session_id = app.create_session(&model, "goal").await.unwrap();
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        model,
        PermissionProfile::Assisted,
        false,
    ));
    (tmp, app, client, session_id)
}

#[tokio::test]
async fn duplicate_command_id_dispatches_once() {
    let (_tmp, _app, client, session_id) = build_client().await;
    let mut rx = client.subscribe();
    let envelope = CommandEnvelope {
        command_id: CommandId::new("cmd-dup"),
        session_id: session_id.clone(),
        expected_version: None,
        issued_at: "2026-07-12T00:00:00Z".to_string(),
        command: ClientCommand::SubmitMessage {
            session_id: session_id.clone(),
            content: "hi".to_string(),
            attachments: vec![],
        },
    };
    client.deliver(envelope.clone()).await.unwrap();
    client.deliver(envelope).await.unwrap(); // same command_id, at-least-once retry

    let mut user_messages = 0;
    while let Ok(event) = rx.try_recv() {
        if matches!(event, RuntimeEvent::UserMessageAdded { .. }) {
            user_messages += 1;
        }
    }
    assert_eq!(
        user_messages, 1,
        "a duplicate command_id must not dispatch the action twice"
    );
}

#[tokio::test]
async fn stale_expected_version_is_rejected() {
    let (_tmp, _app, client, session_id) = build_client().await;
    // The fresh session's log is at 0; a command expecting version 999 was
    // issued against a stale view and must be rejected (resync required).
    let envelope = CommandEnvelope {
        command_id: CommandId::new("cmd-ver"),
        session_id: session_id.clone(),
        expected_version: Some(999),
        issued_at: "2026-07-12T00:00:00Z".to_string(),
        command: ClientCommand::RequestDiff {
            session_id: session_id.clone(),
        },
    };
    let err = client.deliver(envelope).await.unwrap_err();
    assert!(
        matches!(err, ClientError::Runtime(_)),
        "stale version must be a runtime error, got {err:?}"
    );
}

#[tokio::test]
async fn envelope_command_session_mismatch_is_rejected() {
    let (_tmp, _app, client, session_id) = build_client().await;
    // The envelope targets the real session, but the command payload targets a
    // different one — version/receipt checks would key off A while B is acted on.
    let envelope = CommandEnvelope {
        command_id: CommandId::new("cmd-mismatch"),
        session_id: session_id.clone(),
        expected_version: None,
        issued_at: "2026-07-12T00:00:00Z".to_string(),
        command: ClientCommand::RequestDiff {
            session_id: SessionId::new("some-other-session"),
        },
    };
    let err = client.deliver(envelope).await.unwrap_err();
    assert!(
        matches!(&err, ClientError::Runtime(message) if message.contains("mismatch")),
        "a cross-session envelope must be rejected up front, got {err:?}"
    );
}

#[tokio::test]
async fn reused_command_id_with_different_payload_is_rejected() {
    let (_tmp, _app, client, session_id) = build_client().await;
    let first = CommandEnvelope {
        command_id: CommandId::new("cmd-payload-conflict"),
        session_id: session_id.clone(),
        expected_version: None,
        issued_at: "2026-07-12T00:00:00Z".to_string(),
        command: ClientCommand::RequestDiff {
            session_id: session_id.clone(),
        },
    };
    client.deliver(first.clone()).await.unwrap();
    let mut conflicting = first;
    conflicting.command = ClientCommand::ClearConversation {
        session_id: session_id.clone(),
    };
    let err = client.deliver(conflicting).await.unwrap_err();
    assert!(
        matches!(&err, ClientError::Runtime(message) if message.contains("different session or payload")),
        "id reuse with another payload must be a clear conflict, got {err:?}"
    );
}

#[tokio::test]
async fn daemon_session_runtime_options_are_isolated_per_session() {
    let (_tmp, app, client, _existing_session) = build_client().await;
    let first = client
        .create_session(CreateSessionRequest {
            goal: "first".to_string(),
            model: None,
            mode: WirePermissionProfile::RequestApproval,
        })
        .await
        .unwrap();
    let second = client
        .create_session(CreateSessionRequest {
            goal: "second".to_string(),
            model: None,
            mode: WirePermissionProfile::FullAccess,
        })
        .await
        .unwrap();

    let first_after_second = client.snapshot(&first.session.id).await.unwrap();
    assert_eq!(
        first_after_second.mode,
        WirePermissionProfile::RequestApproval
    );
    assert_eq!(second.session.mode, WirePermissionProfile::FullAccess);

    drop(client);
    let restored = InProcessRuntimeClient::new(
        app,
        ModelRef::new("mock", "m"),
        PermissionProfile::Assisted,
        false,
    );
    assert_eq!(
        restored.snapshot(&first.session.id).await.unwrap().mode,
        WirePermissionProfile::RequestApproval,
        "daemon restart must restore the session's persisted runtime options"
    );
}

#[tokio::test]
async fn creating_a_daemon_session_does_not_reap_another_live_turn() {
    let (_tmp, app, client, live_session) = build_client().await;
    let db = app.open_database().await.unwrap();
    TurnRepository::new(&db)
        .start(&live_session, "chat", None, leveler_core::now())
        .await
        .unwrap();

    client
        .create_session(CreateSessionRequest {
            goal: "another session".to_string(),
            model: None,
            mode: WirePermissionProfile::Assisted,
        })
        .await
        .unwrap();

    let turns = TurnRepository::new(&db).list(&live_session).await.unwrap();
    assert_eq!(turns[0].status, "running");
    assert!(
        turns[0].finished_at.is_none(),
        "a live daemon turn belongs to the daemon and must not be treated as a zombie"
    );
}

#[tokio::test]
async fn daemon_snapshots_keep_checkpoints_scoped_to_their_session() {
    let (_tmp, _app, client, _existing_session) = build_client().await;
    let first = client
        .create_session(CreateSessionRequest {
            goal: "first".to_string(),
            model: None,
            mode: WirePermissionProfile::Assisted,
        })
        .await
        .unwrap();
    let second = client
        .create_session(CreateSessionRequest {
            goal: "second".to_string(),
            model: None,
            mode: WirePermissionProfile::Assisted,
        })
        .await
        .unwrap();

    client
        .send(ClientCommand::SubmitMessage {
            session_id: first.session.id.clone(),
            content: "first checkpoint".to_string(),
            attachments: vec![],
        })
        .await
        .unwrap();
    client
        .send(ClientCommand::SubmitMessage {
            session_id: second.session.id.clone(),
            content: "second checkpoint".to_string(),
            attachments: vec![],
        })
        .await
        .unwrap();

    let first_snapshot = client.snapshot(&first.session.id).await.unwrap();
    let second_snapshot = client.snapshot(&second.session.id).await.unwrap();
    assert_eq!(first_snapshot.checkpoints.len(), 1);
    assert_eq!(first_snapshot.checkpoints[0].label, "first checkpoint");
    assert_eq!(second_snapshot.checkpoints.len(), 1);
    assert_eq!(second_snapshot.checkpoints[0].label, "second checkpoint");

    client
        .send(ClientCommand::CancelCurrentTurn {
            session_id: first.session.id,
        })
        .await
        .unwrap();
    client
        .send(ClientCommand::CancelCurrentTurn {
            session_id: second.session.id,
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn daemon_event_subscriptions_are_isolated_per_session() {
    let (_tmp, _app, client, _existing_session) = build_client().await;
    let first = client
        .create_session(CreateSessionRequest {
            goal: "first".to_string(),
            model: None,
            mode: WirePermissionProfile::RequestApproval,
        })
        .await
        .unwrap();
    let second = client
        .create_session(CreateSessionRequest {
            goal: "second".to_string(),
            model: None,
            mode: WirePermissionProfile::RequestApproval,
        })
        .await
        .unwrap();
    let mut first_events = client.subscribe_session(&first.session.id);
    let mut second_events = client.subscribe_session(&second.session.id);

    client
        .send(ClientCommand::SetPermissionProfile {
            session_id: first.session.id.clone(),
            mode: WirePermissionProfile::FullAccess,
        })
        .await
        .unwrap();

    assert!(matches!(
        first_events.recv().await.unwrap(),
        RuntimeEvent::SessionUpdated { .. }
    ));
    assert!(matches!(
        second_events.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn socket_clients_receive_only_their_session_events() {
    let (tmp, _app, runtime, _existing_session) = build_client().await;
    let path = tmp.path().join("runtime.sock");
    let server = LocalSocketServer::bind(&path, runtime).await.unwrap();
    let shutdown = CancellationToken::new();
    let task = tokio::spawn(server.serve(shutdown.clone()));
    let client = LocalSocketRuntimeClient::connect(&path).await.unwrap();

    let first = client
        .create_session(CreateSessionRequest {
            goal: "first over socket".to_string(),
            model: None,
            mode: WirePermissionProfile::RequestApproval,
        })
        .await
        .unwrap();
    let second = client
        .create_session(CreateSessionRequest {
            goal: "second over socket".to_string(),
            model: None,
            mode: WirePermissionProfile::RequestApproval,
        })
        .await
        .unwrap();
    let mut first_events = client.subscribe_session(&first.session.id);
    let mut second_events = client.subscribe_session(&second.session.id);

    client
        .send(ClientCommand::SetPermissionProfile {
            session_id: first.session.id,
            mode: WirePermissionProfile::FullAccess,
        })
        .await
        .unwrap();

    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(1), first_events.recv())
            .await
            .unwrap()
            .unwrap(),
        RuntimeEvent::SessionUpdated { .. }
    ));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), second_events.recv())
            .await
            .is_err(),
        "a socket subscriber must not receive another session's event"
    );

    shutdown.cancel();
    task.await.unwrap().unwrap();
}

/// Wait for the spawned background turn to reach a terminal event so the next
/// SubmitMessage is not rejected by the one-active-turn-per-session guard.
async fn wait_turn_settled(rx: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>) {
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
            .await
            .expect("turn must settle within 10s")
            .expect("event stream open");
        if matches!(
            event,
            RuntimeEvent::TurnCompleted
                | RuntimeEvent::TurnAnswered
                | RuntimeEvent::TurnTruncated { .. }
                | RuntimeEvent::TurnIncomplete { .. }
                | RuntimeEvent::TurnCompletedUnverified { .. }
                | RuntimeEvent::TurnFailed { .. }
                | RuntimeEvent::TurnCancelled
        ) {
            return;
        }
    }
}

/// The first real message names a placeholder interactive session: the goal
/// column (what the web/TUI sidebars show) becomes the message's first
/// sentence. Later messages never rename, and a session created with a real
/// goal keeps it.
#[tokio::test]
async fn first_message_retitles_a_placeholder_session() {
    let (_tmp, app, client, _existing) = build_client().await;
    let bootstrap = client
        .create_session(CreateSessionRequest {
            goal: "interactive session".to_string(),
            model: None,
            mode: WirePermissionProfile::Assisted,
        })
        .await
        .unwrap();
    let session_id = bootstrap.session.id.clone();
    let mut events = client.subscribe_session(&session_id);

    client
        .send(ClientCommand::SubmitMessage {
            session_id: session_id.clone(),
            content: "帮我修复登录超时的 bug。另外顺便看下日志轮转。".to_string(),
            attachments: vec![],
        })
        .await
        .unwrap();
    wait_turn_settled(&mut events).await;

    let db = app.open_database().await.unwrap();
    let repo = leveler_storage::SessionRepository::new(&db);
    let record = repo.get(&session_id).await.unwrap().unwrap();
    assert_eq!(
        record.goal, "帮我修复登录超时的 bug",
        "placeholder goal must become the first sentence of the first message"
    );

    // A second message must not rename the session again.
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session_id.clone(),
            content: "再帮我看看别的问题。".to_string(),
            attachments: vec![],
        })
        .await
        .unwrap();
    wait_turn_settled(&mut events).await;
    let record = repo.get(&session_id).await.unwrap().unwrap();
    assert_eq!(
        record.goal, "帮我修复登录超时的 bug",
        "no rename on later messages"
    );

    // A session created with a real goal keeps it untouched.
    let named = client
        .create_session(CreateSessionRequest {
            goal: "已有正式目标".to_string(),
            model: None,
            mode: WirePermissionProfile::Assisted,
        })
        .await
        .unwrap();
    let mut named_events = client.subscribe_session(&named.session.id);
    client
        .send(ClientCommand::SubmitMessage {
            session_id: named.session.id.clone(),
            content: "随便聊聊。".to_string(),
            attachments: vec![],
        })
        .await
        .unwrap();
    wait_turn_settled(&mut named_events).await;
    let record = repo.get(&named.session.id).await.unwrap().unwrap();
    assert_eq!(record.goal, "已有正式目标");
}

/// The session-menu commands: rename overwrites the title, archive hides the
/// session from the default list (transcript intact), fork clones record +
/// transcript into a fresh session leaving the original untouched.
#[tokio::test]
async fn session_menu_rename_archive_fork_roundtrip() {
    let (_tmp, app, client, session_id) = build_client().await;
    let db = app.open_database().await.unwrap();
    let sessions = leveler_storage::SessionRepository::new(&db);
    let messages = leveler_storage::MessageRepository::new(&db);

    // Seed a transcript so fork has something to copy.
    messages
        .append(
            &session_id,
            &[
                r#"{"role":"user","content":[{"type":"text","text":"修复登录"}]}"#.into(),
                r#"{"role":"assistant","content":[{"type":"text","text":"好的"}]}"#.into(),
            ],
            leveler_core::now(),
        )
        .await
        .unwrap();

    // Rename.
    client
        .send(ClientCommand::RenameSession {
            session_id: session_id.clone(),
            name: "  登录修复方案  ".to_string(),
        })
        .await
        .unwrap();
    assert_eq!(
        sessions.get(&session_id).await.unwrap().unwrap().goal,
        "登录修复方案",
        "rename trims and overwrites the title"
    );

    // Fork: a new session appears with the transcript copied.
    client
        .send(ClientCommand::ForkSession {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    let all = sessions.list().await.unwrap();
    assert_eq!(all.len(), 2, "fork adds one session: {all:?}");
    let fork = all
        .iter()
        .find(|r| r.id != session_id.as_str())
        .expect("forked session listed");
    assert_eq!(fork.goal, "登录修复方案 (分叉)");
    let fork_id = SessionId::new(fork.id.clone());
    assert_eq!(
        messages.load(&fork_id).await.unwrap().len(),
        2,
        "fork copies the transcript"
    );
    assert_eq!(
        messages.load(&session_id).await.unwrap().len(),
        2,
        "original transcript untouched"
    );

    // Archive: leaves the default list, transcript intact.
    client
        .send(ClientCommand::ArchiveSession {
            session_id: fork_id.clone(),
        })
        .await
        .unwrap();
    let listed = sessions.list().await.unwrap();
    assert_eq!(listed.len(), 1, "archived fork left the list");
    assert!(
        sessions.get(&fork_id).await.unwrap().is_some(),
        "archive is not delete"
    );
}
