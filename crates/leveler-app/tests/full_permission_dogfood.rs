//! Full Permission Contract dogfood — the CodeLeveler-owned, real-runtime half
//! of the Release Gate.
//!
//! Run by `eval/scripts/full_permission_gate.py` (Release Gate, L1). It is
//! `#[ignore]`d so the ordinary `cargo test --workspace` stays fast; the gate
//! runs it explicitly with `--ignored`. The `full_permission_lifecycle.rs`
//! regression tests are the always-on version of the same invariant.
//!
//! The six-case acceptance matrix (`Full` × rm -rf / git reset --hard / kill /
//! network, and `Auto` × dangerous / benign) is the Release Gate half here and
//! the fast always-on half in `full_permission_acceptance.rs`.
//! `rm_out_of_workspace`, `git_reset_hard`, `process_signal` and
//! `network_public` are the Full cases; an `auto_asks` operation on the
//! Auto→Full path is the Auto-positive case.
//!
//! WHAT IT PROVES (the long-term product invariant):
//!
//! ```text
//! effective_permission_mode == Full
//!     => CodeLeveler-owned ApprovalRequested count == 0
//!     && no pending permission approval in any snapshot
//!     && no PermissionDecision::Deny reaches the operation
//! ```
//!
//! It runs real sessions through the production runtime — `Application` +
//! `InProcessRuntimeClient`, the exact type the daemon serves — and asserts the
//! invariant with a GLOBAL checker that sees every event on a session, not with
//! per-case assertions only. Any new tool or classifier that forgets the Full
//! bypass is caught by the checker, not by a case that happened to cover it.
//!
//! Entry paths covered (DOGFOOD 6): a session created in Full (A), Auto that
//! switches to Full while an approval waits (B), a Full session reconnected
//! (C), and a Full session resumed after its runtime restarted (D).
//!
//! Scenarios:
//!   * DOGFOOD 1 — Full direct: representative dangerous operations, 0 asks.
//!   * DOGFOOD 2 — Auto pending → Full: supersede, re-admit, continue.
//!   * DOGFOOD 3 — stale Approve/Deny after the switch are refused.
//!   * DOGFOOD 4 — reconnect/snapshot never restores the superseded question.
//!   * DOGFOOD 5 — a Full session resumes Full and still asks nothing.
//!   * DOGFOOD 6 — operation × entry-path matrix under the global checker.
//!   * INVARIANT B — the selected mode survives create/persist/restart/resume.
//!   * INVARIANT C — one live authority per session: a stale turn snapshot (a
//!     turn staged before a `SetPermissionProfile`) must not write the older
//!     mode back over the switch; `/clear` and a restart keep the row, the
//!     snapshot and the live cell on one value.
//!   * AUTO MATRIX — Auto asks for a dangerous command and does not ask for an
//!     ordinary one, both on real sessions.
//!
//! MCP is reported UNMEASURED here: no MCP fixture server is wired into this
//! harness. It is covered at the policy layer by
//! `is_mcp_tool` and the Full contract test; an honest UNMEASURED is not a
//! pass. Wire a fixture server before claiming the gate covers MCP.

#[path = "support/observed_command.rs"]
mod observed_command;
use observed_command::ObservedSettings;

use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    ApprovalDecision, ClientCommand, InteractiveRuntimeClient, PermissionProfile as WirePermission,
    RuntimeEvent, UiApprovalRequest, UiPendingInteraction, UiSessionSnapshot,
};
use leveler_core::SessionId;
use leveler_execution::PermissionProfile;
use leveler_local_transport::{
    CreateSessionRequest, CreateWorkspaceSelection, LocalRuntimeService,
};
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_test_support::{MockResponse, MockServer};
use tokio::sync::broadcast;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Pass,
    Fail,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Pass => "PASS",
            Status::Fail => "FAIL",
        }
    }
}

#[derive(Debug, Clone)]
struct Case {
    id: String,
    status: Status,
    detail: String,
}

#[derive(Debug, Clone)]
struct Violation {
    case: String,
    detail: String,
}

/// The invariant checker. It watches every event on one session and holds the
/// mode in force, so an approval observed while the mode is Full is a fatal
/// violation regardless of which case produced it.
struct ContractWatcher {
    case: String,
    mode: WirePermission,
    violations: Vec<Violation>,
}

impl ContractWatcher {
    fn new(case: impl Into<String>, mode: WirePermission) -> Self {
        Self {
            case: case.into(),
            mode,
            violations: Vec::new(),
        }
    }

    fn observe(&mut self, event: &RuntimeEvent) {
        if let RuntimeEvent::SessionUpdated { session } = event {
            self.mode = session.mode;
        }
        if matches!(event, RuntimeEvent::ApprovalRequested { .. })
            && self.mode == WirePermission::FullAccess
        {
            self.violations.push(Violation {
                case: self.case.clone(),
                detail: format!(
                    "FULL_PERMISSION_CONTRACT_VIOLATION: ApprovalRequested while mode=Full: {event:?}"
                ),
            });
        }
    }

    fn check_snapshot(&mut self, snapshot: &UiSessionSnapshot) {
        if snapshot.mode != WirePermission::FullAccess {
            return;
        }
        for item in &snapshot.pending_interactions {
            if let UiPendingInteraction::Approval(request) = item {
                self.violations.push(Violation {
                    case: self.case.clone(),
                    detail: format!(
                        "FULL_PERMISSION_CONTRACT_VIOLATION: pending permission approval \
                         while mode=Full: {request:?}"
                    ),
                });
            }
        }
    }
}

// ── harness ──────────────────────────────────────────────────────────────────

