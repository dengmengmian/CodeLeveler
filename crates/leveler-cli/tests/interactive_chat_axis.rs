//! The interactive TUI's collaboration axis is a product contract, not a
//! transport choice.
//!
//! `leveler tui` and `leveler tui --in-process` open the same product thing — a
//! new interactive session — so both must record `chat`. Before this, only the
//! daemon path carried that intent (through `interactive_session_request`); the
//! embedded path called `Application::create_session_with_mode`, which resolves
//! the axis from the process default (`goal`). The two transports therefore
//! disagreed, and the pure-function test that covered the request builder was
//! green the whole time.
//!
//! These tests drive the REAL binary down both transports and read the durable
//! row, so a future divergence on either path fails here. The TUI cannot render
//! without a terminal, but the session is created and committed before the
//! terminal is taken over — which is exactly the step under test.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use leveler_storage::{Database, SessionRepository};
use leveler_test_support::{ManagedChild, git};

struct TestEnv {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    config_dir: PathBuf,
    repo: PathBuf,
}

/// A fully isolated environment: its own home (state, sockets, global config),
/// config bundle (mock provider), and repository.
fn test_env(base_url: &str) -> TestEnv {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let repo = tmp.path().join("repo");
    let config_dir = tmp.path().join("configs");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(config_dir.join("providers")).unwrap();
    std::fs::create_dir_all(config_dir.join("models")).unwrap();
    // The global config is what makes `leveler tui` skippable from the first-run
    // guide. Auto-update is off so no test reaches the network.
    std::fs::write(
        home.join("config.toml"),
        "default_model = \"mock/m\"\n\n[update]\nauto_update = false\n",
    )
    .unwrap();
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
    git::init_repo(&repo);
    std::fs::write(repo.join("seed.txt"), "seed\n").unwrap();
    git::run(&repo, &["add", "-A"]);
    git::run(&repo, &["commit", "-qm", "seed"]);
    TestEnv {
        _tmp: tmp,
        home,
        config_dir,
        repo,
    }
}

