//! Runtime P2A process-level E2E: the real `leveler` binary, real daemons,
//! real sockets, isolated `LEVELER_HOME`/`LEVELER_CONFIG_DIR` per test.
//!
//! - Scenario A: RuntimeId survives a clean daemon stop/restart.
//! - Scenario D: two daemons racing the same repository elect exactly one,
//!   and clients discover exactly one identity.
//! - Scenario E: SIGKILL during a running task; a restarted daemon reaps the
//!   orphan turn (no zombie `running` rows), the transcript survives without
//!   duplication, and the session remains openable.
//!
//! Terminal-rendering-level TUI automation is intentionally out of scope
//! (NOT VERIFIED IN THIS ENVIRONMENT); these tests prove the runtime facts
//! the TUI is a client of.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use leveler_client_protocol::{ClientCommand, InteractiveRuntimeClient};
use leveler_local_transport::{
    CreateSessionRequest, LocalRuntimeService, LocalSocketRuntimeClient,
};

struct TestEnv {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    config_dir: PathBuf,
    repo: PathBuf,
}

/// A fully isolated environment: its own home (state, sockets), config
/// bundle (mock provider), and repository.
fn test_env(base_url: &str) -> TestEnv {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let repo = tmp.path().join("repo");
    let config_dir = tmp.path().join("configs");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(config_dir.join("providers")).unwrap();
    std::fs::create_dir_all(config_dir.join("models")).unwrap();
    std::fs::write(
        config_dir.join("providers/mock.yaml"),
        format!("id: mock\nprotocol: openai_chat\nbase_url: {base_url}\n"),
    )
    .unwrap();
    std::fs::write(
        config_dir.join("models/m.yaml"),
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
    TestEnv {
        _tmp: tmp,
        home,
        config_dir,
        repo,
    }
}

fn spawn_serve(env: &TestEnv, ready: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_leveler"))
        .arg("--repo")
        .arg(&env.repo)
        .arg("serve")
        .arg("--ready-json")
        .arg(ready)
        .env("LEVELER_HOME", &env.home)
        .env("LEVELER_CONFIG_DIR", &env.config_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn leveler serve")
}

#[cfg(feature = "test-crash-barrier")]
fn spawn_serve_with_after_turn_started_barrier(
    env: &TestEnv,
    ready: &Path,
    barrier: &Path,
) -> Child {
    spawn_serve_with_barrier(
        env,
        ready,
        "LEVELER_TEST_AFTER_TURN_STARTED_BARRIER",
        barrier,
    )
}

#[cfg(feature = "test-crash-barrier")]
fn spawn_serve_with_barrier(
    env: &TestEnv,
    ready: &Path,
    barrier_var: &str,
    barrier: &Path,
) -> Child {
    Command::new(env!("CARGO_BIN_EXE_leveler"))
        .arg("--repo")
        .arg(&env.repo)
        .arg("serve")
        .arg("--ready-json")
        .arg(ready)
        .env("LEVELER_HOME", &env.home)
        .env("LEVELER_CONFIG_DIR", &env.config_dir)
        .env(barrier_var, barrier)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn leveler serve with crash barrier")
}

/// Wait for the daemon's ready file; panics with the child's status on
/// premature exit so a startup failure is diagnosable.
fn wait_ready(ready: &Path, child: &mut Child, timeout: Duration) -> serde_json::Value {
    let deadline = Instant::now() + timeout;
    loop {
        if ready.is_file()
            && let Ok(raw) = std::fs::read_to_string(ready)
            && let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw)
        {
            return value;
        }
        if let Some(status) = child.try_wait().expect("child status") {
            panic!("daemon exited before readiness: {status}");
        }
        assert!(
            Instant::now() < deadline,
            "daemon never became ready within {timeout:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn stop_daemon(child: &mut Child) {
    // SIGINT = the daemon's documented Ctrl+C shutdown path.
    unsafe {
        libc_kill(child.id() as i32, 2);
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if child.try_wait().expect("child status").is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("daemon did not stop on SIGINT");
}

/// Minimal libc-free kill(2) via the `kill` binary would race PID reuse in
/// theory; direct syscall through std is not exposed, so shell out is the
/// pragmatic choice for a test.
unsafe fn libc_kill(pid: i32, signal: i32) {
    let _ = Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(pid.to_string())
        .status();
}

/// The single socket the environment's daemon listens on.
fn find_socket(env: &TestEnv) -> PathBuf {
    let sock_dir = env.home.join("run/sockets");
    let mut sockets: Vec<PathBuf> = std::fs::read_dir(&sock_dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "sock"))
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(sockets.len(), 1, "expected exactly one daemon socket");
    sockets.pop().unwrap()
}

/// The single per-repo state dir under the isolated home.
fn find_state_dir(env: &TestEnv) -> PathBuf {
    let projects = env.home.join("state/projects");
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&projects)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(dirs.len(), 1, "expected exactly one project state dir");
    dirs.pop().unwrap()
}

/// Scenario A: a daemon restart keeps the same RuntimeId; the id equals the
/// persisted state-dir identity.
#[test]
fn runtime_id_survives_a_daemon_restart() {
    let env = test_env("http://127.0.0.1:9");
    let ready1 = env.home.join("ready1.json");
    let mut daemon = spawn_serve(&env, &ready1);
    let first = wait_ready(&ready1, &mut daemon, Duration::from_secs(30));
    let first_id = first["runtime_id"]
        .as_str()
        .expect("runtime_id")
        .to_string();
    assert!(!first_id.is_empty());
    stop_daemon(&mut daemon);

    let ready2 = env.home.join("ready2.json");
    let mut daemon = spawn_serve(&env, &ready2);
    let second = wait_ready(&ready2, &mut daemon, Duration::from_secs(30));
    let second_id = second["runtime_id"]
        .as_str()
        .expect("runtime_id")
        .to_string();
    stop_daemon(&mut daemon);

    assert_eq!(
        first_id, second_id,
        "a restart must keep the runtime identity"
    );
    let persisted = std::fs::read_to_string(find_state_dir(&env).join("runtime-id")).unwrap();
    assert_eq!(persisted.trim(), first_id);
}

/// Scenario D: two daemons racing one repository — exactly one survives, and
/// the socket answers with exactly that identity.
#[tokio::test]
async fn concurrent_daemon_starts_elect_exactly_one_runtime() {
    let env = test_env("http://127.0.0.1:9");
    let ready_a = env.home.join("ready-a.json");
    let ready_b = env.home.join("ready-b.json");
    let mut a = spawn_serve(&env, &ready_a);
    let mut b = spawn_serve(&env, &ready_b);

    // One contender must exit (the election loser); the other must be ready.
    let deadline = Instant::now() + Duration::from_secs(30);
    let (mut winner, winner_ready) = loop {
        let a_exit = a.try_wait().expect("a status");
        let b_exit = b.try_wait().expect("b status");
        match (a_exit, b_exit) {
            (Some(status), None) => {
                assert!(
                    !status.success(),
                    "the losing daemon must exit with an error, got {status}"
                );
                break (b, ready_b.clone());
            }
            (None, Some(status)) => {
                assert!(!status.success());
                break (a, ready_a.clone());
            }
            (Some(_), Some(_)) => panic!("both daemons exited; nobody won the election"),
            (None, None) => {
                assert!(
                    Instant::now() < deadline,
                    "election never settled: both daemons still alive"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    };

    let ready = wait_ready(&winner_ready, &mut winner, Duration::from_secs(30));
    let winner_id = ready["runtime_id"]
        .as_str()
        .expect("runtime_id")
        .to_string();

    // Discovery: a client connecting to the repo's socket reaches exactly
    // the winner's identity, which is also the persisted one.
    let socket = find_socket(&env);
    let client = LocalSocketRuntimeClient::connect(&socket).await.unwrap();
    let info = LocalRuntimeService::runtime_info(&client).await.unwrap();
    assert_eq!(info.runtime_id.as_str(), winner_id);
    let persisted = std::fs::read_to_string(find_state_dir(&env).join("runtime-id")).unwrap();
    assert_eq!(persisted.trim(), winner_id);

    drop(client);
    stop_daemon(&mut winner);
}

/// A model endpoint that accepts and holds connections open forever, so a
/// turn is genuinely running when the daemon is killed.
async fn hold_open_model_endpoint() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                // Hold until the peer goes away.
                let _stream = stream;
                std::future::pending::<()>().await;
            });
        }
    });
    (format!("http://{addr}"), handle)
}