fn isolate_global_config() {
    use std::sync::OnceLock;
    static HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    let dir = HOME.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        // A minimal MCP fixture so DOGFOOD 1 can run a real MCP tool call
        // through the production registry. It answers `initialize`,
        // `tools/list` and `tools/call`; it never touches the network.
        let script = dir.path().join("mcp_fixture.py");
        std::fs::write(
            &script,
            r#"import json,sys
for line in sys.stdin:
    try:
        req=json.loads(line)
    except Exception:
        continue
    mid=req.get('id'); method=req.get('method')
    if mid is None:
        continue
    if method=='initialize':
        out={'protocolVersion':'2024-11-05','capabilities':{},'serverInfo':{'name':'fixture','version':'1'}}
    elif method=='tools/list':
        out={'tools':[{'name':'probe','description':'dogfood probe','inputSchema':{'type':'object'}}]}
    elif method=='tools/call':
        out={'content':[{'type':'text','text':'dogfood-ok'}]}
    else:
        out={}
    print(json.dumps({'jsonrpc':'2.0','id':mid,'result':out}),flush=True)
"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            format!(
                "[[mcp_servers]]\nname = \"fixture\"\ncommand = \"python3\"\nargs = [\"-u\", {:?}]\n",
                script.display().to_string()
            ),
        )
        .unwrap();
        dir
    });
    unsafe {
        std::env::set_var("LEVELER_HOME", dir.path());
    }
}

fn sse(frames: Vec<String>) -> MockResponse {
    let mut body = String::new();
    for frame in frames {
        body.push_str("data: ");
        body.push_str(&frame);
        body.push_str("\n\n");
    }
    body.push_str("data: [DONE]\n\n");
    MockResponse::Sse { body }
}

fn text(content: &str) -> MockResponse {
    sse(vec![
        serde_json::json!({"choices": [{"delta": {"content": content}, "finish_reason": "stop"}]})
            .to_string(),
    ])
}

fn tool_call(name: &str, arguments: serde_json::Value) -> MockResponse {
    sse(vec![
        serde_json::json!({
            "choices": [{
                "index": 0,
                "delta": {"tool_calls": [{
                    "index": 0,
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": name, "arguments": arguments.to_string()},
                }]},
            }]
        })
        .to_string(),
        serde_json::json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]})
            .to_string(),
    ])
}

fn write_config(root: &std::path::Path, base_url: &str) {
    isolate_global_config();
    std::fs::create_dir_all(root.join("configs/providers")).unwrap();
    std::fs::create_dir_all(root.join("configs/models")).unwrap();
    std::fs::write(
        root.join("configs/providers/mock.yaml"),
        format!("id: mock\nprotocol: openai_chat\nbase_url: {base_url}\n"),
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

fn layout(root: &std::path::Path) -> Layout {
    Layout::from_parts(root.to_path_buf(), root.join("configs"), root.join("state"))
}

/// Initialize a throwaway git repo so push/reset have something to act on.
fn init_git(root: &std::path::Path) {
    let run = |args: &[&str]| {
        let _ = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_AUTHOR_NAME", "dogfood")
            .env("GIT_AUTHOR_EMAIL", "dogfood@example.invalid")
            .env("GIT_COMMITTER_NAME", "dogfood")
            .env("GIT_COMMITTER_EMAIL", "dogfood@example.invalid")
            .output();
    };
    run(&["init", "-q"]);
    std::fs::write(root.join("tracked.txt"), "x\n").unwrap();
    run(&["add", "tracked.txt"]);
    run(&["commit", "-q", "-m", "init", "--no-gpg-sign"]);
    // A local bare remote, so a real `git push` cannot touch a network host.
    let remote = root.join("remote.git");
    let _ = std::process::Command::new("git")
        .args(["init", "--bare", "-q"])
        .arg(&remote)
        .output();
    run(&["remote", "add", "origin", remote.to_str().unwrap()]);
}

/// One representative Full-sensitive operation. `Auto asks` is the operation's
/// Auto behaviour, used to decide whether the Auto→Full path has a question to
/// supersede.
struct Op {
    id: &'static str,
    tool: &'static str,
    arguments: serde_json::Value,
    auto_asks: bool,
}

fn operations(workroot: &std::path::Path, home: &std::path::Path) -> Vec<Op> {
    let outside = workroot.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let outside_file = outside.join("victim.txt");
    std::fs::write(&outside_file, "x").unwrap();
    let home_file = home.join("home-write.txt");
    let credential = home.join(".env");
    std::fs::write(&credential, "TOKEN=x\n").unwrap();
    vec![
        Op {
            id: "rm_out_of_workspace",
            tool: "run_command",
            arguments: serde_json::json!({
                "program": "rm", "args": ["-rf", outside_file.display().to_string()]
            }),
            auto_asks: true,
        },
        Op {
            id: "write_outside_workspace",
            tool: "write_file",
            arguments: serde_json::json!({
                "path": outside.join("created.txt").display().to_string(), "content": "x"
            }),
            auto_asks: true,
        },
        Op {
            id: "write_home",
            tool: "write_file",
            arguments: serde_json::json!({
                "path": home_file.display().to_string(), "content": "x"
            }),
            auto_asks: true,
        },
        Op {
            id: "read_credential_path",
            tool: "read_file",
            arguments: serde_json::json!({"path": credential.display().to_string()}),
            auto_asks: true,
        },
        Op {
            id: "git_push",
            tool: "run_command",
            arguments: serde_json::json!({"program": "git", "args": ["push", "origin", "HEAD"]}),
            auto_asks: true,
        },
        Op {
            id: "git_reset_hard",
            tool: "run_command",
            arguments: serde_json::json!({"program": "git", "args": ["reset", "--hard"]}),
            auto_asks: true,
        },
        Op {
            id: "process_spawn",
            tool: "run_command",
            arguments: serde_json::json!({"program": "echo", "args": ["dogfood"]}),
            auto_asks: false,
        },
        Op {
            id: "process_signal",
            tool: "run_command",
            arguments: serde_json::json!({"program": "kill", "args": ["-0", "1"]}),
            auto_asks: false,
        },
        Op {
            id: "background_task",
            tool: "run_command",
            arguments: serde_json::json!({
                "program": "sleep", "args": ["0.2"], "background": true
            }),
            auto_asks: false,
        },
        Op {
            id: "network_public",
            tool: "web_fetch",
            arguments: serde_json::json!({"url": "https://example.invalid/"}),
            auto_asks: false,
        },
        Op {
            id: "request_permissions",
            tool: "request_permissions",
            arguments: serde_json::json!({"action": "dogfood probe", "network": true}),
            auto_asks: true,
        },
        Op {
            id: "mcp_invocation",
            tool: "mcp__fixture__probe",
            arguments: serde_json::json!({}),
            // Not part of the Auto→Full supersede path: under a confined
            // profile the ExternalTools capability must be enabled first, so
            // the model would not reach the call in one round. The MCP
            // approval path itself is covered by unit tests (`is_mcp_tool`).
            auto_asks: false,
        },
    ]
}

// ── observation helpers ──────────────────────────────────────────────────────

/// Everything one case needs to observe, accumulated by a single consumer so no
/// event is discarded while waiting for a different one (the first harness bug:
/// `await_full` consumed the `ToolCallCompleted` that arrived before
/// `SessionUpdated`, so the case then "never completed").
#[derive(Default)]
struct Signals {
    approval: Option<UiApprovalRequest>,
    full_seen: bool,
    tool_completed: bool,
    terminal: bool,
    turn_error: Option<String>,
}

fn record(signals: &mut Signals, watcher: &mut ContractWatcher, event: &RuntimeEvent) {
    if std::env::var_os("FULL_PERMISSION_TRACE").is_some() {
        eprintln!("[dogfood-event] {event:?}");
    }
    watcher.observe(event);
    match event {
        RuntimeEvent::ApprovalRequested { request } => {
            if signals.approval.is_none() {
                signals.approval = Some(request.clone());
            }
        }
        RuntimeEvent::SessionUpdated { session } if session.mode == WirePermission::FullAccess => {
            signals.full_seen = true;
        }
        RuntimeEvent::ToolCallCompleted { .. } => signals.tool_completed = true,
        RuntimeEvent::TurnCompleted
        | RuntimeEvent::TurnCompletedWithWarnings { .. }
        | RuntimeEvent::TurnAnswered
        | RuntimeEvent::TurnCancelled
        | RuntimeEvent::TaskCancelled => signals.terminal = true,
        // A clean stop that did NOT reach a successful terminal (budget,
        // unresolved goal, output limit). Under Full an allowed operation must
        // answer, so this is a failure to report, not a pass.
        RuntimeEvent::TurnIncomplete { reason } => {
            signals.terminal = true;
            signals.turn_error = Some(format!("turn incomplete: {reason}"));
        }
        RuntimeEvent::TurnTruncated { error } => {
            signals.terminal = true;
            signals.turn_error = Some(format!("turn truncated: {error}"));
        }
        RuntimeEvent::TurnFailed { error, .. } => {
            signals.terminal = true;
            signals.turn_error = Some(error.clone());
        }
        _ => {}
    }
}

/// Consume events until `predicate` holds, recording every one into the watcher
/// and signals. Returns whether the predicate held when the wait ended.
async fn pump_until<F: Fn(&Signals) -> bool>(
    events: &mut broadcast::Receiver<RuntimeEvent>,
    watcher: &mut ContractWatcher,
    signals: &mut Signals,
    predicate: F,
    timeout: Duration,
) -> bool {
    if predicate(signals) {
        return true;
    }
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return predicate(signals);
        }
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Ok(event)) => {
                record(signals, watcher, &event);
                if predicate(signals) {
                    return true;
                }
            }
            Ok(Err(_)) => return predicate(signals),
            Err(_) => return predicate(signals),
        }
    }
}

