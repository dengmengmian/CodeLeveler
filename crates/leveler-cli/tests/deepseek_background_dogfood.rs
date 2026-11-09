//! REAL DeepSeek dogfood for background execution and runtime-crash recovery.
//!
//! Two questions, answered only from real processes and real model output:
//!
//! * **D-A** — can the real model autonomously start a controlled background
//!   command through the production tool surface, wait on the real Execution
//!   Host result, and report the actual exit code (7) and real output?
//! * **D-B** — with that real background process running, if the runtime is
//!   really SIGKILLed, does the Execution Host keep supervising the SAME
//!   process, and does a NEW real runtime on the same isolated state let the
//!   same session's model observe the ORIGINAL task id (same pid, same start
//!   identity, one spawn, one side effect) and report its real result?
//! * **D-C** — can the real model enable `external_tools`, call a real stdio
//!   MCP tool whose transport drops after one side effect, receive an Unknown
//!   result (not success/failure), inspect the server's own files, and report
//!   the side effect plus the uncertainty WITHOUT retrying the call?
//! * **D-D** — after the real model's `kill_task` returns
//!   "Stop REQUESTED / NOT confirmed", if the runtime is really SIGKILLed,
//!   does a NEW runtime on the same isolated state let the same session's
//!   model observe the ORIGINAL task id's real terminal status and distinguish
//!   the requested state from the confirmed/unknown one?
//!
//! Nothing in the model path is mocked: the provider is the configured
//! DeepSeek gateway, the runtime is the real `leveler serve` binary, the
//! supervisor is the real `leveler execution-host`, and the workload is a real
//! `/bin/sh` process. The harness supplies only the tool surface and
//! observation opportunities; it never submits tool calls for the model.
//!
//! Ignored by default because it spends real tokens and real processes:
//!
//! ```text
//! set -a; . ~/.leveler/deepseek.env; set +a
//! R3_EVIDENCE_DIR=<abs>/evidence/deepseek \
//!   cargo test -p leveler-cli --test deepseek_background_dogfood \
//!   -- --ignored --nocapture --test-threads=1
//! ```
#![cfg(unix)]

use std::collections::BTreeSet;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use leveler_client_protocol::{ClientCommand, InteractiveRuntimeClient, RuntimeEvent};
use leveler_local_transport::{CollaborationMode, CreateSessionRequest, LocalSocketRuntimeClient};
use leveler_storage::{Database, EventStore as _};
use leveler_test_support::ManagedChild;

const PROVIDER_BASE_URL: &str = "https://taotoken.net/api/v1";
const MODEL_REF: &str = "deepseek/deepseek-v4.1-flash-1017";
const JOB_SCRIPT: &str = "r3-job.sh";
const RELEASE_FILE: &str = "r3-release";
const SPAWN_LOG: &str = "r3-spawn.log";
const HEARTBEAT_LOG: &str = "r3-heartbeat.log";
const SIDE_EFFECT_LOG: &str = "r3-side-effect.log";

struct Environment {
    _temp: tempfile::TempDir,
    home: PathBuf,
    config: PathBuf,
    repo: PathBuf,
}

impl Environment {
    fn new(nonce: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let home = root.join("home");
        let config = root.join("config");
        let repo = root.join("repo");
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
        // The model decides WHEN to run this script and in WHAT tool. The
        // script itself is the controlled workload the operator prepared.
        std::fs::write(repo.join(JOB_SCRIPT), job_script(nonce)).unwrap();
        std::fs::write(
            home.join("config.toml"),
            format!(
                r#"
default_model = "{MODEL_REF}"
lang = "en"

[update]
auto_update = false

[providers.deepseek]
protocol = "openai_chat"
base_url = "{PROVIDER_BASE_URL}"
api_key_env = "DEEPSEEK_API_KEY"

[models."deepseek-v4.1-flash-1017"]
provider = "deepseek"
model_id = "deepseek-v4.1-flash-1017"
protocol = "openai_chat"
context_window = 1048576
reliable_context = 786432
max_output_tokens = 65536
reasoning = true
reasoning_style = "thinking_flag"
supported_efforts = ["low", "high", "max"]
reasoning_effort = "low"
thinking_supports_forced_tool_choice = false
passback_reasoning_content = true
"#
            ),
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
            .env("HOME", &self.home)
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

    fn host_state(&self) -> PathBuf {
        self.state_dir().join("execution-host")
    }

    fn db(&self) -> PathBuf {
        self.state_dir().join("sessions.db")
    }

    fn repo_path(&self, name: &str) -> PathBuf {
        self.repo.join(name)
    }

    fn read_repo(&self, name: &str) -> String {
        std::fs::read_to_string(self.repo_path(name)).unwrap_or_default()
    }

    /// Append an isolated-config block (for example a `[[mcp_servers]]` entry)
    /// before the runtime starts. The base config is already the real provider.
    fn append_config(&self, block: &str) {
        let path = self.home.join("config.toml");
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str(block);
        std::fs::write(&path, text).unwrap();
    }
}

/// Prints STARTED, records a unique spawn nonce, heartbeats until the release
/// file exists, records exactly one side effect, prints COMPLETED, exits 7.
fn job_script(nonce: &str) -> String {
    format!(
        r#"#!/bin/sh
echo STARTED
echo "SPAWN_NONCE={nonce}" >> {SPAWN_LOG}
while [ ! -f {RELEASE_FILE} ]; do
  echo "HEARTBEAT={nonce}" >> {HEARTBEAT_LOG}
  sleep 0.2
done
echo "SIDE_EFFECT={nonce}" >> {SIDE_EFFECT_LOG}
echo COMPLETED
exit 7
"#
    )
}

async fn ready(path: &Path, child: &mut ManagedChild) {
    let deadline = Instant::now() + Duration::from_secs(60);
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

fn count_occurrences(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

async fn wait_until<F>(mut probe: F, what: &str, budget: Duration) -> Result<(), String>
where
    F: FnMut() -> Result<bool, String>,
{
    let deadline = Instant::now() + budget;
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

#[derive(Debug, Clone, Default)]
struct ToolCall {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Debug, Clone, Default)]
struct ToolResult {
    id: String,
    ok: bool,
    preview: String,
    exit_code: Option<i32>,
}

#[derive(Debug, Default)]
struct Turn {
    calls: Vec<ToolCall>,
    results: Vec<ToolResult>,
    /// Streamed assistant prose.
    text: String,
    /// The runtime's own named delivery, when it published one.
    delivery: Option<String>,
    outcome: String,
}

impl Turn {
    fn delivery_text(&self) -> String {
        self.delivery
            .clone()
            .filter(|text| !text.trim().is_empty())
            .unwrap_or_else(|| self.text.clone())
    }

    fn calls_named<'a>(&'a self, names: &'a [&'a str]) -> impl Iterator<Item = &'a ToolCall> {
        self.calls
            .iter()
            .filter(move |call| names.contains(&call.name.as_str()))
    }

    fn result_for(&self, call_id: &str) -> Option<&ToolResult> {
        self.results.iter().find(|result| result.id == call_id)
    }
}

/// Collect one turn's real events. The model decides the calls; this only
/// records what the runtime reported.
async fn collect_turn(
    events: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>,
    budget: Duration,
) -> Turn {
    let mut turn = Turn::default();
    let deadline = Instant::now() + budget;
    loop {
        let now = Instant::now();
        if now >= deadline {
            turn.outcome = "timeout".into();
            return turn;
        }
        let wait = (deadline - now).min(Duration::from_secs(30));
        match tokio::time::timeout(wait, events.recv()).await {
            Err(_) => continue,
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped))) => {
                eprintln!("  [event stream lagged, skipped {skipped}]");
                continue;
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                turn.outcome = "stream_closed".into();
                return turn;
            }
            Ok(Ok(event)) => match event {
                RuntimeEvent::ToolCallStarted {
                    id,
                    name,
                    arguments,
                    ..
                } => turn.calls.push(ToolCall {
                    id: id.to_string(),
                    name,
                    arguments,
                }),
                RuntimeEvent::ToolCallCompleted {
                    id,
                    ok,
                    preview,
                    exit_code,
                    ..
                } => turn.results.push(ToolResult {
                    id: id.to_string(),
                    ok,
                    preview,
                    exit_code,
                }),
                RuntimeEvent::AssistantTextDelta { delta, .. } => turn.text.push_str(&delta),
                RuntimeEvent::DeliveryCommitted { text } => turn.delivery = Some(text),
                RuntimeEvent::TurnCompleted => {
                    turn.outcome = "completed".into();
                    return turn;
                }
                RuntimeEvent::TurnCompletedWithWarnings { reason, .. } => {
                    turn.outcome = format!("completed_with_warnings: {reason}");
                    return turn;
                }
                RuntimeEvent::TurnAnswered => {
                    turn.outcome = "answered".into();
                    return turn;
                }
                RuntimeEvent::TurnPaused { reason } => {
                    turn.outcome = format!("paused: {reason}");
                    return turn;
                }
                RuntimeEvent::TurnIncomplete { reason } => {
                    turn.outcome = format!("incomplete: {reason}");
                    return turn;
                }
                RuntimeEvent::TurnTruncated { error } => {
                    turn.outcome = format!("truncated: {error}");
                    return turn;
                }
                RuntimeEvent::TurnFailed { error, .. } => {
                    turn.outcome = format!("failed: {error}");
                    return turn;
                }
                RuntimeEvent::TurnCancelled => {
                    turn.outcome = "interrupted".into();
                    return turn;
                }
                RuntimeEvent::TaskCancelled => {
                    turn.outcome = "task_cancelled".into();
                    return turn;
                }
                _ => {}
            },
        }
    }
}

async fn submit(
    client: &LocalSocketRuntimeClient,
    session: &leveler_core::SessionId,
    content: &str,
) {
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: content.to_string(),
            attachments: Vec::new(),
        })
        .await
        .expect("the real runtime must admit the message");
}

#[derive(Debug, Clone)]
struct ObservedStart {
    task_id: String,
    background_pid: u32,
    start_identity: String,
    origin_call_id: Option<String>,
}

