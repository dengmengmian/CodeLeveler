//! M8: a REAL Runtime SIGKILL while a REAL MCP `tools/call` is in flight, then a
//! REAL restart/resume on the same session.
//!
//! Every process here is real:
//! - the runtime is `leveler serve` (spawned by this test),
//! - the MCP server is a standalone Python stdio process spawned by the
//!   runtime from the isolated `$LEVELER_HOME/config.toml`,
//! - the crash is a real SIGKILL of the runtime process.
//!
//! The only scripted piece is the model: an SSE mock makes the runtime call the
//! configured MCP tool. The MCP transport, the server's side-effect counter and
//! request-id log, the runtime's durable `tool_call_started` row, the crash, and
//! the resume are all real.
#![cfg(unix)]

use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use leveler_client_protocol::{ClientCommand, InteractiveRuntimeClient, RuntimeEvent};
use leveler_local_transport::{CreateSessionRequest, LocalSocketRuntimeClient};
use leveler_storage::{Database, EventStore as _};
use leveler_test_support::{ManagedChild, MockResponse, MockServer};

const PYTHON: &str = "/usr/bin/python3";

/// A real stdio MCP server. `write_then_hang` commits one side effect, records
/// the JSON-RPC request id and its own pid, then never answers the call while
/// the main thread keeps reading stdin. When the runtime dies its stdin closes
/// and the process exits, so no MCP server outlives the test.
const SERVER_SCRIPT: &str = r#"import json,sys,os,time,threading
counter=sys.argv[1]; log_path=sys.argv[2]
def log(fact):
    with open(log_path,'a') as f: f.write(json.dumps(fact)+'\n'); f.flush()
def send(o): print(json.dumps(o),flush=True)
log({'event':'server_start','pid':os.getpid()})
def hang(req_id,mode):
    log({'event':'call_started','request_id':req_id,'pid':os.getpid(),'mode':mode})
    with open(counter,'a') as f: f.write('x'); f.flush()
    log({'event':'side_effect','request_id':req_id,'pid':os.getpid()})
    time.sleep(300)
for line in sys.stdin:
    line=line.strip()
    if not line: continue
    req=json.loads(line); m=req.get('method'); i=req.get('id')
    if m=='notifications/cancelled':
        log({'event':'cancelled','request_id':req.get('params',{}).get('requestId')})
        continue
    if m=='notifications/initialized': continue
    if m=='initialize':
        res={'protocolVersion':'2024-11-05','capabilities':{'tools':{}},'serverInfo':{'name':'controlled','version':'1'}}
    elif m=='tools/list':
        res={'tools':[{'name':'write','description':'','inputSchema':{'type':'object','properties':{'mode':{'type':'string'}}}}]}
    elif m=='tools/call':
        mode=req['params'].get('arguments',{}).get('mode','write')
        if mode=='write_then_hang':
            threading.Thread(target=hang,args=(i,mode),daemon=True).start()
            continue
        res={'content':[{'type':'text','text':'ok:'+mode}],'isError':False}
    else:
        res={}
    if i is not None: send({'jsonrpc':'2.0','id':i,'result':res})
"#;

/// Escape a path for a TOML basic string.
fn toml_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

struct Environment {
    _temp: tempfile::TempDir,
    home: PathBuf,
    config: PathBuf,
    repo: PathBuf,
    counter: PathBuf,
    server_log: PathBuf,
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

        let counter = temp.path().join("mcp-counter.txt");
        let server_log = temp.path().join("mcp-server.jsonl");
        let script = temp.path().join("controlled_server.py");
        std::fs::write(&script, SERVER_SCRIPT).unwrap();
        let mcp_block = format!(
            "\n[[mcp_servers]]\nname = \"controlled\"\ncommand = {python}\nargs = [{u}, {script}, {counter}, {log}]\n",
            python = toml_string(PYTHON),
            u = toml_string("-u"),
            script = toml_string(&script.to_string_lossy()),
            counter = toml_string(&counter.to_string_lossy()),
            log = toml_string(&server_log.to_string_lossy()),
        );
        std::fs::write(
            home.join("config.toml"),
            format!("default_model = \"mock/m\"\n\n[update]\nauto_update = false\n{mcp_block}"),
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
            counter,
            server_log,
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

/// The model enables the ExternalTools pack, which is what exposes configured
/// MCP tools on the following request.
fn capability_enable_response() -> MockResponse {
    let arguments = serde_json::json!({"action": "enable", "id": "external_tools"});
    sse(vec![
        serde_json::json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "call-cap-enable", "function": {"name": "capability", "arguments": arguments.to_string()}}]}}]}),
        serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
    ])
}

