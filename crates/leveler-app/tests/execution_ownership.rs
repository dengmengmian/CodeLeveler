//! Execution ownership lifetime: a session's task is owned while an execution
//! runs in it, not for as long as the window that ran it stays open. Two
//! windows here are two `Application`s on one repository — one runtime, two
//! live boots — each driven through the in-process runtime a TUI embeds.

use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{ClientCommand, ClientError, InteractiveRuntimeClient};
use leveler_core::{BootId, SessionId};
use leveler_execution::PermissionProfile;
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_storage::{OwnershipStore, TaskOwner, TaskStore, TurnRepository};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
             retry:\n  max_attempts: 1\n  initial_backoff_ms: 10\n  max_backoff_ms: 10\n"
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
capabilities: { streaming: true, tool_calling: true, parallel_tool_calls: false, structured_output: true, reasoning: false, vision: false }
limits: { context_window: 8192, reliable_context: 4096, max_output_tokens: 1024, max_tool_schema_bytes: 8192, max_parallel_tool_calls: 1 }
compatibility: { synthesize_tool_call_ids: true, drop_unsupported_fields: true }
"#,
    )
    .unwrap();
}

fn layout(root: &std::path::Path) -> Layout {
    Layout::from_parts(root.to_path_buf(), root.join("configs"), root.join("state"))
}

/// The user message every `submit` in this file sends, and the marker that
/// tells the endpoint which requests the tests govern.
const SUBMITTED_TEXT: &str = "work";

/// The advisory instruction the predictor sends. It is how the endpoint — and
/// the preemption test — recognises a prediction that is in flight. A change to
/// that instruction fails that test loudly instead of leaving it quietly
/// preempting nothing.
const PREDICTION_INSTRUCTION: &str = "Predict the single most likely next message";

/// A model endpoint that holds every request until a test releases it, so a
/// turn stays genuinely running for as long as the test needs. Once failing,
/// every request — the held one and any retry — answers 400 at once. Asking, a
/// request let through calls a command that needs approval.
///
/// A release names the request it is for: [`Gate::release_turn`] releases the
/// next request carrying [`SUBMITTED_TEXT`]. Everything else the app sends on
/// its own — an advisory prompt prediction, the memory extractor after a turn
/// settles — is held but never released, so it cannot spend a release meant
/// for a turn. Handing out whichever request is first in line instead makes
/// the test's outcome depend on which of the app's requests the endpoint
/// happened to receive first.
struct Gate {
    held: Arc<std::sync::Mutex<Held>>,
    fail: Arc<std::sync::atomic::AtomicBool>,
    ask: Arc<std::sync::atomic::AtomicBool>,
    /// Advisory requests a turn cancelled while the endpoint held them. That
    /// is what a preemption looks like from the provider side.
    cancelled: Arc<std::sync::atomic::AtomicUsize>,
    url: String,
}

/// What the endpoint holds, and which releases it still owes.
#[derive(Default)]
struct Held {
    /// Requests waiting for a release, oldest first.
    parked: Vec<Parked>,
    /// Releases that arrived before the request they name.
    released: Vec<String>,
}

struct Parked {
    /// The marker this request carries, or `None` for a request no test
    /// releases.
    marker: Option<String>,
    /// Whether this is the predictor's advisory request.
    prediction: bool,
    serve: tokio::sync::oneshot::Sender<()>,
}

impl Held {
    /// Whether a request carrying `marker` may go now.
    fn take_release(&mut self, marker: Option<&str>) -> bool {
        self.forget_departed();
        let Some(marker) = marker else { return false };
        match self.released.iter().position(|owed| owed == marker) {
            Some(index) => {
                self.released.remove(index);
                true
            }
            None => false,
        }
    }

    /// Forget handlers that already left, so a release is never handed to a
    /// request whose client has hung up.
    fn forget_departed(&mut self) {
        self.parked.retain(|parked| !parked.serve.is_closed());
    }

