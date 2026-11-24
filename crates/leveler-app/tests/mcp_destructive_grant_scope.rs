//! Gate M1: the authorization scope of a destructive MCP call.
//!
//! The fixture is a real stdio MCP server spawned by the product's own MCP
//! transport; the tools are called through the production `ToolHost` and the
//! approval is a real `ClientCommand::ApprovalDecision` from the runtime
//! client. The model side is a deterministic mock provider: this measures the
//! permission decision and its side effects, not model quality.
//!
//! The property: approving `delete_resource` on Resource A must not silently
//! authorize Resource B. The exact argument vector may be reused for the rest
//! of the turn; another target asks again, and a tool that declares destructive
//! behavior offers no durable "always" rule.
//!
//! Run explicitly:
//!
//! ```text
//! MCP_GRANT_SCOPE_REPORT=C:\qlab\evidence\mcp-grant-scope.json \
//!   cargo test -p leveler-app --test mcp_destructive_grant_scope -- --ignored --nocapture --test-threads=1
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    ApprovalDecision, ClientCommand, InteractiveRuntimeClient, RuntimeEvent, UiApprovalRequest,
};
use leveler_execution::{PermissionContract, PermissionProfile};
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_test_support::{MockResponse, MockServer};

const TIMEOUT: Duration = Duration::from_secs(120);

fn isolate_global_config(home: &Path) {
    // `LEVELER_HOME` is process-global; this binary's tests take `env_lock`
    // before setting it, and the suite is expected to run with one test
    // thread (the documented command does).
    unsafe {
        std::env::set_var("LEVELER_HOME", home);
    }
}

async fn env_lock() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    LOCK.lock().await
}

fn python() -> Option<String> {
    [
        "/usr/bin/python3",
        "/opt/homebrew/bin/python3",
        "python3",
        "python",
    ]
    .into_iter()
    .find(|candidate| {
        std::process::Command::new(candidate)
            .arg("--version")
            .output()
            .is_ok_and(|out| out.status.success())
    })
    .map(str::to_string)
}

/// The fixture implements the MCP handshake and four tools. Every request is
/// appended to `protocol.log`; deletions are real filesystem effects.
const FIXTURE: &str = r#"
import json, os, sys

ROOT = os.path.dirname(os.path.abspath(__file__))
RESOURCES = os.path.join(ROOT, "resources")
LOG = os.path.join(ROOT, "protocol.log")

def log(request):
    with open(LOG, "a", encoding="utf-8") as handle:
        handle.write(json.dumps(request, ensure_ascii=False) + "\n")

def result(request_id, payload, error=False):
    if error:
        return {"jsonrpc": "2.0", "id": request_id, "result": {
            "content": [{"type": "text", "text": payload}], "isError": True}}
    return {"jsonrpc": "2.0", "id": request_id, "result": {
        "content": [{"type": "text", "text": payload}], "isError": False}}

def target_path(arguments):
    target = str(arguments.get("target", ""))
    if not target or os.path.basename(target) != target:
        return None
    return os.path.join(RESOURCES, target)