/// Wait until the real Execution Host has a Running task for this session and
/// this script. Records the mechanical facts; it never releases anything.
async fn observe_running_task(
    env: &Environment,
    session: &leveler_core::SessionId,
    budget: Duration,
) -> ObservedStart {
    let host_state = env.host_state();
    let session_id = session.as_str().to_string();
    wait_until(
        || {
            let tasks = host_tasks(&host_state);
            Ok(tasks.iter().any(|task| {
                task["snapshot"]["owner_scope"].as_str() == Some(&session_id)
                    && task["snapshot"]["args"]
                        .as_array()
                        .is_some_and(|args| args.iter().any(|arg| arg.as_str() == Some(JOB_SCRIPT)))
                    && task["snapshot"]["status"].as_str() == Some("Running")
            }))
        },
        "the real Execution Host to show the job Running",
        budget,
    )
    .await
    .unwrap_or_else(|error| {
        panic!(
            "{error}; host tasks = {}",
            serde_json::to_string_pretty(&host_tasks(&host_state)).unwrap_or_default()
        )
    });
    let tasks = host_tasks(&host_state);
    let task = tasks
        .iter()
        .find(|task| {
            task["snapshot"]["owner_scope"].as_str() == Some(&session_id)
                && task["snapshot"]["args"]
                    .as_array()
                    .is_some_and(|args| args.iter().any(|arg| arg.as_str() == Some(JOB_SCRIPT)))
        })
        .expect("the observed task");
    let snapshot = &task["snapshot"];
    ObservedStart {
        task_id: snapshot["id"].as_str().unwrap().to_string(),
        background_pid: snapshot["pid"].as_u64().expect("real background pid") as u32,
        start_identity: snapshot["start_identity"]
            .as_str()
            .expect("durable start identity")
            .to_string(),
        origin_call_id: snapshot["origin_call_id"].as_str().map(str::to_string),
    }
}

/// Create a Goal session on a real runtime and return the session id.
async fn create_goal_session(
    client: &LocalSocketRuntimeClient,
    goal: &str,
) -> leveler_core::SessionId {
    client
        .create_session(CreateSessionRequest {
            request_id: None,
            collaboration: CollaborationMode::Goal,
            workspace: leveler_local_transport::CreateWorkspaceSelection::RuntimeDefault,
            approval_policy: leveler_client_protocol::ApprovalPolicy::AutoApprove,
            goal: goal.to_string(),
            model: None,
            mode: leveler_client_protocol::PermissionProfile::FullAccess,
        })
        .await
        .unwrap()
        .session
        .id
}

/// Durable rows for a session, read through a fresh connection (not the
/// runtime's in-memory projection).
async fn durable_rows(path: &Path, session: &str, types: &[&str]) -> Vec<serde_json::Value> {
    let Ok(db) = Database::connect(path).await else {
        return Vec::new();
    };
    let rows = db
        .load_by_types(&leveler_core::SessionId::new(session), types)
        .await
        .unwrap_or_default();
    rows.into_iter()
        .map(|row| {
            serde_json::json!({
                "event_type": row.event_type,
                "turn_id": row.turn_id,
                "payload": row.payload,
            })
        })
        .collect()
}

/// A durable event row stores the engine event JSON as its payload string.
fn durable_event(row: &serde_json::Value) -> Option<serde_json::Value> {
    let raw = row["payload"].as_str()?;
    serde_json::from_str::<serde_json::Value>(raw).ok()
}

fn run_command_calls(rows: &[serde_json::Value], script: &str) -> Vec<serde_json::Value> {
    rows.iter()
        .filter(|row| {
            let Some(event) = durable_event(row) else {
                return false;
            };
            event["type"].as_str() == Some("tool_call_started")
                && event["payload"]["name"].as_str() == Some("run_command")
                && event["payload"]["arguments"]
                    .as_str()
                    .is_some_and(|args| args.contains(script))
        })
        .cloned()
        .collect()
}

fn parse_arguments(raw: &str) -> Option<serde_json::Value> {
    // Compacted wire JSON is still JSON; a truncated tail is not, and the
    // caller falls back to substring checks.
    serde_json::from_str(raw).ok()
}

/// Assert the model started the script with an explicit background call.
fn assert_background_spawn(rows: &[serde_json::Value], observed: &ObservedStart) {
    let calls = run_command_calls(rows, JOB_SCRIPT);
    assert!(
        !calls.is_empty(),
        "the model never called run_command for {JOB_SCRIPT}: {rows:#?}"
    );
    let call = calls
        .iter()
        .find(|row| {
            durable_event(row)
                .and_then(|event| {
                    parse_arguments(event["payload"]["arguments"].as_str().unwrap_or_default())
                })
                .is_some_and(|args| args["background"] == serde_json::json!(true))
        })
        .unwrap_or_else(|| {
            panic!("no run_command call for {JOB_SCRIPT} set background=true: {calls:#?}")
        });
    let event = durable_event(call).expect("the starting event decodes");
    let call_id = event["payload"]["call_id"]
        .as_str()
        .expect("durable starting call id");
    if let Some(origin) = &observed.origin_call_id {
        assert_eq!(
            call_id, origin,
            "the host task must be attributed to the model's own starting call"
        );
    }
    let args = parse_arguments(event["payload"]["arguments"].as_str().unwrap_or_default())
        .expect("the recorded arguments are JSON");
    assert_eq!(args["program"], serde_json::json!("sh"), "{args}");
    assert_eq!(
        args["args"],
        serde_json::json!([JOB_SCRIPT]),
        "the model must run the controlled script: {args}"
    );
    assert_eq!(
        args["background_lifetime"],
        serde_json::json!("session"),
        "the job must outlive the goal and the runtime: {args}"
    );
}

/// Stop only this test's own Execution Host. The state dir is a temp dir owned
/// by this process, so its `ready.json` pid is unambiguous. `ps` is not used.
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
    let pid = pid as u32;
    if alive(pid) {
        let _ = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status();
    }
}

struct Evidence {
    dir: PathBuf,
}

impl Evidence {
    fn new(label: &str) -> Option<Self> {
        let root = std::env::var_os("R3_EVIDENCE_DIR")?;
        let dir = PathBuf::from(root).join(label);
        std::fs::create_dir_all(&dir).unwrap();
        Some(Self { dir })
    }

    fn write_json(&self, name: &str, value: &serde_json::Value) {
        std::fs::write(
            self.dir.join(name),
            serde_json::to_vec_pretty(value).unwrap(),
        )
        .unwrap();
    }

    fn copy(&self, name: &str, source: &Path) {
        if source.exists() {
            let _ = std::fs::copy(source, self.dir.join(name));
        }
    }

    fn copy_tree(&self, name: &str, source: &Path) {
        let Ok(entries) = std::fs::read_dir(source) else {
            return;
        };
        let target = self.dir.join(name);
        std::fs::create_dir_all(&target).unwrap();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                let _ = std::fs::copy(&path, target.join(path.file_name().unwrap()));
            }
        }
    }

    /// Replace the provider credential in every text artifact before it leaves
    /// the isolated environment.
    fn redact(&self) {
        let Ok(secret) = std::env::var("DEEPSEEK_API_KEY") else {
            return;
        };
        if secret.trim().is_empty() {
            return;
        }
        for entry in std::fs::read_dir(&self.dir).unwrap().flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if path.extension().is_some_and(|ext| {
                matches!(ext.to_str(), Some("db" | "db-wal" | "db-shm" | "sock"))
            }) {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let Ok(text) = String::from_utf8(bytes) else {
                continue;
            };
            if text.contains(&secret) {
                std::fs::write(&path, text.replace(&secret, "<redacted>")).unwrap();
            }
        }
    }
}

fn export_common(evidence: &Evidence, env: &Environment, runtime_stderr: &[&str]) {
    let host = env.host_state();
    for name in [
        "ready.json",
        "tasks.json",
        "intents.json",
        "intents.unresolved.json",
    ] {
        evidence.copy(name, &host.join(name));
    }
    evidence.copy_tree(
        "execution-settlement",
        &env.state_dir().join("execution-settlement"),
    );
    for name in [
        JOB_SCRIPT,
        RELEASE_FILE,
        SPAWN_LOG,
        HEARTBEAT_LOG,
        SIDE_EFFECT_LOG,
    ] {
        evidence.copy(name, &env.repo_path(name));
    }
    for name in runtime_stderr {
        evidence.copy(name, &env.home.join(name));
    }
    // A killed runtime leaves WAL/SHM sidecars; copy the trio together so the
    // durable facts remain readable.
    for name in ["sessions.db", "sessions.db-wal", "sessions.db-shm"] {
        evidence.copy(name, &env.state_dir().join(name));
    }
}

fn turn_json(turn: &Turn) -> serde_json::Value {
    serde_json::json!({
        "outcome": turn.outcome,
        "delivery": turn.delivery,
        "delivery_text": turn.delivery_text(),
        "assistant_text": turn.text,
        "tool_calls": turn.calls.iter().map(|call| serde_json::json!({
            "id": call.id,
            "name": call.name,
            "arguments": call.arguments,
        })).collect::<Vec<_>>(),
        "tool_results": turn.results.iter().map(|result| serde_json::json!({
            "id": result.id,
            "ok": result.ok,
            "exit_code": result.exit_code,
            "preview": result.preview,
        })).collect::<Vec<_>>(),
    })
}

/// Write what the real model did even if a later assertion fails, so a failed
/// dogfood run still leaves its transcript behind for prompt iteration.
async fn export_turn_debug(
    evidence: &Evidence,
    env: &Environment,
    session: &leveler_core::SessionId,
    turn: &Turn,
) {
    evidence.write_json("model_turn_raw.json", &turn_json(turn));
    evidence.copy("tasks_at_turn.json", &env.host_state().join("tasks.json"));
    for name in [
        JOB_SCRIPT,
        RELEASE_FILE,
        SPAWN_LOG,
        HEARTBEAT_LOG,
        SIDE_EFFECT_LOG,
    ] {
        evidence.copy(name, &env.repo_path(name));
    }
    export_durable(evidence, env, session).await;
    evidence.redact();
}

async fn export_durable(evidence: &Evidence, env: &Environment, session: &leveler_core::SessionId) {
    let rows = durable_rows(
        &env.db(),
        session.as_str(),
        &[
            "tool_call_started",
            "tool_call_finished",
            "background_task_started",
            "background_task_finished",
        ],
    )
    .await;
    let turn_ids: BTreeSet<String> = rows
        .iter()
        .filter_map(|row| row["turn_id"].as_str().map(str::to_string))
        .collect();
    evidence.write_json("durable_events.json", &serde_json::Value::Array(rows));
    evidence.write_json(
        "turn_ids.json",
        &serde_json::json!({ "turn_ids": turn_ids.into_iter().collect::<Vec<_>>() }),
    );
}