    /// Advisory predictions the endpoint is holding right now.
    fn held_predictions(&mut self) -> usize {
        self.forget_departed();
        self.parked
            .iter()
            .filter(|parked| parked.prediction)
            .count()
    }
}

impl Gate {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let held = Arc::new(std::sync::Mutex::new(Held::default()));
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ask = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancelled = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let holding = held.clone();
        let failing = fail.clone();
        let asking = ask.clone();
        let cancelling = cancelled.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let held = holding.clone();
                let failing = failing.clone();
                let asking = asking.clone();
                let cancelling = cancelling.clone();
                tokio::spawn(async move {
                    let body = drain_request(&mut stream).await;
                    let fail = failing.load(std::sync::atomic::Ordering::SeqCst);
                    if !fail {
                        let marker = carries_a_turn(&body).then(|| SUBMITTED_TEXT.to_string());
                        let prediction = body.contains(PREDICTION_INSTRUCTION);
                        let owed = {
                            let mut held = held
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            held.take_release(marker.as_deref())
                        };
                        if !owed {
                            // Hold it until the test releases this marker. A
                            // request the app sent on its own waits for a
                            // release nobody owes it until its client hangs up.
                            let (serve, released) = tokio::sync::oneshot::channel();
                            {
                                let mut held = held
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                held.forget_departed();
                                held.parked.push(Parked {
                                    marker: marker.clone(),
                                    prediction,
                                    serve,
                                });
                            }
                            let served = tokio::select! {
                                biased;
                                // The client hung up (the product cancelled an
                                // advisory request). Checked first: a hang-up
                                // that has already arrived must beat a release
                                // handed out in the same instant.
                                _ = stream.read_u8() => false,
                                served = released => served.is_ok(),
                            };
                            if !served {
                                if prediction {
                                    cancelling.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                }
                                return;
                            }
                            // The socket itself is the final word: a release
                            // handed out in the same instant the client hung up
                            // is not spent on a request nobody awaits — it goes
                            // to the next request that wants it.
                            if client_hung_up(&mut stream) {
                                if let Some(marker) = marker {
                                    release(&held, &marker);
                                }
                                return;
                            }
                        }
                    }
                    let response = if fail {
                        let body = r#"{"error":{"message":"boom"}}"#;
                        format!(
                            "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\n\
                             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                    } else {
                        let frame = if asking.load(std::sync::atomic::Ordering::SeqCst) {
                            let arguments = serde_json::json!({
                                "program": "rm", "args": ["scratch.txt"], "reason": "clean up"
                            });
                            serde_json::json!({"choices": [{
                                "delta": {"tool_calls": [{
                                    "index": 0, "id": "call_1", "type": "function",
                                    "function": {"name": "run_command", "arguments": arguments.to_string()}
                                }]},
                                "finish_reason": "tool_calls"
                            }]})
                        } else {
                            serde_json::json!({
                                "choices": [{"delta": {"content": "done"}, "finish_reason": "stop"}]
                            })
                        };
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                             Connection: close\r\n\r\ndata: {frame}\n\ndata: [DONE]\n\n"
                        )
                    };
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        Self {
            held,
            fail,
            ask,
            cancelled,
            url,
        }
    }

    /// Whether the endpoint holds a prediction that is really in flight,
    /// within `within`.
    async fn holds_a_prediction_within(&self, within: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let holding = {
                let mut held = self
                    .held
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                held.held_predictions()
            };
            if holding > 0 {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Predictions the endpoint has seen cancelled so far.
    fn cancelled_predictions(&self) -> usize {
        self.cancelled.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Wait until a held prediction's client hung up: that is a preemption as
    /// the provider sees it.
    async fn wait_for_a_cancelled_prediction_since(&self, cancelled: usize) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while self.cancelled_predictions() <= cancelled {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the endpoint held a prediction and it was never cancelled"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// What this endpoint holds when a test's deadline expires: a turn that
    /// never settled either is waiting for a release this run still owes, or
    /// was never sent.
    fn diagnosis(&self) -> String {
        let mut held = self
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        held.forget_departed();
        format!(
            "gate: {} request(s) held, {} release(s) waiting for a request",
            held.parked.len(),
            held.released.len()
        )
    }

    /// Let the next request carrying `marker` through.
    fn release(&self, marker: &str) {
        release(&self.held, marker);
    }

    /// Let the next turn through, whichever of the app's requests arrive
    /// first.
    fn release_turn(&self) {
        self.release(SUBMITTED_TEXT);
    }

    fn ask_for_approval(&self) {
        self.ask.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    fn fail_from_now(&self) {
        self.fail.store(true, std::sync::atomic::Ordering::SeqCst);
        // A request that was already let through before failure is answered
        // like every request after it.
        self.release_turn();
    }
}

/// Hand `marker`'s release to the next request waiting for it, or keep it for
/// the one that has not arrived yet.
fn release(held: &std::sync::Mutex<Held>, marker: &str) {
    let mut held = held
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    held.forget_departed();
    match held
        .parked
        .iter()
        .position(|parked| parked.marker.as_deref() == Some(marker))
    {
        Some(index) => {
            let parked = held.parked.remove(index);
            let _ = parked.serve.send(());
        }
        None => held.released.push(marker.to_string()),
    }
}

/// Whether a request carries the user message the tests submit.
///
/// The JSON envelope, not the bare word: the app's own background callers (the
/// memory extractor, the prompt predictor) render the transcript as prompt
/// text, so only a real message envelope matches. A request that matched by
/// accident would simply be held, never served.
fn carries_a_turn(body: &str) -> bool {
    body.contains(&format!("\"content\":\"{SUBMITTED_TEXT}\""))
}

/// Whether the client of an already-drained request is gone.
///
/// Read from the socket rather than waiting to be woken by the event: the
/// question is asked exactly when a release was just handed out, which is the
/// one moment a released request can still be one nobody awaits. Only a closed
/// or reset connection qualifies — a read that failed for any other reason (an
/// interrupted one, say) is not evidence that nobody is waiting.
fn client_hung_up(stream: &mut tokio::net::TcpStream) -> bool {
    use std::io::ErrorKind;
    match stream.try_read(&mut [0u8; 1]) {
        Ok(0) => true,
        Ok(_) => false,
        Err(error) => matches!(
            error.kind(),
            ErrorKind::ConnectionReset
                | ErrorKind::ConnectionAborted
                | ErrorKind::BrokenPipe
                | ErrorKind::NotConnected
                | ErrorKind::UnexpectedEof
        ),
    }
}

async fn drain_request(stream: &mut tokio::net::TcpStream) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let Ok(n) = stream.read(&mut chunk).await else {
            return String::new();
        };
        if n == 0 {
            return String::new();
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let headers = String::from_utf8_lossy(&buf[..header_end]).to_ascii_lowercase();
    let length = headers
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    // The body is the one place a request says what it carries, so it is read
    // whole before the endpoint decides what to do with the handler.
    while buf.len() < header_end + length {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return String::new(),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    String::from_utf8_lossy(&buf[header_end..]).into_owned()
}

struct Window {
    app: Arc<Application>,
    client: InProcessRuntimeClient,
}

impl Window {
    fn boot(&self) -> BootId {
        self.app.boot_id().unwrap()
    }

    async fn submit(&self, session: &SessionId) -> Result<(), ClientError> {
        self.client
            .send(ClientCommand::SubmitMessage {
                session_id: session.clone(),
                content: SUBMITTED_TEXT.to_string(),
                attachments: vec![],
            })
            .await
    }
}

struct Windows {
    _tmp: tempfile::TempDir,
    gate: Gate,
    a: Window,
    b: Window,
    session: SessionId,
}

async fn two_windows() -> Windows {
    windows_with(PermissionProfile::Assisted).await
}

async fn windows_with(profile: PermissionProfile) -> Windows {
    let gate = Gate::start().await;
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), &gate.url);
    let model = ModelRef::new("mock", "m");
    let window = |app: Arc<Application>| Window {
        client: InProcessRuntimeClient::new_with_options(
            app.clone(),
            model.clone(),
            profile,
            false,
            false,
        )
        .with_durable_wire_ack(),
        app,
    };
    let a = window(Arc::new(Application::assemble(layout(tmp.path())).unwrap()));
    let session = a.app.create_session(&model, "shared").await.unwrap();
    let b = window(Arc::new(Application::assemble(layout(tmp.path())).unwrap()));
    assert_eq!(a.app.runtime_id().unwrap(), b.app.runtime_id().unwrap());
    assert_ne!(a.boot(), b.boot());
    Windows {
        _tmp: tmp,
        gate,
        a,
        b,
        session,
    }
}

impl Windows {
    async fn turns(&self) -> Vec<(String, Option<String>)> {
        let db = self.a.app.open_database().await.unwrap();
        TurnRepository::new(&db)
            .list(&self.session)
            .await
            .unwrap()
            .into_iter()
            .map(|turn| (turn.status, turn.owner_boot_id))
            .collect()
    }

    async fn owner(&self) -> TaskOwner {
        let db = self.a.app.open_database().await.unwrap();
        let task = TaskStore::task_for_session(&db, &self.session)
            .await
            .unwrap()
            .unwrap();
        OwnershipStore::current(&db, &task).await.unwrap().unwrap()
    }

    /// Wait until `turns` turns exist and the last one has ended with its
    /// task terminal committed (the session is no longer running).
    async fn settled(&self, turns: usize) -> Vec<(String, Option<String>)> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let rows = self.turns().await;
            let db = self.a.app.open_database().await.unwrap();
            let status = leveler_storage::SessionRepository::new(&db)
                .get(&self.session)
                .await
                .unwrap()
                .unwrap()
                .status;
            if rows.len() == turns
                && rows.iter().all(|(status, _)| status != "running")
                && status.as_str() != "running"
            {
                return rows;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "execution never settled: turns {rows:?}, session {status:?} ({})",
                self.gate.diagnosis()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn unowned_at(epoch: u64) -> TaskOwner {
        TaskOwner {
            runtime: None,
            boot: None,
            epoch: leveler_core::OwnerEpoch::new(epoch),
        }
    }
}

fn row(status: &str, boot: &BootId) -> (String, Option<String>) {
    (status.to_string(), Some(boot.as_str().to_string()))
}

/// A: A runs, B is refused. B: A finished and stays open, B runs. C: B runs,
/// A is refused. D: B finished, A runs again. Ownership follows the execution.
#[tokio::test]
async fn ownership_follows_the_running_execution_between_windows() {
    let w = two_windows().await;

    // A — Window A running; Window B's send is an ownership conflict that
    // moves nothing.
    w.a.submit(&w.session)
        .await
        .expect("A's first turn accepted");
    assert_eq!(w.turns().await, [row("running", &w.a.boot())]);
    let running = w.owner().await;
    let refused = w.b.submit(&w.session).await;
    assert!(
        matches!(refused, Err(ClientError::OwnershipConflict(_))),
        "{refused:?}"
    );
    assert_eq!(w.owner().await, running);
    assert_eq!(w.turns().await, [row("running", &w.a.boot())]);

    // B — A completes and stays open; B's send succeeds under a new
    // generation.
    w.gate.release_turn();
    w.settled(1).await;
    assert_eq!(w.owner().await, Windows::unowned_at(running.epoch.get()));
    w.b.submit(&w.session)
        .await
        .expect("an idle window does not block B");
    let b_running = w.owner().await;
    assert_eq!(b_running.boot, Some(w.b.boot()));
    assert_eq!(b_running.epoch, running.epoch.next().unwrap());

    // C — B running; A's send is refused.
    let refused = w.a.submit(&w.session).await;
    assert!(
        matches!(refused, Err(ClientError::OwnershipConflict(_))),
        "{refused:?}"
    );
    assert_eq!(w.owner().await, b_running);

    // D — B completes; A's send succeeds.
    w.gate.release_turn();
    w.settled(2).await;
    w.a.submit(&w.session)
        .await
        .expect("A takes its turn again");
    assert_eq!(w.owner().await.epoch, b_running.epoch.next().unwrap());
    w.gate.release_turn();
    let rows = w.settled(3).await;
    assert_eq!(
        rows,
        [
            row("completed", &w.a.boot()),
            row("completed", &w.b.boot()),
            row("completed", &w.a.boot()),
        ],
        "every turn keeps the boot that ran it"
    );
    assert_eq!(w.owner().await, Windows::unowned_at(3));
}

/// Advisory model work is lower priority than the next user turn. Starting a
/// real turn must cancel an in-flight prompt prediction before the turn sends
/// its own provider request, otherwise the prediction can consume the only
/// available request slot and strand execution.
#[tokio::test]
async fn a_new_turn_preempts_an_in_flight_prompt_prediction() {
    let w = two_windows().await;

    w.a.submit(&w.session).await.unwrap();
    w.gate.release_turn();
    w.settled(1).await;

    // The window drops an advisory request made while it still holds the
    // previous turn's lease, so ask until the endpoint really holds one: a
    // prediction in flight is the premise the turn below must preempt.
    let premise = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        w.a.client
            .send(ClientCommand::RequestPromptSuggestion {
                session_id: w.session.clone(),
            })
            .await
            .unwrap();
        if w.gate
            .holds_a_prediction_within(Duration::from_millis(250))
            .await
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < premise,
            "no advisory request reached the endpoint"
        );
    }
    let cancelled_before = w.gate.cancelled_predictions();

    w.a.submit(&w.session)
        .await
        .expect("the user turn preempts advisory generation");
    w.gate.release_turn();
    assert_eq!(w.settled(2).await.len(), 2);
    // The prediction was in flight when the turn was submitted, and nothing but
    // the turn cancelling it can end it. Without this the test would pass on a
    // product that never preempts anything: a release names the turn it is for,
    // so the prediction cannot spend it either way.
    w.gate
        .wait_for_a_cancelled_prediction_since(cancelled_before)
        .await;
}

/// A cancelled execution is a terminal one: the window that cancelled it
/// stays open and a sibling may run next.
#[tokio::test]
async fn an_interrupted_execution_releases_the_session() {
    let w = two_windows().await;
    w.a.submit(&w.session).await.unwrap();
    w.a.client
        .send(ClientCommand::CancelCurrentTurn {
            session_id: w.session.clone(),
        })
        .await
        .unwrap();
    assert_eq!(w.settled(1).await, [row("interrupted", &w.a.boot())]);
    assert_eq!(w.owner().await, Windows::unowned_at(1));
    w.b.submit(&w.session)
        .await
        .expect("B runs after A's interruption");
    assert_eq!(w.owner().await.boot, Some(w.b.boot()));
    // The interrupted request's release is not spent on it: the release is
    // named, so it goes to the live turn that follows.
    w.gate.release_turn();
    w.settled(2).await;
}

/// A failed execution is a terminal one too.
#[tokio::test]
async fn a_failed_execution_releases_the_session() {
    let w = two_windows().await;
    w.a.submit(&w.session).await.unwrap();
    w.gate.fail_from_now();
    assert_eq!(w.settled(1).await, [row("failed", &w.a.boot())]);
    assert_eq!(w.owner().await, Windows::unowned_at(1));
    w.b.submit(&w.session)
        .await
        .expect("B runs after A's failure");
    assert_eq!(w.owner().await.boot, Some(w.b.boot()));
}

/// Waiting on a human is still the execution: while A's turn waits for an
/// approval, B is refused and nothing moves.
#[tokio::test]
async fn an_execution_awaiting_approval_keeps_the_session() {
    let w = windows_with(PermissionProfile::RequestApproval).await;
    std::fs::write(w._tmp.path().join("scratch.txt"), "x").unwrap();
    w.gate.ask_for_approval();
    w.gate.release_turn();
    let mut events = w.a.client.subscribe();
    w.a.submit(&w.session).await.unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match events.recv().await.unwrap() {
                leveler_client_protocol::RuntimeEvent::ApprovalRequested { .. } => return,
                leveler_client_protocol::RuntimeEvent::TurnCompleted
                | leveler_client_protocol::RuntimeEvent::TurnFailed { .. } => {
                    panic!("the turn ended without asking")
                }
                _ => {}
            }
        }
    })
    .await
    .expect("A's turn reaches the approval");
    let waiting = w.owner().await;
    assert_eq!(waiting.boot, Some(w.a.boot()));

    let refused = w.b.submit(&w.session).await;
    assert!(
        matches!(refused, Err(ClientError::OwnershipConflict(_))),
        "{refused:?}"
    );
    assert_eq!(w.owner().await, waiting);
    assert_eq!(w.turns().await, [row("running", &w.a.boot())]);
}

/// A running turn — with a dangling mutating tool call when `dangling` — left
/// in the session by a boot of this runtime that has ended: its boot never
/// took a lease, so the probe finds it dead.
async fn crashed_turn(app: &Application, session: &SessionId, dangling: bool) -> BootId {
    let db = app.open_database().await.unwrap();
    let mut dead = app.task_engine(&db).unwrap();
    dead.boot.id = BootId::generate();
    let token = dead.acquire_ownership(session).await.unwrap();
    let turn = dead
        .stores
        .turns
        .start_owned(&token, session, "user", None, leveler_core::now())
        .await
        .unwrap();
    if !dangling {
        return dead.boot.id;
    }
    leveler_engine::EventLog::new(&db, session.clone())
        .append(
            Some(&leveler_core::TurnId::new(turn.id)),
            leveler_engine::EngineEvent::ToolCallStarted {
                call_id: "c1".into(),
                name: "apply_patch".into(),
                arguments: "{}".into(),
                parallel: false,
                risk: None,
                agent_id: None,
            },
            &mut |_| {},
        )
        .await
        .unwrap();
    dead.boot.id
}

impl Windows {
    /// Wait for the task's owner to become `expected`.
    async fn owner_becomes(&self, expected: TaskOwner) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let owner = self.owner().await;
            if owner == expected {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "owner stayed {owner:?}, expected {expected:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

impl Window {
    async fn shell(&self, session: &SessionId, command: &str) {
        self.client
            .send(ClientCommand::RunUserShell {
                session_id: session.clone(),
                command: command.to_string(),
            })
            .await
            .unwrap();
    }
}

/// The next user shell event of `kind` on `events`.
async fn shell_event(
    events: &mut tokio::sync::broadcast::Receiver<leveler_client_protocol::RuntimeEvent>,
    started: bool,
) -> leveler_client_protocol::RuntimeEvent {
    use leveler_client_protocol::RuntimeEvent;
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            match events.recv().await.unwrap() {
                event @ RuntimeEvent::UserShellStarted { .. } if started => return event,
                event @ RuntimeEvent::UserShellExited { .. } if !started => return event,
                _ => {}
            }
        }
    })
    .await
    .expect("the user shell event arrives")
}

