//! The durable ACK contract, held to the SAME terms by every interactive
//! client.
//!
//! A successful delivery answer must not let a client believe that a command
//! whose task was never durably admitted is durable. There is now ONE ACK
//! contract: `deliver` returns only after the engine committed the turn's
//! write-ahead input record — the `turns` row carrying the user's message —
//! which is what makes a post-crash retry with the same `command_id` safe.
//! The in-process runtime inside the `leveler` process, the `/web` UI served
//! from that same runtime, and the daemon `leveler serve` exposes through the
//! same client all wait on that acceptance. The old split — the daemon
//! awaiting admission while the embedded configuration answered as soon as a
//! worker was spawned — is gone; `docs/ARCHITECTURE.zh-CN.md` (§13.2.1) was
//! updated with it.
//!
//! The durable consequence is the point of this file: `command_receipts` may
//! record `completed` — the sole proof that lets every later delivery of that
//! `command_id` be answered as a duplicate — only from a real durable
//! admission. An `Ok(())` that meant only "work was started" produced a false
//! durable success for a command the runtime refused, and the user's input was
//! then unrecoverable. The three tests below reproduce that false success and
//! now pass against the fix; they are no longer ignored.
//!
//! The scenario is chosen so that "was this command durably admitted?" has a
//! deterministic answer. A live boot of the same runtime already owns the
//! session and is genuinely mid-turn (the model endpoint accepts and never
//! answers), so the engine refuses the second turn before it starts. No
//! assertion here depends on how fast a spawned turn happens to run.

use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    AttachmentId, AttachmentKind, AttachmentRef, ClientCommand, ClientError, CommandEnvelope,
    InteractiveRuntimeClient, ProtocolEnvelope,
};
use leveler_core::{CommandId, SessionId};
use leveler_execution::PermissionProfile;
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_storage::{Admission, CommandReceiptRepository, TurnRepository};
use sha2::{Digest, Sha256};

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

/// A model endpoint that accepts and never answers, so an admitted turn stays
/// running for as long as the test needs it to own the session.
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

const SUBMITTED_TEXT: &str = "durable ack probe";

fn model() -> ModelRef {
    ModelRef::new("mock", "m")
}

/// The one in-process configuration every interactive client uses: the
/// `leveler` process's TUI runtime, the `/web` UI served from that runtime,
/// and the daemon `leveler serve` exposes. There is no longer a second
/// setting for the ACK boundary.
fn embedded(app: Arc<Application>) -> InProcessRuntimeClient {
    InProcessRuntimeClient::new_with_options(
        app,
        model(),
        PermissionProfile::Assisted,
        false,
        false,
    )
}

/// The same client under its daemon name. Deliberately identical to
/// [`embedded`]: the tests below keep the two names so the contract is
/// exercised from both entry points, and so a future divergence is a compile-
/// time decision rather than a silent one.
fn daemon(app: Arc<Application>) -> InProcessRuntimeClient {
    embedded(app)
}

/// A chat application on an isolated repository, and the session to submit
/// into. The model endpoint is the caller's, so a test can decide whether the
/// admitted turn stays running (`hold_open`) or fails at once (unreachable).
struct FreshWindow {
    _tmp: tempfile::TempDir,
    app: Arc<Application>,
    session: SessionId,
}

async fn fresh_window(base_url: &str) -> FreshWindow {
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), base_url);
    let app = Arc::new(
        Application::assemble(layout(tmp.path()))
            .unwrap()
            .with_collaboration(leveler_agent::CollaborationMode::Chat),
    );
    let session = app.create_session(&model(), "durable ack").await.unwrap();
    FreshWindow {
        _tmp: tmp,
        app,
        session,
    }
}

/// An image the configured `mock` model cannot read (`vision: false`), so a
/// submission carrying it is refused before any worker is spawned.
fn unreadable_image() -> AttachmentRef {
    AttachmentRef {
        id: AttachmentId::new("att-ack"),
        kind: AttachmentKind::Image,
        name: "clipboard.png".to_string(),
        mime_type: "image/png".to_string(),
        size_bytes: 1024,
        sha256: "0".repeat(64),
        width: Some(10),
        height: Some(10),
    }
}