/// A `leveler` invocation against this environment, with a closed stdin: the
/// interactive entry points create their session before they touch the
/// terminal, then fail to enter it, which is all these tests need.
fn leveler(env: &TestEnv, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_leveler"));
    command
        .arg("--repo")
        .arg(&env.repo)
        .args(args)
        .env("LEVELER_HOME", &env.home)
        .env("LEVELER_CONFIG_DIR", &env.config_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

/// Run a `leveler tui` launch to completion. It is expected to fail taking over
/// the terminal; the session is created first either way.
fn run_to_completion(env: &TestEnv, args: &[&str], timeout: Duration) -> ExitStatus {
    let mut child = ManagedChild::spawn(&mut leveler(env, args)).expect("spawn leveler");
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().expect("child status") {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "`leveler {}` never exited within {timeout:?}",
            args.join(" ")
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Spawn a `leveler run` that cannot reach its provider: the session row is
/// written before the first model round, the round fails, and the process
/// exits. Waiting for the exit keeps this test's own database reads from racing
/// the run's migrations.
async fn run_and_read_axis(env: &TestEnv, goal: &str, args: &[&str]) -> String {
    let status = run_to_completion(env, args, Duration::from_secs(60));
    assert!(
        !status.success(),
        "the provider is unreachable, so the run cannot succeed"
    );
    sessions(env)
        .await
        .into_iter()
        .find(|session| session.goal == goal)
        .unwrap_or_else(|| panic!("`leveler run {}` must persist `{goal}`", args.join(" ")))
        .collaboration
}

fn spawn_serve(env: &TestEnv, ready: &Path) -> ManagedChild {
    let mut command = leveler(env, &["serve", "--ready-json"]);
    command.arg(ready);
    ManagedChild::spawn(&mut command).expect("spawn leveler serve")
}

fn wait_ready(ready: &Path, child: &mut ManagedChild, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if ready.is_file() {
            return;
        }
        if let Some(status) = child.try_wait().expect("child status") {
            panic!("daemon exited before readiness: {status}");
        }
        assert!(
            Instant::now() < deadline,
            "daemon never became ready within {timeout:?}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// SIGINT is the daemon's documented Ctrl+C shutdown path.
fn stop_daemon(child: &mut ManagedChild) {
    let _ = Command::new("kill")
        .arg("-2")
        .arg(child.id().to_string())
        .status();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if child.try_wait().expect("child status").is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("daemon did not stop on SIGINT");
}

/// The single per-repository state dir under the isolated home, once this
/// environment has written one.
fn project_state_dir(env: &TestEnv) -> Option<PathBuf> {
    let projects = env.home.join("state/projects");
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&projects)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.is_dir())
                .collect()
        })
        .unwrap_or_default();
    match dirs.len() {
        0 => None,
        1 => dirs.pop(),
        n => panic!("expected at most one project state dir, found {n}"),
    }
}

async fn sessions(env: &TestEnv) -> Vec<leveler_storage::SessionRecord> {
    let Some(state_dir) = project_state_dir(env) else {
        return Vec::new();
    };
    let db = Database::connect(&state_dir.join("sessions.db"))
        .await
        .unwrap();
    SessionRepository::new(&db).list().await.unwrap()
}

/// The sessions a `leveler tui` launch leaves behind: the TUI's own goal string
/// distinguishes them from anything a helper ran.
async fn interactive_sessions(env: &TestEnv) -> Vec<leveler_storage::SessionRecord> {
    sessions(env)
        .await
        .into_iter()
        .filter(|session| session.goal == "interactive session")
        .collect()
}

async fn axis_of(env: &TestEnv, session: &str) -> String {
    let state_dir = project_state_dir(env).expect("the environment has written state");
    let db = Database::connect(&state_dir.join("sessions.db"))
        .await
        .unwrap();
    SessionRepository::new(&db)
        .get(&leveler_core::SessionId::new(session))
        .await
        .unwrap()
        .unwrap()
        .collaboration
}

/// T1 + T2 + T3 — a new interactive session records `chat` on BOTH transports,
/// so the durable rows are identical apart from the session id.
#[tokio::test]
async fn both_transports_open_a_new_interactive_session_as_chat() {
    // The daemon transport: a managed daemon, then the real probe-and-reuse path.
    let daemon_env = test_env("http://127.0.0.1:9");
    let ready = daemon_env.home.join("ready.json");
    let mut daemon = spawn_serve(&daemon_env, &ready);
    wait_ready(&ready, &mut daemon, Duration::from_secs(30));
    let status = run_to_completion(&daemon_env, &["tui"], Duration::from_secs(60));
    assert!(
        !status.success(),
        "the TUI cannot take over a non-tty, but it must have created the session first"
    );
    let daemon_sessions = interactive_sessions(&daemon_env).await;
    assert_eq!(
        daemon_sessions.len(),
        1,
        "`leveler tui` must create exactly one session"
    );
    assert_eq!(
        daemon_sessions[0].collaboration, "chat",
        "the daemon transport must record the interactive axis"
    );
    stop_daemon(&mut daemon);

    // The embedded transport: same entry, runtime inside the TUI process.
    let embedded_env = test_env("http://127.0.0.1:9");
    let status = run_to_completion(
        &embedded_env,
        &["tui", "--in-process"],
        Duration::from_secs(60),
    );
    assert!(
        !status.success(),
        "the TUI cannot take over a non-tty, but it must have created the session first"
    );
    let embedded_sessions = interactive_sessions(&embedded_env).await;
    assert_eq!(
        embedded_sessions.len(),
        1,
        "`leveler tui --in-process` must create exactly one session"
    );
    assert_eq!(
        embedded_sessions[0].collaboration, "chat",
        "the embedded transport must record the SAME interactive axis as the daemon"
    );

    // T3 — parity: the transport is not part of the session's meaning.
    assert_eq!(
        daemon_sessions[0].collaboration, embedded_sessions[0].collaboration,
        "transport must not change the axis"
    );
    assert_eq!(
        daemon_sessions[0].work_profile,
        embedded_sessions[0].work_profile
    );
    assert_eq!(daemon_sessions[0].goal, embedded_sessions[0].goal);
}

/// T6 + T7 + T8 — resume preserves the persisted axis on both transports. The
/// interactive default must not rewrite an existing session's axis.
#[tokio::test]
async fn resuming_a_session_keeps_its_persisted_axis() {
    let env = test_env("http://127.0.0.1:9");

    // A Goal session, through the real headless entry.
    let goal_axis = run_and_read_axis(
        &env,
        "drive the goal",
        &["run", "--auto-approve", "drive the goal"],
    )
    .await;
    assert_eq!(goal_axis, "goal", "`leveler run` keeps the goal axis");
    let goal_id = sessions(&env)
        .await
        .into_iter()
        .find(|session| session.goal == "drive the goal")
        .expect("the goal run persisted its session")
        .id;

    // A Chat session, through the interactive embedded entry under test.
    let _ = run_to_completion(&env, &["tui", "--in-process"], Duration::from_secs(60));
    let chat_id = interactive_sessions(&env)
        .await
        .pop()
        .expect("the interactive entry persists its session")
        .id;
    assert_eq!(axis_of(&env, &chat_id).await, "chat");

    let before = sessions(&env).await.len();

    // T7 — resume the Goal session in-process: it is still a Goal.
    let _ = run_to_completion(
        &env,
        &["tui", "--session", goal_id.as_str(), "--in-process"],
        Duration::from_secs(60),
    );
    assert_eq!(
        axis_of(&env, goal_id.as_str()).await,
        "goal",
        "an embedded resume must not rewrite a Goal session to the interactive default"
    );

    // T6 — resume the same Goal session through the daemon.
    let ready = env.home.join("ready.json");
    let mut daemon = spawn_serve(&env, &ready);
    wait_ready(&ready, &mut daemon, Duration::from_secs(30));
    let _ = run_to_completion(
        &env,
        &["tui", "--session", goal_id.as_str()],
        Duration::from_secs(60),
    );
    assert_eq!(
        axis_of(&env, goal_id.as_str()).await,
        "goal",
        "a daemon resume must not rewrite a Goal session either"
    );

    // T8 — a Chat session stays chat on resume.
    let _ = run_to_completion(
        &env,
        &["tui", "--session", chat_id.as_str(), "--in-process"],
        Duration::from_secs(60),
    );
    assert_eq!(axis_of(&env, &chat_id).await, "chat");
    stop_daemon(&mut daemon);

    assert_eq!(
        sessions(&env).await.len(),
        before,
        "resuming must adopt an existing session, never create another one"
    );
}

/// T9 — the headless entry is untouched: its axis is the product default (goal)
/// unless the user states one.
#[tokio::test]
async fn the_headless_entry_keeps_its_goal_default() {
    let env = test_env("http://127.0.0.1:9");

    let default_axis = run_and_read_axis(
        &env,
        "default axis",
        &["run", "--auto-approve", "default axis"],
    )
    .await;
    assert_eq!(
        default_axis, "goal",
        "the headless default is still the coding-session axis"
    );

    let explicit_axis = run_and_read_axis(
        &env,
        "explicit axis",
        &[
            "run",
            "--collaboration",
            "chat",
            "--auto-approve",
            "explicit axis",
        ],
    )
    .await;
    assert_eq!(explicit_axis, "chat", "an explicit axis wins");
}