/// U1/U4: a user shell is execution too. While it runs, a sibling window is
/// refused and nothing moves; once it is cancelled, the window stays open and
/// the sibling runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_running_user_shell_holds_the_session_until_it_ends() {
    use leveler_client_protocol::RuntimeEvent;
    let w = two_windows().await;
    let mut events = w.a.client.subscribe_session(&w.session);
    w.a.shell(&w.session, "sleep 30").await;
    let RuntimeEvent::UserShellStarted { execution_id, .. } = shell_event(&mut events, true).await
    else {
        unreachable!()
    };
    let running = w.owner().await;
    assert_eq!(running.boot, Some(w.a.boot()));

    let refused = w.b.submit(&w.session).await;
    assert!(
        matches!(refused, Err(ClientError::OwnershipConflict(_))),
        "{refused:?}"
    );
    assert_eq!(w.owner().await, running);

    w.a.client
        .send(ClientCommand::CancelUserShell {
            session_id: w.session.clone(),
            execution_id,
        })
        .await
        .unwrap();
    let RuntimeEvent::UserShellExited { status, .. } = shell_event(&mut events, false).await else {
        unreachable!()
    };
    assert_eq!(status, "cancelled");
    w.owner_becomes(Windows::unowned_at(running.epoch.get()))
        .await;
    w.b.submit(&w.session)
        .await
        .expect("B runs once A's shell has ended");
    assert_eq!(w.owner().await.boot, Some(w.b.boot()));
}