TOOLS = [
    {"name": "read_resource", "description": "Read an isolated resource.",
     "inputSchema": {"type": "object", "properties": {"target": {"type": "string"}},
                     "required": ["target"]},
     "annotations": {"readOnlyHint": True, "destructiveHint": False}},
    {"name": "write_resource", "description": "Write an isolated resource.",
     "inputSchema": {"type": "object", "properties": {"target": {"type": "string"},
                     "content": {"type": "string"}}, "required": ["target", "content"]},
     "annotations": {"readOnlyHint": False, "destructiveHint": False}},
    {"name": "delete_resource", "description": "Delete an isolated resource.",
     "inputSchema": {"type": "object", "properties": {"target": {"type": "string"}},
                     "required": ["target"]},
     "annotations": {"readOnlyHint": False, "destructiveHint": True}},
    {"name": "unannotated_delete", "description": "Delete a resource; this description claims it is destructive but there is no typed annotation.",
     "inputSchema": {"type": "object", "properties": {"target": {"type": "string"}},
                     "required": ["target"]}},
]

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        request = json.loads(line)
    except json.JSONDecodeError:
        continue
    method = request.get("method", "")
    if method == "notifications/initialized":
        continue
    if method is None or method.startswith("notifications/"):
        continue
    log(request)
    request_id = request.get("id")
    if method == "initialize":
        response = {"jsonrpc": "2.0", "id": request_id, "result": {
            "protocolVersion": "2025-06-18", "capabilities": {"tools": {}},
            "serverInfo": {"name": "grant-scope-fixture", "version": "1.0"}}}
    elif method == "tools/list":
        response = {"jsonrpc": "2.0", "id": request_id, "result": {"tools": TOOLS}}
    elif method == "tools/call":
        params = request.get("params", {})
        name = params.get("name", "")
        arguments = params.get("arguments", {})
        path = target_path(arguments)
        if path is None:
            response = result(request_id, "invalid target", error=True)
        elif name == "read_resource":
            try:
                with open(path, encoding="utf-8") as handle:
                    response = result(request_id, handle.read())
            except OSError as error:
                response = result(request_id, "read failed: %s" % error, error=True)
        elif name == "write_resource":
            content = str(arguments.get("content", ""))
            try:
                with open(path, "w", encoding="utf-8") as handle:
                    handle.write(content)
                response = result(request_id, "written")
            except OSError as error:
                response = result(request_id, "write failed: %s" % error, error=True)
        elif name in ("delete_resource", "unannotated_delete"):
            try:
                os.remove(path)
                response = result(request_id, "deleted")
            except OSError as error:
                response = result(request_id, "delete failed: %s" % error, error=True)
        else:
            response = result(request_id, "unknown tool %s" % name, error=True)
    elif method == "ping":
        response = {"jsonrpc": "2.0", "id": request_id, "result": {}}
    else:
        response = {"jsonrpc": "2.0", "id": request_id,
                    "error": {"code": -32601, "message": "unknown method"}}
    print(json.dumps(response, ensure_ascii=False), flush=True)
"#;

struct Fixture {
    _tmp: tempfile::TempDir,
    repo: PathBuf,
    protocol_log: PathBuf,
    resources: PathBuf,
}

impl Fixture {
    fn new(python: &str) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let repo = tmp.path().join("repo");
        let resources = home.join("resources");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&resources).unwrap();
        for name in ["resource-a.txt", "resource-b.txt"] {
            std::fs::write(resources.join(name), format!("{name}-original\n")).unwrap();
        }
        let script = home.join("fixture.py");
        std::fs::write(&script, FIXTURE).unwrap();
        let command = python.replace('\\', "\\\\");
        let script_toml = script.display().to_string().replace('\\', "\\\\");
        std::fs::write(
            home.join("config.toml"),
            format!(
                "[[mcp_servers]]\nname = \"fixture\"\ncommand = \"{command}\"\n\
                 args = [\"-u\", \"{script_toml}\"]\n"
            ),
        )
        .unwrap();
        isolate_global_config(&home);
        Self {
            _tmp: tmp,
            repo,
            protocol_log: home.join("protocol.log"),
            resources,
        }
    }

    fn protocol_requests(&self) -> Vec<serde_json::Value> {
        std::fs::read_to_string(&self.protocol_log)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    fn tool_calls(&self, name: &str) -> Vec<serde_json::Value> {
        self.protocol_requests()
            .into_iter()
            .filter(|request| request["method"] == "tools/call")
            .filter(|request| request["params"]["name"] == name)
            .collect()
    }
}

fn sse_text(content: &str) -> MockResponse {
    let frame =
        serde_json::json!({"choices": [{"delta": {"content": content}, "finish_reason": "stop"}]})
            .to_string();
    MockResponse::sse(&[&frame])
}

fn sse_call(name: &str, arguments: serde_json::Value) -> MockResponse {
    let call = serde_json::json!({"choices": [{"delta": {"tool_calls": [{
        "index": 0,
        "id": "call-1",
        "type": "function",
        "function": {"name": name, "arguments": arguments.to_string()},
    }]}}]})
    .to_string();
    let finish =
        serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}).to_string();
    MockResponse::sse(&[&call, &finish])
}

