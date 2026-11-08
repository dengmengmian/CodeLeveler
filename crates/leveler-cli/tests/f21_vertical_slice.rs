//! F-21 production vertical slice: a REAL Runtime SIGKILL, a REAL surviving
//! supervisor/background process, a REAL durable exit result, and a REAL
//! restart that observes the SAME background task through the production tool
//! surface.
//!
//! Every process here is a real OS process:
//! - the runtime is `leveler serve` (spawned by this test),
//! - the supervisor is `leveler execution-host` (spawned by the runtime),
//! - the background command is `/bin/sh` running a script.
//!
//! The only scripted piece is the model: a mock SSE endpoint makes the runtime
//! request the production `run_command(background=true)` tool and, after the
//! restart, `wait_task`. Persistence is the real Execution Host archive and the
//! real session SQLite database.
#![cfg(unix)]

use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use leveler_client_protocol::{ClientCommand, InteractiveRuntimeClient, RuntimeEvent};
use leveler_local_transport::{CreateSessionRequest, LocalSocketRuntimeClient};
use leveler_storage::{Database, EventStore as _};
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
            home.join("config.toml"),
            "default_model = \"mock/m\"\n\n[update]\nauto_update = false\n",
        )
        .unwrap();
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

    fn db(&self) -> PathBuf {
        self.state_dir().join("sessions.db")
    }
}

fn sse(frames: Vec<serde_json::Value>) -> MockResponse {
    let body = frames
        .into_iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect::<String>()
        + "data: [DONE]\n\n";
    MockResponse::Sse { body }
}

/// The model asks the runtime to start a real background command. The command
/// has a controlled release: it must still be running while the runtime dies.
fn spawn_background_response(nonce: &str) -> MockResponse {
    let script = format!(
        "echo SPAWN_NONCE={nonce} >> f21-spawn.log\n\
         echo STARTED\n\
         while [ ! -f f21-release ]; do echo HEARTBEAT={nonce} >> f21-heartbeat.log; sleep 0.05; done\n\
         echo SIDE_EFFECT={nonce} >> f21-side-effect.log\n\
         echo COMPLETED\n\
         exit 7"
    );
    let arguments = serde_json::json!({
        "program": "sh",
        "args": ["-c", script],
        "background": true
    });
    sse(vec![
        serde_json::json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "spawn-f21", "function": {"name": "run_command", "arguments": arguments.to_string()}}]}}]}),
        serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
    ])
}

fn wait_task_response(task_id: &str) -> MockResponse {
    let arguments = serde_json::json!({ "task_id": task_id });
    sse(vec![
        serde_json::json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "observe-f21", "function": {"name": "wait_task", "arguments": arguments.to_string()}}]}}]}),
        serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
    ])
}

fn final_response(text: &str) -> MockResponse {
    let frame = serde_json::json!({
        "choices": [{"delta": {"content": text}, "finish_reason": "stop"}]
    });
    sse(vec![frame])
}