fn snapshot_mode(snapshot: &UiSessionSnapshot) -> WirePermission {
    snapshot.mode
}

// ── the case runner ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Path {
    FullDirect,
    AutoToFull,
    Reconnect,
    Resume,
}

impl Path {
    fn as_str(self) -> &'static str {
        match self {
            Path::FullDirect => "A_full_direct",
            Path::AutoToFull => "B_auto_to_full",
            Path::Reconnect => "C_reconnect",
            Path::Resume => "D_resume",
        }
    }
}

/// Run one operation through one entry path under the global checker.
async fn run_case(path: Path, op: &Op, workroot: &std::path::Path) -> (Case, Vec<Violation>) {
    let case_id = format!("{}/{}", path.as_str(), op.id);
    let mut violations = Vec::new();

    // Enough model responses for: the operation's tool call, and the
    // `update_goal` that ends the turn (interactive turns are goal turns: a
    // plain answer does not terminate them). The server repeats the last
    // response once exhausted.
    let server = MockServer::start(vec![
        tool_call(op.tool, op.arguments.clone()),
        text("dogfood complete"),
    ])
    .await;

    let root = workroot.join(format!("case-{}", sanitize(&case_id)));
    std::fs::create_dir_all(&root).unwrap();
    write_config(&root, &server.base_url());
    init_git(&root);

    let app = Arc::new(Application::assemble(layout(&root)).unwrap());
    let model = ModelRef::new("mock", "m");
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        model.clone(),
        if path == Path::AutoToFull {
            PermissionProfile::Assisted
        } else {
            PermissionProfile::FullAccess
        },
        false,
    ));

    // A session created in Full persists Full; the Auto path starts Assisted
    // and is switched below. This is the daemon product path.
    let create_mode = if path == Path::AutoToFull {
        WirePermission::Assisted
    } else {
        WirePermission::FullAccess
    };
    let bootstrap = client
        .create_session(CreateSessionRequest {
            request_id: None,
            collaboration: leveler_local_transport::CollaborationMode::Chat,
            workspace: CreateWorkspaceSelection::RuntimeDefault,
            goal: "full permission dogfood".to_string(),
            model: Some(model.clone()),
            mode: create_mode,
            approval_policy: leveler_client_protocol::ApprovalPolicy::Interactive,
        })
        .await
        .unwrap();
    let session_id: SessionId = bootstrap.session.id.clone();

    let mut events = client.subscribe_session(&session_id);
    let mut watcher = ContractWatcher::new(case_id.clone(), snapshot_mode(&bootstrap.session));
    let mut signals = Signals::default();
    if watcher.mode == WirePermission::FullAccess {
        signals.full_seen = true;
    }

    client
        .send(ClientCommand::SubmitMessage {
            session_id: session_id.clone(),
            content: format!("dogfood {}", op.id),
            attachments: vec![],
        })
        .await
        .unwrap();

    if path == Path::AutoToFull {
        // Wait for the question Auto must ask for this operation.
        let asked = pump_until(
            &mut events,
            &mut watcher,
            &mut signals,
            |s| s.approval.is_some() || s.terminal,
            DEFAULT_TIMEOUT,
        )
        .await;
        let approval = match signals.approval.clone() {
            Some(approval) if asked => approval,
            _ => {
                // The operation never produced a question. That is a failure
                // only when Auto is known to ask for it; otherwise it is an
                // honest pass (there was nothing to supersede) with a note.
                let status = if op.auto_asks {
                    Status::Fail
                } else {
                    Status::Pass
                };
                return (
                    Case {
                        id: case_id.clone(),
                        status,
                        detail: if op.auto_asks {
                            "Auto never asked, so there was no question to supersede".to_string()
                        } else {
                            "Auto did not need to ask for this operation".to_string()
                        },
                    },
                    violations,
                );
            }
        };
        // Supersede by switching to Full.
        client
            .send_observed(ClientCommand::SetPermissionProfile {
                session_id: session_id.clone(),
                mode: WirePermission::FullAccess,
            })
            .await
            .unwrap();
        pump_until(
            &mut events,
            &mut watcher,
            &mut signals,
            |s| s.full_seen,
            DEFAULT_TIMEOUT,
        )
        .await;
        // A stale Deny must be refused — DOGFOOD 3.
        let stale = client
            .send(ClientCommand::ApprovalDecision {
                request_id: approval.id.clone(),
                decision: ApprovalDecision::Deny,
            })
            .await;
        if stale.is_ok() {
            violations.push(Violation {
                case: case_id.clone(),
                detail: "FULL_PERMISSION_CONTRACT_VIOLATION: a stale request was still answerable"
                    .to_string(),
            });
        }
        let after = client.snapshot(&session_id).await.unwrap();
        watcher.check_snapshot(&after);
    }

    // The operation must reach a terminal turn, never a permission wait.
    let settled = pump_until(
        &mut events,
        &mut watcher,
        &mut signals,
        |s| s.terminal,
        DEFAULT_TIMEOUT,
    )
    .await;
    if let Some(error) = &signals.turn_error {
        violations.push(Violation {
            case: case_id.clone(),
            detail: format!("turn failed under Full: {error}"),
        });
    } else if !settled {
        violations.push(Violation {
            case: case_id.clone(),
            detail: "operation never settled under Full".to_string(),
        });
    }

    let snapshot = client.snapshot(&session_id).await.unwrap();
    watcher.check_snapshot(&snapshot);

    match path {
        Path::Reconnect => {
            // DOGFOOD 4: a fresh subscriber + snapshot (what a reconnecting TUI
            // renders) must not restore the question.
            let mut reconnected = client.subscribe_session(&session_id);
            let mut reconnect_signals = Signals::default();
            let _ = pump_until(
                &mut reconnected,
                &mut watcher,
                &mut reconnect_signals,
                |_| false,
                Duration::from_millis(300),
            )
            .await;
            let restored = client.snapshot(&session_id).await.unwrap();
            watcher.check_snapshot(&restored);
        }
        Path::Resume => {
            // DOGFOOD 5: drop the runtime and resume from durable state. Full
            // must survive, and still ask nothing.
            drop(events);
            drop(client);
            drop(app);

            let app = Arc::new(Application::assemble(layout(&root)).unwrap());
            let client = Arc::new(InProcessRuntimeClient::new(
                app.clone(),
                model.clone(),
                PermissionProfile::Assisted, // deliberately wrong: the row decides
                false,
            ));
            let resumed = client.snapshot(&session_id).await.unwrap();
            if resumed.mode != WirePermission::FullAccess {
                violations.push(Violation {
                    case: case_id.clone(),
                    detail: format!(
                        "FULL_PERMISSION_CONTRACT_VIOLATION: Full did not survive resume (mode={:?})",
                        resumed.mode
                    ),
                });
            }
            watcher.check_snapshot(&resumed);

            // And a second Full-sensitive operation still asks nothing.
            let mut events = client.subscribe_session(&session_id);
            let mut resumed_signals = Signals {
                full_seen: resumed.mode == WirePermission::FullAccess,
                ..Signals::default()
            };
            client
                .send(ClientCommand::SubmitMessage {
                    session_id: session_id.clone(),
                    content: "dogfood resume".to_string(),
                    attachments: vec![],
                })
                .await
                .unwrap();
            let settled = pump_until(
                &mut events,
                &mut watcher,
                &mut resumed_signals,
                |s| s.terminal,
                DEFAULT_TIMEOUT,
            )
            .await;
            if !settled {
                violations.push(Violation {
                    case: case_id.clone(),
                    detail: "the resumed Full session's operation never settled".to_string(),
                });
            }
            let after = client.snapshot(&session_id).await.unwrap();
            watcher.check_snapshot(&after);
        }
        Path::FullDirect | Path::AutoToFull => {}
    }

    violations.extend(watcher.violations.clone());
    let status = if violations.is_empty() {
        Status::Pass
    } else {
        Status::Fail
    };
    (
        Case {
            id: case_id,
            status,
            detail: if violations.is_empty() {
                "no CodeLeveler approval under Full".to_string()
            } else {
                violations
                    .iter()
                    .map(|v| v.detail.clone())
                    .collect::<Vec<_>>()
                    .join("; ")
            },
        },
        violations,
    )
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