fn write_config(root: &Path, base_url: &str) {
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
capabilities: { streaming: true, tool_calling: true, parallel_tool_calls: false, structured_output: true, reasoning: false, vision: false }
limits: { context_window: 131072, reliable_context: 65536, max_output_tokens: 1024, max_tool_schema_bytes: 8192, max_parallel_tool_calls: 1 }
compatibility: { synthesize_tool_call_ids: true, drop_unsupported_fields: true }
"#,
    )
    .unwrap();
}

#[derive(Debug, Clone)]
struct Observed {
    approvals: Vec<UiApprovalRequest>,
    decisions: Vec<ApprovalDecision>,
    tool_results: Vec<(String, bool, String)>,
    terminal: bool,
    failure: Option<String>,
}

/// Drive one turn. `decide` receives the approval request and returns the
/// decision the client sends back.
async fn run_turn<F>(
    fixture: &Fixture,
    contract: PermissionContract,
    profile: PermissionProfile,
    script: Vec<MockResponse>,
    sandbox: bool,
    mut decide: F,
) -> Observed
where
    F: FnMut(&UiApprovalRequest) -> ApprovalDecision,
{
    let server = MockServer::start(script).await;
    let root = fixture._tmp.path().join(format!(
        "app-{}-{:?}-{}",
        contract.as_str(),
        profile,
        std::process::id()
    ));
    std::fs::create_dir_all(&root).unwrap();
    write_config(&root, &server.base_url());
    let app = Arc::new(
        Application::assemble(Layout::from_parts(
            fixture.repo.clone(),
            root.join("configs"),
            root.join("state"),
        ))
        .unwrap(),
    );
    let model = ModelRef::new("mock", "m");
    let session = app
        .create_session_with_permission_state(
            &model,
            "mcp grant scope",
            contract,
            profile,
            app.collaboration(),
        )
        .await
        .unwrap();
    let client = Arc::new(InProcessRuntimeClient::new(app, model, profile, sandbox));
    let mut events = client.subscribe_session(&session);
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "run the MCP grant scope fixture".into(),
            attachments: vec![],
        })
        .await
        .unwrap();

    let mut approvals = Vec::new();
    let mut decisions = Vec::new();
    let mut tool_results = Vec::new();
    let mut started_tools = std::collections::HashMap::new();
    let mut terminal = false;
    let mut failure = None;
    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline && !terminal {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Ok(RuntimeEvent::ApprovalRequested { request })) => {
                let decision = decide(&request);
                approvals.push(request.clone());
                decisions.push(decision);
                let _ = client
                    .send(ClientCommand::ApprovalDecision {
                        request_id: request.id.clone(),
                        decision,
                    })
                    .await;
            }
            Ok(Ok(RuntimeEvent::ToolCallStarted { id, name, .. })) => {
                started_tools.insert(id, name);
            }
            Ok(Ok(RuntimeEvent::ToolCallCompleted {
                id, ok, preview, ..
            })) => {
                let name = started_tools
                    .remove(&id)
                    .unwrap_or_else(|| "<unknown>".to_string());
                tool_results.push((name, ok, preview));
            }
            Ok(Ok(RuntimeEvent::TurnCompleted))
            | Ok(Ok(RuntimeEvent::TurnCompletedWithWarnings { .. }))
            | Ok(Ok(RuntimeEvent::TurnAnswered))
            | Ok(Ok(RuntimeEvent::TurnCancelled))
            | Ok(Ok(RuntimeEvent::TurnIncomplete { .. }))
            | Ok(Ok(RuntimeEvent::TurnTruncated { .. })) => terminal = true,
            Ok(Ok(RuntimeEvent::TurnFailed { error, .. })) => {
                failure = Some(error);
                terminal = true;
            }
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                failure = Some(format!("event stream closed: {error}"));
                terminal = true;
            }
            Err(_) => {}
        }
    }
    Observed {
        approvals,
        decisions,
        tool_results,
        terminal,
        failure,
    }
}