fn submission_with(
    command_id: &str,
    session_id: &SessionId,
    content: &str,
    attachments: Vec<AttachmentRef>,
) -> CommandEnvelope {
    CommandEnvelope {
        command_id: CommandId::new(command_id),
        session_id: session_id.clone(),
        expected_version: None,
        issued_at: leveler_core::now().to_rfc3339(),
        command: ClientCommand::SubmitMessage {
            session_id: session_id.clone(),
            content: content.to_string(),
            attachments,
        },
    }
}

fn submission(command_id: &str, session_id: &SessionId, content: &str) -> CommandEnvelope {
    submission_with(command_id, session_id, content, vec![])
}

async fn turns(app: &Application, session: &SessionId) -> usize {
    let db = app.open_database().await.unwrap();
    TurnRepository::new(&db).list(session).await.unwrap().len()
}

/// The durable verdict a later boot reads for `envelope` — the same question
/// `deliver` asks before it decides whether to dispatch again.
async fn durable_verdict(app: &Application, envelope: &CommandEnvelope) -> Option<Admission> {
    let db = app.open_database().await.unwrap();
    let fingerprint = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&envelope.command).unwrap())
    );
    CommandReceiptRepository::new(&db)
        .classify_terminal(&envelope.command_id, &envelope.session_id, &fingerprint)
        .await
        .unwrap()
}

/// Let every turn this test spawned finish before the test's runtime goes away.
///
/// A successful ACK is admission, not completion, so the turn a delivery
/// started is still running when the test's assertions are done. This cancels
/// it and waits for the lease to be released, so the test does not tear its
/// runtime down around a live worker.
async fn drain(client: &InProcessRuntimeClient, session: &SessionId) {
    let _ = client
        .send(ClientCommand::CancelCurrentTurn {
            session_id: session.clone(),
        })
        .await;
    for _ in 0..400 {
        if !client.has_live_turn(session) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("a spawned turn never released its lease");
}

/// A session owned by a live, genuinely running turn of boot A.
struct OwnedSession {
    _tmp: tempfile::TempDir,
    owner: Arc<Application>,
    owner_client: InProcessRuntimeClient,
    session: SessionId,
    envelope: CommandEnvelope,
}

async fn owned_session() -> OwnedSession {
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), &hold_open_model_endpoint().await);
    let owner = Arc::new(
        Application::assemble(layout(tmp.path()))
            .unwrap()
            .with_collaboration(leveler_agent::CollaborationMode::Chat),
    );
    let session = owner.create_session(&model(), "held").await.unwrap();
    // The owner's own submission is answered only once its turn is durably
    // admitted, so everything that follows reads a settled ownership state.
    let owner_client = daemon(owner.clone());
    let accepted = owner_client
        .deliver(submission("cmd-owner", &session, "held"))
        .await;
    assert!(
        accepted.is_ok(),
        "the owning boot's first turn must be admitted: {accepted:?}"
    );
    assert_eq!(
        turns(&owner, &session).await,
        1,
        "the owning turn must be durable before the second window acts"
    );
    let envelope = submission("cmd-refused", &session, SUBMITTED_TEXT);
    OwnedSession {
        _tmp: tmp,
        owner,
        owner_client,
        session,
        envelope,
    }
}

/// A second window on the same repository: another boot of the same runtime,
/// so the runtime id matches and only the boot differs.
fn second_window(tmp: &std::path::Path) -> Arc<Application> {
    Arc::new(
        Application::assemble(layout(tmp))
            .unwrap()
            .with_collaboration(leveler_agent::CollaborationMode::Chat),
    )
}

/// Let this test's turns finish: the owner's, and whatever the second window
/// started.
async fn drain_all(owned: &OwnedSession, other: Option<&InProcessRuntimeClient>) {
    if let Some(other) = other {
        drain(other, &owned.session).await;
    }
    drain(&owned.owner_client, &owned.session).await;
}

/// Control: the daemon configuration reports the refused admission in the
/// caller's own terms, and leaves nothing durable behind. This is the
/// behaviour the embedded configuration must match.
#[tokio::test]
async fn the_daemon_configuration_reports_a_refused_admission() {
    let owned = owned_session().await;
    let other = daemon(second_window(owned._tmp.path()));
    let delivered = tokio::time::timeout(
        Duration::from_secs(20),
        other.deliver(owned.envelope.clone()),
    )
    .await
    .expect("the daemon delivery must answer, not hang");

    assert!(
        matches!(delivered, Err(ClientError::OwnershipConflict(_))),
        "the daemon configuration must surface the ownership refusal: {delivered:?}"
    );
    assert_eq!(
        turns(&owned.owner, &owned.session).await,
        1,
        "a refused turn must not leave a turn row"
    );
    drain_all(&owned, Some(&other)).await;
}