async fn ready(path: &Path, child: &mut ManagedChild) {
    let deadline = Instant::now() + Duration::from_secs(40);
    loop {
        if path.exists() {
            return;
        }
        if let Some(status) = child.try_wait().unwrap() {
            let stderr = path
                .with_file_name(
                    path.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .replace("-ready.json", "-stderr.txt"),
                )
                .display()
                .to_string();
            panic!(
                "runtime exited before readiness ({status}); stderr={:?}",
                std::fs::read_to_string(stderr).unwrap_or_default()
            );
        }
        assert!(
            Instant::now() < deadline,
            "runtime did not become ready: {}",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}

fn host_pid(state_dir: &Path) -> u32 {
    let ready: serde_json::Value =
        serde_json::from_slice(&std::fs::read(state_dir.join("ready.json")).unwrap()).unwrap();
    ready["pid"].as_u64().expect("host pid") as u32
}

fn host_tasks(state_dir: &Path) -> Vec<serde_json::Value> {
    let bytes = std::fs::read(state_dir.join("tasks.json")).unwrap_or_default();
    if bytes.is_empty() {
        return Vec::new();
    }
    serde_json::from_slice(&bytes).unwrap_or_default()
}

fn find_task(tasks: &[serde_json::Value], id: &str) -> Option<serde_json::Value> {
    tasks
        .iter()
        .find(|task| task["snapshot"]["id"].as_str() == Some(id))
        .cloned()
}

fn count_occurrences(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

async fn wait_until<F>(mut probe: F, what: &str) -> Result<(), String>
where
    F: FnMut() -> Result<bool, String>,
{
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if probe()? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("timed out waiting for {what}"));
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn event_session(id: &str) -> leveler_core::SessionId {
    leveler_core::SessionId::new(id)
}

/// Poll the real session database until exactly `expected` durable terminal
/// facts exist. Reading through a fresh connection cannot be satisfied by the
/// runtime's in-memory projection.
async fn wait_finished_events(path: &Path, session: &str, expected: usize) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let count = match Database::connect(path).await {
            Ok(db) => db
                .load_by_types(&event_session(session), &["background_task_finished"])
                .await
                .map(|rows| rows.len())
                .unwrap_or(0),
            Err(_) => 0,
        };
        if count >= expected {
            assert_eq!(
                count, expected,
                "the durable terminal fact must be written exactly once"
            );
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the recovered terminal fact never became durable ({count}/{expected})"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[test]
fn runtime_sigkill_recovered_task_is_observed_by_the_same_id() {
    leveler_test_support::bounded_test(
        "runtime_sigkill_recovered_task_is_observed_by_the_same_id",
        Duration::from_secs(240),
        || async {
            let nonce = format!(
                "f21-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            );
            let mock = MockServer::start(vec![]).await;
            let env = Environment::new(&mock.base_url());

            // ---- P1: start the task through the production runtime/tool ------
            let (mut runtime_a, ready_a) = env.serve("runtime-a");
            ready(&ready_a, &mut runtime_a).await;
            let client_a = LocalSocketRuntimeClient::connect(&env.socket())
                .await
                .unwrap();
            let session = client_a
                .create_session(CreateSessionRequest {
                    request_id: None,
                    collaboration: leveler_local_transport::CollaborationMode::Goal,
                    workspace: leveler_local_transport::CreateWorkspaceSelection::RuntimeDefault,
                    approval_policy: leveler_client_protocol::ApprovalPolicy::AutoApprove,
                    goal: "Run the long background command and report its durable result".into(),
                    model: None,
                    mode: leveler_client_protocol::PermissionProfile::FullAccess,
                })
                .await
                .unwrap()
                .session
                .id;
            // The first model step asks for the background command. The second
            // never answers: the turn stays open while we kill the runtime, so
            // goal-terminal cleanup cannot reap the task either.
            mock.push(spawn_background_response(&nonce)).await;
            mock.push(MockResponse::NeverResponds).await;
            client_a
                .send(ClientCommand::SubmitMessage {
                    session_id: session.clone(),
                    content: "Start the long background command.".into(),
                    attachments: vec![],
                })
                .await
                .unwrap();

            let task_id = {
                let deadline = Instant::now() + Duration::from_secs(40);
                loop {
                    let snapshot = client_a.snapshot(&session).await.unwrap();
                    if let Some(task) = snapshot
                        .active_background_tasks
                        .iter()
                        .find(|task| task.task_id.starts_with("host-"))
                    {
                        break task.task_id.clone();
                    }
                    assert!(
                        Instant::now() < deadline,
                        "the production tool never published a supervised task"
                    );
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            };
            assert!(
                env.repo.join("f21-spawn.log").exists(),
                "the background command never ran: {}",
                std::fs::read_to_string(env.repo.join("f21-spawn.log")).unwrap_or_default()
            );
            // The command's stdout is captured by the supervisor, not by the
            // runtime's pipe reader: wait for the durable log to show it.
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                let logs = env.background(&["logs", &task_id]);
                if logs.contains("STARTED") {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "the supervisor never retained the command's STARTED line: {logs}"
                );
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            let spawn_log = std::fs::read_to_string(env.repo.join("f21-spawn.log")).unwrap();
            assert_eq!(count_occurrences(&spawn_log, &nonce), 1, "{spawn_log}");

            // Wait until the runtime has asked the model again; the start
            // tool result is durable at that point.
            wait_until(
                || Ok(mock.request_count() >= 2),
                "the runtime to reach the post-start model step",
            )
            .await
            .unwrap();

            // ---- P2: production observation says running --------------------
            let list = env.background(&["list", "--json"]);
            assert!(list.contains(&task_id), "{list}");
            let logs = env.background(&["logs", &task_id]);
            assert!(logs.contains("STARTED"), "{logs}");

            let runtime_pid = runtime_a.id();
            let host_state = env.state_dir().join("execution-host");
            let supervisor_pid = host_pid(&host_state);
            let tasks = host_tasks(&host_state);
            let started = find_task(&tasks, &task_id).expect("durable started fact");
            let background_pid = started["snapshot"]["pid"].as_u64().unwrap() as u32;
            let start_identity = started["snapshot"]["start_identity"]
                .as_str()
                .expect("durable process start identity")
                .to_string();
            assert_eq!(
                started["snapshot"]["status"].as_str(),
                Some("Running"),
                "{started}"
            );
            assert_ne!(runtime_pid, supervisor_pid);
            assert_ne!(background_pid, supervisor_pid);
            assert!(alive(background_pid));

            // ---- P3: the real SIGKILL --------------------------------------
            runtime_a.kill().expect("SIGKILL the runtime");
            let status = runtime_a.wait().unwrap();
            assert_eq!(
                status.signal(),
                Some(9),
                "the runtime must really die by SIGKILL: {status}"
            );
            assert!(!alive(runtime_pid), "the runtime process must be gone");
            // ---- P4: supervisor and workload survive ------------------------
            assert!(alive(supervisor_pid), "the supervisor must survive");
            assert!(alive(background_pid), "the background process must survive");
            let identity = leveler_execution::process_identity::verify_process_identity(
                background_pid,
                &start_identity,
            );
            assert_eq!(
                identity,
                leveler_execution::process_identity::ProcessIdentity::Matches,
                "the surviving process must still be the one that was started"
            );
            let list_after_kill = env.background(&["list", "--json"]);
            assert!(list_after_kill.contains(&task_id), "{list_after_kill}");
            assert!(list_after_kill.contains("running"), "{list_after_kill}");
            // The start nonce is unchanged: nothing respawned.
            assert_eq!(
                count_occurrences(
                    &std::fs::read_to_string(env.repo.join("f21-spawn.log")).unwrap(),
                    &nonce
                ),
                1
            );
            let heartbeat_before =
                std::fs::read_to_string(env.repo.join("f21-heartbeat.log")).unwrap_or_default();
            tokio::time::sleep(Duration::from_millis(300)).await;
            let heartbeat_after =
                std::fs::read_to_string(env.repo.join("f21-heartbeat.log")).unwrap_or_default();
            assert!(
                heartbeat_after.len() > heartbeat_before.len(),
                "the workload must keep producing output after the runtime died"
            );

            // The dead runtime left its second model request in flight, so that
            // scripted response is still the mock's next frame. Consume it with
            // a throwaway HTTP request, then queue the post-restart script.
            // Without this a resumed runtime would receive the stale frame.
            mock.push(wait_task_response(&task_id)).await;
            let stale_url = format!("{}/v1/chat/completions", mock.base_url());
            tokio::spawn(async move {
                let _ = reqwest::Client::new()
                    .post(stale_url)
                    .body("{}")
                    .send()
                    .await;
            });
            wait_until(
                || Ok(mock.request_count() >= 3),
                "the stale model frame to be consumed",
            )
            .await
            .unwrap();
            mock.push(final_response("The recovered task completed with exit 7."))
                .await;

            // ---- P5: the task completes with the runtime dead ---------------
            std::fs::write(env.repo.join("f21-release"), nonce.as_bytes()).unwrap();
            wait_until(
                || {
                    let tasks = host_tasks(&host_state);
                    Ok(find_task(&tasks, &task_id)
                        .is_some_and(|task| task["snapshot"]["status"].as_str() == Some("Exited")))
                },
                "the supervisor to commit the exit while no runtime exists",
            )
            .await
            .unwrap();
            let finished = find_task(&host_tasks(&host_state), &task_id).unwrap();
            assert_eq!(
                finished["snapshot"]["exit_code"].as_i64(),
                Some(7),
                "{finished}"
            );
            assert!(finished["snapshot"]["signal"].is_null(), "{finished}");
            let log = finished["snapshot"]["log"].as_str().unwrap_or_default();
            assert!(log.contains("STARTED"), "{log}");
            assert!(log.contains("COMPLETED"), "{log}");
            let result = &finished["result"];
            assert!(
                result["result_id"]
                    .as_str()
                    .is_some_and(|id| !id.is_empty()),
                "the durable completion record must carry a result identity: {finished}"
            );
            assert_eq!(result["exit_code"].as_i64(), Some(7), "{finished}");
            assert_eq!(
                result["output_bytes"].as_u64(),
                Some(log.len() as u64),
                "{finished}"
            );
            let side_effect =
                std::fs::read_to_string(env.repo.join("f21-side-effect.log")).unwrap();
            assert_eq!(count_occurrences(&side_effect, &nonce), 1, "{side_effect}");
            // The spawn intent is gone only after the started fact committed.
            let intents: Vec<serde_json::Value> =
                serde_json::from_slice(&std::fs::read(host_state.join("intents.json")).unwrap())
                    .unwrap();
            assert!(
                intents.is_empty(),
                "an ACKed intent must be cleared: {intents:?}"
            );

            // ---- P6: production restart / resume ---------------------------
            let (mut runtime_b, ready_b) = env.serve("runtime-b");
            ready(&ready_b, &mut runtime_b).await;
            let client_b = LocalSocketRuntimeClient::connect(&env.socket())
                .await
                .unwrap();
            let snapshot = client_b.snapshot(&session).await.unwrap();
            let record = snapshot
                .background_tasks
                .iter()
                .find(|record| record.task_id == task_id)
                .unwrap_or_else(|| panic!("no recovered record for {task_id}: {snapshot:?}"));
            assert_eq!(
                record.exit_code,
                Some(7),
                "the recovered projection must carry the real exit: {record:?}"
            );

            // The reconnect boundary persists the terminal fact exactly once.
            wait_finished_events(&env.db(), session.as_str(), 1).await;

            // ---- P7: production tool surface reads the ORIGINAL id ----------
            let mut events_b = client_b.subscribe_session(&session);
            let completed = observe_wait_task(&client_b, &session, &mut events_b).await;
            // Exit 7 is a real failing result, so the tool correctly reports
            // `ok = false` while carrying the exact exit and output.
            assert!(
                !completed.ok,
                "a non-zero exit must not be reported as success: {completed:?}"
            );
            assert!(
                completed.preview.contains(&task_id),
                "the tool result must name the original task: {}",
                completed.preview
            );
            assert!(
                completed.preview.contains("exit_code: Some(7)"),
                "the tool result must carry the real exit: {}",
                completed.preview
            );
            assert!(
                completed.preview.contains("COMPLETED"),
                "the tool result must carry the retained output: {}",
                completed.preview
            );

            // The CLI observation surface reads the same durable fact.
            let logs = env.background(&["logs", &task_id]);
            assert!(logs.contains("COMPLETED"), "{logs}");

            // ---- P8: repeat the recovery; no replay, no second result -------
            // Only one local runtime may own a repository endpoint, so the
            // second restart begins after the first replacement really exits.
            let _ = runtime_b.kill();
            let _ = runtime_b.wait();
            let (mut runtime_c, ready_c) = env.serve("runtime-c");
            ready(&ready_c, &mut runtime_c).await;
            let client_c = LocalSocketRuntimeClient::connect(&env.socket())
                .await
                .unwrap();
            let repeated_snapshot = client_c.snapshot(&session).await.unwrap();
            let repeated = repeated_snapshot
                .background_tasks
                .iter()
                .find(|record| record.task_id == task_id)
                .unwrap_or_else(|| {
                    panic!("no record after a second restart: {repeated_snapshot:?}")
                });
            assert_eq!(
                repeated.exit_code,
                Some(7),
                "a second recovery must keep the same result"
            );
            let logs = env.background(&["logs", &task_id]);
            assert!(logs.contains("COMPLETED"), "{logs}");
            let finished_events = Database::connect(&env.db())
                .await
                .unwrap()
                .load_by_types(
                    &event_session(session.as_str()),
                    &["background_task_finished"],
                )
                .await
                .unwrap();
            assert_eq!(
                finished_events.len(),
                1,
                "a repeated recovery must not append a second result"
            );
            // No respawn and no second side effect.
            assert_eq!(
                count_occurrences(
                    &std::fs::read_to_string(env.repo.join("f21-spawn.log")).unwrap(),
                    &nonce
                ),
                1
            );
            assert_eq!(
                count_occurrences(
                    &std::fs::read_to_string(env.repo.join("f21-side-effect.log")).unwrap(),
                    &nonce
                ),
                1
            );
            let tasks = host_tasks(&host_state);
            assert_eq!(
                tasks
                    .iter()
                    .filter(|task| task["snapshot"]["id"].as_str() == Some(&task_id))
                    .count(),
                1,
                "the durable archive must hold exactly this one task identity"
            );

            eprintln!(
                "F21 PASS nonce={nonce} session={} task={task_id} runtime_pid={runtime_pid} \
                 supervisor_pid={supervisor_pid} background_pid={background_pid} \
                 start_identity={start_identity} runtime_signal=9 exit_code=7 side_effect_count=1",
                session.as_str()
            );

            let _ = runtime_c.kill();
            let _ = runtime_c.wait();
            export_evidence(
                &env,
                &session,
                &task_id,
                &nonce,
                runtime_pid,
                supervisor_pid,
                background_pid,
                &start_identity,
                status.signal().unwrap_or(0),
            )
            .await;
            // The host is test-owned. Stop it only if its argv is this test's.
            stop_isolated_host(&host_state);
        },
    );
}

#[derive(Debug)]
struct Completed {
    ok: bool,
    preview: String,
}

/// Wait until the runtime reports the production `wait_task` call we scripted.
async fn observe_wait_task(
    client: &LocalSocketRuntimeClient,
    session: &leveler_client_protocol::SessionId,
    events: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>,
) -> Completed {
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "Observe the recovered background task with its original id.".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    let mut wait_ids = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        assert!(
            Instant::now() < deadline,
            "the resumed runtime never observed the task"
        );
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("event stream")
            .expect("stream open");
        match event {
            RuntimeEvent::ToolCallStarted { id, name, .. } if name == "wait_task" => {
                wait_ids.push(id);
            }
            RuntimeEvent::ToolCallCompleted {
                id, ok, preview, ..
            } if wait_ids.contains(&id) => {
                return Completed { ok, preview };
            }
            _ => {}
        }
    }
}

/// Copy the real artifacts this run produced into a caller-chosen audit
/// directory. Test-only: the product never reads this path.
async fn export_evidence(
    env: &Environment,
    session: &leveler_client_protocol::SessionId,
    task_id: &str,
    nonce: &str,
    runtime_pid: u32,
    supervisor_pid: u32,
    background_pid: u32,
    start_identity: &str,
    runtime_signal: i32,
) {
    let Some(dir) = std::env::var_os("F21_EVIDENCE_DIR").map(std::path::PathBuf::from) else {
        return;
    };
    std::fs::create_dir_all(&dir).unwrap();
    let host_state = env.state_dir().join("execution-host");
    for name in [
        "ready.json",
        "tasks.json",
        "intents.json",
        "intents.unresolved.json",
    ] {
        let source = host_state.join(name);
        if source.exists() {
            std::fs::copy(&source, dir.join(name)).unwrap();
        }
    }
    let settlement = env.state_dir().join("execution-settlement");
    if let Ok(entries) = std::fs::read_dir(&settlement) {
        let target = dir.join("execution-settlement");
        std::fs::create_dir_all(&target).unwrap();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                let _ = std::fs::copy(&path, target.join(path.file_name().unwrap()));
            }
        }
    }
    for name in [
        "f21-spawn.log",
        "f21-heartbeat.log",
        "f21-side-effect.log",
        "runtime-a-stderr.txt",
        "runtime-b-stderr.txt",
        "runtime-c-stderr.txt",
    ] {
        let source = env.repo.join(name);
        if source.exists() {
            std::fs::copy(&source, dir.join(name)).unwrap();
        }
    }
    let summary = format!(
        "session={}\ntask_id={task_id}\nnonce={nonce}\nruntime_pid={runtime_pid}\n\
         supervisor_pid={supervisor_pid}\nbackground_pid={background_pid}\n\
         process_start_identity={start_identity}\nruntime_sigkill_signal={runtime_signal}\n\
         durable_exit_code=7\nside_effect_count=1\n\
         host_state={}\nsession_db={}\n",
        session.as_str(),
        host_state.display(),
        env.db().display(),
    );
    std::fs::write(dir.join("SYSTEM.txt"), summary).unwrap();
    // Dump the durable terminal event payloads exactly as the engine stored
    // them, so the report can quote the SQLite facts, not just the count.
    let rows = match Database::connect(&env.db()).await {
        Ok(db) => db
            .load_by_types(
                &event_session(session.as_str()),
                &["background_task_finished"],
            )
            .await
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let payloads: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            serde_json::json!({
                "event_type": row.event_type,
                "payload": row.payload,
            })
        })
        .collect();
    std::fs::write(
        dir.join("background_task_finished.json"),
        serde_json::to_vec_pretty(&payloads).unwrap(),
    )
    .unwrap();
}

fn stop_isolated_host(state_dir: &Path) {
    let Ok(bytes) = std::fs::read(state_dir.join("ready.json")) else {
        return;
    };
    let Ok(ready) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return;
    };
    let Some(pid) = ready["pid"].as_u64() else {
        return;
    };
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output();
    let Ok(output) = output else {
        return;
    };
    let argv = String::from_utf8_lossy(&output.stdout);
    if argv.contains("execution-host") && argv.contains(&state_dir.to_string_lossy().to_string()) {
        let _ = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status();
    }
}

/// A cancelled Goal-owned task must be reaped from the supervisor too. The
/// cleanup ticket used to see only the in-process registry; a hosted Goal task
/// that was not in it would survive the goal it belongs to.
#[test]
fn cancelling_a_goal_reaps_its_supervised_background_task() {
    leveler_test_support::bounded_test(
        "cancelling_a_goal_reaps_its_supervised_background_task",
        Duration::from_secs(120),
        || async {
            let mock = MockServer::start(vec![]).await;
            let env = Environment::new(&mock.base_url());
            let (mut runtime, ready_file) = env.serve("runtime-cancel");
            ready(&ready_file, &mut runtime).await;
            let client = LocalSocketRuntimeClient::connect(&env.socket())
                .await
                .unwrap();
            let session = client
                .create_session(CreateSessionRequest {
                    request_id: None,
                    collaboration: leveler_local_transport::CollaborationMode::Goal,
                    workspace: leveler_local_transport::CreateWorkspaceSelection::RuntimeDefault,
                    approval_policy: leveler_client_protocol::ApprovalPolicy::AutoApprove,
                    goal: "Start a service that this cancelled goal owns".into(),
                    model: None,
                    mode: leveler_client_protocol::PermissionProfile::FullAccess,
                })
                .await
                .unwrap()
                .session
                .id;
            let arguments = serde_json::json!({
                "program": "sh",
                "args": ["-c", "echo READY; sleep 300"],
                "background": true
            });
            mock.push(sse(vec![
                serde_json::json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "spawn-cancel", "function": {"name": "run_command", "arguments": arguments.to_string()}}]}}]}),
                serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
            ]))
            .await;
            mock.push(MockResponse::NeverResponds).await;
            client
                .send(ClientCommand::SubmitMessage {
                    session_id: session.clone(),
                    content: "Start the service.".into(),
                    attachments: vec![],
                })
                .await
                .unwrap();
            let (task_id, pid) = {
                let deadline = Instant::now() + Duration::from_secs(40);
                loop {
                    let snapshot = client.snapshot(&session).await.unwrap();
                    if let Some(task) = snapshot
                        .active_background_tasks
                        .iter()
                        .find(|task| task.task_id.starts_with("host-"))
                        && let Some(pid) = task.pid
                    {
                        break (task.task_id.clone(), pid);
                    }
                    assert!(
                        Instant::now() < deadline,
                        "the supervised service never started"
                    );
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            };
            assert!(alive(pid));
            let host_state = env.state_dir().join("execution-host");
            let supervisor = host_pid(&host_state);
            client
                .send(ClientCommand::CancelTask {
                    session_id: session.clone(),
                })
                .await
                .expect("cancel the session task");
            let deadline = Instant::now() + Duration::from_secs(20);
            while alive(pid) {
                assert!(
                    Instant::now() < deadline,
                    "the cancelled goal left its supervised process running"
                );
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            assert!(
                alive(supervisor),
                "the supervisor owns the task lifecycle, not this runtime"
            );
            let list = env.background(&["list", "--json"]);
            let tasks: Vec<serde_json::Value> = serde_json::from_str(&list).unwrap();
            let task = tasks
                .iter()
                .find(|task| task["task_id"].as_str() == Some(&task_id))
                .unwrap_or_else(|| panic!("no host record for {task_id}: {list}"));
            assert_ne!(
                task["status"].as_str(),
                Some("running"),
                "the stopped task must not remain running: {task}"
            );
            let _ = runtime.kill();
            let _ = runtime.wait();
            stop_isolated_host(&host_state);
        },
    );
}