/// Scenario E: SIGKILL mid-task, restart, recover. The restarted daemon reaps
/// the orphan `running` turn, the transcript survives exactly once, and the
/// session snapshot is still served. Side-effect replay conservatism itself
/// is locked by `leveler-engine/tests/crash_recovery_test.rs`; this proves
/// the process-level path into those semantics.
#[tokio::test]
async fn sigkill_during_a_task_recovers_on_restart_without_duplication() {
    let (base_url, _model) = hold_open_model_endpoint().await;
    let env = test_env(&base_url);
    let ready1 = env.home.join("ready1.json");
    let mut daemon = spawn_serve(&env, &ready1);
    let ready = wait_ready(&ready1, &mut daemon, Duration::from_secs(30));
    let runtime_id = ready["runtime_id"].as_str().unwrap().to_string();

    let socket = find_socket(&env);
    let client = LocalSocketRuntimeClient::connect(&socket).await.unwrap();
    let session = client
        .create_session(CreateSessionRequest {
            approval_policy: leveler_client_protocol::ApprovalPolicy::Interactive,
            goal: "crash me".to_string(),
            model: None,
            mode: leveler_client_protocol::PermissionProfile::Assisted,
        })
        .await
        .unwrap()
        .session
        .id;
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "MARKER_BEFORE_CRASH".to_string(),
            attachments: vec![],
        })
        .await
        .unwrap();

    // `send` ACKs only after the running turn and its write-ahead initiating
    // input are durable. A one-shot read immediately after ACK locks that
    // contract; no polling is allowed to hide an early ACK.
    let db_path = find_state_dir(&env).join("sessions.db");
    let db = leveler_storage::Database::connect(&db_path).await.unwrap();
    let turns = leveler_storage::TurnRepository::new(&db)
        .list(&session)
        .await
        .unwrap();
    let running = turns
        .iter()
        .find(|turn| turn.status == "running")
        .expect("ACK must follow the durable running turn");
    assert!(
        running
            .payload
            .as_deref()
            .is_some_and(|payload| payload.contains("MARKER_BEFORE_CRASH")),
        "the running row must carry replayable initiating input: {running:?}"
    );
    drop(client);
    drop(db);

    // SIGKILL: no shutdown path runs — this is the crash case.
    daemon.kill().expect("SIGKILL daemon");
    let _ = daemon.wait();

    // Restart: startup reap must clear the orphan running turn.
    let ready2 = env.home.join("ready2.json");
    let mut daemon = spawn_serve(&env, &ready2);
    let ready = wait_ready(&ready2, &mut daemon, Duration::from_secs(30));
    assert_eq!(
        ready["runtime_id"].as_str().unwrap(),
        runtime_id,
        "a crash must not change the runtime identity"
    );

    let db = leveler_storage::Database::connect(&db_path).await.unwrap();
    let turns = leveler_storage::TurnRepository::new(&db)
        .list(&session)
        .await
        .unwrap();
    assert!(!turns.is_empty(), "the crashed turn must be persisted");
    assert!(
        turns.iter().all(|t| t.status != "running"),
        "restart must reap orphan running turns: {turns:?}"
    );
    let messages = leveler_storage::MessageRepository::new(&db)
        .load(&session)
        .await
        .unwrap();
    let marker_count = messages
        .iter()
        .filter(|p| p.contains("MARKER_BEFORE_CRASH"))
        .count();
    assert_eq!(
        marker_count, 1,
        "recovery must not duplicate the transcript"
    );

    // The session is still served after restart.
    let client = LocalSocketRuntimeClient::connect(&find_socket(&env))
        .await
        .unwrap();
    let snapshot = client.snapshot(&session).await.unwrap();
    assert_eq!(snapshot.id, session);
    assert!(
        snapshot
            .messages
            .iter()
            .any(|m| m.text.contains("MARKER_BEFORE_CRASH")),
        "the reopened session must carry the pre-crash transcript"
    );

    drop(client);
    stop_daemon(&mut daemon);
}