/// The embedded configuration answers the SAME scenario the daemon does: a
/// refusal is surfaced in the caller's own terms, and nothing durable is left
/// behind.
#[tokio::test]
async fn the_embedded_configuration_reports_a_refused_admission() {
    let owned = owned_session().await;
    let other = embedded(second_window(owned._tmp.path()));
    let delivered = tokio::time::timeout(
        Duration::from_secs(20),
        other.deliver(owned.envelope.clone()),
    )
    .await
    .expect("the embedded delivery must answer, not hang");

    assert!(
        matches!(delivered, Err(ClientError::OwnershipConflict(_))),
        "embedded and daemon must answer an admission refusal the same way: {delivered:?}"
    );
    assert_eq!(
        turns(&owned.owner, &owned.session).await,
        1,
        "a refused turn must not leave a turn row"
    );
    drain_all(&owned, Some(&other)).await;
}

/// The durable truth behind the answer above. A delivery whose turn the
/// runtime never admitted is not settled `completed` in the receipt log:
/// `completed` is the sole proof that lets a later retry be answered as a
/// duplicate, and a duplicate is never dispatched again.
#[tokio::test]
async fn an_embedded_refusal_does_not_settle_the_receipt_as_completed() {
    let owned = owned_session().await;
    let other = embedded(second_window(owned._tmp.path()));
    let delivered = tokio::time::timeout(
        Duration::from_secs(20),
        other.deliver(owned.envelope.clone()),
    )
    .await
    .expect("the embedded delivery must answer, not hang");
    let _ = delivered;

    let verdict = durable_verdict(&owned.owner, &owned.envelope).await;
    assert!(
        !matches!(verdict, Some(Admission::AlreadyCompleted)),
        "a refused command must not be durably settled as completed: {verdict:?}"
    );
    assert_eq!(
        turns(&owned.owner, &owned.session).await,
        1,
        "a refused turn must not leave a turn row"
    );
    drain_all(&owned, Some(&other)).await;
}

/// The consequence of the answer above. A delivery the runtime refused stays
/// `dispatching`, never `completed`, so every later retry carrying the same
/// `command_id` is answered as an error (outcome unknown / unresolvable) — it
/// is neither reported delivered nor silently swallowed while the session has
/// no record of the message.
#[tokio::test]
async fn a_refused_submission_is_not_answered_as_already_completed() {
    let owned = owned_session().await;
    let refused_by = embedded(second_window(owned._tmp.path()));
    let first = tokio::time::timeout(
        Duration::from_secs(20),
        refused_by.deliver(owned.envelope.clone()),
    )
    .await
    .expect("the embedded delivery must answer, not hang");
    // The first delivery must report the refusal; what matters after that is
    // the durable verdict a later boot reads for the same command id.
    let _ = first;
    drain_all(&owned, Some(&refused_by)).await;
    drop(refused_by);

    // A later boot of the same runtime retries the SAME envelope, exactly as
    // the TUI's redelivery loop and a reconnecting browser do.
    let other = daemon(second_window(owned._tmp.path()));
    let retried = tokio::time::timeout(
        Duration::from_secs(20),
        other.deliver(owned.envelope.clone()),
    )
    .await
    .expect("the retry must answer, not hang");

    assert!(
        retried.is_err(),
        "a command with no durable effect and no recoverable outcome must not be \
         answered as delivered: {retried:?}"
    );
    assert_eq!(
        turns(&owned.owner, &owned.session).await,
        1,
        "the retry must not have started a turn either"
    );
    drain_all(&owned, Some(&other)).await;
}

// ---------------------------------------------------------------------------
// The success side of the same contract. A refusal is the easy half; these fix
// the meaning of a *successful* ACK: it is durable admission, it is not
// completion, and it is only *then* that the receipt may become `completed`.
// ---------------------------------------------------------------------------