// ───────────────────────────────────────────────────────────────────────────
// D-A — normal autonomous background execution on the real model.
// ───────────────────────────────────────────────────────────────────────────

fn da_prompt() -> String {
    format!(
        "We are validating the real background-task path. Use the real tools exactly as \
described; never simulate a step and never guess a result.\n\n\
Instructions (these are also the requirements for update_goal):\n\
1. Start the script `{JOB_SCRIPT}` in the background exactly ONCE with `run_command`: program \
\"sh\", args [\"{JOB_SCRIPT}\"], background true, background_lifetime \"session\". Never run \
`{JOB_SCRIPT}` a second time. Do not run it in the foreground. Do not create `{RELEASE_FILE}`; I \
create it while you wait.\n\
2. After the start call returns a task id, call `wait_task` with that exact task id, until \
\"exit\", timeout_seconds 240, and wait for the real terminal result.\n\
3. After it is terminal, run one successful confirmation command: `run_command` with program \
\"cat\" and args [\"{SPAWN_LOG}\", \"{SIDE_EFFECT_LOG}\", \"{HEARTBEAT_LOG}\", \
\"tracked.txt\"]. It exits 0; this is the command completion verification needs.\n\
4. In your visible answer, report the exact background task id, the exact exit code from the \
`wait_task` result, and the exact retained output lines that contain STARTED and COMPLETED.\n\
5. Then call `update_goal` with status \"complete\", a short summary, deliver_answer true, \
verification {{\"status\":\"passed\",\"detail\":\"cat {SPAWN_LOG} {SIDE_EFFECT_LOG} \
{HEARTBEAT_LOG} tracked.txt\"}}, and one requirements entry per numbered instruction above, each \
with status \"done\"."
    )
}

#[test]
#[ignore = "real API: needs DEEPSEEK_API_KEY and spends real model tokens + real processes"]
fn real_model_autonomous_background_reports_real_exit() {
    leveler_test_support::bounded_test(
        "real_model_autonomous_background_reports_real_exit",
        Duration::from_secs(600),
        || async {
            assert!(
                std::env::var("DEEPSEEK_API_KEY").is_ok_and(|value| !value.trim().is_empty()),
                "DEEPSEEK_API_KEY must carry the real gateway key"
            );
            let nonce = format!(
                "r3da-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            );
            let env = Environment::new(&nonce);
            let evidence = Evidence::new("d-a");
            eprintln!(
                "D-A start nonce={nonce} repo={} home={}",
                env.repo.display(),
                env.home.display()
            );

            let (mut runtime, ready_file) = env.serve("runtime-a");
            ready(&ready_file, &mut runtime).await;
            let runtime_pid = runtime.id();
            let client = LocalSocketRuntimeClient::connect(&env.socket())
                .await
                .unwrap();
            let session = create_goal_session(
                &client,
                "Start the controlled background job r3-job.sh, observe its real background task, \
                 and report its true exit code and output.",
            )
            .await;
            eprintln!("D-A session={} runtime_pid={runtime_pid}", session.as_str());

            let mut events = client.subscribe_session(&session);

            // The harness observer only watches real facts. It releases the job
            // after the host has shown it Running and its heartbeat advanced.
            let repo = env.repo.clone();
            let state = env.state_dir();
            let observer_session = session.clone();
            let observer = tokio::spawn(async move {
                let host_state = state.join("execution-host");
                let session_id = observer_session.as_str().to_string();
                let matches = |task: &serde_json::Value| {
                    task["snapshot"]["owner_scope"].as_str() == Some(&session_id)
                        && task["snapshot"]["args"].as_array().is_some_and(|args| {
                            args.iter().any(|arg| arg.as_str() == Some(JOB_SCRIPT))
                        })
                };
                wait_until(
                    || {
                        let tasks = host_tasks(&host_state);
                        Ok(tasks
                            .iter()
                            .any(|task| matches(task) && task["snapshot"]["status"] == "Running"))
                    },
                    "the real background job to be Running",
                    Duration::from_secs(180),
                )
                .await
                .unwrap_or_else(|error| {
                    panic!(
                        "{error}; host tasks = {}",
                        serde_json::to_string_pretty(&host_tasks(&host_state)).unwrap_or_default()
                    )
                });
                let tasks = host_tasks(&host_state);
                let task = tasks.iter().find(|task| matches(task)).unwrap().clone();
                let background_pid = task["snapshot"]["pid"].as_u64().unwrap() as u32;
                let start_identity = task["snapshot"]["start_identity"]
                    .as_str()
                    .unwrap()
                    .to_string();
                assert!(alive(background_pid), "the job must be a real live process");
                let before = std::fs::read_to_string(repo.join(HEARTBEAT_LOG)).unwrap_or_default();
                tokio::time::sleep(Duration::from_millis(500)).await;
                let after = std::fs::read_to_string(repo.join(HEARTBEAT_LOG)).unwrap_or_default();
                assert!(
                    after.len() > before.len(),
                    "the job must really be running before the release"
                );
                assert!(alive(background_pid));
                std::fs::write(repo.join(RELEASE_FILE), b"release").unwrap();
                (
                    task["snapshot"]["id"].as_str().unwrap().to_string(),
                    background_pid,
                    start_identity,
                )
            });

            submit(&client, &session, &da_prompt()).await;
            let turn = collect_turn(&mut events, Duration::from_secs(420)).await;
            let (task_id, background_pid, start_identity) = observer.await.unwrap();
            eprintln!(
                "D-A observed task={task_id} background_pid={background_pid} \
                 start_identity={start_identity} turn_outcome={}",
                turn.outcome
            );
            eprintln!(
                "D-A tools={:?}",
                turn.calls
                    .iter()
                    .map(|call| call.name.as_str())
                    .collect::<Vec<_>>()
            );
            eprintln!("D-A delivery: {}", turn.delivery_text());
            if let Some(evidence) = &evidence {
                export_turn_debug(evidence, &env, &session, &turn).await;
            }

            assert_ne!(
                turn.outcome, "timeout",
                "the real model turn never reached a terminal state: {turn:?}"
            );
            assert!(
                !turn.outcome.starts_with("failed"),
                "the real turn failed: {turn:?}"
            );

            // The model itself must have started the real background task.
            let rows = durable_rows(
                &env.db(),
                session.as_str(),
                &["tool_call_started", "tool_call_finished"],
            )
            .await;
            assert!(
                !rows.is_empty(),
                "the durable log carries no tool calls: {rows:#?}"
            );
            let origin_call_id = turn
                .calls
                .iter()
                .find(|call| call.name == "run_command")
                .map(|call| call.id.clone());
            let observed = ObservedStart {
                task_id: task_id.clone(),
                background_pid,
                start_identity: start_identity.clone(),
                origin_call_id,
            };
            assert_background_spawn(&rows, &observed);
            assert!(
                task_id.starts_with("host-"),
                "the production tool must return a supervised host task: {task_id}"
            );

            // The model must have waited on THAT id and the tool must have
            // carried the real result.
            let observation = turn
                .calls_named(&["wait_task", "get_task"])
                .filter(|call| call.arguments.contains(&task_id))
                .collect::<Vec<_>>();
            assert!(
                !observation.is_empty(),
                "the model never observed the original task: {turn:?}"
            );
            let result = observation
                .iter()
                .filter_map(|call| turn.result_for(&call.id))
                .find(|result| result.preview.contains("exit_code: Some(7)"))
                .unwrap_or_else(|| {
                    panic!(
                        "the model's task observation did not carry the real exit code: {turn:?}"
                    )
                });
            assert!(
                result.preview.contains("COMPLETED"),
                "the model's task observation must carry the retained output: {}",
                result.preview
            );

            // Independent real facts: the host archive, not the model's prose.
            let host_state = env.host_state();
            let mut tasks = host_tasks(&host_state);
            wait_until(
                || {
                    tasks = host_tasks(&host_state);
                    Ok(find_task(&tasks, &task_id).is_some_and(|task| {
                        task["snapshot"]["status"].as_str() == Some("Exited")
                            && task["snapshot"]["exit_code"].as_i64() == Some(7)
                    }))
                },
                "the host to commit the real exit 7",
                Duration::from_secs(60),
            )
            .await
            .unwrap();
            let finished = find_task(&tasks, &task_id).unwrap();
            assert_eq!(finished["snapshot"]["exit_code"].as_i64(), Some(7));
            assert!(finished["snapshot"]["signal"].is_null());
            assert_eq!(
                finished["snapshot"]["pid"].as_u64(),
                Some(background_pid as u64)
            );
            assert_eq!(
                finished["snapshot"]["start_identity"].as_str(),
                Some(start_identity.as_str())
            );
            assert!(
                finished["result"]["result_id"]
                    .as_str()
                    .is_some_and(|id| !id.is_empty()),
                "the host must commit a durable result identity: {finished}"
            );
            let log = finished["snapshot"]["log"].as_str().unwrap_or_default();
            assert!(log.contains("STARTED"), "{log}");
            assert!(log.contains("COMPLETED"), "{log}");

            // The side effect happened exactly once, on the same nonce.
            let spawn = env.read_repo(SPAWN_LOG);
            let side_effect = env.read_repo(SIDE_EFFECT_LOG);
            assert_eq!(count_occurrences(&spawn, &nonce), 1, "{spawn}");
            assert_eq!(count_occurrences(&side_effect, &nonce), 1, "{side_effect}");
            assert!(count_occurrences(&env.read_repo(HEARTBEAT_LOG), &nonce) > 0);

            // The model's delivery names the real facts.
            let delivery = turn.delivery_text();
            assert!(
                delivery.contains(&task_id),
                "the delivery must name the real task id: {delivery}"
            );
            assert!(
                delivery.to_ascii_lowercase().contains("exit") && delivery.contains('7'),
                "the delivery must state the real exit code 7: {delivery}"
            );
            assert!(
                delivery.contains("COMPLETED") && delivery.contains("STARTED"),
                "the delivery must carry the real output lines: {delivery}"
            );

            let durable_finished =
                durable_rows(&env.db(), session.as_str(), &["background_task_finished"]).await;
            assert_eq!(
                durable_finished.len(),
                1,
                "exactly one durable terminal fact: {durable_finished:#?}"
            );

            if let Some(evidence) = &evidence {
                export_common(evidence, &env, &["runtime-a-stderr.txt"]);
                export_durable(evidence, &env, &session).await;
                let mut turn_record = turn_json(&turn);
                turn_record["session_id"] = serde_json::json!(session.as_str());
                turn_record["task_id"] = serde_json::json!(task_id);
                evidence.write_json("model_turn.json", &turn_record);
                evidence.write_json(
                    "SYSTEM.json",
                    &serde_json::json!({
                        "verdict": "D-A PASS",
                        "nonce": nonce,
                        "session_id": session.as_str(),
                        "runtime_pid": runtime_pid,
                        "supervisor_pid": host_pid(&env.host_state()),
                        "turn_outcome": turn.outcome,
                        "host_state": host_state.display().to_string(),
                        "task_id": task_id,
                        "background_pid": background_pid,
                        "process_start_identity": start_identity,
                        "exit_code": 7,
                        "durable_terminal_facts": durable_finished.len(),
                        "spawn_log_nonce_count": count_occurrences(&spawn, &nonce),
                        "side_effect_nonce_count": count_occurrences(&side_effect, &nonce),
                        "repo": env.repo.display().to_string(),
                        "home": env.home.display().to_string(),
                    }),
                );
                evidence.redact();
                eprintln!("D-A evidence={}", evidence.dir.display());
            }

            let _ = runtime.kill();
            let _ = runtime.wait();
            stop_isolated_host(&env.host_state());
            eprintln!(
                "D-A PASS nonce={nonce} session={} task={task_id} background_pid={background_pid} \
                 exit_code=7",
                session.as_str()
            );
        },
    );
}

// ───────────────────────────────────────────────────────────────────────────
// D-B — real runtime SIGKILL, then the SAME session resumes the SAME task.
// ───────────────────────────────────────────────────────────────────────────

fn db_prompt() -> String {
    format!(
        "We are validating real background-task recovery across a runtime crash. Use the real \
tools exactly as described; never simulate a step and never guess a result.\n\n\
1. Start the script `{JOB_SCRIPT}` in the background exactly ONCE with `run_command`: program \
\"sh\", args [\"{JOB_SCRIPT}\"], background true, background_lifetime \"session\". Never run \
`{JOB_SCRIPT}` a second time. Do not run it in the foreground. Do not create `{RELEASE_FILE}`; \
it is created later.\n\
2. After the start call returns a task id, call `get_task` once with that exact task id.\n\
3. Reply with the exact task id and its current status. Do not wait for it to exit."
    )
}

fn db_resume_prompt(task_id: &str) -> String {
    format!(
        "The runtime that started the background task was killed by SIGKILL and has been restarted. \
This is the SAME session and the SAME background task `{task_id}`; it must NOT be started again \
and `{JOB_SCRIPT}` must not be run again.\n\n\
1. Call `get_task` and/or `wait_task` with exactly this task id: `{task_id}`.\n\
2. Report its real terminal status, the exact exit code from the tool result, and the exact \
retained output lines that contain STARTED and COMPLETED.\n\
3. Run one successful confirmation command: `run_command` with program \"cat\" and args \
[\"{SPAWN_LOG}\", \"{SIDE_EFFECT_LOG}\"]. It exits 0 and shows the one-time spawn and side \
effect.\n\
Use only real tool results; do not guess."
    )
}

async fn acknowledge_crash_window(
    client: &LocalSocketRuntimeClient,
    session: &leveler_core::SessionId,
    events: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>,
) {
    if client
        .snapshot(session)
        .await
        .unwrap()
        .interrupted_tool_calls
        .is_empty()
    {
        return;
    }
    client
        .send(ClientCommand::AcknowledgeCrashWindow {
            session_id: session.clone(),
        })
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(
            Instant::now() < deadline,
            "the runtime never acknowledged the crash window"
        );
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("event stream")
            .expect("stream open");
        if let RuntimeEvent::CrashWindowAcknowledged { .. } = event {
            return;
        }
    }
}