// ── INVARIANT B: permission-mode persistence ────────────────────────────────

struct ModeCase<'a> {
    cases: &'a mut Vec<Case>,
    violations: &'a mut Vec<Violation>,
}

impl ModeCase<'_> {
    /// Record one "mode must equal expected" assertion. A mismatch is
    /// `FULL_PERMISSION_MODE_DRIFT`, a fatal dogfood violation.
    fn check(
        &mut self,
        case: &str,
        where_: &str,
        expected: WirePermission,
        actual: WirePermission,
    ) {
        if actual == expected {
            self.cases.push(Case {
                id: case.to_string(),
                status: Status::Pass,
                detail: format!("{where_}: {actual:?}"),
            });
            return;
        }
        let detail =
            format!("FULL_PERMISSION_MODE_DRIFT: {where_}: expected {expected:?}, got {actual:?}");
        self.cases.push(Case {
            id: case.to_string(),
            status: Status::Fail,
            detail: detail.clone(),
        });
        self.violations.push(Violation {
            case: case.to_string(),
            detail,
        });
    }
}

async fn app_client(
    app: Arc<Application>,
    default_mode: PermissionProfile,
) -> Arc<InProcessRuntimeClient> {
    Arc::new(InProcessRuntimeClient::new(
        app,
        ModelRef::new("mock", "m"),
        default_mode,
        false,
    ))
}