/// ACK-15 / ACK-03: an embedded success is backed by the turn row, and the
/// receipt becomes `completed` only once that row exists.
///
/// The model endpoint accepts and never answers, so if the ACK were waiting on
/// the turn it would hang forever. It must not: `TurnStarted` is emitted right
/// after `TurnStore::start_owned`, which is exactly the durable admission the
/// caller is owed.
async fn assert_an_embedded_success_ack_is_durable_admission() {
    let window = fresh_window(&hold_open_model_endpoint().await).await;
    let client = embedded(window.app.clone());
    let envelope = submission("cmd-embedded-ok", &window.session, SUBMITTED_TEXT);

    let delivered = tokio::time::timeout(Duration::from_secs(20), client.deliver(envelope.clone()))
        .await
        .expect("an admitted embedded delivery must answer, not hang");
    assert!(
        delivered.is_ok(),
        "the embedded ACK must succeed: {delivered:?}"
    );

    assert_eq!(
        turns(&window.app, &window.session).await,
        1,
        "a successful ACK must be backed by the durable turn row"
    );
    assert!(
        client.has_live_turn(&window.session),
        "the ACK is admission, not completion: the turn is still running"
    );
    assert!(
        matches!(
            durable_verdict(&window.app, &envelope).await,
            Some(Admission::AlreadyCompleted)
        ),
        "a durably admitted command settles completed exactly once"
    );

    drain(&client, &window.session).await;
}

#[tokio::test]
async fn an_embedded_success_ack_is_durable_admission() {
    assert_an_embedded_success_ack_is_durable_admission().await;
}

/// ACK-11: the same scenario on a multi-thread runtime, so the fix is not
/// resting on current-thread scheduling. A blocking worker that could not be
/// awaited would deadlock here rather than merely be slow.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_embedded_success_ack_does_not_deadlock_a_multi_thread_runtime() {
    assert_an_embedded_success_ack_is_durable_admission().await;
}

/// ACK-02: the WebUI's entry point is `deliver_protocol` (the frame the
/// WebSocket handler hands the runtime), not `deliver`. It must reach the same
/// admission barrier — a browser told `ack` may not be told that for a command
/// the runtime refused.
#[tokio::test]
async fn the_web_protocol_entry_reaches_the_same_admission_barrier() {
    let owned = owned_session().await;
    let other = embedded(second_window(owned._tmp.path()));
    let delivered = tokio::time::timeout(
        Duration::from_secs(20),
        other.deliver_protocol(ProtocolEnvelope::wrap(owned.envelope.clone())),
    )
    .await
    .expect("the web entry point must answer, not hang");

    assert!(
        matches!(delivered, Err(ClientError::OwnershipConflict(_))),
        "the WebUI's entry point must surface the refusal, not ack it: {delivered:?}"
    );
    assert_eq!(
        turns(&owned.owner, &owned.session).await,
        1,
        "a refused turn must not leave a turn row"
    );
    drain_all(&owned, Some(&other)).await;
}

/// ACK-16: a command the runtime refuses *before* any worker is spawned — here
/// a model with no vision handed an image — is answered with an error and is
/// never settled `completed`. The retry is an error too, so the client rolls
/// back instead of being told the message was delivered.
#[tokio::test]
async fn a_command_refused_before_its_worker_starts_is_never_completed() {
    let window = fresh_window("http://127.0.0.1:9").await;
    let client = embedded(window.app.clone());
    let envelope = submission_with(
        "cmd-no-worker",
        &window.session,
        SUBMITTED_TEXT,
        vec![unreadable_image()],
    );

    let delivered = tokio::time::timeout(Duration::from_secs(20), client.deliver(envelope.clone()))
        .await
        .expect("the refusal must answer, not hang");
    assert!(
        delivered.is_err(),
        "a submission the runtime refuses before starting a worker must not be acked: {delivered:?}"
    );
    assert_eq!(
        turns(&window.app, &window.session).await,
        0,
        "a refused submission must not leave a turn row"
    );
    let verdict = durable_verdict(&window.app, &envelope).await;
    assert!(
        !matches!(verdict, Some(Admission::AlreadyCompleted)),
        "a refused command must not be durably completed: {verdict:?}"
    );

    // A retry of the same id is not silently answered as delivered either.
    let retried = tokio::time::timeout(Duration::from_secs(20), client.deliver(envelope.clone()))
        .await
        .expect("the retry must answer, not hang");
    assert!(
        retried.is_err(),
        "a command with no durable effect must not be answered as delivered: {retried:?}"
    );
    assert_eq!(turns(&window.app, &window.session).await, 0);
}
