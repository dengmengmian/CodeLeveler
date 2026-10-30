//! Runtime configuration hot reload against a REAL daemon process.
//!
//! The point of this test is the fact a UI cannot fake: after the configuration
//! file changes, the daemon that is already running sends its next model request
//! to the NEW provider, with no restart, while the session it was already
//! serving keeps working. A second, independent Execution Host only runs
//! commands; the model calls it triggers are made by this same daemon, so this
//! is the route the TUI/Desktop actually exercise.
//!
//! Unix-only for the same reason `daemon_e2e` is: it needs the real local
//! socket daemon and a real signal to stop it.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use leveler_client_protocol::{
    ClientCommand, InteractiveRuntimeClient, PermissionProfile, RuntimeEvent,
};
use leveler_local_transport::{
    CreateSessionRequest, LocalRuntimeService, LocalSocketRuntimeClient,
};
use leveler_project::Layout;
use leveler_test_support::{ManagedChild, MockResponse, MockServer};

struct SilentUi;

impl leveler_runtime_host::HandoffUi for SilentUi {
    fn emit(&self, _event: leveler_runtime_host::HandoffEvent) {}

    fn input(
        &self,
    ) -> Option<tokio::sync::mpsc::UnboundedReceiver<leveler_runtime_host::HandoffAction>> {
        None
    }
}

struct TestEnv {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    repo: PathBuf,
}

fn test_env() -> TestEnv {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(repo.join(".leveler")).unwrap();
    TestEnv {
        _tmp: tmp,
        home,
        repo,
    }
}

fn write_config(env: &TestEnv, default_model: &str, a: &str, b: &str) {
    std::fs::write(
        env.home.join("config.toml"),
        format!(
            r#"default_model = "{default_model}"

[providers.a]
protocol = "openai_chat"
base_url = "{a}"

[providers.b]
protocol = "openai_chat"
base_url = "{b}"

[models.alpha]
provider = "a"
model_id = "alpha-model"
context_window = 131072
max_output_tokens = 1024

[models.beta]
provider = "b"
model_id = "beta-model"
context_window = 65536
max_output_tokens = 1024
"#
        ),
    )
    .unwrap();
}

fn text_response(content: &str) -> MockResponse {
    let frame = serde_json::json!({
        "choices": [{"delta": {"content": content}, "finish_reason": "stop"}]
    });
    MockResponse::Sse {
        body: format!("data: {frame}\n\ndata: [DONE]\n\n"),
    }
}

fn spawn_serve(env: &TestEnv, ready: &Path) -> ManagedChild {
    let mut command = Command::new(env!("CARGO_BIN_EXE_leveler"));
    command
        .arg("--repo")
        .arg(&env.repo)
        .arg("serve")
        .arg("--ready-json")
        .arg(ready)
        .env("LEVELER_HOME", &env.home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    ManagedChild::spawn(&mut command).expect("spawn leveler serve")
}

fn wait_ready(ready: &Path, child: &mut ManagedChild, timeout: Duration) -> serde_json::Value {
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
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn find_socket(env: &TestEnv) -> PathBuf {
    let sock_dir = env.home.join("run/sockets");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mut sockets: Vec<PathBuf> = std::fs::read_dir(&sock_dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|e| e == "sock"))
                    .collect()
            })
            .unwrap_or_default();
        if let Some(socket) = sockets.pop() {
            assert!(sockets.is_empty(), "expected exactly one daemon socket");
            return socket;
        }
        assert!(Instant::now() < deadline, "no daemon socket appeared");
        std::thread::sleep(Duration::from_millis(50));
    }
}

async fn connect(env: &TestEnv) -> LocalSocketRuntimeClient {
    let socket = find_socket(env);
    LocalSocketRuntimeClient::connect(&socket)
        .await
        .expect("connect to the real daemon")
}

async fn create_session(client: &LocalSocketRuntimeClient) -> leveler_core::SessionId {
    client
        .create_session(CreateSessionRequest {
            request_id: None,
            collaboration: leveler_local_transport::CollaborationMode::Chat,
            workspace: leveler_local_transport::CreateWorkspaceSelection::RuntimeDefault,
            approval_policy: leveler_client_protocol::ApprovalPolicy::AutoApprove,
            goal: "config hot reload".to_string(),
            model: None,
            mode: PermissionProfile::FullAccess,
        })
        .await
        .expect("create a session on the running daemon")
        .session
        .id
}