/// DOGFOOD paths A–F: the mode a user selected must survive create, persist,
/// reconnect, restart and resume — in both the embedded and the daemon shape —
/// and an explicit CLI override must move it in both directions.
///
/// A client is deliberately constructed with the WRONG default mode: the runtime
/// must answer from the persisted row, not from a process default.
async fn run_mode_persistence(workroot: &std::path::Path) -> (Vec<Case>, Vec<Violation>) {
    let root = workroot.join("mode-persistence");
    std::fs::create_dir_all(&root).unwrap();
    write_config(&root, "http://127.0.0.1:1");
    let model = ModelRef::new("mock", "m");
    let mut cases = Vec::new();
    let mut violations = Vec::new();

    // A. embedded create in Full: the row is Full and the runtime reads it.
    let app = Arc::new(Application::assemble(layout(&root)).unwrap());
    let embedded_id = app
        .create_session_with_mode(&model, "embedded full", PermissionProfile::FullAccess)
        .await
        .unwrap();
    {
        let mut check = ModeCase {
            cases: &mut cases,
            violations: &mut violations,
        };
        let persisted = app
            .persisted_permission_profile(&embedded_id)
            .await
            .unwrap();
        check.check(
            "A_embedded_full_create/persisted",
            "embedded create persisted mode",
            WirePermission::FullAccess,
            match persisted {
                Some(PermissionProfile::FullAccess) => WirePermission::FullAccess,
                _ => WirePermission::RequestApproval,
            },
        );
        let runtime = app_client(app.clone(), PermissionProfile::Assisted).await;
        let effective = runtime.snapshot(&embedded_id).await.unwrap().mode;
        check.check(
            "A_embedded_full_create/effective",
            "embedded create effective mode",
            WirePermission::FullAccess,
            effective,
        );
    }
    drop(app);

    // B. embedded restart/resume keeps Full.
    let restarted = Arc::new(Application::assemble(layout(&root)).unwrap());
    {
        let mut check = ModeCase {
            cases: &mut cases,
            violations: &mut violations,
        };
        let runtime = app_client(restarted.clone(), PermissionProfile::RequestApproval).await;
        let effective = runtime.snapshot(&embedded_id).await.unwrap().mode;
        check.check(
            "B_embedded_restart_resume/effective",
            "embedded restart effective mode",
            WirePermission::FullAccess,
            effective,
        );
    }
    drop(restarted);

    // C. daemon create in Full.
    let app = Arc::new(Application::assemble(layout(&root)).unwrap());
    let daemon = app_client(app.clone(), PermissionProfile::Assisted).await;
    let daemon_id = daemon
        .create_session(CreateSessionRequest {
            request_id: None,
            collaboration: leveler_local_transport::CollaborationMode::Chat,
            workspace: CreateWorkspaceSelection::RuntimeDefault,
            goal: "daemon full".to_string(),
            model: Some(model.clone()),
            mode: WirePermission::FullAccess,
            approval_policy: leveler_client_protocol::ApprovalPolicy::Interactive,
        })
        .await
        .unwrap()
        .session
        .id;
    {
        let mut check = ModeCase {
            cases: &mut cases,
            violations: &mut violations,
        };
        let effective = daemon.snapshot(&daemon_id).await.unwrap().mode;
        check.check(
            "C_daemon_full_create/effective",
            "daemon create effective mode",
            WirePermission::FullAccess,
            effective,
        );
    }
    drop(daemon);
    drop(app);

    // D. daemon restart/resume keeps Full.
    let restarted = Arc::new(Application::assemble(layout(&root)).unwrap());
    let daemon2 = app_client(restarted.clone(), PermissionProfile::Assisted).await;
    {
        let mut check = ModeCase {
            cases: &mut cases,
            violations: &mut violations,
        };
        let effective = daemon2.snapshot(&daemon_id).await.unwrap().mode;
        check.check(
            "D_daemon_restart_resume/effective",
            "daemon restart effective mode",
            WirePermission::FullAccess,
            effective,
        );
    }

    // E. persisted Auto + explicit Full override (the interactive equivalent of
    //    `--permission full` on resume): effective and persisted both Full, and
    //    no permission approval exists under Full.
    let auto_id = restarted
        .create_session_with_mode(&model, "persisted auto", PermissionProfile::Assisted)
        .await
        .unwrap();
    daemon2
        .send_observed(ClientCommand::SetPermissionProfile {
            session_id: auto_id.clone(),
            mode: WirePermission::FullAccess,
        })
        .await
        .unwrap();
    {
        let mut check = ModeCase {
            cases: &mut cases,
            violations: &mut violations,
        };
        let snapshot = daemon2.snapshot(&auto_id).await.unwrap();
        check.check(
            "E_explicit_full_override/effective",
            "explicit override effective mode",
            WirePermission::FullAccess,
            snapshot.mode,
        );
        check.check(
            "E_explicit_full_override/persisted",
            "explicit override persisted mode",
            WirePermission::FullAccess,
            match restarted
                .persisted_permission_profile(&auto_id)
                .await
                .unwrap()
            {
                Some(PermissionProfile::FullAccess) => WirePermission::FullAccess,
                _ => WirePermission::RequestApproval,
            },
        );
        if let Some(UiPendingInteraction::Approval(request)) = snapshot
            .pending_interactions
            .iter()
            .find(|item| matches!(item, UiPendingInteraction::Approval(_)))
        {
            violations.push(Violation {
                case: "E_explicit_full_override/pending".to_string(),
                detail: format!(
                    "FULL_PERMISSION_CONTRACT_VIOLATION: approval pending under Full: {request:?}"
                ),
            });
        }
    }

    // F. persisted Full + resume with no explicit flag keeps Full.
    drop(daemon2);
    drop(restarted);
    let resumed = Arc::new(Application::assemble(layout(&root)).unwrap());
    let resumed_client = app_client(resumed.clone(), PermissionProfile::Assisted).await;
    {
        let mut check = ModeCase {
            cases: &mut cases,
            violations: &mut violations,
        };
        let effective = resumed_client.snapshot(&daemon_id).await.unwrap().mode;
        check.check(
            "F_resume_without_override/effective",
            "resume without override effective mode",
            WirePermission::FullAccess,
            effective,
        );
    }

    (cases, violations)
}