fn destructive_script() -> Vec<MockResponse> {
    vec![
        sse_call(
            "capability",
            serde_json::json!({"action": "enable", "id": "external_tools"}),
        ),
        sse_call(
            "mcp__fixture__write_resource",
            serde_json::json!({"target": "note.txt", "content": "ordinary-mcp"}),
        ),
        sse_call(
            "mcp__fixture__delete_resource",
            serde_json::json!({"target": "resource-a.txt"}),
        ),
        // The exact same call again: the turn grant covers this argument
        // vector, so it must not ask a second time.
        sse_call(
            "mcp__fixture__delete_resource",
            serde_json::json!({"target": "resource-a.txt"}),
        ),
        sse_call(
            "mcp__fixture__delete_resource",
            serde_json::json!({"target": "resource-b.txt"}),
        ),
        sse_text("done"),
    ]
}

fn write_report(name: &str, value: &serde_json::Value) {
    let path = match std::env::var("MCP_GRANT_SCOPE_REPORT") {
        Ok(dir) => {
            let dir = PathBuf::from(dir);
            let _ = std::fs::create_dir_all(&dir);
            dir.join(format!("{name}.json"))
        }
        Err(_) => PathBuf::from(format!("target/{name}.json")),
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&path, serde_json::to_string_pretty(value).unwrap()).unwrap();
    println!("{}", serde_json::to_string_pretty(value).unwrap());
    println!("report: {}", path.display());
}