async fn submit_and_settle(
    client: &LocalSocketRuntimeClient,
    events: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>,
    session: &leveler_core::SessionId,
    content: &str,
) {
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: content.to_string(),
            attachments: vec![],
        })
        .await
        .expect("submit");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, events.recv()).await {
            Ok(Ok(event)) => match event {
                RuntimeEvent::TurnCompleted
                | RuntimeEvent::TurnCompletedWithWarnings { .. }
                | RuntimeEvent::TurnAnswered
                | RuntimeEvent::TurnTruncated { .. }
                | RuntimeEvent::TurnIncomplete { .. }
                | RuntimeEvent::TurnFailed { .. }
                | RuntimeEvent::TurnCancelled
                | RuntimeEvent::TaskCancelled => return,
                _ => continue,
            },
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            _ => panic!("the turn never settled"),
        }
    }
}

async fn models_requested(server: &MockServer) -> Vec<String> {
    server
        .request_bodies()
        .await
        .into_iter()
        .filter_map(|body| {
            serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|value| value["model"].as_str().map(str::to_string))
        })
        .collect()
}

#[tokio::test]
async fn a_running_daemon_switches_the_real_provider_on_reload() {
    let first = MockServer::start_one(text_response("from a")).await;
    let second = MockServer::start_one(text_response("from b")).await;
    let env = test_env();
    write_config(&env, "a/alpha", &first.base_url(), &second.base_url());
    // The daemon and this test must agree about the home it reads.
    unsafe {
        std::env::set_var("LEVELER_HOME", &env.home);
    }
    let ready = env.home.join("ready.json");
    let mut daemon = spawn_serve(&env, &ready);
    let ready_json = wait_ready(&ready, &mut daemon, Duration::from_secs(60));
    let pid = ready_json["pid"].as_u64().expect("daemon pid");

    let client = connect(&env).await;
    // Canonicalize the repo: the daemon does, and the socket path is keyed by
    // that canonical spelling on macOS (`/var` vs `/private/var`).
    let repo = std::fs::canonicalize(&env.repo).unwrap();
    let layout = Layout::from_parts(repo.clone(), repo.join("configs"), env.home.join("state"));
    let before = leveler_app::runtime_config_fingerprint(&layout).unwrap();
    let info = client.runtime_info().await.expect("runtime info");
    assert_eq!(
        info.config_fingerprint.as_deref(),
        Some(before.as_str()),
        "the test and the daemon must agree on the configuration sources"
    );
    let mut events = client.subscribe();
    let session = create_session(&client).await;
    submit_and_settle(&client, &mut events, &session, "say hello").await;
    assert!(
        !models_requested(&first).await.is_empty()
            && models_requested(&first)
                .await
                .iter()
                .all(|model| model == "alpha-model"),
        "the first turn must use the configured default: {:?}",
        models_requested(&first).await
    );
    assert!(models_requested(&second).await.is_empty());

    // The external edit — an editor or the Desktop settings page.
    write_config(&env, "b/beta", &first.base_url(), &second.base_url());

    // The path a TUI/Desktop start takes when it notices the file generation
    // moved: it must ADOPT the running daemon (reloading it), never replace it.
    let adopted = leveler_runtime_host::reconcile_runtime_generation(
        &layout.socket_path(),
        &layout,
        &SilentUi,
    )
    .await
    .expect("the client attaches to the same daemon");
    let adopted = adopted.expect("the running daemon must be adopted");
    let adopted_info = adopted.runtime_info().await.unwrap();
    assert_eq!(
        adopted_info.pid as u64, pid,
        "a configuration change must not replace the daemon"
    );
    drop(adopted);

    client
        .send(ClientCommand::ReloadConfig { query_id: None })
        .await
        .expect("reload is accepted by the running daemon");

    // The runtime's own answer, not a UI label: the generation it reports is
    // the one the files describe, and the daemon is the same process.
    let expected = leveler_app::runtime_config_fingerprint(&layout).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let info = client.runtime_info().await.expect("runtime info");
        if info.config_fingerprint.as_deref() == Some(expected.as_str()) {
            assert_eq!(info.pid as u64, pid, "a reload must not replace the daemon");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the daemon never adopted the new generation: {:?}",
            info.config_fingerprint
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // The SAME session's next turn inherits the new default (it never chose a
    // model itself), and the request really lands on the new provider.
    submit_and_settle(&client, &mut events, &session, "say hello again").await;
    let requested = models_requested(&second).await;
    assert!(
        !requested.is_empty() && requested.iter().all(|model| model == "beta-model"),
        "the next turn must call the new provider/model: {requested:?}"
    );

    // A session created now starts on the current default too.
    let fresh = create_session(&client).await;
    submit_and_settle(&client, &mut events, &fresh, "hello from a new session").await;
    let requested = models_requested(&second).await;
    assert!(
        requested.len() >= 2 && requested.iter().all(|model| model == "beta-model"),
        "a new session starts on the new default: {requested:?}"
    );

    stop_daemon(&mut daemon);
}