// ── INVARIANT C: one live permission authority per session ──────────────────

/// Build a session-scoped engine with the production sharing semantics.
async fn engine_for_scope(
    app: &Application,
    model: &ModelRef,
    mode: PermissionProfile,
    scope: &str,
) -> leveler_agent::coding::CodingRuntime {
    app.engine_for_session(
        model,
        mode,
        false,
        Arc::new(leveler_execution::AutoApprove),
        Arc::new(leveler_agent::AutoClarify),
        false,
        Some(scope),
    )
    .await
    .unwrap()
}

fn live_mode(engine: &leveler_agent::coding::CodingRuntime) -> PermissionProfile {
    engine.factory.tool_context.policy.mode()
}

/// The stale-turn-snapshot regression, plus the `/clear` and restart forms of
/// the same one-authority rule:
///
/// * a turn whose config snapshot predates a `SetPermissionProfile` must NOT
///   write the older mode back into the session's live cell (this is the exact
///   defect: the UI chip said `full` while the running turn kept authorizing
///   under `auto` and asked for approval on `rm -rf`);
/// * `/clear` opens a sibling under the requester's mode, with the durable row,
///   the runtime snapshot and the live cell on one value;
/// * a restart reads that value back from the row.
async fn run_live_profile_authority(workroot: &std::path::Path) -> (Vec<Case>, Vec<Violation>) {
    let root = workroot.join("live-profile-authority");
    std::fs::create_dir_all(&root).unwrap();
    write_config(&root, "http://127.0.0.1:1");
    let model = ModelRef::new("mock", "m");
    let mut cases = Vec::new();
    let mut violations = Vec::new();

    let app = Arc::new(Application::assemble(layout(&root)).unwrap());
    let session_id = app
        .create_session_with_mode(&model, "live authority", PermissionProfile::Assisted)
        .await
        .unwrap();
    // The client's default is deliberately wrong: the session row decides.
    let client = app_client(app.clone(), PermissionProfile::Assisted).await;

    // C1. A stale Assisted turn snapshot cannot downgrade the live Full.
    let first = engine_for_scope(
        &app,
        &model,
        PermissionProfile::Assisted,
        session_id.as_str(),
    )
    .await;
    // The product switch: one write moves the durable row and the live cell.
    client
        .send_observed(ClientCommand::SetPermissionProfile {
            session_id: session_id.clone(),
            mode: WirePermission::FullAccess,
        })
        .await
        .unwrap();
    let stale = engine_for_scope(
        &app,
        &model,
        PermissionProfile::Assisted,
        session_id.as_str(),
    )
    .await;
    let (stale_mode, first_mode) = (live_mode(&stale), live_mode(&first));
    if stale_mode == PermissionProfile::FullAccess && first_mode == PermissionProfile::FullAccess {
        cases.push(Case {
            id: "C1_stale_snapshot_keeps_live_full".to_string(),
            status: Status::Pass,
            detail: "a stale Assisted turn snapshot did not downgrade the live Full profile"
                .to_string(),
        });
    } else {
        let detail = format!(
            "FULL_PERMISSION_LIVE_AUTHORITY_VIOLATION: a stale turn snapshot moved the live \
             profile to {stale_mode:?} (already-running turn: {first_mode:?}) while the user \
             selected Full"
        );
        cases.push(Case {
            id: "C1_stale_snapshot_keeps_live_full".to_string(),
            status: Status::Fail,
            detail: detail.clone(),
        });
        violations.push(Violation {
            case: "C1_stale_snapshot_keeps_live_full".to_string(),
            detail,
        });
    }

    // C2. `/clear` opens a sibling under the requester's Full, and the row, the
    //     client snapshot and the live cell agree.
    let mut events = client.subscribe_session(&session_id);
    client
        .send(ClientCommand::NewSessionFor {
            requester_session_id: session_id.clone(),
        })
        .await
        .unwrap();
    let sibling = tokio::time::timeout(DEFAULT_TIMEOUT, async {
        loop {
            if let Ok(RuntimeEvent::SessionOpened { session }) = events.recv().await
                && session.id != session_id
            {
                return session;
            }
        }
    })
    .await;
    let sibling = match sibling {
        Ok(session) => session,
        Err(_) => {
            let detail = "FULL_PERMISSION_LIVE_AUTHORITY_VIOLATION: `/clear` never announced a \
                          sibling session"
                .to_string();
            cases.push(Case {
                id: "C2_clear_sibling_effective".to_string(),
                status: Status::Fail,
                detail: detail.clone(),
            });
            violations.push(Violation {
                case: "C2_clear_sibling_effective".to_string(),
                detail,
            });
            return (cases, violations);
        }
    };
    let persisted = app.persisted_permission_profile(&sibling.id).await.unwrap();
    let sibling_engine = engine_for_scope(
        &app,
        &model,
        PermissionProfile::FullAccess,
        sibling.id.as_str(),
    )
    .await;
    {
        let mut check = ModeCase {
            cases: &mut cases,
            violations: &mut violations,
        };
        check.check(
            "C2_clear_sibling/effective",
            "`/clear` sibling snapshot mode",
            WirePermission::FullAccess,
            sibling.mode,
        );
        check.check(
            "C2_clear_sibling/persisted",
            "`/clear` sibling persisted mode",
            WirePermission::FullAccess,
            match persisted {
                Some(PermissionProfile::FullAccess) => WirePermission::FullAccess,
                _ => WirePermission::RequestApproval,
            },
        );
        check.check(
            "C2_clear_sibling/live",
            "`/clear` sibling live cell",
            WirePermission::FullAccess,
            match live_mode(&sibling_engine) {
                PermissionProfile::FullAccess => WirePermission::FullAccess,
                _ => WirePermission::RequestApproval,
            },
        );
        if let Some(UiPendingInteraction::Approval(request)) = sibling
            .pending_interactions
            .iter()
            .find(|item| matches!(item, UiPendingInteraction::Approval(_)))
        {
            violations.push(Violation {
                case: "C2_clear_sibling/pending".to_string(),
                detail: format!(
                    "FULL_PERMISSION_CONTRACT_VIOLATION: approval pending under Full: {request:?}"
                ),
            });
        }
    }

    // C3. A restart reads Full back from the durable row.
    let sibling_id = sibling.id.clone();
    drop(sibling_engine);
    drop(client);
    let restarted = Arc::new(Application::assemble(layout(&root)).unwrap());
    let restarted_client = app_client(restarted.clone(), PermissionProfile::Assisted).await;
    {
        let mut check = ModeCase {
            cases: &mut cases,
            violations: &mut violations,
        };
        check.check(
            "C3_clear_sibling_restart/effective",
            "`/clear` sibling mode after restart",
            WirePermission::FullAccess,
            restarted_client.snapshot(&sibling_id).await.unwrap().mode,
        );
    }

    (cases, violations)
}

