//! Real binary / real HTTP process acceptance for the execution failure domain.
//! The model response is scripted; process ownership, runtime death, reconnect,
//! offline logging and stop all run through production paths.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use leveler_client_protocol::{ClientCommand, InteractiveRuntimeClient};
use leveler_local_transport::{
    CreateSessionRequest, LocalRuntimeService, LocalSocketRuntimeClient,
};
use leveler_test_support::{ManagedChild, MockResponse, MockServer};

struct Environment {
    _temp: tempfile::TempDir,
    home: PathBuf,
    config: PathBuf,
    repo: PathBuf,
}

impl Environment {
    fn new(model_url: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let config = temp.path().join("config");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(config.join("providers")).unwrap();
        std::fs::create_dir_all(config.join("models")).unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .arg(&repo)
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(repo.join("tracked.txt"), "before").unwrap();
        std::fs::write(
            config.join("providers/mock.yaml"),
            format!("id: mock\nprotocol: openai_chat\nbase_url: {model_url}\n"),
        )
        .unwrap();
        std::fs::write(
            config.join("models/m.yaml"),
            r#"id: m
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
        Self {
            _temp: temp,
            home,
            config,
            repo,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_leveler"));
        command
            .arg("--repo")
            .arg(&self.repo)
            .env("LEVELER_HOME", &self.home)
            .env("LEVELER_CONFIG_DIR", &self.config)
            .stdin(Stdio::null());
        command
    }

    fn serve(&self, name: &str) -> (ManagedChild, PathBuf) {
        let ready = self.home.join(format!("{name}-ready.json"));
        let stderr = self.home.join(format!("{name}-stderr.txt"));
        let mut command = self.command();
        command
            .args(["serve", "--ready-json"])
            .arg(&ready)
            .stdout(Stdio::null())
            .stderr(Stdio::from(std::fs::File::create(&stderr).unwrap()));
        (ManagedChild::spawn(&mut command).unwrap(), ready)
    }

    fn state_dir(&self) -> PathBuf {
        let dirs: Vec<_> = std::fs::read_dir(self.home.join("state/projects"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.is_dir())
            .collect();
        assert_eq!(dirs.len(), 1, "one isolated project state namespace");
        dirs[0].clone()
    }

    fn socket(&self) -> PathBuf {
        let sockets: Vec<_> = std::fs::read_dir(self.home.join("run/sockets"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "sock"))
            .collect();
        assert_eq!(sockets.len(), 1, "one runtime socket");
        sockets[0].clone()
    }

    fn background(&self, args: &[&str]) -> String {
        let output = self
            .command()
            .arg("background")
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "background {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }
}

fn response(frames: Vec<serde_json::Value>) -> MockResponse {
    let body = frames
        .into_iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect::<String>()
        + "data: [DONE]\n\n";
    MockResponse::Sse { body }
}

fn spawn_response(port: u16) -> MockResponse {
    // Every HTTP response proves which OS process actually served the request.
    // Request-specific log markers let us prove capture while runtime is absent.
    let python = format!(
        r#"import os, pathlib
from http.server import HTTPServer, BaseHTTPRequestHandler
class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        try:
            print('DOGFOOD_LOG ' + self.path, flush=True)
        except BrokenPipeError:
            pass  # Host crash closes logs; HTTP remains the liveness oracle.
        self.send_response(200)
        self.end_headers()
        self.wfile.write(str(os.getpid()).encode())
        if self.path == '/exit-offline':
            pathlib.Path('offline-settlement.txt').write_text('written while runtime absent')
            self.server.exiting = True
    def log_message(self, *args):
        pass
server = HTTPServer(('127.0.0.1', {port}), Handler)
server.exiting = False
print('DOGFOOD_STARTED ' + str(os.getpid()), flush=True)
while not server.exiting:
    server.handle_request()
server.server_close()
print('DOGFOOD_PROCESS_EXITED', flush=True)
"#
    );
    let arguments = serde_json::json!({
        "program": "python3", "args": ["-u", "-c", python],
        "background": true, "background_lifetime": "persistent"
    });
    response(vec![
        serde_json::json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "spawn-persistent", "function": {"name": "run_command", "arguments": arguments.to_string()}}]}}]}),
        serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
    ])
}