/// U2/U3: a shell that succeeds and one that fails both end their generation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_finished_user_shell_releases_the_session() {
    use leveler_client_protocol::RuntimeEvent;
    let w = two_windows().await;
    let mut events = w.a.client.subscribe_session(&w.session);
    for (epoch, (command, expected)) in [("echo done", "success"), ("exit 3", "failed")]
        .into_iter()
        .enumerate()
    {
        w.a.shell(&w.session, command).await;
        let RuntimeEvent::UserShellExited { status, .. } = shell_event(&mut events, false).await
        else {
            unreachable!()
        };
        assert_eq!(status, expected, "{command}");
        w.owner_becomes(Windows::unowned_at(epoch as u64 + 1)).await;
    }
    w.b.submit(&w.session)
        .await
        .expect("an idle window's finished shells do not hold the session");
    let owner = w.owner().await;
    assert_eq!(owner.boot, Some(w.b.boot()));
    assert_eq!(owner.epoch.get(), 3);
}

/// R1/R2: recovering a dead boot's turn is finite work. The recovering window
/// stays open, the dead boot keeps its turn's provenance, and another window
/// runs next without waiting for the recovering one to exit.
#[tokio::test]
async fn recovery_leaves_a_dead_boots_session_to_whoever_runs_next() {
    let w = two_windows().await;
    let dead = crashed_turn(&w.a.app, &w.session, false).await;

    // Window B starts up beside it: its startup recovery settles the turn.
    w.b.app
        .create_session(&ModelRef::new("mock", "m"), "beside")
        .await
        .unwrap();
    assert_eq!(w.turns().await, [row("interrupted", &dead)]);
    assert_eq!(w.owner().await, Windows::unowned_at(2));

    w.a.submit(&w.session)
        .await
        .expect("A runs next while the recovering window stays open");
    assert_eq!(w.owner().await.boot, Some(w.a.boot()));
}

/// A1/A2: acknowledging a crash window closes its dangling calls and is then
/// done — it starts no execution, so it leaves the session unowned.
#[tokio::test]
async fn acknowledging_a_crash_window_leaves_the_session_to_whoever_runs_next() {
    let w = two_windows().await;
    crashed_turn(&w.a.app, &w.session, true).await;

    let closed = w.b.app.acknowledge_crash_window(&w.session).await.unwrap();
    assert_eq!(closed, 1);
    assert_eq!(w.owner().await, Windows::unowned_at(2));

    w.a.submit(&w.session)
        .await
        .expect("A runs next while the acknowledging window stays open");
    assert_eq!(w.owner().await.boot, Some(w.a.boot()));
}