// ── Auto's own half of the permission contract ───────────────────────────────

struct AutoObserved {
    approval: Option<UiApprovalRequest>,
    terminal: bool,
}

/// Run one operation on a real Auto session and observe whether it asked. A
/// question is answered `Deny` so the turn settles; the observation is the
/// question itself, not the decision.
async fn run_auto_operation(
    root: &std::path::Path,
    tool: &str,
    arguments: serde_json::Value,
) -> AutoObserved {
    let server =
        MockServer::start(vec![tool_call(tool, arguments), text("dogfood complete")]).await;
    std::fs::create_dir_all(root).unwrap();
    write_config(root, &server.base_url());
    init_git(root);
    let app = Arc::new(Application::assemble(layout(root)).unwrap());
    let model = ModelRef::new("mock", "m");
    let client = app_client(app.clone(), PermissionProfile::Assisted).await;
    let session_id = client
        .create_session(CreateSessionRequest {
            request_id: None,
            collaboration: leveler_local_transport::CollaborationMode::Chat,
            workspace: CreateWorkspaceSelection::RuntimeDefault,
            goal: "auto matrix".to_string(),
            model: Some(model.clone()),
            mode: WirePermission::Assisted,
            approval_policy: leveler_client_protocol::ApprovalPolicy::Interactive,
        })
        .await
        .unwrap()
        .session
        .id;
    let mut events = client.subscribe_session(&session_id);
    let mut watcher = ContractWatcher::new("auto_matrix", WirePermission::Assisted);
    let mut signals = Signals::default();
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session_id.clone(),
            content: "auto matrix".to_string(),
            attachments: vec![],
        })
        .await
        .unwrap();
    // Wait for the question (if any) or the terminal.
    pump_until(
        &mut events,
        &mut watcher,
        &mut signals,
        |s| s.approval.is_some() || s.terminal,
        DEFAULT_TIMEOUT,
    )
    .await;
    if let Some(approval) = signals.approval.clone() {
        let _ = client
            .send(ClientCommand::ApprovalDecision {
                request_id: approval.id,
                decision: ApprovalDecision::Deny,
            })
            .await;
        pump_until(
            &mut events,
            &mut watcher,
            &mut signals,
            |s| s.terminal,
            DEFAULT_TIMEOUT,
        )
        .await;
    }
    AutoObserved {
        approval: signals.approval,
        terminal: signals.terminal,
    }
}

