//! Gate A: real background lifecycle and Execution Host failure matrix.
//!
//! Every process here is a real OS process: `leveler serve` (runtime),
//! `leveler execution-host` (supervisor), and `/bin/sh` (the background
//! command). The model is scripted only because this file tests mechanical
//! lifecycle facts, never model behaviour. Durable facts are read from the
//! Execution Host archive, the semantic journal, the session SQLite database
//! and the real process table.
//!
//! Host crash points are injected by a DEBUG-ONLY, one-shot env hook in
//! `execution_host.rs`. The hook writes a marker and SIGKILLs the host, so the
//! crash is a real process death with no cleanup.
#![cfg(unix)]

use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use leveler_client_protocol::{ClientCommand, InteractiveRuntimeClient, RuntimeEvent};
use leveler_execution::execution_host::{ExecutionHostClient, ExecutionHostConfig};
use leveler_execution::{BackgroundTaskRegistry, process_identity};
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
        self.serve_with_env(name, &[])
    }

    fn serve_with_env(&self, name: &str, envs: &[(&str, &Path)]) -> (ManagedChild, PathBuf) {
        let ready = self.home.join(format!("{name}-ready.json"));
        let stderr = self.home.join(format!("{name}-stderr.txt"));
        let mut command = self.command();
        command
            .args(["serve", "--ready-json"])
            .arg(&ready)
            .stdout(Stdio::null())
            .stderr(Stdio::from(std::fs::File::create(&stderr).unwrap()));
        for (key, value) in envs {
            command.env(key, value);
        }
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

    fn host_state(&self) -> PathBuf {
        self.state_dir().join("execution-host")
    }

    fn semantic_dir(&self) -> PathBuf {
        self.state_dir().join("execution-settlement")
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

    fn host_config(&self) -> ExecutionHostConfig {
        ExecutionHostConfig {
            state_dir: self.host_state(),
            repo_root: std::fs::canonicalize(&self.repo).unwrap(),
            executable: PathBuf::from(env!("CARGO_BIN_EXE_leveler")),
        }
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

fn spawn_response(call_id: &str, script: &str) -> MockResponse {
    let arguments = serde_json::json!({
        "program": "sh",
        "args": ["-c", script],
        "background": true
    });
    sse(vec![
        serde_json::json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": call_id, "function": {"name": "run_command", "arguments": arguments.to_string()}}]}}]}),
        serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
    ])
}

fn tool_response(call_id: &str, name: &str, arguments: serde_json::Value) -> MockResponse {
    sse(vec![
        serde_json::json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": call_id, "function": {"name": name, "arguments": arguments.to_string()}}]}}]}),
        serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
    ])
}

fn final_response(text: &str) -> MockResponse {
    sse(vec![serde_json::json!({
        "choices": [{"delta": {"content": text}, "finish_reason": "stop"}]
    })])
}

async fn ready(path: &Path, child: &mut ManagedChild) {
    let deadline = Instant::now() + Duration::from_secs(40);
    loop {
        if path.exists() {
            return;
        }
        if let Some(status) = child.try_wait().unwrap() {
            let stderr = path.with_file_name(
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .replace("-ready.json", "-stderr.txt"),
            );
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

fn read_json(path: &Path) -> Vec<serde_json::Value> {
    let bytes = std::fs::read(path).unwrap_or_default();
    if bytes.is_empty() {
        return Vec::new();
    }
    serde_json::from_slice(&bytes).unwrap_or_default()
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

async fn finished_events(path: &Path, session: &str) -> Vec<serde_json::Value> {
    let Ok(db) = Database::connect(path).await else {
        return Vec::new();
    };
    db.load_by_types(&event_session(session), &["background_task_finished"])
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|row| {
            serde_json::json!({
                "event_type": row.event_type,
                "payload": row.payload,
            })
        })
        .collect()
}

async fn wait_finished_events(path: &Path, session: &str, expected: usize) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let count = finished_events(path, session).await.len();
        if count >= expected {
            assert_eq!(count, expected, "the durable terminal fact is written once");
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the recovered terminal fact never became durable ({count}/{expected})"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The semantic journal record for one task: `None` while admission is pending.
fn journal_settlement(env: &Environment, task_id: &str) -> Option<serde_json::Value> {
    let dir = env.semantic_dir();
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let bytes = std::fs::read(entry.path()).ok()?;
        let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        if value["task_id"].as_str() == Some(task_id) {
            return value.get("settlement").cloned();
        }
    }
    None
}

async fn create_goal_session(client: &LocalSocketRuntimeClient, goal: &str) -> String {
    client
        .create_session(CreateSessionRequest {
            request_id: None,
            collaboration: leveler_local_transport::CollaborationMode::Goal,
            workspace: leveler_local_transport::CreateWorkspaceSelection::RuntimeDefault,
            approval_policy: leveler_client_protocol::ApprovalPolicy::AutoApprove,
            goal: goal.into(),
            model: None,
            mode: leveler_client_protocol::PermissionProfile::FullAccess,
        })
        .await
        .unwrap()
        .session
        .id
        .to_string()
}

async fn submit(client: &LocalSocketRuntimeClient, session: &str, content: &str) {
    client
        .send(ClientCommand::SubmitMessage {
            session_id: leveler_client_protocol::SessionId::new(session),
            content: content.to_string(),
            attachments: vec![],
        })
        .await
        .unwrap();
}

/// Wait for the first hosted task to appear in the production snapshot.
async fn wait_hosted_task(
    client: &LocalSocketRuntimeClient,
    session: &str,
) -> (String, Option<u32>) {
    wait_hosted_task_with(client, session, None).await
}

/// As [`wait_hosted_task`], but on timeout include the runtime's captured
/// stderr (the only place the supervisor-unavailable reason is written).
async fn wait_hosted_task_with(
    client: &LocalSocketRuntimeClient,
    session: &str,
    env: Option<&Environment>,
) -> (String, Option<u32>) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let snapshot = client
            .snapshot(&leveler_client_protocol::SessionId::new(session))
            .await
            .unwrap();
        if let Some(task) = snapshot
            .active_background_tasks
            .iter()
            .find(|task| task.task_id.starts_with("host-"))
        {
            return (task.task_id.clone(), task.pid);
        }
        if Instant::now() >= deadline {
            let mut report = String::new();
            if let Some(env) = env {
                for entry in std::fs::read_dir(&env.home).into_iter().flatten().flatten() {
                    let path = entry.path();
                    if path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.ends_with("-stderr.txt"))
                    {
                        report.push_str(&format!(
                            "\n--- {} ---\n{}",
                            path.display(),
                            std::fs::read_to_string(&path).unwrap_or_default()
                        ));
                    }
                }
            }
            panic!("the production tool never published a supervised task{report}");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Wait for one durable archive fact.
async fn wait_archived(
    state_dir: &Path,
    id: &str,
    predicate: impl Fn(&serde_json::Value) -> bool + Copy,
    what: &str,
) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(task) = find_task(&host_tasks(state_dir), id)
            && predicate(&task)
        {
            return task;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
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
    // The env var is set only by this test file, so a host started from this
    // fixture is test-owned even though `ps` is denied by the sandbox.
    let _ = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status();
}

fn fault_env<'a>(marker: &'a Path, point: &'a str) -> [(&'a str, &'a Path); 2] {
    [
        ("LEVELER_EXECUTION_HOST_FAULT", Path::new(point)),
        ("LEVELER_EXECUTION_HOST_FAULT_MARKER", marker),
    ]
}

async fn stop_workload(pid: u32) {
    if alive(pid) {
        let _ = Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status();
        let deadline = Instant::now() + Duration::from_secs(10);
        while alive(pid) {
            assert!(Instant::now() < deadline, "test workload {pid} did not die");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

// ---------------------------------------------------------------------------
// H1 / B2 — the host dies after the durable spawn intent, before the OS spawn.
// ---------------------------------------------------------------------------
#[test]
fn h1_host_crash_before_spawn_keeps_an_intent_and_never_fabricates_running() {
    leveler_test_support::bounded_test(
        "h1_host_crash_before_spawn_keeps_an_intent_and_never_fabricates_running",
        Duration::from_secs(120),
        || async {
            let nonce = format!("h1-{}", std::process::id());
            let mock = MockServer::start(vec![]).await;
            let env = Environment::new(&mock.base_url());
            let marker = env.home.join("h1-fault.marker");
            let (mut runtime_a, ready_a) =
                env.serve_with_env("h1-a", &fault_env(&marker, "after-intent"));
            ready(&ready_a, &mut runtime_a).await;
            let client = LocalSocketRuntimeClient::connect(&env.socket())
                .await
                .unwrap();
            let session = create_goal_session(&client, "h1 crash before spawn").await;
            mock.push(spawn_response(
                "h1-spawn",
                &format!("echo SPAWN={nonce} >> h1-spawn.log; sleep 300"),
            ))
            .await;
            mock.push(MockResponse::NeverResponds).await;
            submit(&client, &session, "Start the background command.").await;

            wait_until(|| Ok(marker.exists()), "the host fault marker")
                .await
                .unwrap();
            let host_state = env.host_state();
            let host = host_pid(&host_state);
            wait_until(|| Ok(!alive(host)), "the injected host death")
                .await
                .unwrap();

            let intents = read_json(&host_state.join("intents.json"));
            assert_eq!(
                intents.len(),
                1,
                "the durable intent must survive: {intents:?}"
            );
            let intent_id = intents[0]["id"].as_str().unwrap().to_string();
            assert!(intent_id.starts_with("host-"), "{intent_id}");
            assert!(
                host_tasks(&host_state).is_empty(),
                "no process fact may exist before the OS spawn"
            );
            assert!(
                !env.repo.join("h1-spawn.log").exists(),
                "the command must never have run"
            );

            let _ = runtime_a.kill();
            let _ = runtime_a.wait();
            let (mut runtime_b, ready_b) = env.serve("h1-b");
            ready(&ready_b, &mut runtime_b).await;
            let client_b = LocalSocketRuntimeClient::connect(&env.socket())
                .await
                .unwrap();
            let list: Vec<serde_json::Value> =
                serde_json::from_str(&env.background(&["list", "--json"])).unwrap();
            assert!(
                list.iter()
                    .all(|task| task["task_id"] != intent_id.as_str()),
                "an unresolved intent is not a running task: {list:?}"
            );
            assert!(
                host_tasks(&host_state).is_empty(),
                "recovery must not synthesize a process fact"
            );
            assert!(
                read_json(&host_state.join("intents.json")).is_empty(),
                "recovery moved the intent out of the active set"
            );
            let unresolved = read_json(&host_state.join("intents.unresolved.json"));
            assert_eq!(unresolved.len(), 1, "{unresolved:?}");
            assert_eq!(unresolved[0]["intent"]["id"], intent_id.as_str());
            assert!(
                !env.repo.join("h1-spawn.log").exists(),
                "recovery must not replay the spawn"
            );
            let snapshot = client_b
                .snapshot(&leveler_client_protocol::SessionId::new(&session))
                .await
                .unwrap();
            assert!(
                snapshot
                    .background_tasks
                    .iter()
                    .all(|task| task.task_id != intent_id),
                "the session projection must not claim an unproven process"
            );
            eprintln!(
                "H1 PASS intent={intent_id} host_pid={host} runtime_a={} tasks=0 spawned=false",
                runtime_a.id()
            );
            let _ = runtime_b.kill();
            let _ = runtime_b.wait();
            stop_isolated_host(&host_state);
        },
    );
}

// ---------------------------------------------------------------------------
// H2 / B3 — the host dies after the OS spawn, before the started fact.
// ---------------------------------------------------------------------------
#[test]
fn h2_host_crash_after_spawn_before_started_never_replays_the_side_effect() {
    leveler_test_support::bounded_test(
        "h2_host_crash_after_spawn_before_started_never_replays_the_side_effect",
        Duration::from_secs(120),
        || async {
            let nonce = format!("h2-{}", std::process::id());
            let mock = MockServer::start(vec![]).await;
            let env = Environment::new(&mock.base_url());
            let marker = env.home.join("h2-fault.marker");
            let (mut runtime_a, ready_a) =
                env.serve_with_env("h2-a", &fault_env(&marker, "after-spawn"));
            ready(&ready_a, &mut runtime_a).await;
            let client = LocalSocketRuntimeClient::connect(&env.socket())
                .await
                .unwrap();
            let session = create_goal_session(&client, "h2 crash after spawn").await;
            let script = format!(
                "echo SPAWN_NONCE={nonce} >> h2-spawn.log\n\
                 echo $$ > h2-child.pid\n\
                 while [ ! -f h2-release ]; do sleep 0.05; done\n\
                 echo SIDE_EFFECT={nonce} >> h2-side.log\n\
                 exit 5"
            );
            mock.push(spawn_response("h2-spawn", &script)).await;
            mock.push(MockResponse::NeverResponds).await;
            submit(&client, &session, "Start the background command.").await;

            wait_until(|| Ok(marker.exists()), "the host fault marker")
                .await
                .unwrap();
            let host_state = env.host_state();
            let host = host_pid(&host_state);
            wait_until(|| Ok(!alive(host)), "the injected host death")
                .await
                .unwrap();
            wait_until(
                || Ok(env.repo.join("h2-child.pid").exists()),
                "the real child process",
            )
            .await
            .unwrap();
            let child: u32 = std::fs::read_to_string(env.repo.join("h2-child.pid"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            assert!(alive(child), "the OS spawn really happened");
            let intents = read_json(&host_state.join("intents.json"));
            assert_eq!(intents.len(), 1, "{intents:?}");
            let intent_id = intents[0]["id"].as_str().unwrap().to_string();
            assert!(
                find_task(&host_tasks(&host_state), &intent_id).is_none(),
                "the started fact was never durable"
            );
            assert_eq!(
                count_occurrences(
                    &std::fs::read_to_string(env.repo.join("h2-spawn.log")).unwrap(),
                    &nonce
                ),
                1
            );

            let _ = runtime_a.kill();
            let _ = runtime_a.wait();
            let (mut runtime_b, ready_b) = env.serve("h2-b");
            ready(&ready_b, &mut runtime_b).await;
            let _ = LocalSocketRuntimeClient::connect(&env.socket())
                .await
                .unwrap();
            let list: Vec<serde_json::Value> =
                serde_json::from_str(&env.background(&["list", "--json"])).unwrap();
            assert!(
                list.iter()
                    .all(|task| task["task_id"] != intent_id.as_str()),
                "an unproven spawn is not a task: {list:?}"
            );
            assert!(
                find_task(&host_tasks(&host_state), &intent_id).is_none(),
                "recovery must not invent a started fact"
            );
            let unresolved = read_json(&host_state.join("intents.unresolved.json"));
            assert_eq!(unresolved.len(), 1, "{unresolved:?}");
            assert_eq!(unresolved[0]["intent"]["id"], intent_id.as_str());
            assert!(
                alive(child),
                "the orphaned workload is honestly still alive"
            );
            assert_eq!(
                count_occurrences(
                    &std::fs::read_to_string(env.repo.join("h2-spawn.log")).unwrap(),
                    &nonce
                ),
                1,
                "recovery must not spawn a second copy"
            );

            std::fs::write(env.repo.join("h2-release"), b"go").unwrap();
            wait_until(
                || Ok(env.repo.join("h2-side.log").exists()),
                "the workload's single side effect",
            )
            .await
            .unwrap();
            assert_eq!(
                count_occurrences(
                    &std::fs::read_to_string(env.repo.join("h2-side.log")).unwrap(),
                    &nonce
                ),
                1
            );
            wait_until(|| Ok(!alive(child)), "the workload exit")
                .await
                .unwrap();
            assert!(
                find_task(&host_tasks(&host_state), &intent_id).is_none(),
                "no exit fact can be fabricated for an unproven spawn"
            );
            eprintln!(
                "H2 PASS intent={intent_id} host_pid={host} child_pid={child} \
                 spawn_count=1 side_effect_count=1 replayed=false"
            );
            let _ = runtime_b.kill();
            let _ = runtime_b.wait();
            stop_isolated_host(&host_state);
        },
    );
}

// ---------------------------------------------------------------------------
// H3 / B5 — the host itself dies while the child keeps running.
// ---------------------------------------------------------------------------
#[test]
fn h3_host_sigkill_while_running_is_unknown_present_and_never_settles() {
    leveler_test_support::bounded_test(
        "h3_host_sigkill_while_running_is_unknown_present_and_never_settles",
        Duration::from_secs(120),
        || async {
            let nonce = format!("h3-{}", std::process::id());
            let mock = MockServer::start(vec![]).await;
            let env = Environment::new(&mock.base_url());
            let (mut runtime_a, ready_a) = env.serve("h3-a");
            ready(&ready_a, &mut runtime_a).await;
            let client = LocalSocketRuntimeClient::connect(&env.socket())
                .await
                .unwrap();
            let session = create_goal_session(&client, "h3 host crash while running").await;
            let script = format!("echo $$ > h3-child.pid; echo READY_{nonce}; sleep 300");
            mock.push(spawn_response("h3-spawn", &script)).await;
            mock.push(MockResponse::NeverResponds).await;
            submit(&client, &session, "Start the long background command.").await;
            let (task_id, pid) = wait_hosted_task(&client, &session).await;
            let pid = pid.expect("started pid");
            let host_state = env.host_state();
            let host = host_pid(&host_state);
            let started = wait_archived(
                &host_state,
                &task_id,
                |task| task["snapshot"]["status"] == "Running",
                "the durable started fact",
            )
            .await;
            let start_identity = started["snapshot"]["start_identity"]
                .as_str()
                .unwrap()
                .to_string();
            assert_eq!(
                process_identity::verify_process_identity(pid, &start_identity),
                process_identity::ProcessIdentity::Matches
            );
            assert!(alive(host));

            // The supervisor itself dies; the workload is untouched.
            let _ = Command::new("kill")
                .args(["-KILL", &host.to_string()])
                .status();
            wait_until(|| Ok(!alive(host)), "the host to die")
                .await
                .unwrap();
            assert!(alive(pid), "the workload outlives its supervisor");
            let list: Vec<serde_json::Value> =
                serde_json::from_str(&env.background(&["list", "--json"])).unwrap();
            let entry = list
                .iter()
                .find(|task| task["task_id"] == task_id.as_str())
                .unwrap_or_else(|| panic!("the lost task must stay visible: {list:?}"));
            assert_eq!(entry["status"], "unknown", "{entry}");
            let recovered = find_task(&host_tasks(&host_state), &task_id).unwrap();
            assert_eq!(recovered["snapshot"]["status"], "Unknown");
            assert!(recovered["result"].is_null(), "no fabricated completion");
            assert_eq!(recovered["loss"]["previous"], "Running");
            assert_eq!(recovered["loss"]["process"], "Present");

            // A replacement host owns the endpoint but not the workload.
            let config = env.host_config();
            let _replacement = ExecutionHostClient::ensure(config.clone())
                .await
                .expect("a lost host must not block startup");
            let registry = BackgroundTaskRegistry::new().with_execution_host(config);
            let seen = registry
                .get_authorized(&task_id, &session, true)
                .await
                .unwrap();
            assert_eq!(
                seen.status,
                leveler_execution::BackgroundTaskStatus::Unknown
            );
            assert!(
                journal_settlement(&env, &task_id).is_none_or(|value| value.is_null()),
                "an unproven process's namespace must not be settled"
            );
            // The stop path must return the terminal fact, never signal a PID.
            let stopped = registry
                .kill_authorized(&task_id, &session, true)
                .await
                .unwrap();
            assert_eq!(
                stopped.status,
                leveler_execution::BackgroundTaskStatus::Unknown
            );
            assert!(alive(pid), "recovery never signals an uncertain process");
            assert!(
                journal_settlement(&env, &task_id).is_none_or(|value| value.is_null()),
                "a stop read must not settle the unproven namespace"
            );
            // The production session projection is what persists the terminal
            // engine fact, exactly once, for a recovered hosted record.
            let _ = client
                .snapshot(&leveler_client_protocol::SessionId::new(&session))
                .await
                .unwrap();
            wait_finished_events(&env.db(), &session, 1).await;
            let finished = finished_events(&env.db(), &session).await;
            let payload: serde_json::Value =
                serde_json::from_str(finished[0]["payload"].as_str().unwrap_or_default()).unwrap();
            let payload = &payload["payload"];
            assert_eq!(payload["status"], "Unknown", "payload={payload}");
            assert!(
                payload["exit_code"].is_null(),
                "no exit code was observed: {payload}"
            );

            eprintln!(
                "H3 PASS task={task_id} host_pid={host} child_pid={pid} \
                 start_identity={start_identity} status=unknown process=present settlement=pending \
                 no_signal=true"
            );
            stop_workload(pid).await;
            let _ = runtime_a.kill();
            let _ = runtime_a.wait();
            stop_isolated_host(&host_state);
        },
    );
}

// ---------------------------------------------------------------------------
// H4 — the child exited, the host dies before the completion record.
// ---------------------------------------------------------------------------
#[test]
fn h4_host_crash_after_exit_before_result_is_unknown_not_completed() {
    leveler_test_support::bounded_test(
        "h4_host_crash_after_exit_before_result_is_unknown_not_completed",
        Duration::from_secs(120),
        || async {
            let nonce = format!("h4-{}", std::process::id());
            let mock = MockServer::start(vec![]).await;
            let env = Environment::new(&mock.base_url());
            let marker = env.home.join("h4-fault.marker");
            let (mut runtime_a, ready_a) =
                env.serve_with_env("h4-a", &fault_env(&marker, "before-terminal-persist"));
            ready(&ready_a, &mut runtime_a).await;
            let client = LocalSocketRuntimeClient::connect(&env.socket())
                .await
                .unwrap();
            let session = create_goal_session(&client, "h4 crash after exit").await;
            let script = format!(
                "echo STARTED_{nonce}\n\
                 while [ ! -f h4-release ]; do sleep 0.02; done\n\
                 echo DONE_{nonce}\n\
                 exit 3"
            );
            mock.push(spawn_response("h4-spawn", &script)).await;
            mock.push(MockResponse::NeverResponds).await;
            submit(&client, &session, "Run the command.").await;
            let (task_id, _) = wait_hosted_task(&client, &session).await;
            wait_archived(
                &env.host_state(),
                &task_id,
                |task| task["snapshot"]["status"] == "Running",
                "the durable started fact before exit",
            )
            .await;
            std::fs::write(env.repo.join("h4-release"), b"go").unwrap();

            wait_until(|| Ok(marker.exists()), "the host fault marker")
                .await
                .unwrap();
            let host_state = env.host_state();
            let host = host_pid(&host_state);
            wait_until(|| Ok(!alive(host)), "the injected host death")
                .await
                .unwrap();
            let tasks = host_tasks(&host_state);
            assert_eq!(tasks.len(), 1, "{tasks:?}");
            let task = &tasks[0];
            assert_eq!(task["snapshot"]["id"], task_id.as_str());
            assert_eq!(task["snapshot"]["status"], "Running");
            assert!(
                task["result"].is_null(),
                "the completion record was never durable: {task}"
            );

            let _ = runtime_a.kill();
            let _ = runtime_a.wait();
            let (mut runtime_b, ready_b) = env.serve("h4-b");
            ready(&ready_b, &mut runtime_b).await;
            let list: Vec<serde_json::Value> =
                serde_json::from_str(&env.background(&["list", "--json"])).unwrap();
            let entry = list
                .iter()
                .find(|task| task["task_id"] == task_id.as_str())
                .unwrap_or_else(|| panic!("{list:?}"));
            assert_eq!(entry["status"], "unknown", "{entry}");
            assert!(entry["exit_code"].is_null(), "{entry}");
            let recovered = find_task(&host_tasks(&host_state), &task_id).unwrap();
            assert_eq!(recovered["snapshot"]["status"], "Unknown");
            assert!(
                recovered["result"].is_null(),
                "no completion may be synthesized: {recovered}"
            );
            let client_b = LocalSocketRuntimeClient::connect(&env.socket())
                .await
                .unwrap();
            let _ = client_b
                .snapshot(&leveler_client_protocol::SessionId::new(&session))
                .await
                .unwrap();
            wait_finished_events(&env.db(), &session, 1).await;
            assert_eq!(recovered["loss"]["previous"], "Running");
            assert_eq!(recovered["loss"]["process"], "Gone");
            assert!(
                recovered["snapshot"]["exit_code"].is_null(),
                "an uncommitted exit code must not survive as Unknown: {recovered}"
            );
            assert!(recovered["snapshot"]["signal"].is_null(), "{recovered}");

            let finished = finished_events(&env.db(), &session).await;
            assert_eq!(finished.len(), 1, "{finished:?}");
            let payload: serde_json::Value =
                serde_json::from_str(finished[0]["payload"].as_str().unwrap()).unwrap();
            let payload = &payload["payload"];
            assert_eq!(payload["status"], "Unknown", "payload={payload}");
            assert!(payload["exit_code"].is_null(), "payload={payload}");
            assert_eq!(payload["is_error"], true, "payload={payload}");
            eprintln!(
                "H4 PASS task={task_id} host_pid={host} status=unknown result=none \
                 observed_exit_code=none loss=gone"
            );
            let _ = runtime_b.kill();
            let _ = runtime_b.wait();
            stop_isolated_host(&host_state);
        },
    );
}

// ---------------------------------------------------------------------------
// H5 / B1 / B6 / B11 — result durable, host dies, repeated recovery is stable.
// ---------------------------------------------------------------------------
#[test]
fn h5_result_durable_before_host_death_survives_repeated_recovery() {
    leveler_test_support::bounded_test(
        "h5_result_durable_before_host_death_survives_repeated_recovery",
        Duration::from_secs(150),
        || async {
            let nonce = format!("h5-{}", std::process::id());
            let mock = MockServer::start(vec![]).await;
            let env = Environment::new(&mock.base_url());
            let (mut runtime_a, ready_a) = env.serve("h5-a");
            ready(&ready_a, &mut runtime_a).await;
            let client = LocalSocketRuntimeClient::connect(&env.socket())
                .await
                .unwrap();
            let session = create_goal_session(&client, "h5 durable result").await;
            let script = format!(
                "echo STARTED_{nonce}\n\
                 while [ ! -f h5-release ]; do sleep 0.02; done\n\
                 echo SIDE_{nonce} >> h5-side.log\n\
                 echo DONE_{nonce}\n\
                 exit 7"
            );
            mock.push(spawn_response("h5-spawn", &script)).await;
            mock.push(MockResponse::NeverResponds).await;
            submit(&client, &session, "Run the command.").await;

            let (task_id, pid) = wait_hosted_task(&client, &session).await;
            let pid = pid.expect("pid");
            std::fs::write(env.repo.join("h5-release"), b"go").unwrap();
            let host_state = env.host_state();
            let host = host_pid(&host_state);
            let finished = wait_archived(
                &host_state,
                &task_id,
                |task| task["snapshot"]["status"] == "Exited" && task["result"].is_object(),
                "the durable completion record",
            )
            .await;
            let result_id = finished["result"]["result_id"]
                .as_str()
                .unwrap()
                .to_string();
            assert_eq!(finished["result"]["exit_code"], 7);
            assert_eq!(finished["snapshot"]["signal"], serde_json::Value::Null);
            let log = finished["snapshot"]["log"].as_str().unwrap();
            assert!(log.contains("STARTED"), "{log}");
            assert!(log.contains("DONE"), "{log}");
            assert_eq!(finished["result"]["output_bytes"], log.len());

            // The supervisor dies only after the result was durable.
            let _ = Command::new("kill")
                .args(["-KILL", &host.to_string()])
                .status();
            wait_until(|| Ok(!alive(host)), "the host to die")
                .await
                .unwrap();

            let _ = runtime_a.kill();
            let _ = runtime_a.wait();
            let (mut runtime_b, ready_b) = env.serve("h5-b");
            ready(&ready_b, &mut runtime_b).await;
            let client_b = LocalSocketRuntimeClient::connect(&env.socket())
                .await
                .unwrap();
            let snapshot = client_b
                .snapshot(&leveler_client_protocol::SessionId::new(&session))
                .await
                .unwrap();
            let record = snapshot
                .background_tasks
                .iter()
                .find(|record| record.task_id == task_id)
                .unwrap_or_else(|| panic!("no recovered record: {snapshot:?}"));
            assert_eq!(record.exit_code, Some(7));
            let recovered = find_task(&host_tasks(&host_state), &task_id).unwrap();
            assert_eq!(recovered["result"]["result_id"], result_id.as_str());
            assert!(
                recovered["loss"].is_null(),
                "a proven exit needs no loss record"
            );

            // A second runtime crash/recovery cycle must not duplicate anything.
            let _ = runtime_b.kill();
            let _ = runtime_b.wait();
            let (mut runtime_c, ready_c) = env.serve("h5-c");
            ready(&ready_c, &mut runtime_c).await;
            let _ = LocalSocketRuntimeClient::connect(&env.socket())
                .await
                .unwrap();
            let tasks = host_tasks(&host_state);
            let records: Vec<_> = tasks
                .iter()
                .filter(|task| task["snapshot"]["id"] == task_id.as_str())
                .collect();
            assert_eq!(records.len(), 1, "one task identity: {tasks:?}");
            assert_eq!(records[0]["result"]["result_id"], result_id.as_str());
            let finished = finished_events(&env.db(), &session).await;
            assert_eq!(finished.len(), 1, "one terminal fact: {finished:?}");
            assert_eq!(
                count_occurrences(
                    &std::fs::read_to_string(env.repo.join("h5-side.log")).unwrap(),
                    &nonce
                ),
                1
            );
            eprintln!(
                "H5 PASS task={task_id} host_pid={host} background_pid={pid} result_id={result_id} \
                 exit_code=7 repeated_recovery=stable terminal_events=1"
            );
            let _ = runtime_c.kill();
            let _ = runtime_c.wait();
            stop_isolated_host(&host_state);
        },
    );
}

// ---------------------------------------------------------------------------
// B4 / B5 / B6 / B14 — two real runtime SIGKILLs, one execution instance.
// ---------------------------------------------------------------------------
#[test]
fn b14_two_runtime_sigkills_observe_the_same_running_instance() {
    leveler_test_support::bounded_test(
        "b14_two_runtime_sigkills_observe_the_same_running_instance",
        Duration::from_secs(180),
        || async {
            let nonce = format!("b14-{}", std::process::id());
            let mock = MockServer::start(vec![]).await;
            let env = Environment::new(&mock.base_url());
            let (mut runtime_a, ready_a) = env.serve("b14-a");
            ready(&ready_a, &mut runtime_a).await;
            let client_a = LocalSocketRuntimeClient::connect(&env.socket())
                .await
                .unwrap();
            let session = create_goal_session(&client_a, "b14 double runtime crash").await;
            let script = format!(
                "echo SPAWN_NONCE={nonce} >> b14-spawn.log\n\
                 echo $$ > b14-child.pid\n\
                 while [ ! -f b14-release ]; do echo HEARTBEAT={nonce} >> b14-heartbeat.log; echo HEARTBEAT; sleep 0.05; done\n\
                 echo SIDE_EFFECT={nonce} >> b14-side.log\n\
                 echo COMPLETED\n\
                 exit 0"
            );
            mock.push(spawn_response("b14-spawn", &script)).await;
            mock.push(MockResponse::NeverResponds).await;
            submit(&client_a, &session, "Start the long background command.").await;

            let (task_id, pid) = wait_hosted_task(&client_a, &session).await;
            let pid = pid.expect("pid");
            let host_state = env.host_state();
            let host = host_pid(&host_state);
            let started = wait_archived(
                &host_state,
                &task_id,
                |task| task["snapshot"]["status"] == "Running",
                "the started fact",
            )
            .await;
            let start_identity = started["snapshot"]["start_identity"]
                .as_str()
                .unwrap()
                .to_string();

            // First real runtime SIGKILL.
            runtime_a.kill().unwrap();
            assert_eq!(runtime_a.wait().unwrap().signal(), Some(9));
            assert!(alive(host) && alive(pid));
            let (mut runtime_b, ready_b) = env.serve("b14-b");
            ready(&ready_b, &mut runtime_b).await;
            let client_b = LocalSocketRuntimeClient::connect(&env.socket())
                .await
                .unwrap();
            let snapshot = client_b
                .snapshot(&leveler_client_protocol::SessionId::new(&session))
                .await
                .unwrap();
            let recovered = snapshot
                .active_background_tasks
                .iter()
                .find(|task| task.task_id == task_id)
                .unwrap_or_else(|| panic!("{snapshot:?}"));
            assert_eq!(recovered.pid, Some(pid));

            // Second real runtime SIGKILL while the SAME task still runs.
            runtime_b.kill().unwrap();
            assert_eq!(runtime_b.wait().unwrap().signal(), Some(9));
            assert!(alive(host) && alive(pid));
            assert_eq!(
                process_identity::verify_process_identity(pid, &start_identity),
                process_identity::ProcessIdentity::Matches
            );
            let (mut runtime_c, ready_c) = env.serve("b14-c");
            ready(&ready_c, &mut runtime_c).await;
            let client_c = LocalSocketRuntimeClient::connect(&env.socket())
                .await
                .unwrap();
            let snapshot = client_c
                .snapshot(&leveler_client_protocol::SessionId::new(&session))
                .await
                .unwrap();
            let recovered = snapshot
                .active_background_tasks
                .iter()
                .find(|task| task.task_id == task_id)
                .unwrap_or_else(|| panic!("{snapshot:?}"));
            assert_eq!(recovered.pid, Some(pid), "same execution instance");
            let logs = env.background(&["logs", &task_id]);
            assert!(logs.contains("HEARTBEAT"), "{logs}");

            // The task still exits once, with one side effect.
            std::fs::write(env.repo.join("b14-release"), b"go").unwrap();
            let finished = wait_archived(
                &host_state,
                &task_id,
                |task| task["snapshot"]["status"] == "Exited" && task["result"].is_object(),
                "the completion record",
            )
            .await;
            assert_eq!(finished["snapshot"]["exit_code"], 0);
            assert_eq!(
                count_occurrences(
                    &std::fs::read_to_string(env.repo.join("b14-spawn.log")).unwrap(),
                    &nonce
                ),
                1,
                "no respawn across two runtime crashes"
            );
            assert_eq!(
                count_occurrences(
                    &std::fs::read_to_string(env.repo.join("b14-side.log")).unwrap(),
                    &nonce
                ),
                1
            );
            assert_eq!(
                host_tasks(&host_state)
                    .iter()
                    .filter(|task| task["snapshot"]["id"] == task_id.as_str())
                    .count(),
                1
            );
            eprintln!(
                "B14 PASS task={task_id} host_pid={host} background_pid={pid} \
                 start_identity={start_identity} runtime_sigkills=2 spawn_count=1 side_effect_count=1"
            );
            let _ = runtime_c.kill();
            let _ = runtime_c.wait();
            stop_isolated_host(&host_state);
        },
    );
}

// ---------------------------------------------------------------------------
// B7 / B8 — StopRequested is not StopConfirmed; the real signal is the fact.
// ---------------------------------------------------------------------------
#[test]
fn b7_b8_stop_request_is_unconfirmed_until_the_host_observes_the_signal() {
    leveler_test_support::bounded_test(
        "b7_b8_stop_request_is_unconfirmed_until_the_host_observes_the_signal",
        Duration::from_secs(120),
        || async {
            let nonce = format!("b7-{}", std::process::id());
            let mock = MockServer::start(vec![]).await;
            let env = Environment::new(&mock.base_url());
            let (mut runtime, ready_file) = env.serve("b7-runtime");
            ready(&ready_file, &mut runtime).await;
            let client = LocalSocketRuntimeClient::connect(&env.socket())
                .await
                .unwrap();
            let session = create_goal_session(&client, "b7 stop semantics").await;
            let script = format!(
                "trap '' TERM\n\
                 echo $$ > b7-child.pid\n\
                 echo READY_{nonce}\n\
                 while true; do sleep 0.05; done"
            );
            mock.push(spawn_response("b7-spawn", &script)).await;
            submit(&client, &session, "Start the long background command.").await;
            let (task_id, pid) = wait_hosted_task(&client, &session).await;
            let pid = pid.expect("pid");
            let host_state = env.host_state();
            let host = host_pid(&host_state);
            wait_archived(
                &host_state,
                &task_id,
                |task| task["snapshot"]["status"] == "Running",
                "the started fact",
            )
            .await;
            assert!(alive(pid));

            // The model now asks the production stop tool.
            mock.push(tool_response(
                "b7-stop",
                "kill_task",
                serde_json::json!({"task_id": task_id}),
            ))
            .await;
            mock.push(final_response("The stop request was issued."))
                .await;
            let mut events =
                client.subscribe_session(&leveler_client_protocol::SessionId::new(&session));
            let deadline = Instant::now() + Duration::from_secs(60);
            let preview = loop {
                assert!(Instant::now() < deadline, "kill_task never completed");
                let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
                    .await
                    .expect("events")
                    .expect("open");
                if let RuntimeEvent::ToolCallCompleted { id, preview, .. } = event
                    && id.to_string() == "b7-stop"
                {
                    break preview;
                }
            };
            assert!(
                preview.contains("Stop REQUESTED"),
                "the stop request must be distinguishable from confirmation: {preview}"
            );
            assert!(
                preview.contains("NOT confirmed"),
                "the tool must not claim a confirmed stop before the terminal fact: {preview}"
            );

            // The host, not the tool response, owns the real terminal fact.
            let finished = wait_archived(
                &host_state,
                &task_id,
                |task| task["snapshot"]["status"] == "Killed" && task["result"].is_object(),
                "the host-observed signal termination",
            )
            .await;
            assert_eq!(finished["snapshot"]["signal"], 9, "{finished}");
            assert!(finished["snapshot"]["exit_code"].is_null(), "{finished}");
            assert_eq!(finished["result"]["status"], "Killed");
            assert_eq!(finished["result"]["signal"], 9);
            let deadline = Instant::now() + Duration::from_secs(20);
            while alive(pid) {
                assert!(Instant::now() < deadline, "the real process must be gone");
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            let list: Vec<serde_json::Value> =
                serde_json::from_str(&env.background(&["list", "--json"])).unwrap();
            let entry = list
                .iter()
                .find(|task| task["task_id"] == task_id.as_str())
                .unwrap();
            assert_eq!(entry["status"], "killed", "{entry}");
            assert_eq!(entry["exit_code"], serde_json::Value::Null);
            eprintln!(
                "B7_B8 PASS task={task_id} host_pid={host} background_pid={pid} \
                 stop_requested=unconfirmed terminal=killed signal=9 exit_code=none"
            );
            let _ = runtime.kill();
            let _ = runtime.wait();
            stop_isolated_host(&host_state);
        },
    );
}

// ---------------------------------------------------------------------------
// B9 — a mismatched start token never authorizes a signal to the live pid.
// ---------------------------------------------------------------------------
#[test]
fn b9_start_identity_mismatch_never_signals_the_unrelated_live_process() {
    leveler_test_support::bounded_test(
        "b9_start_identity_mismatch_never_signals_the_unrelated_live_process",
        Duration::from_secs(120),
        || async {
            let nonce = format!("b9-{}", std::process::id());
            let mock = MockServer::start(vec![]).await;
            let env = Environment::new(&mock.base_url());
            let (mut runtime, ready_file) = env.serve("b9-runtime");
            ready(&ready_file, &mut runtime).await;
            let client = LocalSocketRuntimeClient::connect(&env.socket())
                .await
                .unwrap();
            let session = create_goal_session(&client, "b9 identity mismatch").await;
            let script = format!("echo $$ > b9-child.pid; echo READY_{nonce}; sleep 300");
            mock.push(spawn_response("b9-spawn", &script)).await;
            mock.push(MockResponse::NeverResponds).await;
            submit(&client, &session, "Start the long background command.").await;
            let (task_id, pid) = wait_hosted_task(&client, &session).await;
            let pid = pid.expect("pid");
            let host_state = env.host_state();
            let host = host_pid(&host_state);
            wait_archived(
                &host_state,
                &task_id,
                |task| task["snapshot"]["status"] == "Running",
                "the started fact",
            )
            .await;

            // Inject a different start token for the same pid: this is the
            // durable shape a recycled pid leaves behind.
            let mut tasks = host_tasks(&host_state);
            for task in &mut tasks {
                if task["snapshot"]["id"] == task_id.as_str() {
                    task["snapshot"]["start_identity"] = serde_json::json!("macos:1:1");
                }
            }
            std::fs::write(
                host_state.join("tasks.json"),
                serde_json::to_vec(&tasks).unwrap(),
            )
            .unwrap();
            let _ = Command::new("kill")
                .args(["-KILL", &host.to_string()])
                .status();
            wait_until(|| Ok(!alive(host)), "the host to die")
                .await
                .unwrap();
            assert!(alive(pid));

            let list: Vec<serde_json::Value> =
                serde_json::from_str(&env.background(&["list", "--json"])).unwrap();
            let entry = list
                .iter()
                .find(|task| task["task_id"] == task_id.as_str())
                .unwrap();
            assert_eq!(entry["status"], "unknown", "{entry}");
            let recovered = find_task(&host_tasks(&host_state), &task_id).unwrap();
            assert_eq!(
                recovered["loss"]["process"], "Present",
                "a mismatched token is conservative, not a licence to kill: {recovered}"
            );

            let config = env.host_config();
            let _replacement = ExecutionHostClient::ensure(config.clone()).await.unwrap();
            let registry = BackgroundTaskRegistry::new().with_execution_host(config);
            let stopped = registry
                .kill_authorized(&task_id, &session, true)
                .await
                .unwrap();
            assert_eq!(
                stopped.status,
                leveler_execution::BackgroundTaskStatus::Unknown
            );
            assert!(
                alive(pid),
                "an identity mismatch must never signal the live pid"
            );
            assert!(
                journal_settlement(&env, &task_id).is_none_or(|value| value.is_null()),
                "the uncertain workload keeps its write reservation"
            );
            eprintln!(
                "B9 PASS task={task_id} host_pid={host} live_pid={pid} injected_token=macos:1:1 \
                 status=unknown process=present signalled=false"
            );
            stop_workload(pid).await;
            let _ = runtime.kill();
            let _ = runtime.wait();
            stop_isolated_host(&host_state);
        },
    );
}

// ---------------------------------------------------------------------------
// B10 / B13 — two sessions, isolated ownership and cleanup boundaries.
// ---------------------------------------------------------------------------
#[test]
fn b10_b13_two_sessions_keep_background_ownership_and_cleanup_isolated() {
    leveler_test_support::bounded_test(
        "b10_b13_two_sessions_keep_background_ownership_and_cleanup_isolated",
        Duration::from_secs(180),
        || async {
            let mock = MockServer::start(vec![]).await;
            let env = Environment::new(&mock.base_url());
            let (mut runtime, ready_file) = env.serve("b10-runtime");
            ready(&ready_file, &mut runtime).await;
            let client = LocalSocketRuntimeClient::connect(&env.socket())
                .await
                .unwrap();

            let session_a = create_goal_session(&client, "b10 session A").await;
            let script_a = "echo $$ > b10-a.pid; echo READY_A; sleep 300";
            mock.push(spawn_response("b10-spawn-a", script_a)).await;
            mock.push(MockResponse::NeverResponds).await;
            submit(&client, &session_a, "Start service A.").await;
            let (task_a, pid_a) = wait_hosted_task_with(&client, &session_a, Some(&env)).await;
            let pid_a = pid_a.expect("pid a");

            // A's continuation round consumes its own held response before B
            // starts. Wait for that request, then drop the held entry: the
            // mock repeats its last response, so it would otherwise be the
            // first thing B's first request received.
            let deadline = Instant::now() + Duration::from_secs(10);
            while mock.request_count() < 2 {
                assert!(
                    Instant::now() < deadline,
                    "A's continuation round never reached the model"
                );
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            mock.clear().await;

            let session_b = create_goal_session(&client, "b10 session B").await;
            let script_b = "echo $$ > b10-b.pid; echo READY_B; sleep 300";
            mock.push(spawn_response("b10-spawn-b", script_b)).await;
            mock.push(MockResponse::NeverResponds).await;
            submit(&client, &session_b, "Start service B.").await;
            let (task_b, pid_b) = wait_hosted_task_with(&client, &session_b, Some(&env)).await;
            let pid_b = pid_b.expect("pid b");
            assert_ne!(task_a, task_b);
            assert!(alive(pid_a) && alive(pid_b));
            let host_state = env.host_state();
            let tasks = host_tasks(&host_state);
            let owner_a = find_task(&tasks, &task_a).unwrap()["snapshot"]["owner_scope"]
                .as_str()
                .unwrap()
                .to_string();
            let owner_b = find_task(&tasks, &task_b).unwrap()["snapshot"]["owner_scope"]
                .as_str()
                .unwrap()
                .to_string();
            assert_eq!(owner_a, session_a);
            assert_eq!(owner_b, session_b);
            assert_ne!(owner_a, owner_b);

            // Cancelling A's goal reaps A's service only.
            client
                .send(ClientCommand::CancelTask {
                    session_id: leveler_client_protocol::SessionId::new(&session_a),
                })
                .await
                .unwrap();
            wait_until(|| Ok(!alive(pid_a)), "A's cancelled service")
                .await
                .unwrap();
            assert!(alive(pid_b), "B's service must not inherit A's cleanup");
            let list: Vec<serde_json::Value> =
                serde_json::from_str(&env.background(&["list", "--json"])).unwrap();
            assert_eq!(
                list.iter()
                    .find(|task| task["task_id"] == task_b.as_str())
                    .unwrap()["status"],
                "running"
            );

            // Cancelling B is the only thing that stops B.
            client
                .send(ClientCommand::CancelTask {
                    session_id: leveler_client_protocol::SessionId::new(&session_b),
                })
                .await
                .unwrap();
            wait_until(|| Ok(!alive(pid_b)), "B's cancelled service")
                .await
                .unwrap();
            // The durable record catches up at a snapshot boundary: a client
            // that reconnects reads it there. Ask once per session, as a
            // reconnecting client would, before reading the log.
            let _ = client
                .snapshot(&leveler_client_protocol::SessionId::new(&session_a))
                .await
                .unwrap();
            let _ = client
                .snapshot(&leveler_client_protocol::SessionId::new(&session_b))
                .await
                .unwrap();
            // Each session's terminal fact stays in its own transcript.
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                let a = finished_events(&env.db(), &session_a).await;
                let b = finished_events(&env.db(), &session_b).await;
                if !a.is_empty() && !b.is_empty() {
                    let a_payload: serde_json::Value =
                        serde_json::from_str(a[0]["payload"].as_str().unwrap()).unwrap();
                    let b_payload: serde_json::Value =
                        serde_json::from_str(b[0]["payload"].as_str().unwrap()).unwrap();
                    assert_eq!(a_payload["payload"]["task_id"], task_a.as_str());
                    assert_eq!(b_payload["payload"]["task_id"], task_b.as_str());
                    break;
                }
                assert!(Instant::now() < deadline, "terminal facts never landed");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            eprintln!(
                "B10_B13 PASS task_a={task_a} pid_a={pid_a} owner_a={owner_a} \
                 task_b={task_b} pid_b={pid_b} owner_b={owner_b} cross_reap=false"
            );
            let _ = runtime.kill();
            let _ = runtime.wait();
            stop_isolated_host(&host_state);
        },
    );
}

// ---------------------------------------------------------------------------
// Perf: one production spawn, result commit, boot recovery and query cost.
// ---------------------------------------------------------------------------
#[test]
fn perf_spawn_result_recovery_and_query_cost() {
    leveler_test_support::bounded_test(
        "perf_spawn_result_recovery_and_query_cost",
        Duration::from_secs(150),
        || async {
            let nonce = format!("perf-{}", std::process::id());
            let mock = MockServer::start(vec![]).await;
            let env = Environment::new(&mock.base_url());
            let (mut runtime_a, ready_a) = env.serve("perf-a");
            ready(&ready_a, &mut runtime_a).await;
            let client = LocalSocketRuntimeClient::connect(&env.socket())
                .await
                .unwrap();
            let session = create_goal_session(&client, "perf").await;
            let script =
                "echo STARTED\nwhile [ ! -f perf-release ]; do sleep 0.01; done\necho DONE\nexit 0"
                    .to_string();
            mock.push(spawn_response("perf-spawn", &script)).await;
            mock.push(MockResponse::NeverResponds).await;
            let spawn_started = Instant::now();
            submit(&client, &session, "Start the measured command.").await;
            let (task_id, _) = wait_hosted_task(&client, &session).await;
            let spawn_ms = spawn_started.elapsed().as_millis();

            let host_state = env.host_state();
            wait_archived(
                &host_state,
                &task_id,
                |task| task["snapshot"]["status"] == "Running",
                "the started fact",
            )
            .await;
            let result_started = Instant::now();
            std::fs::write(env.repo.join("perf-release"), b"go").unwrap();
            wait_archived(
                &host_state,
                &task_id,
                |task| task["snapshot"]["status"] == "Exited" && task["result"].is_object(),
                "the completion record",
            )
            .await;
            let result_ms = result_started.elapsed().as_millis();

            runtime_a.kill().unwrap();
            assert_eq!(runtime_a.wait().unwrap().signal(), Some(9));
            let recovery_started = Instant::now();
            let (mut runtime_b, ready_b) = env.serve("perf-b");
            ready(&ready_b, &mut runtime_b).await;
            let client_b = LocalSocketRuntimeClient::connect(&env.socket())
                .await
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                let snapshot = client_b
                    .snapshot(&leveler_client_protocol::SessionId::new(&session))
                    .await
                    .unwrap();
                if snapshot
                    .background_tasks
                    .iter()
                    .any(|task| task.task_id == task_id)
                {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "recovery never projected the task"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let recovery_ms = recovery_started.elapsed().as_millis();

            let query_started = Instant::now();
            for _ in 0..5 {
                let list: Vec<serde_json::Value> =
                    serde_json::from_str(&env.background(&["list", "--json"])).unwrap();
                assert!(list.iter().any(|task| task["task_id"] == task_id.as_str()));
            }
            let query_ms = query_started.elapsed().as_millis() / 5;
            eprintln!(
                "PERF spawn_ms={spawn_ms} result_commit_ms={result_ms} boot_recovery_ms={recovery_ms} \
                 query_ms={query_ms} task={task_id} nonce={nonce}"
            );
            let _ = runtime_b.kill();
            let _ = runtime_b.wait();
            stop_isolated_host(&host_state);
        },
    );
}

// ---------------------------------------------------------------------------
// Degradation: a host that cannot start must run locally, visibly, and must
// never be labelled as a durable (`host-`) task.
// ---------------------------------------------------------------------------
#[test]
fn degraded_host_unavailable_runs_local_and_is_never_labelled_durable() {
    leveler_test_support::bounded_test(
        "degraded_host_unavailable_runs_local_and_is_never_labelled_durable",
        Duration::from_secs(120),
        || async {
            let nonce = format!("local-{}", std::process::id());
            let mock = MockServer::start(vec![]).await;
            let env = Environment::new(&mock.base_url());
            let (mut runtime, ready_file) = env.serve("degraded-runtime");
            ready(&ready_file, &mut runtime).await;
            let client = LocalSocketRuntimeClient::connect(&env.socket())
                .await
                .unwrap();
            let session = create_goal_session(&client, "degraded execution host").await;
            // A host state directory that is not owner-only is refused by the
            // production `secure_dir` guard, so `ensure` cannot start a host.
            let host_state = env.host_state();
            std::fs::create_dir_all(&host_state).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&host_state, std::fs::Permissions::from_mode(0o755))
                    .unwrap();
            }
            let script = format!(
                // Keep the local run alive when the runtime is killed: the
                // point of the case is a task whose completion the degraded
                // runtime cannot vouch for. Without the sleep the command can
                // finish first, and a platform that commits the completion
                // before the kill would turn this race into a false failure.
                "echo LOCAL_RAN_{nonce}; echo LOCAL_SIDE_{nonce} >> local-side.log; sleep 30"
            );
            mock.push(spawn_response("local-spawn", &script)).await;
            mock.push(final_response("The local fallback command completed."))
                .await;
            submit(&client, &session, "Start the command.").await;

            let deadline = Instant::now() + Duration::from_secs(40);
            let task_id = loop {
                let snapshot = client
                    .snapshot(&leveler_client_protocol::SessionId::new(&session))
                    .await
                    .unwrap();
                if let Some(task) = snapshot
                    .active_background_tasks
                    .iter()
                    .find(|task| task.task_id.starts_with("bg-"))
                {
                    break task.task_id.clone();
                }
                if let Some(task) = snapshot
                    .background_tasks
                    .iter()
                    .find(|task| task.task_id.starts_with("bg-"))
                {
                    break task.task_id.clone();
                }
                assert!(
                    Instant::now() < deadline,
                    "the degraded local task never appeared: {snapshot:?}"
                );
                tokio::time::sleep(Duration::from_millis(25)).await;
            };
            assert!(
                !task_id.starts_with("host-"),
                "a non-durable task must never wear a hosted id: {task_id}"
            );
            assert!(
                !env.state_dir().join("execution-host/tasks.json").exists()
                    || host_tasks(&host_state).is_empty(),
                "a degraded task must not be recorded as host-backed"
            );
            let deadline = Instant::now() + Duration::from_secs(20);
            while !env.repo.join("local-side.log").exists() {
                assert!(Instant::now() < deadline, "the local command never ran");
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            assert_eq!(
                count_occurrences(
                    &std::fs::read_to_string(env.repo.join("local-side.log")).unwrap(),
                    &nonce
                ),
                1
            );
            let stderr = std::fs::read_to_string(env.home.join("degraded-runtime-stderr.txt"))
                .unwrap_or_default();
            assert!(
                stderr.contains("background supervisor unavailable")
                    || stderr.contains("without durable recovery"),
                "the degradation must be visible, not silent: {stderr}"
            );
            // A restart must not retroactively promote the local task.
            let _ = runtime.kill();
            let _ = runtime.wait();
            let (mut runtime_b, ready_b) = env.serve("degraded-runtime-b");
            ready(&ready_b, &mut runtime_b).await;
            let client_b = LocalSocketRuntimeClient::connect(&env.socket())
                .await
                .unwrap();
            let snapshot = client_b
                .snapshot(&leveler_client_protocol::SessionId::new(&session))
                .await
                .unwrap();
            assert!(
                snapshot
                    .background_tasks
                    .iter()
                    .all(|task| !task.task_id.starts_with("host-")),
                "the restart must not invent a host-backed record: {snapshot:?}"
            );
            if let Some(record) = snapshot
                .background_tasks
                .iter()
                .find(|task| task.task_id == task_id)
            {
                assert!(
                    record.exit_code.is_none() && record.output.is_empty(),
                    "a lost local task must not be presented as a durable completion: {record:?}"
                );
                assert!(
                    format!("{:?}", record.state).contains("Lost"),
                    "a local task that cannot be recovered must be Lost: {record:?}"
                );
            }
            eprintln!(
                "DEGRADED PASS task={task_id} local=true durable=false \
                 warning_visible=true nonce={nonce}"
            );
            let _ = runtime_b.kill();
            let _ = runtime_b.wait();
        },
    );
}