#[test]
#[ignore = "real API: needs DEEPSEEK_API_KEY and spends real model tokens + real SIGKILL"]
fn real_model_runtime_sigkill_resumes_the_same_background_task() {
    leveler_test_support::bounded_test(
        "real_model_runtime_sigkill_resumes_the_same_background_task",
        Duration::from_secs(600),
        || async {
            assert!(
                std::env::var("DEEPSEEK_API_KEY").is_ok_and(|value| !value.trim().is_empty()),
                "DEEPSEEK_API_KEY must carry the real gateway key"
            );
            let nonce = format!(
                "r3db-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            );
            let env = Environment::new(&nonce);
            let evidence = Evidence::new("d-b");
            eprintln!(
                "D-B start nonce={nonce} repo={} home={}",
                env.repo.display(),
                env.home.display()
            );

            // ── P1: the model starts the real background job ───────────────
            let (mut runtime_a, ready_a) = env.serve("runtime-a");
            ready(&ready_a, &mut runtime_a).await;
            let runtime_a_pid = runtime_a.id();
            let client_a = LocalSocketRuntimeClient::connect(&env.socket())
                .await
                .unwrap();
            let session = create_goal_session(
                &client_a,
                "Start the controlled background job r3-job.sh, report its task id and status, \
                 and leave it running.",
            )
            .await;
            eprintln!(
                "D-B session={} runtime_a_pid={runtime_a_pid}",
                session.as_str()
            );
            submit(&client_a, &session, &db_prompt()).await;

            let observed = observe_running_task(&env, &session, Duration::from_secs(180)).await;
            let task_id = observed.task_id.clone();
            let host_state = env.host_state();
            let supervisor_pid = host_pid(&host_state);
            eprintln!(
                "D-B running task={task_id} host_pid={supervisor_pid} \
                 background_pid={} start_identity={} origin_call_id={:?}",
                observed.background_pid, observed.start_identity, observed.origin_call_id
            );
            assert!(alive(observed.background_pid));
            assert_ne!(runtime_a_pid, supervisor_pid);
            assert_ne!(observed.background_pid, supervisor_pid);
            if let Some(evidence) = &evidence {
                evidence.copy("tasks_before_kill.json", &host_state.join("tasks.json"));
                for name in [
                    JOB_SCRIPT,
                    RELEASE_FILE,
                    SPAWN_LOG,
                    HEARTBEAT_LOG,
                    SIDE_EFFECT_LOG,
                ] {
                    evidence.copy(name, &env.repo_path(name));
                }
                export_durable(evidence, &env, &session).await;
                let snapshot = client_a.snapshot(&session).await.unwrap();
                evidence.write_json(
                    "snapshot_before_kill.json",
                    &serde_json::to_value(&snapshot).unwrap(),
                );
                evidence.redact();
            }
            let spawn_before = env.read_repo(SPAWN_LOG);
            assert_eq!(
                count_occurrences(&spawn_before, &nonce),
                1,
                "{spawn_before}"
            );
            let task_before = find_task(&host_tasks(&host_state), &task_id).unwrap();
            assert!(
                task_before["snapshot"]["log"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("STARTED"),
                "the supervisor must have retained the job's STARTED line before the crash:                  {task_before}"
            );

            // Give the real model a moment to consume the start result; this is
            // observation time, not a scripted step.
            tokio::time::sleep(Duration::from_millis(500)).await;

            // ── P2: the real SIGKILL ────────────────────────────────────────
            runtime_a.kill().expect("SIGKILL the runtime");
            let status = runtime_a.wait().unwrap();
            assert_eq!(
                status.signal(),
                Some(9),
                "the runtime must really die by SIGKILL: {status}"
            );
            assert!(!alive(runtime_a_pid), "the runtime process must be gone");
            assert!(alive(supervisor_pid), "the Execution Host must survive");
            assert!(alive(observed.background_pid), "the workload must survive");
            assert_eq!(
                leveler_execution::process_identity::verify_process_identity(
                    observed.background_pid,
                    &observed.start_identity,
                ),
                leveler_execution::process_identity::ProcessIdentity::Matches,
                "the surviving process must still be the one that was started"
            );
            let heartbeat_before = env.read_repo(HEARTBEAT_LOG);
            tokio::time::sleep(Duration::from_millis(400)).await;
            let heartbeat_after = env.read_repo(HEARTBEAT_LOG);
            assert!(
                heartbeat_after.len() > heartbeat_before.len(),
                "the workload must keep producing output after the runtime died"
            );

            // ── P3: it finishes with no runtime at all ─────────────────────
            std::fs::write(env.repo_path(RELEASE_FILE), b"release").unwrap();
            wait_until(
                || {
                    let tasks = host_tasks(&host_state);
                    Ok(find_task(&tasks, &task_id).is_some_and(|task| {
                        task["snapshot"]["status"].as_str() == Some("Exited")
                            && task["snapshot"]["exit_code"].as_i64() == Some(7)
                    }))
                },
                "the host to commit exit 7 while no runtime exists",
                Duration::from_secs(60),
            )
            .await
            .unwrap();
            let finished = find_task(&host_tasks(&host_state), &task_id).unwrap();
            assert_eq!(finished["snapshot"]["exit_code"].as_i64(), Some(7));
            assert!(finished["snapshot"]["signal"].is_null());
            assert_eq!(
                finished["snapshot"]["pid"].as_u64(),
                Some(observed.background_pid as u64)
            );
            assert_eq!(
                finished["snapshot"]["start_identity"].as_str(),
                Some(observed.start_identity.as_str())
            );
            let result_id = finished["result"]["result_id"]
                .as_str()
                .expect("the host must commit a durable result identity")
                .to_string();
            let log = finished["snapshot"]["log"].as_str().unwrap_or_default();
            assert!(log.contains("STARTED"), "{log}");
            assert!(log.contains("COMPLETED"), "{log}");
            let side_effect = env.read_repo(SIDE_EFFECT_LOG);
            assert_eq!(count_occurrences(&side_effect, &nonce), 1, "{side_effect}");

            // ── P4: a NEW real runtime resumes the SAME session ────────────
            let (mut runtime_b, ready_b) = env.serve("runtime-b");
            ready(&ready_b, &mut runtime_b).await;
            let runtime_b_pid = runtime_b.id();
            assert_ne!(runtime_a_pid, runtime_b_pid);
            let client_b = LocalSocketRuntimeClient::connect(&env.socket())
                .await
                .unwrap();
            let recovered = client_b.snapshot(&session).await.unwrap();
            if let Some(evidence) = &evidence {
                evidence.write_json(
                    "snapshot_after_restart.json",
                    &serde_json::to_value(&recovered).unwrap(),
                );
            }
            let record = recovered
                .background_tasks
                .iter()
                .find(|record| record.task_id == task_id)
                .unwrap_or_else(|| panic!("no recovered record for {task_id}: {recovered:?}"));
            assert_eq!(
                record.exit_code,
                Some(7),
                "the recovered projection must carry the real exit: {record:?}"
            );
            let mut events_b = client_b.subscribe_session(&session);
            acknowledge_crash_window(&client_b, &session, &mut events_b).await;

            submit(&client_b, &session, &db_resume_prompt(&task_id)).await;
            let turn = collect_turn(&mut events_b, Duration::from_secs(420)).await;
            eprintln!(
                "D-B resumed runtime_pid={runtime_b_pid} turn_outcome={} tools={:?}",
                turn.outcome,
                turn.calls
                    .iter()
                    .map(|call| call.name.as_str())
                    .collect::<Vec<_>>()
            );
            eprintln!("D-B delivery: {}", turn.delivery_text());
            if let Some(evidence) = &evidence {
                export_turn_debug(evidence, &env, &session, &turn).await;
            }
            assert_ne!(
                turn.outcome, "timeout",
                "the resumed real model turn never reached a terminal state: {turn:?}"
            );
            assert!(
                !turn.outcome.starts_with("failed"),
                "the resumed real turn failed: {turn:?}"
            );

            // The model itself must observe the ORIGINAL id through the real
            // tool surface, and that tool result must carry exit 7 / COMPLETED.
            let observation = turn
                .calls_named(&["get_task", "wait_task"])
                .filter(|call| call.arguments.contains(&task_id))
                .collect::<Vec<_>>();
            assert!(
                !observation.is_empty(),
                "the resumed model never named the original task id: {turn:?}"
            );
            let result = observation
                .iter()
                .filter_map(|call| turn.result_for(&call.id))
                .find(|result| result.preview.contains(&task_id))
                .unwrap_or_else(|| {
                    panic!("the resumed model's task tool result did not name {task_id}: {turn:?}")
                });
            assert!(
                result.preview.contains("exit_code: Some(7)"),
                "the resumed task result must carry the real exit: {}",
                result.preview
            );
            assert!(
                result.preview.contains("COMPLETED"),
                "the resumed task result must carry the retained output: {}",
                result.preview
            );
            let delivery = turn.delivery_text();
            assert!(
                delivery.contains(&task_id),
                "the resumed delivery must name the original task id: {delivery}"
            );
            assert!(
                delivery.to_ascii_lowercase().contains("exit") && delivery.contains('7'),
                "the resumed delivery must state the real exit code 7: {delivery}"
            );
            assert!(
                delivery.contains("COMPLETED"),
                "the resumed delivery must carry the real output: {delivery}"
            );

            // ── P5: it is the SAME instance, not a respawn ─────────────────
            let tasks = host_tasks(&host_state);
            assert_eq!(
                tasks
                    .iter()
                    .filter(|task| task["snapshot"]["id"].as_str() == Some(&task_id))
                    .count(),
                1,
                "the host archive must hold exactly one record for this identity"
            );
            let spawn = env.read_repo(SPAWN_LOG);
            let side_effect = env.read_repo(SIDE_EFFECT_LOG);
            assert_eq!(count_occurrences(&spawn, &nonce), 1, "{spawn}");
            assert_eq!(count_occurrences(&side_effect, &nonce), 1, "{side_effect}");
            let final_task = find_task(&tasks, &task_id).unwrap();
            assert_eq!(
                final_task["snapshot"]["pid"].as_u64(),
                Some(observed.background_pid as u64),
                "the recovered task must be the same background process"
            );
            assert_eq!(
                final_task["snapshot"]["start_identity"].as_str(),
                Some(observed.start_identity.as_str())
            );
            assert_eq!(
                final_task["result"]["result_id"].as_str(),
                Some(result_id.as_str()),
                "the recovered result must be the same durable record"
            );
            let durable_finished =
                durable_rows(&env.db(), session.as_str(), &["background_task_finished"]).await;
            assert_eq!(
                durable_finished.len(),
                1,
                "the reconnect must not append a second terminal fact: {durable_finished:#?}"
            );
            // The starting call is the model's own, recorded before the crash.
            let rows = durable_rows(
                &env.db(),
                session.as_str(),
                &["tool_call_started", "tool_call_finished"],
            )
            .await;
            let observed_for_assert = ObservedStart {
                task_id: task_id.clone(),
                background_pid: observed.background_pid,
                start_identity: observed.start_identity.clone(),
                origin_call_id: observed.origin_call_id.clone(),
            };
            assert_background_spawn(&rows, &observed_for_assert);

            if let Some(evidence) = &evidence {
                export_common(
                    evidence,
                    &env,
                    &["runtime-a-stderr.txt", "runtime-b-stderr.txt"],
                );
                export_durable(evidence, &env, &session).await;
                let mut turn_record = turn_json(&turn);
                turn_record["session_id"] = serde_json::json!(session.as_str());
                turn_record["task_id"] = serde_json::json!(task_id);
                evidence.write_json("resumed_model_turn.json", &turn_record);
                evidence.write_json(
                    "SYSTEM.json",
                    &serde_json::json!({
                        "verdict": "D-B PASS",
                        "nonce": nonce,
                        "session_id": session.as_str(),
                        "resumed_turn_outcome": turn.outcome,
                        "runtime_a_pid": runtime_a_pid,
                        "runtime_b_pid": runtime_b_pid,
                        "runtime_sigkill_signal": status.signal(),
                        "supervisor_pid": supervisor_pid,
                        "task_id": task_id,
                        "background_pid": observed.background_pid,
                        "process_start_identity": observed.start_identity,
                        "origin_call_id": observed.origin_call_id,
                        "result_id": result_id,
                        "exit_code": 7,
                        "durable_terminal_facts": durable_finished.len(),
                        "host_archive_records_for_id": tasks
                            .iter()
                            .filter(|task| task["snapshot"]["id"].as_str() == Some(&task_id))
                            .count(),
                        "spawn_log_nonce_count": count_occurrences(&spawn, &nonce),
                        "side_effect_nonce_count": count_occurrences(&side_effect, &nonce),
                        "repo": env.repo.display().to_string(),
                        "home": env.home.display().to_string(),
                    }),
                );
                evidence.redact();
                eprintln!("D-B evidence={}", evidence.dir.display());
            }

            let _ = runtime_b.kill();
            let _ = runtime_b.wait();
            stop_isolated_host(&host_state);
            eprintln!(
                "D-B PASS nonce={nonce} session={} task={task_id} runtime_a_pid={runtime_a_pid} \
                 runtime_b_pid={runtime_b_pid} supervisor_pid={supervisor_pid} \
                 background_pid={} start_identity={} signal=9 exit_code=7",
                session.as_str(),
                observed.background_pid,
                observed.start_identity
            );
        },
    );
}

// ───────────────────────────────────────────────────────────────────────────
// D-C — real model calls a real MCP tool whose transport drops mid-call.
// ───────────────────────────────────────────────────────────────────────────

/// A real stdio MCP server. `write_then_drop` commits exactly one side-effect
/// byte, records the JSON-RPC request id and its own pid, then exits WITHOUT a
/// result. The runtime therefore sees a closed response channel: the call's
/// real outcome is Unknown, not success and not a confirmed failure.
const MCP_SERVER_SCRIPT: &str = r#"import json,sys,os
counter=sys.argv[1]; log_path=sys.argv[2]
def log(fact):
    with open(log_path,'a') as f: f.write(json.dumps(fact)+'\n'); f.flush()
def send(o): print(json.dumps(o),flush=True)
log({'event':'server_start','pid':os.getpid()})
for line in sys.stdin:
    line=line.strip()
    if not line: continue
    req=json.loads(line); m=req.get('method'); i=req.get('id')
    if m=='notifications/initialized': continue
    if m=='initialize':
        res={'protocolVersion':'2024-11-05','capabilities':{'tools':{}},'serverInfo':{'name':'controlled','version':'1'}}
    elif m=='tools/list':
        res={'tools':[{'name':'write','description':'','inputSchema':{'type':'object','properties':{'mode':{'type':'string'}}}}]}
    elif m=='tools/call':
        mode=req['params'].get('arguments',{}).get('mode','write')
        log({'event':'call_started','request_id':i,'pid':os.getpid(),'mode':mode})
        if mode=='write_then_drop':
            with open(counter,'a') as f: f.write('x'); f.flush()
            log({'event':'side_effect','request_id':i,'pid':os.getpid()})
            os._exit(0)
        res={'content':[{'type':'text','text':'ok:'+mode}],'isError':False}
    else:
        res={}
    if i is not None: send({'jsonrpc':'2.0','id':i,'result':res})
"#;

const SERVICE_SCRIPT: &str = "r3-service.sh";

/// A real long-running service. It ignores SIGTERM, so `kill_task` can only
/// report "requested, not confirmed" until the Execution Host's SIGKILL
/// backstop lands.
fn service_script() -> String {
    r#"#!/bin/sh
trap '' TERM
echo READY
echo $$ > r3-service.pid
while :; do
  echo HEARTBEAT
  sleep 0.2
done
"#
    .to_string()
}

fn toml_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn read_jsonl(path: &Path) -> Vec<serde_json::Value> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn facts_named(path: &Path, event: &str) -> Vec<serde_json::Value> {
    read_jsonl(path)
        .into_iter()
        .filter(|fact| fact["event"] == event)
        .collect()
}

fn side_effect_count(counter: &Path) -> usize {
    std::fs::read_to_string(counter)
        .map(|text| text.matches('x').count())
        .unwrap_or(0)
}

fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

fn dc_prompt() -> String {
    r#"A real MCP server named "controlled" is configured, but its tools are not exposed yet. This server deliberately commits a side effect and then closes its transport without a result, so the real outcome of a call is Unknown. Use the real tools exactly as described; never simulate a step and never guess a result.

1. First call the `capability` tool with exactly {"action":"enable","id":"external_tools"} to expose the configured MCP tools on the next request. Do not skip this step.
2. Then call the MCP tool `mcp__controlled__write` exactly ONCE with arguments {"mode":"write_then_drop"}. Do not call it a second time, not even to verify.
3. The server writes exactly one side-effect byte to `r3-mcp-counter.txt` and a JSONL request log to `r3-mcp-server.jsonl` in the workspace root, logs the JSON-RPC request id and its own pid, and then exits without sending a result. The tool result will say the outcome is unknown.
4. After the call, inspect the real state with `read_file` or `run_command` (for example `cat r3-mcp-counter.txt r3-mcp-server.jsonl`). Determine the real side effect from those files.
5. In your visible answer, report: the MCP tool call id, the real side-effect count, the exact uncertainty text from the tool result, and what is verified versus unknown. Do not claim success or failure; the call's outcome is Unknown. Do not call the MCP tool again."#
        .to_string()
}

#[test]
#[ignore = "real API: needs DEEPSEEK_API_KEY and spends real tokens + a real MCP process"]
fn real_model_reports_mcp_unknown_without_retry() {
    leveler_test_support::bounded_test(
        "real_model_reports_mcp_unknown_without_retry",
        Duration::from_secs(600),
        || async {
            assert!(
                std::env::var("DEEPSEEK_API_KEY").is_ok_and(|value| !value.trim().is_empty()),
                "DEEPSEEK_API_KEY must carry the real gateway key"
            );
            let nonce = format!(
                "r3dc-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            );
            let env = Environment::new(&nonce);
            let evidence = Evidence::new("d-c");
            let server_script = env.repo.join("controlled_mcp_server.py");
            std::fs::write(&server_script, MCP_SERVER_SCRIPT).unwrap();
            let counter = env.repo.join("r3-mcp-counter.txt");
            let server_log = env.repo.join("r3-mcp-server.jsonl");
            env.append_config(&format!(
                "\n[[mcp_servers]]\nname = \"controlled\"\ncommand = {}\nargs = [{}, {}, {}, {}]\n",
                toml_string("/usr/bin/python3"),
                toml_string("-u"),
                toml_string(&server_script.to_string_lossy()),
                toml_string(&counter.to_string_lossy()),
                toml_string(&server_log.to_string_lossy()),
            ));
            eprintln!(
                "D-C start nonce={nonce} repo={} home={}",
                env.repo.display(),
                env.home.display()
            );

            let (mut runtime, ready_file) = env.serve("runtime-a");
            ready(&ready_file, &mut runtime).await;
            let runtime_pid = runtime.id();
            let client = LocalSocketRuntimeClient::connect(&env.socket())
                .await
                .unwrap();
            let session = create_goal_session(
                &client,
                "Call the configured MCP tool once, then determine its real outcome from the \
                 workspace.",
            )
            .await;
            eprintln!("D-C session={} runtime_pid={runtime_pid}", session.as_str());
            let mut events = client.subscribe_session(&session);
            submit(&client, &session, &dc_prompt()).await;
            let turn = collect_turn(&mut events, Duration::from_secs(420)).await;
            eprintln!(
                "D-C turn outcome={} tools={:?}",
                turn.outcome,
                turn.calls
                    .iter()
                    .map(|call| call.name.as_str())
                    .collect::<Vec<_>>()
            );
            eprintln!("D-C delivery: {}", turn.delivery_text());
            if let Some(evidence) = &evidence {
                export_turn_debug(evidence, &env, &session, &turn).await;
            }

            assert_ne!(
                turn.outcome, "timeout",
                "the real model turn never reached a terminal state: {turn:?}"
            );
            assert!(
                !turn.outcome.starts_with("failed"),
                "the real turn failed: {turn:?}"
            );

            // The real MCP server saw exactly one tools/call and committed one
            // side effect before dropping the transport.
            let calls = facts_named(&server_log, "call_started");
            assert_eq!(
                calls.len(),
                1,
                "the MCP server must see exactly one tools/call: {calls:#?}"
            );
            let call = &calls[0];
            assert_eq!(call["mode"], serde_json::json!("write_then_drop"), "{call}");
            let mcp_request_id = call["request_id"]
                .as_u64()
                .expect("the MCP server logs the JSON-RPC request id");
            let mcp_server_pid = call["pid"]
                .as_u64()
                .expect("the MCP server logs its own pid") as u32;
            assert_eq!(
                side_effect_count(&counter),
                1,
                "exactly one real side-effect byte"
            );
            assert_eq!(facts_named(&server_log, "side_effect").len(), 1);
            wait_until(
                || Ok(!alive(mcp_server_pid)),
                "the dropped MCP server process to exit",
                Duration::from_secs(30),
            )
            .await
            .unwrap();

            // Durable tool calls: capability first, then exactly one MCP call.
            let rows = durable_rows(
                &env.db(),
                session.as_str(),
                &["tool_call_started", "tool_call_finished"],
            )
            .await;
            let parsed = rows
                .iter()
                .filter_map(|row| durable_event(row).map(|event| (row, event)))
                .collect::<Vec<_>>();
            let mut capability_index = None;
            let mut mcp_index = None;
            let mut mcp_call_id = None;
            for (index, (_, event)) in parsed.iter().enumerate() {
                if event["type"].as_str() != Some("tool_call_started") {
                    continue;
                }
                let name = event["payload"]["name"].as_str().unwrap_or_default();
                let arguments = event["payload"]["arguments"].as_str().unwrap_or_default();
                if name == "capability"
                    && arguments.contains("external_tools")
                    && arguments.contains("enable")
                {
                    capability_index = Some(index);
                }
                if name == "mcp__controlled__write" {
                    assert!(
                        mcp_index.is_none(),
                        "the model called the MCP tool more than once: {parsed:#?}"
                    );
                    mcp_index = Some(index);
                    mcp_call_id = event["payload"]["call_id"].as_str().map(str::to_string);
                }
            }
            let capability_index = capability_index
                .expect("the model must enable external_tools through `capability` first");
            let mcp_index = mcp_index.expect("the model never called mcp__controlled__write");
            assert!(
                capability_index < mcp_index,
                "external_tools must be enabled before the MCP call: {parsed:#?}"
            );
            let mcp_call_id = mcp_call_id.expect("the MCP call carries a call id");
            let finished = parsed
                .iter()
                .filter_map(|(_, event)| {
                    (event["type"].as_str() == Some("tool_call_finished")
                        && event["payload"]["call_id"].as_str() == Some(mcp_call_id.as_str()))
                    .then_some(event)
                })
                .next()
                .expect("the runtime must persist a result for the MCP call");
            let preview = finished["payload"]["preview"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            assert!(
                contains_any(
                    &preview,
                    &[
                        "MCP 调用结果未知",
                        "结果未知",
                        "UNKNOWN_OUTCOME",
                        "outcome is unknown"
                    ]
                ),
                "the durable MCP result must be Unknown, not success/failure: {preview}"
            );
            assert!(
                !preview.contains("ok:write_then_drop"),
                "the runtime must not fabricate success: {preview}"
            );

            // The model must explore the real side-effect evidence itself.
            let inspection = parsed.iter().skip(mcp_index + 1).any(|(_, event)| {
                if event["type"].as_str() != Some("tool_call_started") {
                    return false;
                }
                let name = event["payload"]["name"].as_str().unwrap_or_default();
                let arguments = event["payload"]["arguments"].as_str().unwrap_or_default();
                matches!(name, "read_file" | "run_command" | "shell_command")
                    && (arguments.contains("r3-mcp-counter") || arguments.contains("r3-mcp-server"))
            });
            assert!(
                inspection,
                "the model must inspect the real counter/log after the Unknown call: {parsed:#?}"
            );

            // No second MCP call and no second side effect.
            let mcp_calls = parsed
                .iter()
                .filter(|(_, event)| {
                    event["type"].as_str() == Some("tool_call_started")
                        && event["payload"]["name"].as_str() == Some("mcp__controlled__write")
                })
                .count();
            assert_eq!(mcp_calls, 1, "a retried MCP call is a D-C failure");
            assert_eq!(side_effect_count(&counter), 1);
            assert_eq!(facts_named(&server_log, "call_started").len(), 1);

            // The delivery must report the real side effect and the uncertainty.
            let delivery = turn.delivery_text();
            assert!(
                contains_any(
                    &delivery,
                    &[
                        "side effect",
                        "side-effect",
                        "副作用",
                        "counter",
                        "r3-mcp-counter"
                    ]
                ),
                "the delivery must report the real side effect: {delivery}"
            );
            assert!(
                contains_any(
                    &delivery,
                    &[
                        "unknown",
                        "Unknown",
                        "uncertain",
                        "未知",
                        "未确认",
                        "not confirmed"
                    ]
                ),
                "the delivery must report the uncertainty, not success/failure: {delivery}"
            );
            assert!(
                delivery.contains('1') || delivery.to_lowercase().contains("one"),
                "the delivery must state the real one-time side effect: {delivery}"
            );

            eprintln!(
                "D-C PASS nonce={nonce} session={} mcp_call={mcp_call_id} \
                 mcp_request_id={mcp_request_id} mcp_server_pid={mcp_server_pid} \
                 runtime_pid={runtime_pid} side_effect_count=1 tools_call_count=1",
                session.as_str()
            );
            if let Some(evidence) = &evidence {
                export_common(evidence, &env, &["runtime-a-stderr.txt"]);
                export_durable(evidence, &env, &session).await;
                evidence.copy("mcp_server.py", &server_script);
                evidence.copy("mcp_server.jsonl", &server_log);
                evidence.copy("mcp_counter.txt", &counter);
                evidence.copy("config.toml", &env.home.join("config.toml"));
                let mut turn_record = turn_json(&turn);
                turn_record["session_id"] = serde_json::json!(session.as_str());
                evidence.write_json("model_turn.json", &turn_record);
                evidence.write_json(
                    "SYSTEM.json",
                    &serde_json::json!({
                        "verdict": "D-C PASS",
                        "nonce": nonce,
                        "session_id": session.as_str(),
                        "runtime_pid": runtime_pid,
                        "turn_outcome": turn.outcome,
                        "mcp_tool_call_id": mcp_call_id,
                        "mcp_request_id": mcp_request_id,
                        "mcp_server_pid": mcp_server_pid,
                        "side_effect_count": side_effect_count(&counter),
                        "tools_call_count": facts_named(&server_log, "call_started").len(),
                        "durable_result_unknown": true,
                        "no_retry": true,
                    }),
                );
                evidence.redact();
                eprintln!("D-C evidence={}", evidence.dir.display());
            }
            let _ = runtime.kill();
            let _ = runtime.wait();
            stop_isolated_host(&env.host_state());
        },
    );
}

// ───────────────────────────────────────────────────────────────────────────
// D-D — stop REQUESTED/NOT confirmed, then a runtime SIGKILL during the stop.
// ───────────────────────────────────────────────────────────────────────────

fn dd_prompt() -> String {
    format!(
        "We are validating a real stop of a background service. Use the real tools exactly as \
described; never simulate a step and never guess a result.\n\n\
1. The workspace root contains `{SERVICE_SCRIPT}`. Start it in the background exactly ONCE with \
`run_command`: program \"sh\", args [\"{SERVICE_SCRIPT}\"], background true, \
background_lifetime \"session\". It ignores SIGTERM, writes its pid to `r3-service.pid`, prints \
READY, and keeps running. Do not run it in the foreground and do not start it twice.\n\
2. Wait for it to be ready: call `wait_task` on the task id returned by the start call, with \
until \"match\", pattern \"READY\", timeout_seconds 120.\n\
3. Then call `kill_task` with that exact task id and report the exact result. Do not start \
another process."
    )
}

fn dd_resume_prompt(task_id: &str) -> String {
    format!(
        "The runtime was killed by SIGKILL immediately after `kill_task` returned \"Stop \
REQUESTED ... NOT confirmed\". The Execution Host kept observing the stop. This is the SAME \
session and the SAME background task `{task_id}`; do not start or kill anything.\n\n\
1. Call `get_task` and/or `wait_task` with exactly this task id: `{task_id}`.\n\
2. Report the exact task id, the real current status (killed, exited, or unknown), the signal \
or exit code, and whether the stop is now confirmed.\n\
3. Explicitly distinguish the earlier \"Stop REQUESTED / NOT confirmed\" result from the current \
fact. Use only real tool results; do not guess."
    )
}

#[derive(Default)]
struct StopWatch {
    calls: Vec<ToolCall>,
    results: Vec<ToolResult>,
    task_before: Option<serde_json::Value>,
    pid_alive_at_kill_start: Option<bool>,
    kill_call_id: Option<String>,
    kill_arguments: Option<String>,
    kill_preview: Option<String>,
    text: String,
    outcome: String,
}

/// Watch the first real turn until the model's `kill_task` completes. On the
/// kill start it snapshots the Host's Running record, so the pid/start
/// identity are captured BEFORE the stop mutates them.
async fn watch_kill_task(
    events: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>,
    host_state: &Path,
    session: &leveler_core::SessionId,
    budget: Duration,
) -> StopWatch {
    let mut watch = StopWatch::default();
    let mut kill_started: Option<(String, String)> = None;
    let deadline = Instant::now() + budget;
    loop {
        let now = Instant::now();
        if now >= deadline {
            watch.outcome = "timeout".into();
            return watch;
        }
        let wait = (deadline - now).min(Duration::from_secs(30));
        match tokio::time::timeout(wait, events.recv()).await {
            Err(_) => continue,
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped))) => {
                eprintln!("  [D-D event stream lagged, skipped {skipped}]");
                continue;
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                watch.outcome = "stream_closed".into();
                return watch;
            }
            Ok(Ok(event)) => match event {
                RuntimeEvent::ToolCallStarted {
                    id,
                    name,
                    arguments,
                    ..
                } => {
                    if name == "kill_task" {
                        kill_started = Some((id.to_string(), arguments.clone()));
                        watch.task_before = host_tasks(host_state).into_iter().find(|task| {
                            task["snapshot"]["owner_scope"].as_str() == Some(session.as_str())
                                && task["snapshot"]["args"].as_array().is_some_and(|args| {
                                    args.iter().any(|arg| arg.as_str() == Some(SERVICE_SCRIPT))
                                })
                        });
                        watch.pid_alive_at_kill_start = watch
                            .task_before
                            .as_ref()
                            .and_then(|task| task["snapshot"]["pid"].as_u64())
                            .map(|pid| alive(pid as u32));
                    }
                    watch.calls.push(ToolCall {
                        id: id.to_string(),
                        name,
                        arguments,
                    });
                }
                RuntimeEvent::ToolCallCompleted {
                    id,
                    ok,
                    preview,
                    exit_code,
                    ..
                } => {
                    watch.results.push(ToolResult {
                        id: id.to_string(),
                        ok,
                        preview: preview.clone(),
                        exit_code,
                    });
                    if kill_started
                        .as_ref()
                        .is_some_and(|(call_id, _)| call_id == id.as_str())
                    {
                        watch.kill_call_id = Some(id.to_string());
                        watch.kill_arguments = kill_started.as_ref().map(|(_, args)| args.clone());
                        watch.kill_preview = Some(preview);
                        return watch;
                    }
                }
                RuntimeEvent::AssistantTextDelta { delta, .. } => watch.text.push_str(&delta),
                RuntimeEvent::TurnCompleted => {
                    watch.outcome = "completed".into();
                    return watch;
                }
                RuntimeEvent::TurnCompletedWithWarnings { reason, .. } => {
                    watch.outcome = format!("completed_with_warnings: {reason}");
                    return watch;
                }
                RuntimeEvent::TurnAnswered => {
                    watch.outcome = "answered".into();
                    return watch;
                }
                RuntimeEvent::TurnPaused { reason } => {
                    watch.outcome = format!("paused: {reason}");
                    return watch;
                }
                RuntimeEvent::TurnIncomplete { reason } => {
                    watch.outcome = format!("incomplete: {reason}");
                    return watch;
                }
                RuntimeEvent::TurnTruncated { error } => {
                    watch.outcome = format!("truncated: {error}");
                    return watch;
                }
                RuntimeEvent::TurnFailed { error, .. } => {
                    watch.outcome = format!("failed: {error}");
                    return watch;
                }
                RuntimeEvent::TurnCancelled => {
                    watch.outcome = "interrupted".into();
                    return watch;
                }
                RuntimeEvent::TaskCancelled => {
                    watch.outcome = "task_cancelled".into();
                    return watch;
                }
                _ => {}
            },
        }
    }
}

#[test]
#[ignore = "real API: needs DEEPSEEK_API_KEY and spends real tokens + a real SIGKILL"]
fn real_model_stop_requested_recovery_after_sigkill() {
    leveler_test_support::bounded_test(
        "real_model_stop_requested_recovery_after_sigkill",
        Duration::from_secs(600),
        || async {
            assert!(
                std::env::var("DEEPSEEK_API_KEY").is_ok_and(|value| !value.trim().is_empty()),
                "DEEPSEEK_API_KEY must carry the real gateway key"
            );
            let nonce = format!(
                "r3dd-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            );
            let env = Environment::new(&nonce);
            std::fs::write(env.repo_path(SERVICE_SCRIPT), service_script()).unwrap();
            let evidence = Evidence::new("d-d");
            eprintln!(
                "D-D start nonce={nonce} repo={} home={}",
                env.repo.display(),
                env.home.display()
            );

            // ── P1: the model starts the service and requests a stop ────────
            let (mut runtime_a, ready_a) = env.serve("runtime-a");
            ready(&ready_a, &mut runtime_a).await;
            let runtime_a_pid = runtime_a.id();
            let client_a = LocalSocketRuntimeClient::connect(&env.socket())
                .await
                .unwrap();
            let session = create_goal_session(
                &client_a,
                "Start the controlled service, stop it, and report the real stop state.",
            )
            .await;
            let host_state = env.host_state();
            eprintln!(
                "D-D session={} runtime_a_pid={runtime_a_pid}",
                session.as_str()
            );
            let mut events_a = client_a.subscribe_session(&session);
            submit(&client_a, &session, &dd_prompt()).await;
            let watch = watch_kill_task(
                &mut events_a,
                &host_state,
                &session,
                Duration::from_secs(300),
            )
            .await;
            let kill_preview = watch.kill_preview.clone().unwrap_or_else(|| {
                panic!(
                    "the model never produced a kill_task result: calls={:?} outcome={}",
                    watch
                        .calls
                        .iter()
                        .map(|call| call.name.as_str())
                        .collect::<Vec<_>>(),
                    watch.outcome
                )
            });
            eprintln!("D-D kill_task result: {kill_preview}");
            assert!(
                kill_preview.contains("Stop REQUESTED"),
                "kill_task must report the stop as requested, not done: {kill_preview}"
            );
            assert!(
                kill_preview.contains("NOT confirmed"),
                "kill_task must explicitly say the termination is not confirmed: {kill_preview}"
            );
            let kill_arguments = watch.kill_arguments.clone().expect("kill_task arguments");
            let kill_task_id = serde_json::from_str::<serde_json::Value>(&kill_arguments)
                .ok()
                .and_then(|args| args["task_id"].as_str().map(str::to_string))
                .expect("kill_task names the task id");
            let task_before = watch
                .task_before
                .clone()
                .expect("the host's Running record before the stop");
            let task_id = task_before["snapshot"]["id"].as_str().unwrap().to_string();
            assert_eq!(kill_task_id, task_id);
            let background_pid = task_before["snapshot"]["pid"]
                .as_u64()
                .expect("the service's real pid") as u32;
            let start_identity = task_before["snapshot"]["start_identity"]
                .as_str()
                .expect("the durable start identity")
                .to_string();
            // The Execution Host starts with the first real background task, so
            // its readiness is only knowable now.
            let supervisor_pid = host_pid(&host_state);

            // Preserve the real pre-kill facts before any assertion can fail.
            if let Some(evidence) = &evidence {
                evidence.copy("tasks_before_kill.json", &host_state.join("tasks.json"));
                evidence.copy("r3-service.sh", &env.repo_path(SERVICE_SCRIPT));
                evidence.copy("r3-service.pid", &env.repo_path("r3-service.pid"));
                let snapshot = client_a.snapshot(&session).await.unwrap();
                evidence.write_json(
                    "snapshot_before_kill.json",
                    &serde_json::to_value(&snapshot).unwrap(),
                );
                export_durable(evidence, &env, &session).await;
                evidence.write_json(
                    "first_turn_watch.json",
                    &serde_json::json!({
                        "kill_call_id": watch.kill_call_id,
                        "kill_arguments": watch.kill_arguments,
                        "kill_preview": watch.kill_preview,
                        "pid_alive_at_kill_start": watch.pid_alive_at_kill_start,
                        "calls": watch.calls.iter().map(|call| serde_json::json!({
                            "id": call.id,
                            "name": call.name,
                            "arguments": call.arguments,
                        })).collect::<Vec<_>>(),
                        "assistant_text": watch.text,
                    }),
                );
                evidence.redact();
            }

            assert_eq!(
                task_before["snapshot"]["status"].as_str(),
                Some("Running"),
                "the host must still show the service Running when the stop is requested: \
                 {task_before}"
            );
            // Liveness is sampled when kill_task STARTS. The stop escalation
            // (SIGTERM, a 100 ms grace, SIGKILL) can already have killed the
            // process by the time kill_task returns its requested-unconfirmed
            // result; the runtime has still not confirmed the stop.
            assert_eq!(
                watch.pid_alive_at_kill_start,
                Some(true),
                "the service must have been a real live process when the stop was requested"
            );
            assert!(
                task_before["snapshot"]["log"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("READY"),
                "the service must have printed READY before the stop: {task_before}"
            );
            let service_pid = env.read_repo("r3-service.pid").trim().to_string();
            assert_eq!(
                service_pid,
                background_pid.to_string(),
                "the service's own pid must be the host's process pid"
            );
            assert_ne!(runtime_a_pid, supervisor_pid);

            // ── P2: SIGKILL the runtime while the stop is still requested ───
            runtime_a.kill().expect("SIGKILL the runtime");
            let status = runtime_a.wait().unwrap();
            assert_eq!(
                status.signal(),
                Some(9),
                "the runtime must really die by SIGKILL: {status}"
            );
            assert!(!alive(runtime_a_pid), "the runtime process must be gone");
            assert!(alive(supervisor_pid), "the Execution Host must survive");

            // ── P3: the host reaches the real terminal stop fact ───────────
            wait_until(
                || {
                    let tasks = host_tasks(&host_state);
                    Ok(find_task(&tasks, &task_id).is_some_and(|task| {
                        task["result"]["result_id"]
                            .as_str()
                            .is_some_and(|id| !id.is_empty())
                            && matches!(
                                task["snapshot"]["status"].as_str(),
                                Some("Killed" | "Exited")
                            )
                    }))
                },
                "the host to commit the real terminal stop fact",
                Duration::from_secs(60),
            )
            .await
            .unwrap();
            let finished = find_task(&host_tasks(&host_state), &task_id).unwrap();
            let host_status = finished["snapshot"]["status"].as_str().unwrap().to_string();
            let real_signal = finished["snapshot"]["signal"].as_i64();
            let real_exit = finished["snapshot"]["exit_code"].as_i64();
            assert!(
                real_signal == Some(9) || real_exit.is_some(),
                "the host must retain a real termination fact: {finished}"
            );
            let result_id = finished["result"]["result_id"]
                .as_str()
                .expect("the host commits a result id")
                .to_string();
            assert_eq!(
                finished["snapshot"]["pid"].as_u64(),
                Some(background_pid as u64),
                "the terminal fact must belong to the original process"
            );
            assert_eq!(
                finished["snapshot"]["start_identity"].as_str(),
                Some(start_identity.as_str())
            );
            eprintln!(
                "D-D host terminal task={task_id} status={host_status} signal={real_signal:?} \
                 exit={real_exit:?} result_id={result_id}"
            );

            // ── P4: a NEW real runtime resumes the SAME session ────────────
            let (mut runtime_b, ready_b) = env.serve("runtime-b");
            ready(&ready_b, &mut runtime_b).await;
            let runtime_b_pid = runtime_b.id();
            assert_ne!(runtime_a_pid, runtime_b_pid);
            let client_b = LocalSocketRuntimeClient::connect(&env.socket())
                .await
                .unwrap();
            let recovered = client_b.snapshot(&session).await.unwrap();
            if let Some(evidence) = &evidence {
                evidence.write_json(
                    "snapshot_after_restart.json",
                    &serde_json::to_value(&recovered).unwrap(),
                );
            }
            let record = recovered
                .background_tasks
                .iter()
                .find(|record| record.task_id == task_id)
                .unwrap_or_else(|| panic!("no recovered record for {task_id}: {recovered:?}"));
            assert!(
                record.output.contains("READY"),
                "the recovered record must retain the service output: {record:?}"
            );
            let mut events_b = client_b.subscribe_session(&session);
            acknowledge_crash_window(&client_b, &session, &mut events_b).await;
            submit(&client_b, &session, &dd_resume_prompt(&task_id)).await;
            let turn = collect_turn(&mut events_b, Duration::from_secs(420)).await;
            eprintln!(
                "D-D resumed runtime_pid={runtime_b_pid} turn outcome={} tools={:?}",
                turn.outcome,
                turn.calls
                    .iter()
                    .map(|call| call.name.as_str())
                    .collect::<Vec<_>>()
            );
            eprintln!("D-D delivery: {}", turn.delivery_text());
            if let Some(evidence) = &evidence {
                export_turn_debug(evidence, &env, &session, &turn).await;
            }
            assert_ne!(
                turn.outcome, "timeout",
                "the resumed real model turn never reached a terminal state: {turn:?}"
            );
            assert!(
                !turn.outcome.starts_with("failed"),
                "the resumed real turn failed: {turn:?}"
            );

            // The model itself must observe the original id after the restart.
            let observation = turn
                .calls_named(&["get_task", "wait_task"])
                .filter(|call| call.arguments.contains(&task_id))
                .collect::<Vec<_>>();
            assert!(
                !observation.is_empty(),
                "the resumed model never observed the original task id: {turn:?}"
            );
            let result = observation
                .iter()
                .filter_map(|call| turn.result_for(&call.id))
                .find(|result| result.preview.contains(&task_id))
                .unwrap_or_else(|| {
                    panic!("the resumed task result did not name {task_id}: {turn:?}")
                });
            let observed_status = if let Some(signal) = real_signal {
                assert!(
                    result.preview.contains(&format!("signal: Some({signal})")),
                    "the resumed result must carry the real signal {signal}: {}",
                    result.preview
                );
                "killed"
            } else if let Some(code) = real_exit {
                assert!(
                    result.preview.contains(&format!("exit_code: Some({code})")),
                    "the resumed result must carry the real exit {code}: {}",
                    result.preview
                );
                "exited"
            } else {
                "unknown"
            };
            assert!(
                result.preview.contains(observed_status),
                "the resumed result must state the real status `{observed_status}`: {}",
                result.preview
            );

            // The delivery must distinguish requested from confirmed/unknown.
            let delivery = turn.delivery_text();
            let lower = delivery.to_lowercase();
            assert!(
                delivery.contains(&task_id),
                "the resumed delivery must name the original task id: {delivery}"
            );
            assert!(
                lower.contains(observed_status),
                "the resumed delivery must state the real status `{observed_status}`: {delivery}"
            );
            assert!(
                lower.contains("requested"),
                "the resumed delivery must recall the requested state: {delivery}"
            );
            assert!(
                contains_any(
                    &lower,
                    &["confirmed", "unknown", "未知", "未确认", "not confirmed"]
                ),
                "the resumed delivery must distinguish confirmed from unknown: {delivery}"
            );

            // ── P5: the terminal fact belongs to the SAME original process ──
            let tasks = host_tasks(&host_state);
            assert_eq!(
                tasks
                    .iter()
                    .filter(|task| task["snapshot"]["id"].as_str() == Some(&task_id))
                    .count(),
                1,
                "the host archive must hold exactly one record for this identity"
            );
            let final_task = find_task(&tasks, &task_id).unwrap();
            assert_eq!(
                final_task["snapshot"]["pid"].as_u64(),
                Some(background_pid as u64)
            );
            assert_eq!(
                final_task["snapshot"]["start_identity"].as_str(),
                Some(start_identity.as_str())
            );
            assert_eq!(
                final_task["result"]["result_id"].as_str(),
                Some(result_id.as_str())
            );

            eprintln!(
                "D-D PASS nonce={nonce} session={} task={task_id} runtime_a_pid={runtime_a_pid} \
                 runtime_b_pid={runtime_b_pid} supervisor_pid={supervisor_pid} \
                 background_pid={background_pid} start_identity={start_identity} \
                 signal={real_signal:?} exit={real_exit:?} status={host_status} \
                 result_id={result_id}",
                session.as_str()
            );
            if let Some(evidence) = &evidence {
                export_common(
                    evidence,
                    &env,
                    &["runtime-a-stderr.txt", "runtime-b-stderr.txt"],
                );
                evidence.copy("r3-service.sh", &env.repo_path(SERVICE_SCRIPT));
                evidence.copy("r3-service.pid", &env.repo_path("r3-service.pid"));
                export_durable(evidence, &env, &session).await;
                let mut turn_record = turn_json(&turn);
                turn_record["session_id"] = serde_json::json!(session.as_str());
                turn_record["task_id"] = serde_json::json!(task_id);
                evidence.write_json("resumed_model_turn.json", &turn_record);
                evidence.write_json(
                    "SYSTEM.json",
                    &serde_json::json!({
                        "verdict": "D-D PASS",
                        "nonce": nonce,
                        "session_id": session.as_str(),
                        "runtime_a_pid": runtime_a_pid,
                        "runtime_b_pid": runtime_b_pid,
                        "runtime_sigkill_signal": 9,
                        "supervisor_pid": supervisor_pid,
                        "task_id": task_id,
                        "background_pid": background_pid,
                        "process_start_identity": start_identity,
                        "kill_task_call_id": watch.kill_call_id,
                        "kill_task_result": watch.kill_preview,
                        "host_terminal_status": host_status,
                        "host_terminal_signal": real_signal,
                        "host_terminal_exit_code": real_exit,
                        "result_id": result_id,
                        "resumed_turn_outcome": turn.outcome,
                    }),
                );
                evidence.redact();
                eprintln!("D-D evidence={}", evidence.dir.display());
            }

            let _ = runtime_b.kill();
            let _ = runtime_b.wait();
            stop_isolated_host(&env.host_state());
        },
    );
}