/// The model asks the runtime for the configured MCP tool. The server holds the
/// call open after committing its side effect.
fn mcp_call_response(call_id: &str) -> MockResponse {
    let arguments = serde_json::json!({ "mode": "write_then_hang" });
    sse(vec![
        serde_json::json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": call_id, "function": {"name": "mcp__controlled__write", "arguments": arguments.to_string()}}]}}]}),
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

async fn wait_until<F>(mut probe: F, what: &str) -> Result<(), String>
where
    F: FnMut() -> Result<bool, String>,
{
    let deadline = Instant::now() + Duration::from_secs(40);
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

fn read_server_facts(path: &Path) -> Vec<serde_json::Value> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn facts_named(path: &Path, event: &str) -> Vec<serde_json::Value> {
    read_server_facts(path)
        .into_iter()
        .filter(|fact| fact["event"] == event)
        .collect()
}

fn event_session(id: &str) -> leveler_core::SessionId {
    leveler_core::SessionId::new(id)
}

/// Wait until the real session DB has a durable `tool_call_started` for
/// `call_id`; return its row (turn id + payload).
async fn wait_tool_started(
    db: &Path,
    session: &str,
    call_id: &str,
) -> leveler_storage::EventRecord {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(db) = Database::connect(db).await
            && let Ok(rows) = db
                .load_by_types(&event_session(session), &["tool_call_started"])
                .await
            && let Some(row) = rows.into_iter().find(|row| {
                serde_json::from_str::<serde_json::Value>(&row.payload)
                    .ok()
                    .and_then(|envelope| envelope.get("payload").cloned())
                    .is_some_and(|payload| payload["call_id"].as_str() == Some(call_id))
            })
        {
            return row;
        }
        assert!(
            Instant::now() < deadline,
            "the runtime never persisted tool_call_started for {call_id}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_server_fact(path: &Path, event: &str, what: &str) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(40);
    loop {
        if let Some(fact) = facts_named(path, event).into_iter().next() {
            return fact;
        }
        assert!(
            Instant::now() < deadline,
            "the MCP server never recorded {what}; facts={:?}",
            read_server_facts(path)
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn side_effect_count(counter: &Path) -> usize {
    std::fs::read_to_string(counter)
        .map(|text| text.matches('x').count())
        .unwrap_or(0)
}

/// Wait for one runtime event matching `predicate`.
async fn wait_event(
    events: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>,
    what: &str,
    mut predicate: impl FnMut(&RuntimeEvent) -> bool,
) -> RuntimeEvent {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        assert!(
            Instant::now() < deadline,
            "the runtime never emitted {what}"
        );
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap_or_else(|_| panic!("event stream stalled waiting for {what}"))
            .expect("event stream open");
        if predicate(&event) {
            return event;
        }
    }
}

fn request_body_messages(body: &str) -> Vec<serde_json::Value> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| value.get("messages").and_then(|m| m.as_array()).cloned())
        .unwrap_or_default()
}

#[test]
fn runtime_sigkill_with_dangling_mcp_call_resumes_as_unknown() {
    leveler_test_support::bounded_test(
        "runtime_sigkill_with_dangling_mcp_call_resumes_as_unknown",
        Duration::from_secs(240),
        || async {
            let call_id = "call-mcp-m8";
            // Request 1: enable ExternalTools. Request 2: the real MCP call,
            // which hangs after its side effect. Request 3: the resumed turn.
            let mock = MockServer::start(vec![
                capability_enable_response(),
                mcp_call_response(call_id),
                final_response("The interrupted MCP call stayed unknown; I did not replay it."),
            ])
            .await;
            let env = Environment::new(&mock.base_url());

            // ---- P1: create a real Goal session on the real runtime ---------
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
                    goal: "Call the configured MCP tool and report what it did".into(),
                    model: None,
                    mode: leveler_client_protocol::PermissionProfile::FullAccess,
                })
                .await
                .unwrap()
                .session
                .id;

            // ---- P2: the real tools/call is in flight ----------------------
            let submitted = client_a
                .send(ClientCommand::SubmitMessage {
                    session_id: session.clone(),
                    content: "Call mcp__controlled__write with mode write_then_hang.".into(),
                    attachments: vec![],
                })
                .await;
            submitted.expect("the goal turn is accepted");
            let started = wait_server_fact(
                &env.server_log,
                "call_started",
                "the runtime's real tools/call",
            )
            .await;
            assert_eq!(started["mode"], "write_then_hang", "{started}");
            let mcp_request_id = started["request_id"]
                .as_u64()
                .expect("the MCP server saw the JSON-RPC tools/call id");
            assert!(mcp_request_id > 0, "{started}");
            let mcp_server_pid = started["pid"]
                .as_u64()
                .expect("the MCP server logged its pid") as u32;
            wait_until(
                || Ok(side_effect_count(&env.counter) == 1),
                "the MCP server's single side effect",
            )
            .await
            .unwrap();
            // The side effect already happened before the crash.
            assert_eq!(side_effect_count(&env.counter), 1);

            // The runtime must have persisted the call's start durably.
            let started_row = wait_tool_started(&env.db(), session.as_str(), call_id).await;
            let turn_id = started_row
                .turn_id
                .clone()
                .expect("tool_call_started carries the turn id");
            let envelope: serde_json::Value = serde_json::from_str(&started_row.payload).unwrap();
            let payload = envelope
                .get("payload")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            assert_eq!(payload["call_id"], call_id, "{payload}");
            assert_eq!(payload["name"], "mcp__controlled__write", "{payload}");
            let runtime_pid = runtime_a.id();
            assert!(alive(runtime_pid), "the runtime must be running");
            assert!(
                alive(mcp_server_pid),
                "the MCP server must still be alive mid-call"
            );

            // ---- P3: the real SIGKILL --------------------------------------
            runtime_a.kill().expect("SIGKILL the runtime");
            let status = runtime_a.wait().unwrap();
            assert_eq!(
                status.signal(),
                Some(9),
                "the runtime must really die by SIGKILL: {status}"
            );
            assert!(!alive(runtime_pid), "the runtime process must be gone");
            // The orphaned MCP server sees stdin EOF and exits; its side effect
            // is already durable on disk.
            wait_until(
                || Ok(!alive(mcp_server_pid)),
                "the orphaned MCP server to exit after stdin EOF",
            )
            .await
            .unwrap();
            assert_eq!(
                side_effect_count(&env.counter),
                1,
                "the crash only happened after exactly one side effect"
            );

            // ---- P4: restart and resume on the same session -----------------
            let (mut runtime_b, ready_b) = env.serve("runtime-b");
            ready(&ready_b, &mut runtime_b).await;
            let client_b = LocalSocketRuntimeClient::connect(&env.socket())
                .await
                .unwrap();
            let mut events_b = client_b.subscribe_session(&session);

            // The resumed turn reconciles the crash window first: it must stop
            // for explicit acknowledgement instead of replaying the call.
            let first = client_b
                .send(ClientCommand::SubmitMessage {
                    session_id: session.clone(),
                    content: "Continue after the crash without replaying the call.".into(),
                    attachments: vec![],
                })
                .await;
            // A dangling side-effecting call must stop the resumed turn for
            // explicit acknowledgement; the runtime must not proceed silently.
            match &first {
                Err(leveler_client_protocol::ClientError::RecoveryRequired(message)) => {
                    assert!(
                        message.contains(call_id),
                        "the recovery stop must name the interrupted call: {message}"
                    );
                    assert!(
                        message.contains("mcp__controlled__write"),
                        "the recovery stop must name the MCP tool: {message}"
                    );
                }
                Ok(()) => {
                    let event = wait_event(
                        &mut events_b,
                        "the recovery stop from the resumed turn",
                        |event| {
                            matches!(
                                event,
                                RuntimeEvent::TurnFailed {
                                    recovery: Some(_),
                                    ..
                                }
                            )
                        },
                    )
                    .await;
                    let RuntimeEvent::TurnFailed {
                        recovery: Some(recovery),
                        ..
                    } = event
                    else {
                        unreachable!()
                    };
                    assert_eq!(recovery.call_id, call_id, "recovery names our call");
                    assert_eq!(
                        recovery.tool, "mcp__controlled__write",
                        "recovery names the MCP tool"
                    );
                }
                other => panic!(
                    "a dangling side-effecting MCP call must stop for explicit acknowledgement, \
                     not proceed silently: {other:?}"
                ),
            }

            // The runtime reattached the configured MCP server for this session;
            // it must be a NEW real process, not the crashed one.
            wait_until(
                || {
                    Ok(facts_named(&env.server_log, "server_start")
                        .iter()
                        .any(|fact| fact["pid"].as_u64() != Some(mcp_server_pid as u64)))
                },
                "the resumed runtime to spawn a fresh MCP server",
            )
            .await
            .unwrap();
            let resumed_server_pid = facts_named(&env.server_log, "server_start")
                .iter()
                .filter_map(|fact| fact["pid"].as_u64())
                .find(|pid| *pid != mcp_server_pid as u64)
                .expect("the resumed runtime's MCP server pid")
                as u32;
            assert_ne!(resumed_server_pid, mcp_server_pid);

            // ---- P5: explicit acknowledgement, then the model sees Unknown --
            client_b
                .send(ClientCommand::AcknowledgeCrashWindow {
                    session_id: session.clone(),
                })
                .await
                .unwrap();
            let ack = wait_event(&mut events_b, "the crash-window acknowledgement", |event| {
                matches!(event, RuntimeEvent::CrashWindowAcknowledged { .. })
            })
            .await;
            let RuntimeEvent::CrashWindowAcknowledged { closed, .. } = ack else {
                unreachable!()
            };
            assert_eq!(closed, 1, "exactly the one dangling MCP call is closed");

            let resumed = client_b
                .send(ClientCommand::SubmitMessage {
                    session_id: session.clone(),
                    content: "Continue with the recovery result.".into(),
                    attachments: vec![],
                })
                .await;
            resumed.expect("the acknowledged session resumes");
            wait_event(&mut events_b, "the resumed turn to finish", |event| {
                matches!(
                    event,
                    RuntimeEvent::TurnCompleted
                        | RuntimeEvent::TurnCompletedWithWarnings { .. }
                        | RuntimeEvent::TurnIncomplete { .. }
                        | RuntimeEvent::TurnFailed { .. }
                        | RuntimeEvent::TurnAnswered
                        | RuntimeEvent::TurnPaused { .. }
                        | RuntimeEvent::TurnCancelled
                        | RuntimeEvent::TaskCancelled
                )
            })
            .await;

            // Inspect exactly what the model was sent after resume.
            wait_until(
                || Ok(mock.request_count() >= 2),
                "the resumed model request",
            )
            .await
            .unwrap();
            let bodies = mock.request_bodies().await;
            let body = bodies
                .last()
                .expect("the resumed request body was recorded")
                .clone();
            std::fs::write(env.home.join("m8-resumed-request.json"), &body).unwrap();
            let messages = request_body_messages(&body);
            let assistant_call = messages.iter().find(|message| {
                message["tool_calls"].as_array().is_some_and(|calls| {
                    calls.iter().any(|call| {
                        call["id"].as_str() == Some(call_id)
                            && call["function"]["name"].as_str() == Some("mcp__controlled__write")
                    })
                })
            });
            assert!(
                assistant_call.is_some(),
                "the resumed conversation must keep the interrupted call: {messages:#?}"
            );
            let tool_result = messages
                .iter()
                .find(|message| {
                    message["role"].as_str() == Some("tool")
                        && message["tool_call_id"].as_str() == Some(call_id)
                })
                .unwrap_or_else(|| {
                    panic!("the resumed request has no tool result for {call_id}: {messages:#?}")
                });
            let content = tool_result["content"].as_str().unwrap_or_default();
            assert!(
                content.contains("outcome is unknown") || content.contains("结果未知"),
                "the dangling call must surface as an Unknown result, not success: {content}"
            );
            assert!(
                !content.contains("ok:write_then_hang") && !content.contains("isError: false"),
                "the recovered result must not fabricate success: {content}"
            );

            // ---- P6: no automatic replay ------------------------------------
            assert_eq!(
                side_effect_count(&env.counter),
                1,
                "the resumed runtime must not replay the MCP call"
            );
            assert_eq!(
                facts_named(&env.server_log, "call_started").len(),
                1,
                "the MCP server saw exactly one tools/call across both runtimes"
            );

            eprintln!(
                "M8 PASS session={} turn={turn_id} tool_call={call_id} mcp_request_id={mcp_request_id} \
                 mcp_server_pid={mcp_server_pid} resumed_mcp_server_pid={resumed_server_pid} \
                 runtime_pid={runtime_pid} runtime_signal=9 recovery_stop=RecoveryRequired \
                 ack_closed={closed} side_effect_count=1 model_requests={}",
                session.as_str(),
                mock.request_count()
            );
            export_evidence(
                &env,
                &session,
                &turn_id,
                call_id,
                mcp_request_id,
                mcp_server_pid,
                resumed_server_pid,
                runtime_pid,
                9,
                closed,
                &body,
                mock.request_count(),
            )
            .await;

            let _ = runtime_b.kill();
            let _ = runtime_b.wait();
        },
    );
}

/// Copy the real artifacts this run produced into a caller-chosen audit
/// directory. Test-only: the product never reads this path.
#[allow(clippy::too_many_arguments)]
async fn export_evidence(
    env: &Environment,
    session: &leveler_core::SessionId,
    turn_id: &str,
    call_id: &str,
    mcp_request_id: u64,
    mcp_server_pid: u32,
    resumed_mcp_server_pid: u32,
    runtime_pid: u32,
    runtime_signal: i32,
    ack_closed: u32,
    resumed_request_body: &str,
    model_requests: usize,
) {
    let Some(dir) = std::env::var_os("MCP_M8_EVIDENCE_DIR").map(PathBuf::from) else {
        return;
    };
    std::fs::create_dir_all(&dir).unwrap();
    for (source, name) in [
        (&env.server_log, "mcp_server_facts.jsonl"),
        (&env.counter, "mcp_side_effect_counter.txt"),
    ] {
        if source.exists() {
            std::fs::copy(source, dir.join(name)).unwrap();
        }
    }
    for name in [
        "config.toml",
        "runtime-a-stderr.txt",
        "runtime-b-stderr.txt",
    ] {
        let source = env.home.join(name);
        if source.exists() {
            std::fs::copy(&source, dir.join(name)).unwrap();
        }
    }
    let request = env.home.join("m8-resumed-request.json");
    if request.exists() {
        std::fs::copy(&request, dir.join("resumed_model_request.json")).unwrap();
    }
    // Durable rows, exactly as SQLite stored them. The payload column keeps the
    // event envelope, so the report can quote the real call/turn ids.
    let rows = match Database::connect(&env.db()).await {
        Ok(db) => db.load(session).await.unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let relevant: Vec<serde_json::Value> = rows
        .iter()
        .filter(|row| {
            matches!(
                row.event_type.as_str(),
                "task_started"
                    | "task_finished"
                    | "turn_started"
                    | "turn_finished"
                    | "tool_call_started"
                    | "tool_call_finished"
                    | "goal_checkpoint_created"
            )
        })
        .map(|row| {
            serde_json::json!({
                "event_type": row.event_type,
                "turn_id": row.turn_id,
                "sequence": row.sequence,
                "payload": row.payload,
            })
        })
        .collect();
    std::fs::write(
        dir.join("session_events.json"),
        serde_json::to_vec_pretty(&relevant).unwrap(),
    )
    .unwrap();
    let server_pids: Vec<u64> = facts_named(&env.server_log, "server_start")
        .iter()
        .filter_map(|fact| fact["pid"].as_u64())
        .collect();
    let summary = serde_json::json!({
        "session_id": session.as_str(),
        "crashed_turn_id": turn_id,
        "tool_call_id": call_id,
        "mcp_request_id": mcp_request_id,
        "mcp_server_pid": mcp_server_pid,
        "resumed_mcp_server_pid": resumed_mcp_server_pid,
        "mcp_server_pids_seen": server_pids,
        "runtime_pid": runtime_pid,
        "runtime_sigkill_signal": runtime_signal,
        "recovery_stop": "RecoveryRequired",
        "crash_window_ack_closed": ack_closed,
        "side_effect_count": side_effect_count(&env.counter),
        "mcp_call_started_facts": facts_named(&env.server_log, "call_started").len(),
        "model_request_count": model_requests,
        "resumed_request_carries_unknown_result": resumed_request_body
            .contains("outcome is unknown"),
    });
    std::fs::write(
        dir.join("summary.json"),
        serde_json::to_vec_pretty(&summary).unwrap(),
    )
    .unwrap();
    std::fs::write(
        dir.join("SYSTEM.txt"),
        format!(
            "session={}\ncrashed_turn={turn_id}\ntool_call={call_id}\n\
             mcp_request_id={mcp_request_id}\nmcp_server_pid={mcp_server_pid}\n\
             resumed_mcp_server_pid={resumed_mcp_server_pid}\nruntime_pid={runtime_pid}\n\
             runtime_sigkill_signal={runtime_signal}\nrecovery_stop=RecoveryRequired\n\
             crash_window_ack_closed={ack_closed}\nside_effect_count=1\n\
             mcp_call_started_facts=1\nmodel_request_count={model_requests}\n\
             server_log={}\nside_effect_counter={}\n",
            session.as_str(),
            env.server_log.display(),
            env.counter.display(),
        ),
    )
    .unwrap();
}