/// A runtime that starts AFTER the crash must see the still-running task by its
/// original id, read its live output, and be able to stop it through the
/// production observation/stop surface — without adopting or respawning it.
#[test]
fn a_restarted_runtime_observes_and_stops_the_same_running_task() {
    leveler_test_support::bounded_test(
        "a_restarted_runtime_observes_and_stops_the_same_running_task",
        Duration::from_secs(120),
        || async {
            let nonce = format!("f21-live-{}", std::process::id());
            let mock = MockServer::start(vec![]).await;
            let env = Environment::new(&mock.base_url());
            let (mut runtime_a, ready_a) = env.serve("runtime-live-a");
            ready(&ready_a, &mut runtime_a).await;
            let client_a = LocalSocketRuntimeClient::connect(&env.socket())
                .await
                .unwrap();
            let session = client_a
                .create_session(CreateSessionRequest {
                    request_id: None,
                    collaboration: leveler_local_transport::CollaborationMode::Goal,
                    workspace: leveler_local_transport::CreateWorkspaceSelection::RuntimeDefault,
                    approval_policy: leveler_client_protocol::ApprovalPolicy::AutoApprove,
                    goal: "Keep a supervised task alive across a runtime restart".into(),
                    model: None,
                    mode: leveler_client_protocol::PermissionProfile::FullAccess,
                })
                .await
                .unwrap()
                .session
                .id;
            let arguments = serde_json::json!({
                "program": "sh",
                "args": ["-c", format!("echo READY {nonce}; sleep 300")],
                "background": true
            });
            mock.push(sse(vec![
                serde_json::json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "spawn-live", "function": {"name": "run_command", "arguments": arguments.to_string()}}]}}]}),
                serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
            ]))
            .await;
            mock.push(MockResponse::NeverResponds).await;
            client_a
                .send(ClientCommand::SubmitMessage {
                    session_id: session.clone(),
                    content: "Start it.".into(),
                    attachments: vec![],
                })
                .await
                .unwrap();
            let (task_id, pid) = {
                let deadline = Instant::now() + Duration::from_secs(40);
                loop {
                    let snapshot = client_a.snapshot(&session).await.unwrap();
                    if let Some(task) = snapshot
                        .active_background_tasks
                        .iter()
                        .find(|task| task.task_id.starts_with("host-"))
                        && let Some(pid) = task.pid
                    {
                        break (task.task_id.clone(), pid);
                    }
                    assert!(Instant::now() < deadline, "the task never started");
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            };
            let host_state = env.state_dir().join("execution-host");
            let supervisor = host_pid(&host_state);
            runtime_a.kill().unwrap();
            assert_eq!(runtime_a.wait().unwrap().signal(), Some(9));
            assert!(alive(pid), "the workload must survive the runtime");
            assert!(alive(supervisor), "the supervisor must survive the runtime");

            let (mut runtime_b, ready_b) = env.serve("runtime-live-b");
            ready(&ready_b, &mut runtime_b).await;
            let client_b = LocalSocketRuntimeClient::connect(&env.socket())
                .await
                .unwrap();
            let snapshot = client_b.snapshot(&session).await.unwrap();
            let recovered = snapshot
                .active_background_tasks
                .iter()
                .find(|task| task.task_id == task_id)
                .unwrap_or_else(|| {
                    panic!("the restarted runtime did not recover the live task: {snapshot:?}")
                });
            assert_eq!(recovered.pid, Some(pid), "same process, not a new one");
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                let logs = env.background(&["logs", &task_id]);
                if logs.contains(&nonce) {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "the recovered task's live output was not observable: {logs}"
                );
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            let stopped = env.background(&["stop", &task_id]);
            assert!(stopped.contains(&task_id), "{stopped}");
            let deadline = Instant::now() + Duration::from_secs(20);
            while alive(pid) {
                assert!(
                    Instant::now() < deadline,
                    "the stop request did not reach the original process"
                );
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            assert!(alive(supervisor), "the stop targets the task, not the host");
            let _ = runtime_b.kill();
            let _ = runtime_b.wait();
            stop_isolated_host(&host_state);
        },
    );
}