/// The Auto acceptance matrix: a dangerous command must ASK, an ordinary
/// development command must RUN without asking. Both are real runtime sessions.
async fn run_auto_matrix(workroot: &std::path::Path) -> (Vec<Case>, Vec<Violation>) {
    let mut cases = Vec::new();
    let mut violations = Vec::new();

    let dangerous_root = workroot.join("auto-dangerous");
    std::fs::create_dir_all(&dangerous_root).unwrap();
    let victim = dangerous_root.join("victim.txt");
    std::fs::write(&victim, "x").unwrap();
    let dangerous = run_auto_operation(
        &dangerous_root,
        "run_command",
        serde_json::json!({"program": "rm", "args": ["-rf", victim.display().to_string()]}),
    )
    .await;
    if dangerous.approval.is_some() {
        cases.push(Case {
            id: "A1_auto_dangerous_asks".to_string(),
            status: Status::Pass,
            detail: "Auto asked before the dangerous command ran".to_string(),
        });
    } else {
        let detail = "Auto ran a dangerous command without asking".to_string();
        cases.push(Case {
            id: "A1_auto_dangerous_asks".to_string(),
            status: Status::Fail,
            detail: detail.clone(),
        });
        violations.push(Violation {
            case: "A1_auto_dangerous_asks".to_string(),
            detail,
        });
    }
    if !victim.exists() {
        let detail =
            "Auto's denied dangerous command still ran (the victim file is gone)".to_string();
        violations.push(Violation {
            case: "A1_auto_dangerous_asks".to_string(),
            detail,
        });
    }

    let benign = run_auto_operation(
        &workroot.join("auto-benign"),
        "run_command",
        serde_json::json!({"program": "echo", "args": ["benign"]}),
    )
    .await;
    if benign.approval.is_none() && benign.terminal {
        cases.push(Case {
            id: "A2_auto_benign_does_not_ask".to_string(),
            status: Status::Pass,
            detail: "Auto ran the ordinary command without asking".to_string(),
        });
    } else {
        let detail = format!(
            "Auto did not run an ordinary command cleanly (approval={}, terminal={})",
            benign.approval.is_some(),
            benign.terminal
        );
        cases.push(Case {
            id: "A2_auto_benign_does_not_ask".to_string(),
            status: Status::Fail,
            detail: detail.clone(),
        });
        violations.push(Violation {
            case: "A2_auto_benign_does_not_ask".to_string(),
            detail,
        });
    }

    (cases, violations)
}

// ── the test entry points ────────────────────────────────────────────────────

fn artifact_path() -> Option<std::path::PathBuf> {
    std::env::var_os("FULL_PERMISSION_ARTIFACT").map(std::path::PathBuf::from)
}

fn write_report(cases: &[Case], violations: &[Violation]) {
    let failed = cases.iter().filter(|c| c.status == Status::Fail).count();
    let verdict = if failed > 0 || !violations.is_empty() {
        "FAIL"
    } else {
        "PASS"
    };
    let report = serde_json::json!({
        "gate": "full_permission_contract",
        "verdict": verdict,
        "cases": cases.iter().map(|c| serde_json::json!({
            "id": c.id,
            "status": c.status.as_str(),
            "detail": c.detail,
        })).collect::<Vec<_>>(),
        "violations": violations.iter().map(|v| serde_json::json!({
            "case": v.case,
            "detail": v.detail,
        })).collect::<Vec<_>>(),
    });
    let text = serde_json::to_string_pretty(&report).unwrap();
    println!("{text}");
    if let Some(path) = artifact_path() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(path, &text).unwrap();
    }
}

fn selected_paths() -> Vec<Path> {
    match std::env::var("FULL_PERMISSION_PATHS").as_deref() {
        Ok(value) => value
            .split(',')
            .filter_map(|token| match token.trim().to_ascii_uppercase().as_str() {
                "A" => Some(Path::FullDirect),
                "B" => Some(Path::AutoToFull),
                "C" => Some(Path::Reconnect),
                "D" => Some(Path::Resume),
                _ => None,
            })
            .collect(),
        Err(_) => vec![
            Path::FullDirect,
            Path::AutoToFull,
            Path::Reconnect,
            Path::Resume,
        ],
    }
}

fn selected_ops(ops: Vec<Op>) -> Vec<Op> {
    match std::env::var("FULL_PERMISSION_OP") {
        Ok(wanted) => ops.into_iter().filter(|op| op.id == wanted).collect(),
        Err(_) => ops,
    }
}

#[tokio::test]
#[ignore = "dogfood: run explicitly by eval/scripts/full_permission_gate.py"]
async fn full_permission_contract_dogfood() {
    let workroot = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let ops = selected_ops(operations(workroot.path(), home.path()));
    let paths = selected_paths();

    let mut cases = Vec::new();
    let mut violations = Vec::new();

    for path in &paths {
        let selected: Vec<&Op> = match path {
            // The Auto→Full and Resume paths only carry operations Auto would
            // actually ask about; the rest have no question to supersede.
            Path::AutoToFull | Path::Resume => ops.iter().filter(|op| op.auto_asks).collect(),
            Path::FullDirect | Path::Reconnect => ops.iter().collect(),
        };
        for op in selected {
            let (case, v) = run_case(*path, op, workroot.path()).await;
            cases.push(case);
            violations.extend(v);
        }
    }

    // INVARIANT B: the selected mode survives create/persist/restart/resume in
    // both the embedded and the daemon shape, and an explicit override moves it
    // in both directions. No model is needed: this is durable-state truth.
    let (persistence_cases, persistence_violations) = run_mode_persistence(workroot.path()).await;
    cases.extend(persistence_cases);
    violations.extend(persistence_violations);

    // INVARIANT C: one live authority per session (stale turn snapshot,
    // `/clear`, restart) and Auto's own half of the matrix.
    let (live_cases, live_violations) = run_live_profile_authority(workroot.path()).await;
    cases.extend(live_cases);
    violations.extend(live_violations);
    let (auto_cases, auto_violations) = run_auto_matrix(workroot.path()).await;
    cases.extend(auto_cases);
    violations.extend(auto_violations);

    write_report(&cases, &violations);

    assert!(
        violations.is_empty(),
        "FULL PERMISSION DOGFOOD: {} violation(s): {violations:#?}",
        violations.len()
    );
    let failed: Vec<_> = cases.iter().filter(|c| c.status == Status::Fail).collect();
    assert!(
        failed.is_empty(),
        "FULL PERMISSION DOGFOOD FAIL: {failed:#?}"
    );
}