fn completed_response() -> MockResponse {
    response(vec![
        serde_json::json!({"choices": [{"delta": {"content": "The requested persistent development service is started."}, "finish_reason": "stop"}]}),
    ])
}

async fn ready(path: &Path, child: &mut ManagedChild) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if path.exists() {
            return;
        }
        assert!(
            child.try_wait().unwrap().is_none(),
            "runtime exited before readiness"
        );
        assert!(
            Instant::now() < deadline,
            "runtime did not become ready: {}",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn reserve_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn http_pid(port: u16, path: &str) -> Result<u32, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let body = client
        .get(format!("http://127.0.0.1:{port}{path}"))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .text()
        .await
        .map_err(|e| e.to_string())?;
    body.parse().map_err(|e| format!("invalid server pid: {e}"))
}

async fn start_service(
    env: &Environment,
    daemon: &mut ManagedChild,
    ready_file: &Path,
    port: u16,
) -> (
    LocalSocketRuntimeClient,
    leveler_client_protocol::SessionId,
    String,
    u32,
) {
    ready(ready_file, daemon).await;
    let client = LocalSocketRuntimeClient::connect(&env.socket())
        .await
        .unwrap();
    let session = client
        .create_session(CreateSessionRequest {
            collaboration: leveler_local_transport::CollaborationMode::Chat,
            workspace: leveler_local_transport::CreateWorkspaceSelection::RuntimeDefault,
            approval_policy: leveler_client_protocol::ApprovalPolicy::AutoApprove,
            goal: "Start this user-requested persistent Python development HTTP service".into(),
            model: None,
            mode: leveler_client_protocol::PermissionProfile::FullAccess,
        })
        .await
        .unwrap()
        .session
        .id;
    let mut events = client.subscribe_session(&session);
    client.send(ClientCommand::SubmitMessage {
        session_id: session.clone(),
        content: "Start the requested persistent HTTP development service and keep it running until I explicitly stop it.".into(),
        attachments: vec![],
    }).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(40);
    let pid = loop {
        if let Ok(pid) = http_pid(port, "/online").await {
            break pid;
        }
        if Instant::now() >= deadline {
            let snapshot = client.snapshot(&session).await;
            let mut observed = Vec::new();
            while let Ok(event) = events.try_recv() {
                observed.push(format!("{event:?}"));
            }
            panic!(
                "persistent service never served HTTP; snapshot={snapshot:?}; events={observed:?}; daemon stderr={:?}",
                std::fs::read_to_string(env.home.join("runtime-a-stderr.txt"))
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let task_id = loop {
        let snapshot = client.snapshot(&session).await.unwrap();
        let info = client.runtime_info().await.unwrap();
        if info.health.active_turns == 0 {
            assert_eq!(
                info.health.active_background_tasks, 0,
                "host-owned service must not block runtime update"
            );
            assert!(
                info.health.blockers.is_empty(),
                "host service must not appear among update blockers"
            );
            if let Some(task) = snapshot.active_background_tasks.first() {
                break task.task_id.clone();
            }
        }
        assert!(
            Instant::now() < deadline,
            "runtime never published its surviving service"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    (client, session, task_id, pid)
}

async fn wait_log(env: &Environment, task_id: &str, marker: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if env.background(&["logs", task_id]).contains(marker) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "host did not retain log marker {marker}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Test-only cleanup of the independently owned daemon. The host must expose
/// its pid in private ready metadata; no production teardown is inferred from
/// runtime teardown. Read pid while the isolated metadata directory exists.
struct HostCleanup {
    state: PathBuf,
    repo: PathBuf,
    home: PathBuf,
    config: PathBuf,
}
impl Drop for HostCleanup {
    fn drop(&mut self) {
        // Ask the actual owner to stop every test task before terminating the
        // independent host. Killing the host alone cannot prove tree cleanup.
        let mut inventory = Command::new(env!("CARGO_BIN_EXE_leveler"));
        inventory
            .arg("--repo")
            .arg(&self.repo)
            .args(["background", "list", "--json"])
            .env("LEVELER_HOME", &self.home)
            .env("LEVELER_CONFIG_DIR", &self.config);
        if let Ok(output) = inventory.output()
            && let Ok(value) = serde_json::from_slice::<serde_json::Value>(&output.stdout)
            && let Some(tasks) = value.as_array()
        {
            for task in tasks {
                let id = task
                    .get("task_id")
                    .or_else(|| task.get("id"))
                    .or_else(|| task.get("snapshot").and_then(|v| v.get("id")))
                    .and_then(|v| v.as_str());
                if let Some(id) = id {
                    let _ = Command::new(env!("CARGO_BIN_EXE_leveler"))
                        .arg("--repo")
                        .arg(&self.repo)
                        .args(["background", "stop", id])
                        .env("LEVELER_HOME", &self.home)
                        .env("LEVELER_CONFIG_DIR", &self.config)
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status();
                }
            }
        }
        fn visit(path: &Path) {
            let Ok(entries) = std::fs::read_dir(path) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    visit(&path);
                    continue;
                }
                if path.extension().is_some_and(|ext| ext == "json")
                    && let Ok(bytes) = std::fs::read(&path)
                    && let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes)
                    && let Some(pid) = value.get("pid").and_then(|v| v.as_u64())
                {
                    // The process is test-owned only if its actual argv names
                    // this isolated host state. This prevents stale PID reuse.
                    let output = Command::new("ps")
                        .args(["-p", &pid.to_string(), "-o", "command="])
                        .output();
                    if let Ok(output) = output {
                        let argv = String::from_utf8_lossy(&output.stdout);
                        if argv.contains("execution-host")
                            && argv.contains(path.parent().unwrap().to_string_lossy().as_ref())
                        {
                            let _ = Command::new("kill")
                                .args(["-TERM", &pid.to_string()])
                                .status();
                        }
                    }
                }
            }
        }
        visit(&self.state);
    }
}

#[test]
fn persistent_service_survives_runtime_sigkill_reconnects_logs_and_stops() {
    leveler_test_support::bounded_test(
        "persistent_service_survives_runtime_sigkill_reconnects_logs_and_stops",
        Duration::from_secs(120),
        || async {
            let port = reserve_port();
            let mock = MockServer::start(vec![spawn_response(port), completed_response()]).await;
            let env = Environment::new(&mock.base_url());
            let (mut daemon, ready_a) = env.serve("runtime-a");
            ready(&ready_a, &mut daemon).await;
            let _host_cleanup = HostCleanup {
                state: env.state_dir().join("execution-host"),
                repo: env.repo.clone(),
                home: env.home.clone(),
                config: env.config.clone(),
            };
            let (client, session, task_id, pid) =
                start_service(&env, &mut daemon, &ready_a, port).await;
            assert!(env.background(&["list", "--json"]).contains(&task_id));
            drop(client);
            daemon.kill().unwrap();
            let status = daemon.wait().unwrap();
            assert!(!status.success(), "the old runtime was actually killed");
            for marker in ["/offline-1", "/offline-2", "/offline-3"] {
                assert_eq!(
                    http_pid(port, marker).await.unwrap(),
                    pid,
                    "same service process survives complete runtime exit"
                );
            }
            // CLI must control the host even while no Runtime exists.
            assert!(env.background(&["list", "--json"]).contains(&task_id));
            wait_log(&env, &task_id, "/offline-3").await;
            let (mut next, ready_b) = env.serve("runtime-b");
            ready(&ready_b, &mut next).await;
            let next_client = LocalSocketRuntimeClient::connect(&env.socket())
                .await
                .unwrap();
            let snapshot = next_client.snapshot(&session).await.unwrap();
            assert!(
                snapshot
                    .active_background_tasks
                    .iter()
                    .any(|task| task.task_id == task_id)
            );
            assert_eq!(http_pid(port, "/reconnected").await.unwrap(), pid);
            wait_log(&env, &task_id, "/offline-1").await;
            env.background(&["stop", &task_id]);
            let deadline = Instant::now() + Duration::from_secs(15);
            while http_pid(port, "/must-stop").await.is_ok() {
                assert!(
                    Instant::now() < deadline,
                    "stop did not close original service port"
                );
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            loop {
                let alive = Command::new("kill")
                    .args(["-0", &pid.to_string()])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .unwrap();
                if !alive.success() {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "original OS process must be gone, not merely disconnected"
                );
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            eprintln!(
                "DOGFOOD PASS task={task_id} original_pid={pid} old_runtime_exit={status} same_pid_offline_http=3 offline_logs_retained=true reconnect_same_task=true original_process_gone=true"
            );
        },
    );
}

#[test]
fn process_exiting_while_runtime_absent_is_settled_after_reconnect() {
    leveler_test_support::bounded_test(
        "process_exiting_while_runtime_absent_is_settled_after_reconnect",
        Duration::from_secs(120),
        || async {
            let port = reserve_port();
            let mock = MockServer::start(vec![spawn_response(port), completed_response()]).await;
            let env = Environment::new(&mock.base_url());
            let (mut daemon, ready_a) = env.serve("runtime-a");
            ready(&ready_a, &mut daemon).await;
            let _host_cleanup = HostCleanup {
                state: env.state_dir().join("execution-host"),
                repo: env.repo.clone(),
                home: env.home.clone(),
                config: env.config.clone(),
            };
            let (client, session, task_id, pid) =
                start_service(&env, &mut daemon, &ready_a, port).await;
            drop(client);
            daemon.kill().unwrap();
            daemon.wait().unwrap();
            assert_eq!(http_pid(port, "/exit-offline").await.unwrap(), pid);
            let semantic_dir = env.state_dir().join("execution-settlement");
            let deadline = Instant::now() + Duration::from_secs(15);
            while http_pid(port, "/gone").await.is_ok() {
                assert!(Instant::now() < deadline, "service did not exit offline");
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            assert_eq!(
                std::fs::read_to_string(env.repo.join("offline-settlement.txt")).unwrap(),
                "written while runtime absent"
            );
            let journal =
                leveler_execution::host_settlement::SettlementJournal::open(&semantic_dir).unwrap();
            assert!(
                journal
                    .pending()
                    .unwrap()
                    .iter()
                    .any(|record| record.task_id == task_id),
                "OS exit must not itself settle semantic changes"
            );
            let (mut next, ready_b) = env.serve("runtime-b");
            ready(&ready_b, &mut next).await;
            let next_client = LocalSocketRuntimeClient::connect(&env.socket())
                .await
                .unwrap();
            next_client.snapshot(&session).await.unwrap();
            let deadline = Instant::now() + Duration::from_secs(15);
            let settlement = loop {
                if let Some(settlement) = journal.read(&task_id).unwrap().settlement {
                    break settlement;
                }
                assert!(
                    Instant::now() < deadline,
                    "reconnect did not reconcile process exit"
                );
                tokio::time::sleep(Duration::from_millis(25)).await;
            };
            assert!(
                settlement
                    .modified
                    .iter()
                    .any(|path| path == "offline-settlement.txt"),
                "offline write must enter semantic settlement: {settlement:?}"
            );
            assert!(
                settlement.note.is_none(),
                "settlement must have real snapshot evidence"
            );
            eprintln!(
                "DOGFOOD PASS offline_exit_task={task_id} pid={pid} semantic_pending_before_reconnect=true offline_file_settled=true"
            );
        },
    );
}

/// When the host is unavailable, ask only the verified test service itself to
/// exit. Host crash cleanup must not rely on signaling a persisted process id.
#[cfg(target_os = "macos")]
struct ServiceSelfExit {
    port: u16,
    pid: u32,
}
#[cfg(target_os = "macos")]
impl Drop for ServiceSelfExit {
    fn drop(&mut self) {
        fn request(port: u16, path: &str) -> Option<String> {
            use std::io::{Read, Write};
            let mut stream = std::net::TcpStream::connect_timeout(
                &format!("127.0.0.1:{port}").parse().ok()?,
                Duration::from_secs(1),
            )
            .ok()?;
            stream.set_read_timeout(Some(Duration::from_secs(1))).ok()?;
            write!(stream, "GET {path} HTTP/1.0\r\nHost: localhost\r\n\r\n").ok()?;
            let mut response = String::new();
            stream.read_to_string(&mut response).ok()?;
            response
                .split_once("\r\n\r\n")
                .map(|(_, body)| body.to_string())
        }
        if request(self.port, "/cleanup-identity").as_deref() == Some(&self.pid.to_string()) {
            let _ = request(self.port, "/exit-offline");
        }
    }
}

#[test]
#[cfg(target_os = "macos")]
fn execution_host_sigkill_reports_unknown_and_does_not_adopt_or_signal_services() {
    leveler_test_support::bounded_test(
        "execution_host_sigkill_reports_unknown_and_does_not_adopt_or_signal_services",
        Duration::from_secs(120),
        || async {
            let port = reserve_port();
            let mock = MockServer::start(vec![spawn_response(port), completed_response()]).await;
            let env = Environment::new(&mock.base_url());
            let (mut daemon, ready_a) = env.serve("runtime-a");
            ready(&ready_a, &mut daemon).await;
            let host_state = env.state_dir().join("execution-host");
            let _host_cleanup = HostCleanup {
                state: host_state.clone(),
                repo: env.repo.clone(),
                home: env.home.clone(),
                config: env.config.clone(),
            };
            let (client, _session, task_id, service_pid) =
                start_service(&env, &mut daemon, &ready_a, port).await;
            let _service_cleanup = ServiceSelfExit {
                port,
                pid: service_pid,
            };
            let ready: serde_json::Value =
                serde_json::from_slice(&std::fs::read(host_state.join("ready.json")).unwrap())
                    .unwrap();
            let host_pid = ready["pid"].as_u64().expect("actual host readiness pid");
            assert_ne!(
                host_pid,
                u64::from(daemon.id()),
                "Host and Runtime must be distinct OS processes"
            );
            let actual_argv = Command::new("ps")
                .args(["-p", &host_pid.to_string(), "-o", "command="])
                .output()
                .unwrap();
            let actual_argv = String::from_utf8_lossy(&actual_argv.stdout);
            assert!(
                actual_argv.contains("execution-host")
                    && actual_argv.contains(host_state.to_string_lossy().as_ref()),
                "SIGKILL target must be this isolated test's actual Host"
            );
            assert!(
                Command::new("kill")
                    .args(["-KILL", &host_pid.to_string()])
                    .status()
                    .unwrap()
                    .success()
            );
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let alive = Command::new("kill")
                    .args(["-0", &host_pid.to_string()])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .unwrap();
                if !alive.success() {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "Host did not really exit after SIGKILL"
                );
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            assert_eq!(
                http_pid(port, "/after-host-sigkill").await.unwrap(),
                service_pid,
                "macOS observation: service remains alive, so no false kill-tree guarantee"
            );
            let config = leveler_execution::execution_host::ExecutionHostConfig {
                state_dir: host_state.clone(),
                repo_root: std::fs::canonicalize(&env.repo).unwrap(),
                executable: PathBuf::from(env!("CARGO_BIN_EXE_leveler")),
            };
            let error = match leveler_execution::execution_host::ExecutionHostClient::ensure(config)
                .await
            {
                Ok(_) => panic!("Host with uncertain live services must not be silently replaced"),
                Err(error) => error,
            };
            assert!(
                error.contains("unknown")
                    || error.contains("unmanaged")
                    || error.contains("recovery"),
                "explicit recovery diagnostic: {error}"
            );
            let preserved_ready: serde_json::Value =
                serde_json::from_slice(&std::fs::read(host_state.join("ready.json")).unwrap())
                    .unwrap();
            assert_eq!(
                preserved_ready["pid"], ready["pid"],
                "no replacement Host was launched"
            );
            assert_eq!(
                http_pid(port, "/after-refused-adoption").await.unwrap(),
                service_pid,
                "refused recovery must never signal a potentially reused persisted PID"
            );
            let output = env
                .command()
                .args(["background", "list", "--json"])
                .output()
                .unwrap();
            assert!(
                !output.status.success(),
                "CLI must not report an empty successful inventory after Host loss"
            );
            let diagnostic = String::from_utf8_lossy(&output.stderr);
            assert!(
                diagnostic.contains("unknown")
                    || diagnostic.contains("unavailable")
                    || diagnostic.contains("recovery"),
                "CLI must expose lost ownership: {diagnostic}"
            );
            assert!(
                daemon.try_wait().unwrap().is_none(),
                "Runtime is a separate surviving failure domain"
            );
            drop(client);
            assert_eq!(http_pid(port, "/exit-offline").await.unwrap(), service_pid);
            let deadline = Instant::now() + Duration::from_secs(10);
            while http_pid(port, "/gone").await.is_ok() {
                assert!(
                    Instant::now() < deadline,
                    "test service self-exit did not finish"
                );
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            eprintln!(
                "DOGFOOD PASS host_crash_task={task_id} host_pid={host_pid} service_pid={service_pid} actual_host_sigkill=true same_pid_service_survived=true recovery_explicit_unknown=true no_replacement_host=true cli_no_false_empty=true"
            );
        },
    );
}