/// Running turns and task owners, read straight from the shared database.
async fn running_turns(db: &leveler_storage::Database) -> Vec<leveler_storage::TurnRecord> {
    leveler_storage::TurnStore::list_running(db, None)
        .await
        .unwrap()
}

async fn task_owner(
    db: &leveler_storage::Database,
    session: &leveler_core::SessionId,
) -> leveler_storage::TaskOwner {
    let task = leveler_storage::TaskStore::task_for_session(db, session)
        .await
        .unwrap()
        .expect("a session that ran has a task");
    leveler_storage::OwnershipStore::current(db, &task)
        .await
        .unwrap()
        .unwrap()
}

/// Real processes, one repository, one RuntimeId. A daemon runs a turn; a
/// `leveler run` process starts beside it and must not interrupt or fence it.
/// Then the daemon is SIGKILLed: the next daemon reaps the dead daemon's turn
/// — and leaves the still-live `leveler run` turn alone.
#[tokio::test]
async fn live_processes_keep_their_turns_and_only_a_killed_ones_turn_is_reaped() {
    let (base_url, _model) = hold_open_model_endpoint().await;
    let env = test_env(&base_url);
    let ready1 = env.home.join("ready1.json");
    let mut daemon = spawn_serve(&env, &ready1);
    wait_ready(&ready1, &mut daemon, Duration::from_secs(30));
    let client = LocalSocketRuntimeClient::connect(&find_socket(&env))
        .await
        .unwrap();
    let daemon_session = client
        .create_session(CreateSessionRequest {
            approval_policy: leveler_client_protocol::ApprovalPolicy::Interactive,
            goal: "daemon work".to_string(),
            model: None,
            mode: leveler_client_protocol::PermissionProfile::Assisted,
        })
        .await
        .unwrap()
        .session
        .id;
    client
        .send(ClientCommand::SubmitMessage {
            session_id: daemon_session.clone(),
            content: "daemon keeps working".to_string(),
            attachments: vec![],
        })
        .await
        .unwrap();
    drop(client);

    let db_path = find_state_dir(&env).join("sessions.db");
    let db = leveler_storage::Database::connect(&db_path).await.unwrap();
    let daemon_turn = running_turns(&db)
        .await
        .pop()
        .expect("the daemon's turn runs");
    let daemon_owner = task_owner(&db, &daemon_session).await;
    assert!(daemon_turn.owner_boot_id.is_some());
    assert_eq!(
        daemon_owner.boot.as_ref().map(|b| b.as_str()),
        daemon_turn.owner_boot_id.as_deref()
    );

    // A second host on the same repository: `leveler run` creates a session
    // (with its startup recovery) and starts its own turn.
    let mut sibling = Command::new(env!("CARGO_BIN_EXE_leveler"))
        .arg("--repo")
        .arg(&env.repo)
        .arg("run")
        .arg("sibling work")
        .env("LEVELER_HOME", &env.home)
        .env("LEVELER_CONFIG_DIR", &env.config_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn leveler run");
    let deadline = Instant::now() + Duration::from_secs(30);
    let sibling_turn = loop {
        if let Some(turn) = running_turns(&db)
            .await
            .into_iter()
            .find(|turn| turn.session_id != daemon_session.as_str())
        {
            break turn;
        }
        assert!(
            sibling.try_wait().unwrap().is_none(),
            "leveler run exited before starting its turn"
        );
        assert!(
            Instant::now() < deadline,
            "leveler run never started a turn"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let sibling_session = leveler_core::SessionId::new(sibling_turn.session_id.clone());
    let sibling_owner = task_owner(&db, &sibling_session).await;
    assert_ne!(sibling_turn.owner_boot_id, daemon_turn.owner_boot_id);

    // LIVE_OWNER_FALSE_REAP = 0: the daemon's turn survived the sibling's start.
    let still = running_turns(&db).await;
    assert!(
        still.iter().any(|turn| turn.id == daemon_turn.id),
        "a starting sibling interrupted the live daemon's turn: {still:?}"
    );
    assert_eq!(task_owner(&db, &daemon_session).await, daemon_owner);

    // The crash.
    daemon.kill().expect("SIGKILL daemon");
    let _ = daemon.wait();

    let ready2 = env.home.join("ready2.json");
    let mut daemon = spawn_serve(&env, &ready2);
    wait_ready(&ready2, &mut daemon, Duration::from_secs(30));

    // DEAD_OWNER_MISSED_REAP = 0, and still no false reap of the live sibling.
    let turns = leveler_storage::TurnRepository::new(&db)
        .list(&daemon_session)
        .await
        .unwrap();
    let reaped = turns.iter().find(|turn| turn.id == daemon_turn.id).unwrap();
    assert_eq!(
        reaped.status, "interrupted",
        "the killed daemon's turn is reaped"
    );
    assert_eq!(reaped.owner_boot_id, daemon_turn.owner_boot_id);
    // Recovery is finite: the restarted daemon stays up and leaves the
    // session it settled unowned, one generation on.
    let recovered = task_owner(&db, &daemon_session).await;
    assert_eq!((recovered.runtime, recovered.boot), (None, None));
    assert_eq!(recovered.epoch, daemon_owner.epoch.next().unwrap());
    let running = running_turns(&db).await;
    assert!(
        running.iter().any(|turn| turn.id == sibling_turn.id),
        "the restarted daemon interrupted the live `leveler run` turn: {running:?}"
    );
    assert_eq!(task_owner(&db, &sibling_session).await, sibling_owner);

    let _ = sibling.kill();
    let _ = sibling.wait();
    stop_daemon(&mut daemon);
}

/// Real processes, one repository: a daemon runs a user shell while this test
/// process is another boot on the same state, probing liveness through the
/// real boot leases. While the shell runs the session is the daemon's; once it
/// exits the daemon stays up and the session passes to the other boot.
#[tokio::test]
async fn a_live_daemons_user_shell_holds_the_session_only_while_it_runs() {
    let (base_url, _model) = hold_open_model_endpoint().await;
    let env = test_env(&base_url);
    let ready = env.home.join("ready-shell.json");
    let mut daemon = spawn_serve(&env, &ready);
    wait_ready(&ready, &mut daemon, Duration::from_secs(30));
    let client = LocalSocketRuntimeClient::connect(&find_socket(&env))
        .await
        .unwrap();
    let session = client
        .create_session(CreateSessionRequest {
            approval_policy: leveler_client_protocol::ApprovalPolicy::Interactive,
            goal: "shell work".to_string(),
            model: None,
            mode: leveler_client_protocol::PermissionProfile::Assisted,
        })
        .await
        .unwrap()
        .session
        .id;
    client
        .send(ClientCommand::RunUserShell {
            session_id: session.clone(),
            command: "sleep 5".to_string(),
        })
        .await
        .unwrap();

    let state_dir = find_state_dir(&env);
    let db = leveler_storage::Database::connect(&state_dir.join("sessions.db"))
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let held = loop {
        let owner = task_owner(&db, &session).await;
        if owner.boot.is_some() {
            break owner;
        }
        assert!(
            Instant::now() < deadline,
            "the daemon's shell never took the session"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let sibling = leveler_engine::TaskEngine {
        stores: leveler_storage::EngineStores::from_database(&db),
        runtime_id: held.runtime.clone().unwrap(),
        boot: leveler_engine::EngineBoot {
            id: leveler_core::BootId::generate(),
            liveness: std::sync::Arc::new(leveler_app::runtime_boot::StateDirBootLiveness::new(
                &state_dir,
            )),
        },
    };

    let refused = sibling.acquire_ownership(&session).await;
    assert!(
        matches!(
            refused,
            Err(leveler_engine::EngineError::OwnedByLiveBoot { .. })
        ),
        "{refused:?}"
    );
    assert_eq!(task_owner(&db, &session).await, held);

    let deadline = Instant::now() + Duration::from_secs(30);
    while task_owner(&db, &session).await.boot.is_some() {
        assert!(
            Instant::now() < deadline,
            "the finished shell kept the session"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(daemon.try_wait().unwrap().is_none(), "the daemon stays up");
    let next = sibling
        .acquire_ownership(&session)
        .await
        .expect("another boot takes the session while the daemon stays up");
    assert_eq!(next.owner_epoch, held.epoch.next().unwrap());

    drop(client);
    stop_daemon(&mut daemon);
}

/// Deterministic C2/C5/C8 boundary: ACK has been returned and the running turn
/// carries its canonical input, while the transcript append is held behind an
/// exact test barrier. SIGKILL there must reconstruct one user message; a
/// second restart must remain a no-op.
#[cfg(feature = "test-crash-barrier")]
#[tokio::test]
async fn sigkill_after_durable_ack_before_transcript_append_recovers_once() {
    let (base_url, _model) = hold_open_model_endpoint().await;
    let env = test_env(&base_url);
    let ready1 = env.home.join("ready-barrier-1.json");
    let barrier = env.home.join(format!(
        ".test-crash-barrier-{}",
        leveler_core::new_uuid_string()
    ));
    let mut daemon = spawn_serve_with_after_turn_started_barrier(&env, &ready1, &barrier);
    wait_ready(&ready1, &mut daemon, Duration::from_secs(30));

    let client = LocalSocketRuntimeClient::connect(&find_socket(&env))
        .await
        .unwrap();
    let session = client
        .create_session(CreateSessionRequest {
            approval_policy: leveler_client_protocol::ApprovalPolicy::Interactive,
            goal: "deterministic crash".to_string(),
            model: None,
            mode: leveler_client_protocol::PermissionProfile::Assisted,
        })
        .await
        .unwrap()
        .session
        .id;
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "DETERMINISTIC_CRASH_MARKER".to_string(),
            attachments: vec![],
        })
        .await
        .expect("ACK follows durable turn input");

    let deadline = Instant::now() + Duration::from_secs(10);
    while !barrier.is_file() {
        assert!(
            Instant::now() < deadline,
            "daemon never reached the exact post-TurnStarted barrier"
        );
        assert!(
            daemon.try_wait().unwrap().is_none(),
            "daemon exited before the crash barrier"
        );
        std::thread::yield_now();
    }

    let db_path = find_state_dir(&env).join("sessions.db");
    let db = leveler_storage::Database::connect(&db_path).await.unwrap();
    let turns = leveler_storage::TurnRepository::new(&db)
        .list(&session)
        .await
        .unwrap();
    let running = turns
        .iter()
        .find(|turn| turn.status == "running")
        .expect("barrier requires a durable running turn");
    assert!(
        running
            .payload
            .as_deref()
            .is_some_and(|payload| payload.contains("DETERMINISTIC_CRASH_MARKER")),
        "running turn must carry canonical input"
    );
    let before = leveler_storage::MessageRepository::new(&db)
        .load(&session)
        .await
        .unwrap();
    assert!(
        before
            .iter()
            .all(|payload| !payload.contains("DETERMINISTIC_CRASH_MARKER")),
        "barrier must stop before the transcript projection"
    );
    drop(client);
    drop(db);

    daemon.kill().expect("SIGKILL at exact crash barrier");
    let _ = daemon.wait();

    let ready2 = env.home.join("ready-barrier-2.json");
    let mut daemon = spawn_serve(&env, &ready2);
    wait_ready(&ready2, &mut daemon, Duration::from_secs(30));
    let db = leveler_storage::Database::connect(&db_path).await.unwrap();
    let messages = leveler_storage::MessageRepository::new(&db)
        .load(&session)
        .await
        .unwrap();
    assert_eq!(
        messages
            .iter()
            .filter(|payload| payload.contains("DETERMINISTIC_CRASH_MARKER"))
            .count(),
        1,
        "restart must project the accepted input exactly once"
    );
    assert!(
        leveler_storage::TurnRepository::new(&db)
            .list(&session)
            .await
            .unwrap()
            .iter()
            .all(|turn| turn.status != "running"),
        "restart recovery is complete only after the turn is terminal"
    );
    drop(db);
    stop_daemon(&mut daemon);

    let ready3 = env.home.join("ready-barrier-3.json");
    let mut daemon = spawn_serve(&env, &ready3);
    wait_ready(&ready3, &mut daemon, Duration::from_secs(30));
    let db = leveler_storage::Database::connect(&db_path).await.unwrap();
    let messages = leveler_storage::MessageRepository::new(&db)
        .load(&session)
        .await
        .unwrap();
    assert_eq!(
        messages
            .iter()
            .filter(|payload| payload.contains("DETERMINISTIC_CRASH_MARKER"))
            .count(),
        1,
        "repeated restart must not duplicate recovery"
    );
    stop_daemon(&mut daemon);
}

/// The dispatching crash window, with a real process and a real SIGKILL: the
/// daemon admits a submission and dispatches it, then dies before recording
/// that. Its boot lease dies with it. A restarted daemon must answer the same
/// command id — over the socket — as unresolvable, every time, and never run it
/// a second time.
#[cfg(feature = "test-crash-barrier")]
#[tokio::test]
async fn sigkill_before_the_receipt_settles_is_unresolvable_after_restart() {
    let (base_url, _model) = hold_open_model_endpoint().await;
    let env = test_env(&base_url);
    let ready1 = env.home.join("ready-receipt-1.json");
    let barrier = env.home.join(format!(
        ".test-crash-barrier-{}",
        leveler_core::new_uuid_string()
    ));
    let mut daemon = spawn_serve_with_barrier(
        &env,
        &ready1,
        "LEVELER_TEST_BEFORE_RECEIPT_SETTLED_BARRIER",
        &barrier,
    );
    wait_ready(&ready1, &mut daemon, Duration::from_secs(30));

    let client = LocalSocketRuntimeClient::connect(&find_socket(&env))
        .await
        .unwrap();
    let session = client
        .create_session(CreateSessionRequest {
            approval_policy: leveler_client_protocol::ApprovalPolicy::Interactive,
            goal: "receipt crash".to_string(),
            model: None,
            mode: leveler_client_protocol::PermissionProfile::Assisted,
        })
        .await
        .unwrap()
        .session
        .id;
    let envelope = leveler_client_protocol::CommandEnvelope {
        command_id: leveler_client_protocol::CommandId::new("cmd-sigkill-receipt"),
        session_id: session.clone(),
        expected_version: None,
        issued_at: "2026-09-15T00:00:00Z".to_string(),
        command: ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "RECEIPT_CRASH_MARKER".to_string(),
            attachments: vec![],
        },
    };
    let first_client = std::sync::Arc::new(client);
    let in_flight = {
        let first_client = first_client.clone();
        let envelope = envelope.clone();
        tokio::spawn(async move { first_client.deliver(envelope).await })
    };

    let deadline = Instant::now() + Duration::from_secs(10);
    while !barrier.is_file() {
        assert!(
            Instant::now() < deadline,
            "daemon never reached the barrier before settling the receipt"
        );
        assert!(
            daemon.try_wait().unwrap().is_none(),
            "daemon exited before the barrier"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    daemon
        .kill()
        .expect("SIGKILL with the receipt still dispatching");
    let _ = daemon.wait();
    in_flight.abort();
    drop(first_client);

    let ready2 = env.home.join("ready-receipt-2.json");
    let mut daemon = spawn_serve(&env, &ready2);
    wait_ready(&ready2, &mut daemon, Duration::from_secs(30));
    let client = LocalSocketRuntimeClient::connect(&find_socket(&env))
        .await
        .unwrap();
    for attempt in 0..2 {
        let error = client
            .deliver(envelope.clone())
            .await
            .expect_err("the crashed command must not be answered delivered");
        assert!(
            matches!(error, leveler_client_protocol::ClientError::Unresolvable(_)),
            "attempt {attempt}: {error:?}"
        );
    }

    let db_path = find_state_dir(&env).join("sessions.db");
    let db = leveler_storage::Database::connect(&db_path).await.unwrap();
    let turns = leveler_storage::TurnRepository::new(&db)
        .list(&session)
        .await
        .unwrap();
    assert_eq!(
        turns.len(),
        1,
        "the command ran once, before the crash: {turns:?}"
    );
    drop(db);
    stop_daemon(&mut daemon);
}

/// Gate Scenario A: a running daemon reports identity + admission health
/// over the socket, and the numbers reflect reality (no active work yet).
#[tokio::test]
async fn health_reports_identity_and_admission() {
    let env = test_env("http://127.0.0.1:9");
    let ready = env.home.join("ready.json");
    let mut daemon = spawn_serve(&env, &ready);
    let ready_doc = wait_ready(&ready, &mut daemon, Duration::from_secs(30));

    let client = LocalSocketRuntimeClient::connect(&find_socket(&env))
        .await
        .unwrap();
    let info = LocalRuntimeService::runtime_info(&client).await.unwrap();
    assert_eq!(
        info.runtime_id.as_str(),
        ready_doc["runtime_id"].as_str().unwrap()
    );
    assert!(info.health.accepting_work, "an idle daemon accepts work");
    assert_eq!(info.health.active_turns, 0);
    assert!(info.health.turn_capacity.unwrap_or(0) > 0, "real capacity");
    assert!(!info.health.shutting_down);

    drop(client);
    stop_daemon(&mut daemon);
}

/// Gate Scenarios C+D: the daemon dies (SIGKILL) under a connected client
/// mid-task; after a restart the SAME client object reaches the SAME
/// RuntimeId, the session snapshot is served again, the orphan turn was
/// recovered, and the ownership epoch advanced (old tokens powerless).
#[tokio::test]
async fn connected_client_recovers_after_daemon_sigkill() {
    let (base_url, _model) = hold_open_model_endpoint().await;
    let env = test_env(&base_url);
    let ready1 = env.home.join("ready1.json");
    let mut daemon = spawn_serve(&env, &ready1);
    let first = wait_ready(&ready1, &mut daemon, Duration::from_secs(30));
    let runtime_id = first["runtime_id"].as_str().unwrap().to_string();

    let client = LocalSocketRuntimeClient::connect(&find_socket(&env))
        .await
        .unwrap();
    let session = client
        .create_session(CreateSessionRequest {
            approval_policy: leveler_client_protocol::ApprovalPolicy::Interactive,
            goal: "survive the crash".to_string(),
            model: None,
            mode: leveler_client_protocol::PermissionProfile::Assisted,
        })
        .await
        .unwrap()
        .session
        .id;
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "MARKER".to_string(),
            attachments: vec![],
        })
        .await
        .unwrap();
    // Wait for a durable running turn, then note the pre-crash epoch.
    let db_path = find_state_dir(&env).join("sessions.db");
    let db = leveler_storage::Database::connect(&db_path).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let turns = leveler_storage::TurnRepository::new(&db)
            .list(&session)
            .await
            .unwrap();
        if turns.iter().any(|t| t.status == "running") {
            break;
        }
        assert!(Instant::now() < deadline, "turn never became durable");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let task = leveler_storage::TaskStore::task_for_session(&db, &session)
        .await
        .unwrap()
        .unwrap();
    let epoch_before = leveler_storage::OwnershipStore::current(&db, &task)
        .await
        .unwrap()
        .unwrap()
        .epoch;
    drop(db);

    daemon.kill().expect("SIGKILL");
    let _ = daemon.wait();

    // The supervisor semantics: a (revived) daemon comes back for the same
    // state dir. Here the test plays the reviver's role directly.
    let ready2 = env.home.join("ready2.json");
    let mut daemon = spawn_serve(&env, &ready2);
    let second = wait_ready(&ready2, &mut daemon, Duration::from_secs(30));
    assert_eq!(second["runtime_id"].as_str().unwrap(), runtime_id);

    // SAME client object: per-request connections + the subscription
    // reconnect loop reach the restarted daemon without a rebuild.
    let deadline = Instant::now() + Duration::from_secs(10);
    let snapshot = loop {
        match client.snapshot(&session).await {
            Ok(snapshot) => break snapshot,
            Err(_) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await
            }
            Err(error) => panic!("session unreachable after restart: {error}"),
        }
    };
    assert_eq!(snapshot.id, session);
    let info = LocalRuntimeService::runtime_info(&client).await.unwrap();
    assert_eq!(info.runtime_id.as_str(), runtime_id);
    assert!(info.health.accepting_work);

    // Ownership: the restart reap reacquired — the epoch advanced, so every
    // pre-crash token is powerless; the orphan turn is no longer running.
    let db = leveler_storage::Database::connect(&db_path).await.unwrap();
    let epoch_after = leveler_storage::OwnershipStore::current(&db, &task)
        .await
        .unwrap()
        .unwrap()
        .epoch;
    assert!(
        epoch_after > epoch_before,
        "restart recovery must advance the owner epoch ({epoch_before} -> {epoch_after})"
    );
    let turns = leveler_storage::TurnRepository::new(&db)
        .list(&session)
        .await
        .unwrap();
    assert!(turns.iter().all(|t| t.status != "running"));

    drop(client);
    stop_daemon(&mut daemon);
}