/// Approve the first delete (turn/session grant), deny the second target.
#[tokio::test]
#[ignore = "Gate M1 live MCP fixture; run explicitly with --ignored"]
async fn a_destructive_mcp_turn_grant_covers_only_the_approved_target() {
    let _env = env_lock().await;
    let Some(python) = python() else {
        panic!("no Python interpreter is available for the MCP fixture");
    };
    let fixture = Fixture::new(&python);
    let mut approvals = 0usize;
    let observed = run_turn(
        &fixture,
        PermissionContract::V2,
        PermissionProfile::Assisted,
        destructive_script(),
        false,
        |_request| {
            approvals += 1;
            if approvals == 1 {
                ApprovalDecision::ApproveSession
            } else {
                ApprovalDecision::Deny
            }
        },
    )
    .await;

    let resource_a = fixture.resources.join("resource-a.txt");
    let resource_b = fixture.resources.join("resource-b.txt");
    let note = std::fs::read_to_string(fixture.resources.join("note.txt")).unwrap_or_default();
    let delete_calls = fixture.tool_calls("delete_resource");
    let write_calls = fixture.tool_calls("write_resource");
    let deleted_targets: Vec<String> = delete_calls
        .iter()
        .map(|call| {
            call["params"]["arguments"]["target"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    let report = serde_json::json!({
        "gate": "M1",
        "fixture": "real stdio MCP server via product transport",
        "contract": "v2",
        "profile": "assisted",
        "approvals": observed.approvals.iter().map(|request| serde_json::json!({
            "tool": request.tool,
            "reason_code": request.reason_code,
            "always_persists": request.always_persists,
            "call_id": request.call_id,
            "summary": request.summary,
        })).collect::<Vec<_>>(),
        "decisions": observed.decisions,
        "protocol": {
            "delete_tool_calls": deleted_targets,
            "write_tool_calls": write_calls.len(),
        },
        "side_effects": {
            "resource_a_exists": resource_a.exists(),
            "resource_b_exists": resource_b.exists(),
            "note": note,
        },
        "tool_results": observed.tool_results,
        "terminal": observed.terminal,
        "failure": observed.failure,
    });
    write_report("mcp-grant-scope-v2-assisted", &report);

    assert!(observed.terminal, "{:?}", observed.failure);
    assert_eq!(
        observed.approvals.len(),
        2,
        "resource A and resource B each need their own approval: {:?}",
        observed.approvals
    );
    for request in &observed.approvals {
        assert_eq!(
            request.reason_code.as_deref(),
            Some("irreversible.mcp_destructive")
        );
        assert!(
            !request.always_persists,
            "a destructive MCP approval must not offer a durable tool-wide rule"
        );
    }
    assert_eq!(observed.decisions[0], ApprovalDecision::ApproveSession);
    assert_eq!(observed.decisions[1], ApprovalDecision::Deny);
    assert!(!resource_a.exists(), "the approved target must be deleted");
    assert!(
        resource_b.exists(),
        "the denied target must survive an approval for resource A"
    );
    assert_eq!(note, "ordinary-mcp", "ordinary MCP must run unapproved");
    assert_eq!(
        deleted_targets,
        vec!["resource-a.txt".to_string(), "resource-a.txt".to_string()],
        "only the approved argument vector may reach the server"
    );
    assert_eq!(
        write_calls.len(),
        1,
        "ordinary MCP write must reach the server exactly once"
    );
    assert!(
        observed
            .tool_results
            .iter()
            .any(|(tool, ok, _)| tool.ends_with("write_resource") && *ok),
        "the ordinary MCP call must succeed: {:?}",
        observed.tool_results
    );
}

/// Full never asks. Tier 1 (old Auto) keeps its own MCP semantics: a normal
/// MCP call is asked once and its "this turn" grant covers the tool, while a
/// destructive tool's approval is exact-call scoped there too.
#[tokio::test]
#[ignore = "Gate M1 live MCP fixture; run explicitly with --ignored"]
async fn full_and_tier_one_keep_their_mcp_semantics() {
    let _env = env_lock().await;
    let Some(python) = python() else {
        panic!("no Python interpreter is available for the MCP fixture");
    };

    // Full: no approval, both targets deleted.
    let fixture = Fixture::new(&python);
    let observed = run_turn(
        &fixture,
        PermissionContract::V2,
        PermissionProfile::FullAccess,
        destructive_script(),
        false,
        |_request| ApprovalDecision::Deny,
    )
    .await;
    let full = serde_json::json!({
        "arm": "full",
        "approvals": observed.approvals.len(),
        "resource_a_deleted": !fixture.resources.join("resource-a.txt").exists(),
        "resource_b_deleted": !fixture.resources.join("resource-b.txt").exists(),
        "delete_tool_calls": fixture.tool_calls("delete_resource").len(),
        "terminal": observed.terminal,
        "failure": observed.failure,
    });

    // Tier 1 + destructive MCP: A and B each ask. The turn approval for A
    // covers only A's argument vector; a repeat of A runs, B asks again.
    let fixture = Fixture::new(&python);
    let mut destructive_approvals = 0usize;
    let observed = run_turn(
        &fixture,
        PermissionContract::V2,
        PermissionProfile::RequestApproval,
        vec![
            sse_call(
                "capability",
                serde_json::json!({"action": "enable", "id": "external_tools"}),
            ),
            sse_call(
                "mcp__fixture__delete_resource",
                serde_json::json!({"target": "resource-a.txt"}),
            ),
            sse_call(
                "mcp__fixture__delete_resource",
                serde_json::json!({"target": "resource-a.txt"}),
            ),
            sse_call(
                "mcp__fixture__delete_resource",
                serde_json::json!({"target": "resource-b.txt"}),
            ),
            sse_text("done"),
        ],
        false,
        |_request| {
            destructive_approvals += 1;
            if destructive_approvals == 1 {
                ApprovalDecision::ApproveSession
            } else {
                ApprovalDecision::Deny
            }
        },
    )
    .await;
    let delete_targets: Vec<String> = fixture
        .tool_calls("delete_resource")
        .iter()
        .map(|call| {
            call["params"]["arguments"]["target"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    let tier1_destructive = serde_json::json!({
        "arm": "v2_request_approval_destructive",
        "approvals": observed.approvals.len(),
        "reason_codes": observed.approvals.iter().map(|r| r.reason_code.clone()).collect::<Vec<_>>(),
        "decisions": observed.decisions,
        "resource_a_deleted": !fixture.resources.join("resource-a.txt").exists(),
        "resource_b_deleted": !fixture.resources.join("resource-b.txt").exists(),
        "delete_targets": delete_targets,
        "terminal": observed.terminal,
        "failure": observed.failure,
    });

    // Tier 1 + ordinary MCP: the first approval is the configured tool for
    // the turn, so a different argument vector does not ask again.
    let fixture = Fixture::new(&python);
    let mut ordinary_approvals = 0usize;
    let observed = run_turn(
        &fixture,
        PermissionContract::V2,
        PermissionProfile::RequestApproval,
        vec![
            sse_call(
                "capability",
                serde_json::json!({"action": "enable", "id": "external_tools"}),
            ),
            sse_call(
                "mcp__fixture__write_resource",
                serde_json::json!({"target": "note.txt", "content": "first"}),
            ),
            sse_call(
                "mcp__fixture__write_resource",
                serde_json::json!({"target": "note.txt", "content": "second"}),
            ),
            sse_text("done"),
        ],
        false,
        |_request| {
            ordinary_approvals += 1;
            ApprovalDecision::ApproveSession
        },
    )
    .await;
    let tier1_ordinary = serde_json::json!({
        "arm": "v2_request_approval_ordinary",
        "approvals": observed.approvals.len(),
        "note": std::fs::read_to_string(fixture.resources.join("note.txt")).unwrap_or_default(),
        "write_tool_calls": fixture.tool_calls("write_resource").len(),
        "terminal": observed.terminal,
        "failure": observed.failure,
    });

    let report = serde_json::json!({
        "gate": "M1",
        "full": full,
        "tier1_destructive": tier1_destructive,
        "tier1_ordinary": tier1_ordinary,
    });
    write_report("mcp-grant-scope-full-and-tier1", &report);

    assert_eq!(full["approvals"], 0, "{full}");
    assert_eq!(full["resource_a_deleted"], true, "{full}");
    assert_eq!(full["resource_b_deleted"], true, "{full}");
    assert_eq!(full["terminal"], true, "{full}");

    assert_eq!(tier1_destructive["approvals"], 2, "{tier1_destructive}");
    assert_eq!(
        tier1_destructive["resource_a_deleted"], true,
        "{tier1_destructive}"
    );
    assert_eq!(
        tier1_destructive["resource_b_deleted"], false,
        "tier 1 must not reuse A's approval for B: {tier1_destructive}"
    );
    assert_eq!(
        tier1_destructive["delete_targets"],
        serde_json::json!(["resource-a.txt", "resource-a.txt"]),
        "{tier1_destructive}"
    );
    assert_eq!(tier1_destructive["terminal"], true, "{tier1_destructive}");

    assert_eq!(tier1_ordinary["approvals"], 1, "{tier1_ordinary}");
    assert_eq!(tier1_ordinary["note"], "second", "{tier1_ordinary}");
    assert_eq!(
        tier1_ordinary["write_tool_calls"], 2,
        "the old Auto tool-scoped turn grant must be preserved: {tier1_ordinary}"
    );
    assert_eq!(tier1_ordinary["terminal"], true, "{tier1_ordinary}");
}

/// A tool without a typed `destructiveHint` is ordinary MCP under V2 Auto:
/// the accepted residual risk, not a permission defect hidden by this gate.
#[tokio::test]
#[ignore = "Gate M1 live MCP fixture; run explicitly with --ignored"]
async fn an_unannotated_mcp_delete_stays_ordinary_auto() {
    let _env = env_lock().await;
    let Some(python) = python() else {
        panic!("no Python interpreter is available for the MCP fixture");
    };
    let fixture = Fixture::new(&python);
    let observed = run_turn(
        &fixture,
        PermissionContract::V2,
        PermissionProfile::Assisted,
        vec![
            sse_call(
                "capability",
                serde_json::json!({"action": "enable", "id": "external_tools"}),
            ),
            sse_call(
                "mcp__fixture__unannotated_delete",
                serde_json::json!({"target": "resource-a.txt"}),
            ),
            sse_text("done"),
        ],
        false,
        |_request| ApprovalDecision::Deny,
    )
    .await;
    let report = serde_json::json!({
        "gate": "M1",
        "case": "unannotated_delete",
        "approvals": observed.approvals.len(),
        "resource_a_deleted": !fixture.resources.join("resource-a.txt").exists(),
        "delete_tool_calls": fixture.tool_calls("unannotated_delete").len(),
        "terminal": observed.terminal,
        "failure": observed.failure,
    });
    write_report("mcp-grant-scope-unannotated", &report);
    assert!(observed.terminal, "{:?}", observed.failure);
    assert_eq!(observed.approvals.len(), 0);
    assert!(!fixture.resources.join("resource-a.txt").exists());
}
