//! Windows-native acceptance for Permission Model V2.
//!
//! Every case runs through the production runtime (`Application` +
//! `InProcessRuntimeClient`), the real Windows filesystem and process model,
//! and real tool execution. The model side is a deterministic mock provider so
//! the gate measures permission decisions and their OS consequences, not model
//! quality. Approvals are real `ClientCommand::ApprovalDecision` messages from
//! the client.
//!
//! Run explicitly (one test thread):
//!
//! ```text
//! WINDOWS_PERMISSION_V2_REPORT=C:\qlab\evidence\windows-permission-v2.json \
//!   cargo test -p leveler-app --test windows_permission_v2_acceptance -- --ignored --nocapture --test-threads=1
//! ```

#![cfg(windows)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use leveler_agent::{AgentEvent, AutoClarify};
use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    ApprovalDecision, ClientCommand, InteractiveRuntimeClient, RuntimeEvent, UiApprovalRequest,
};
use leveler_execution::{AutoApprove, PermissionContract, PermissionProfile, PolicyKind};
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_test_support::{MockResponse, MockServer};
use tokio_util::sync::CancellationToken;

const TIMEOUT: Duration = Duration::from_secs(120);

fn isolate_global_config(home: &Path) {
    unsafe {
        std::env::set_var("LEVELER_HOME", home);
    }
}

async fn env_lock() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    LOCK.lock().await
}

fn python() -> Option<String> {
    ["python", "python3"]
        .into_iter()
        .find(|candidate| {
            std::process::Command::new(candidate)
                .arg("--version")
                .output()
                .is_ok_and(|out| out.status.success())
        })
        .map(str::to_string)
}

fn go() -> Option<String> {
    if let Ok(configured) = std::env::var("LEVELER_ACCEPTANCE_GO") {
        return Path::new(&configured).is_file().then_some(configured);
    }
    let candidate = r"C:\qlab\win11-full-20261118\artifacts\toolchains\go\bin\go.exe".to_string();
    Path::new(&candidate).is_file().then_some(candidate)
}

fn powershell() -> String {
    let candidate = r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe";
    if Path::new(candidate).is_file() {
        candidate.to_string()
    } else {
        "powershell".to_string()
    }
}

fn git(args: &[&str], cwd: &Path) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "acceptance")
        .env("GIT_AUTHOR_EMAIL", "acceptance@example.invalid")
        .env("GIT_COMMITTER_NAME", "acceptance")
        .env("GIT_COMMITTER_EMAIL", "acceptance@example.invalid")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

struct HttpServer {
    address: std::net::SocketAddr,
    hits: Arc<AtomicUsize>,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn start_http_server() -> HttpServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            use tokio::io::AsyncWriteExt;
            counter.fetch_add(1, Ordering::SeqCst);
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await;
        }
    });
    HttpServer {
        address,
        hits,
        handle,
    }
}

struct Fixture {
    _tmp: tempfile::TempDir,
    repo: PathBuf,
    outside: PathBuf,
    remote: PathBuf,
    http: HttpServer,
}

impl Fixture {
    async fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        git(&["init", "-q"], &repo);
        std::fs::write(repo.join("seed.txt"), "seed\n").unwrap();
        std::fs::write(repo.join("real-data.txt"), "precious\n").unwrap();
        std::fs::write(repo.join("disposable.txt"), "disposable\n").unwrap();
        std::fs::write(
            repo.join("indirect.py"),
            "import pathlib\n\
             pathlib.Path('script-ran.txt').write_text('python\\n', encoding='utf-8')\n\
             pathlib.Path('disposable.txt').unlink()\n",
        )
        .unwrap();
        std::fs::write(repo.join("go.mod"), "module acceptance\n\ngo 1.21\n").unwrap();
        std::fs::write(
            repo.join("calc.go"),
            "package acceptance\n\nfunc Add(a, b int) int { return a + b }\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("calc_test.go"),
            "package acceptance\n\nimport \"testing\"\n\nfunc TestAdd(t *testing.T) {\n\tif Add(1, 2) != 3 {\n\t\tt.Fatal(\"bad\")\n\t}\n}\n",
        )
        .unwrap();
        git(&["add", "-A"], &repo);
        git(&["commit", "-qm", "seed", "--no-gpg-sign"], &repo);
        let remote = tmp.path().join("remote.git");
        let output = std::process::Command::new("git")
            .args(["init", "--bare", "-q", remote.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(output.status.success());
        git(
            &["remote", "add", "origin", remote.to_str().unwrap()],
            &repo,
        );
        let http = start_http_server().await;
        Self {
            _tmp: tmp,
            repo,
            outside,
            remote,
            http,
        }
    }

    fn reset(&self) {
        // Each arm must observe its own push, not a ref an earlier arm left.
        let _ = std::process::Command::new("git")
            .args([
                "--git-dir",
                self.remote.to_str().unwrap(),
                "update-ref",
                "-d",
                "refs/heads/main",
            ])
            .output();
        std::fs::write(self.repo.join("seed.txt"), "seed\n").unwrap();
        std::fs::write(self.repo.join("real-data.txt"), "precious\n").unwrap();
        std::fs::write(self.repo.join("disposable.txt"), "disposable\n").unwrap();
        for name in [
            "inside.txt",
            "inside-ps.txt",
            "inside-cmd.txt",
            "script-ran.txt",
            "out.txt",
            "new-dir",
            "victim-del.txt",
            "victim-clear.txt",
            "victim-dir",
            "victim-erase.txt",
            "victim-rd",
            "victim-shell.txt",
        ] {
            let path = self.repo.join(name);
            if path.is_dir() {
                let _ = std::fs::remove_dir_all(path);
            } else {
                let _ = std::fs::remove_file(path);
            }
        }
        let _ = std::fs::remove_file(self.outside.join("written-outside.txt"));
        let _ = std::fs::remove_file(self.outside.join("deleted-outside.txt"));
    }

    fn write_outside_path(&self) -> PathBuf {
        self.outside.join("written-outside.txt")
    }

    fn remote_ref_exists(&self) -> bool {
        std::process::Command::new("git")
            .args([
                "--git-dir",
                self.remote.to_str().unwrap(),
                "rev-parse",
                "refs/heads/main",
            ])
            .output()
            .is_ok_and(|output| output.status.success())
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

#[derive(Debug, Clone, serde::Serialize)]
struct ApprovalRecord {
    tool: String,
    reason_code: Option<String>,
    always_persists: bool,
    summary: String,
}

#[derive(Debug, Clone)]
struct Observed {
    approvals: Vec<ApprovalRecord>,
    decisions: Vec<ApprovalDecision>,
    tool_results: Vec<(String, bool, String)>,
    terminal: bool,
    failure: Option<String>,
    policy_kind: String,
}

fn decision_name(decision: ApprovalDecision) -> &'static str {
    match decision {
        ApprovalDecision::ApproveOnce => "approve_once",
        ApprovalDecision::ApproveSession => "approve_session",
        ApprovalDecision::ApproveProject => "approve_project",
        ApprovalDecision::ApproveAlways => "approve_always",
        ApprovalDecision::Deny => "deny",
    }
}

/// Counts every approval question and denies it. The explicit network denial
/// must fail the call without asking; the counter is the evidence.
#[derive(Clone, Default)]
struct CountingApprover(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl leveler_execution::Approver for CountingApprover {
    async fn decide(
        &self,
        _request: &leveler_execution::ApprovalRequest,
    ) -> leveler_execution::ApprovalDecision {
        self.0.fetch_add(1, Ordering::SeqCst);
        leveler_execution::ApprovalDecision::Deny
    }
}

#[derive(Debug, Clone)]
struct HeadlessObserved {
    approvals: usize,
    tool_results: Vec<(String, bool, String)>,
    stop: String,
    failure: Option<String>,
}

/// The `leveler run --deny-network` path: `run_in_session` carries the run-level
/// sandbox flag, which is not the session's stored setting.
async fn run_headless_case(
    fixture: &Fixture,
    case: &str,
    script: Vec<MockResponse>,
    sandbox: bool,
) -> HeadlessObserved {
    let server = MockServer::start(script).await;
    let root = fixture
        ._tmp
        .path()
        .join(format!("headless-{case}-{}", std::process::id()));
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
            "windows v2 network denial",
            PermissionContract::V2,
            PermissionProfile::Assisted,
            app.collaboration(),
        )
        .await
        .unwrap();
    let approver = CountingApprover::default();
    let approvals = approver.0.clone();
    let mut tool_results = Vec::new();
    let outcome = app
        .run_in_session(
            &session,
            &model,
            PermissionProfile::Assisted,
            "deny network acceptance",
            Arc::new(approver),
            sandbox,
            &mut |event| {
                if let AgentEvent::ToolResult {
                    name,
                    is_error,
                    preview,
                    ..
                } = event
                {
                    tool_results.push((name, !is_error, preview));
                }
            },
            CancellationToken::new(),
        )
        .await;
    let (stop, failure) = match outcome {
        Ok(outcome) => (format!("{:?}", outcome.stop_reason), None),
        Err(error) => ("<error>".into(), Some(error.to_string())),
    };
    HeadlessObserved {
        approvals: approvals.load(Ordering::SeqCst),
        tool_results,
        stop,
        failure,
    }
}

async fn run_case<F>(
    fixture: &Fixture,
    contract: PermissionContract,
    profile: PermissionProfile,
    case: &str,
    script: Vec<MockResponse>,
    sandbox: bool,
    mut decide: F,
) -> Observed
where
    F: FnMut(&UiApprovalRequest) -> ApprovalDecision,
{
    let server = MockServer::start(script).await;
    let root = fixture._tmp.path().join(format!(
        "app-{}-{case}-{}",
        profile.as_str(),
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
            "windows v2 acceptance",
            contract,
            profile,
            app.collaboration(),
        )
        .await
        .unwrap();
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        model.clone(),
        profile,
        sandbox,
    ));
    let mut events = client.subscribe_session(&session);
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: format!("run the acceptance case {case}"),
            attachments: vec![],
        })
        .await
        .unwrap();

    let mut approvals = Vec::new();
    let mut decisions = Vec::new();
    let mut tool_results = Vec::new();
    let mut started_tools: HashMap<_, String> = HashMap::new();
    let mut terminal = false;
    let mut failure = None;
    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline && !terminal {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Ok(RuntimeEvent::ApprovalRequested { request })) => {
                approvals.push(ApprovalRecord {
                    tool: request.tool.clone(),
                    reason_code: request.reason_code.clone(),
                    always_persists: request.always_persists,
                    summary: request.summary.clone(),
                });
                let decision = decide(&request);
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

    let engine = app
        .engine_for_session(
            &model,
            profile,
            sandbox,
            Arc::new(AutoApprove),
            Arc::new(AutoClarify),
            false,
            Some(session.as_str()),
        )
        .await
        .unwrap();
    let policy_kind = engine.factory.tool_context.policy.policy_kind();
    Observed {
        approvals,
        decisions,
        tool_results,
        terminal,
        failure,
        policy_kind: policy_kind.as_str().to_string(),
    }
}

fn write_report(name: &str, value: &serde_json::Value) {
    let path = match std::env::var("WINDOWS_PERMISSION_V2_REPORT") {
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

fn observed_json(observed: &Observed) -> serde_json::Value {
    serde_json::json!({
        "approvals": observed.approvals,
        "decisions": observed.decisions.iter().map(|d| decision_name(*d)).collect::<Vec<_>>(),
        "tool_results": observed.tool_results.iter().map(|(tool, ok, preview)| serde_json::json!({
            "tool": tool, "ok": ok, "preview": preview,
        })).collect::<Vec<_>>(),
        "terminal": observed.terminal,
        "failure": observed.failure,
        "policy_kind": observed.policy_kind,
    })
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "real Windows Permission Model V2 acceptance; run explicitly with --ignored"]
async fn windows_three_tier_matrix() {
    let _env = env_lock().await;
    let Some(python) = python() else {
        panic!("no Python interpreter is available");
    };
    let go = go();
    let powershell = powershell();
    let home = tempfile::tempdir().unwrap();
    isolate_global_config(home.path());
    unsafe {
        std::env::set_var("GOTOOLCHAIN", "local");
    }
    let fixture = Fixture::new().await;
    // The confined legacy profiles may not write outside the workspace. Keep
    // the Go toolchain's caches inside it so `go test` measures the permission
    // decision rather than a sandbox denial of the compiler's scratch space.
    unsafe {
        std::env::set_var("GOCACHE", fixture.repo.join(".gocache"));
        std::env::set_var("GOPATH", fixture.repo.join(".gopath"));
        std::env::set_var("GOTMPDIR", fixture.repo.join(".gotmp"));
        std::env::set_var("GOTELEMETRY", "off");
    }
    let _ = std::fs::create_dir_all(fixture.repo.join(".gotmp"));
    let network_url = format!("http://{}/probe", fixture.http.address);

    let arms = [
        (
            "v1_request_approval",
            PermissionContract::V1,
            PermissionProfile::RequestApproval,
        ),
        (
            "v1_assisted",
            PermissionContract::V1,
            PermissionProfile::Assisted,
        ),
        (
            "v2_request_approval",
            PermissionContract::V2,
            PermissionProfile::RequestApproval,
        ),
        (
            "v2_assisted",
            PermissionContract::V2,
            PermissionProfile::Assisted,
        ),
        (
            "full",
            PermissionContract::V2,
            PermissionProfile::FullAccess,
        ),
    ];
    let mut results = Vec::new();
    for (id, contract, profile) in arms {
        fixture.reset();
        let mut cases = Vec::new();

        // 1. Ordinary workspace write.
        let observed = run_case(
            &fixture,
            contract,
            profile,
            "write_inside",
            vec![
                sse_call(
                    "write_file",
                    serde_json::json!({"path": "inside.txt", "content": "inside\n"}),
                ),
                sse_text("done"),
            ],
            false,
            |_| ApprovalDecision::Deny,
        )
        .await;
        cases.push(serde_json::json!({
            "case": "write_inside",
            "policy_kind": observed.policy_kind,
            "file": fixture.repo.join("inside.txt").exists(),
            "observed": observed_json(&observed),
        }));

        // 2. Ordinary write outside the workspace.
        let outside = fixture.write_outside_path();
        let observed = run_case(
            &fixture,
            contract,
            profile,
            "write_outside",
            vec![
                sse_call(
                    "write_file",
                    serde_json::json!({
                        "path": outside.display().to_string(),
                        "content": "outside\n"
                    }),
                ),
                sse_text("done"),
            ],
            false,
            |_| ApprovalDecision::Deny,
        )
        .await;
        cases.push(serde_json::json!({
            "case": "write_outside",
            "policy_kind": observed.policy_kind,
            "file": outside.exists(),
            "observed": observed_json(&observed),
        }));

        // 3. Ordinary PowerShell.
        let observed = run_case(
            &fixture,
            contract,
            profile,
            "powershell_ordinary",
            vec![
                sse_call(
                    "run_command",
                    serde_json::json!({
                        "program": powershell.clone(),
                        "args": ["-NoProfile", "-Command",
                                 "Set-Content -LiteralPath 'inside-ps.txt' -Value 'ps-ok'"]
                    }),
                ),
                sse_text("done"),
            ],
            false,
            |_| ApprovalDecision::Deny,
        )
        .await;
        cases.push(serde_json::json!({
            "case": "powershell_ordinary",
            "policy_kind": observed.policy_kind,
            "file": fixture.repo.join("inside-ps.txt").exists(),
            "observed": observed_json(&observed),
        }));

        // 4. Ordinary CMD.
        let observed = run_case(
            &fixture,
            contract,
            profile,
            "cmd_ordinary",
            vec![
                sse_call(
                    "run_command",
                    serde_json::json!({
                        "program": "cmd",
                        "args": ["/C", "echo cmd-ok> inside-cmd.txt"]
                    }),
                ),
                sse_text("done"),
            ],
            false,
            |_| ApprovalDecision::Deny,
        )
        .await;
        cases.push(serde_json::json!({
            "case": "cmd_ordinary",
            "policy_kind": observed.policy_kind,
            "file": fixture.repo.join("inside-cmd.txt").exists(),
            "observed": observed_json(&observed),
        }));

        // 5. A python script performing an indirect deletion: Auto must not
        //    inspect the script body.
        let observed = run_case(
            &fixture,
            contract,
            profile,
            "python_indirect",
            vec![
                sse_call(
                    "run_command",
                    serde_json::json!({"program": python.clone(), "args": ["indirect.py"]}),
                ),
                sse_text("done"),
            ],
            false,
            |_| ApprovalDecision::Deny,
        )
        .await;
        cases.push(serde_json::json!({
            "case": "python_indirect",
            "policy_kind": observed.policy_kind,
            "ran": fixture.repo.join("script-ran.txt").exists(),
            "deleted": !fixture.repo.join("disposable.txt").exists(),
            "observed": observed_json(&observed),
        }));

        // 6. Ordinary build/test.
        if let Some(go) = &go {
            let observed = run_case(
                &fixture,
                contract,
                profile,
                "go_test",
                vec![
                    sse_call(
                        "run_command",
                        serde_json::json!({"program": go, "args": ["test", "./..."]}),
                    ),
                    sse_text("done"),
                ],
                false,
                |_| ApprovalDecision::Deny,
            )
            .await;
            cases.push(serde_json::json!({
                "case": "go_test",
                "policy_kind": observed.policy_kind,
                "observed": observed_json(&observed),
            }));
        } else {
            cases.push(serde_json::json!({
                "case": "go_test",
                "status": "NOT_VERIFIED: no Go toolchain",
                "observed": serde_json::Value::Null,
            }));
        }

        // 7. Ordinary local Git push.
        let observed = run_case(
            &fixture,
            contract,
            profile,
            "git_push_local",
            vec![
                sse_call(
                    "run_command",
                    serde_json::json!({
                        "program": "git",
                        "args": ["push", "origin", "HEAD:refs/heads/main"]
                    }),
                ),
                sse_text("done"),
            ],
            false,
            |_| ApprovalDecision::Deny,
        )
        .await;
        cases.push(serde_json::json!({
            "case": "git_push_local",
            "policy_kind": observed.policy_kind,
            "pushed": fixture.remote_ref_exists(),
            "observed": observed_json(&observed),
        }));

        // 8. A direct deletion of real data. Deny proves no side effect.
        let victim = fixture.repo.join("real-data.txt");
        let observed = run_case(
            &fixture,
            contract,
            profile,
            "delete_cmd_denied",
            vec![
                sse_call(
                    "shell_command",
                    serde_json::json!({
                        "cmd": format!("del /F /Q \"{}\"", victim.display())
                    }),
                ),
                sse_text("done"),
            ],
            false,
            |_| ApprovalDecision::Deny,
        )
        .await;
        cases.push(serde_json::json!({
            "case": "delete_cmd_denied",
            "policy_kind": observed.policy_kind,
            "victim_exists": victim.exists(),
            "observed": observed_json(&observed),
        }));

        // 9. Git history loss: a dirty change must survive a denial.
        std::fs::write(fixture.repo.join("seed.txt"), "dirty\n").unwrap();
        let observed = run_case(
            &fixture,
            contract,
            profile,
            "git_reset_denied",
            vec![
                sse_call(
                    "run_command",
                    serde_json::json!({"program": "git", "args": ["reset", "--hard"]}),
                ),
                sse_text("done"),
            ],
            false,
            |_| ApprovalDecision::Deny,
        )
        .await;
        let dirty_survived = std::fs::read_to_string(fixture.repo.join("seed.txt"))
            .is_ok_and(|content| content == "dirty\n");
        cases.push(serde_json::json!({
            "case": "git_reset_denied",
            "policy_kind": observed.policy_kind,
            "dirty_survived": dirty_survived,
            "observed": observed_json(&observed),
        }));

        // 10. Ordinary network to a loopback destination.
        let before = fixture.http.hits.load(Ordering::SeqCst);
        let observed = run_case(
            &fixture,
            contract,
            profile,
            "network_loopback",
            vec![
                sse_call(
                    "capability",
                    serde_json::json!({"action": "enable", "id": "web"}),
                ),
                sse_call("web_fetch", serde_json::json!({"url": network_url.clone()})),
                sse_text("done"),
            ],
            false,
            |_| ApprovalDecision::Deny,
        )
        .await;
        let served = fixture.http.hits.load(Ordering::SeqCst) > before;
        cases.push(serde_json::json!({
            "case": "network_loopback",
            "policy_kind": observed.policy_kind,
            "served": served,
            "observed": observed_json(&observed),
        }));

        results.push(serde_json::json!({
            "arm": id,
            "contract": contract.as_str(),
            "profile": profile.as_str(),
            "expected_policy_kind": PolicyKind::of(contract, profile).as_str(),
            "cases": cases,
        }));
    }

    // Full executes the real deletion and reset: prove consent is not merely
    // recorded but limited to its contract.
    fixture.reset();
    std::fs::write(fixture.repo.join("seed.txt"), "dirty\n").unwrap();
    let victim = fixture.repo.join("real-data.txt");
    let delete_full = run_case(
        &fixture,
        PermissionContract::V2,
        PermissionProfile::FullAccess,
        "delete_cmd_full",
        vec![
            sse_call(
                "shell_command",
                serde_json::json!({
                    "cmd": format!("del /F /Q \"{}\"", victim.display())
                }),
            ),
            sse_text("done"),
        ],
        false,
        |_| ApprovalDecision::Deny,
    )
    .await;
    // Measure the deletion before the following `git reset --hard` restores
    // the tracked victim from HEAD.
    let delete_victim_exists = victim.exists();
    let reset_full = run_case(
        &fixture,
        PermissionContract::V2,
        PermissionProfile::FullAccess,
        "git_reset_full",
        vec![
            sse_call(
                "run_command",
                serde_json::json!({"program": "git", "args": ["reset", "--hard"]}),
            ),
            sse_text("done"),
        ],
        false,
        |_| ApprovalDecision::Deny,
    )
    .await;
    let full_execution = serde_json::json!({
        "delete": {
            "victim_exists": delete_victim_exists,
            "observed": observed_json(&delete_full),
        },
        "reset": {
            "dirty_survived": std::fs::read_to_string(fixture.repo.join("seed.txt"))
                .is_ok_and(|content| content == "dirty\n"),
            "observed": observed_json(&reset_full),
        },
    });

    let report = serde_json::json!({
        "gate": "W2/W6",
        "host": { "os": std::env::consts::OS, "arch": std::env::consts::ARCH },
        "provider": "deterministic mock model (real runtime, real Windows tools)",
        "network_url": network_url,
        "arms": results,
        "full_execution": full_execution,
    });
    write_report("windows-permission-v2-matrix", &report);

    // Assertions: the tier-2 contract and Full, then tier-1 compatibility.
    let arm = |id: &str| {
        results
            .iter()
            .find(|arm| arm["arm"] == id)
            .unwrap_or_else(|| panic!("missing arm {id}"))
    };
    let approvals = |arm: &serde_json::Value, name: &str| -> usize {
        case_at(arm, name)["observed"]["approvals"]
            .as_array()
            .unwrap()
            .len()
    };
    let v2 = arm("v2_assisted");
    for name in [
        "write_inside",
        "write_outside",
        "powershell_ordinary",
        "cmd_ordinary",
        "python_indirect",
        "go_test",
        "git_push_local",
        "network_loopback",
    ] {
        assert_eq!(approvals(v2, name), 0, "v2_assisted/{name} must not ask");
    }
    assert_eq!(case_at(v2, "write_inside")["file"], true);
    assert_eq!(case_at(v2, "write_outside")["file"], true);
    assert_eq!(case_at(v2, "powershell_ordinary")["file"], true);
    assert_eq!(case_at(v2, "cmd_ordinary")["file"], true);
    assert_eq!(case_at(v2, "python_indirect")["ran"], true);
    assert_eq!(case_at(v2, "python_indirect")["deleted"], true);
    assert_eq!(case_at(v2, "git_push_local")["pushed"], true);
    assert_eq!(case_at(v2, "network_loopback")["served"], true);
    assert_eq!(
        approvals(v2, "delete_cmd_denied"),
        1,
        "a direct Windows deletion must ask"
    );
    assert_eq!(
        case_at(v2, "delete_cmd_denied")["victim_exists"],
        true,
        "a denied deletion must not remove the target"
    );
    assert_eq!(
        approvals(v2, "git_reset_denied"),
        1,
        "git reset --hard must ask"
    );
    assert_eq!(case_at(v2, "git_reset_denied")["dirty_survived"], true);
    if go.is_some() {
        assert_eq!(approvals(v2, "go_test"), 0);
        let go_result = &case_at(v2, "go_test")["observed"]["tool_results"];
        assert!(
            go_result
                .as_array()
                .unwrap()
                .iter()
                .any(|result| result["tool"] == "run_command" && result["ok"] == true),
            "go test must execute successfully: {go_result}"
        );
    }

    let full = arm("full");
    for name in [
        "write_inside",
        "write_outside",
        "powershell_ordinary",
        "cmd_ordinary",
        "python_indirect",
        "git_push_local",
        "network_loopback",
        "delete_cmd_denied",
        "git_reset_denied",
    ] {
        assert_eq!(approvals(full, name), 0, "full/{name} must never ask");
    }
    assert_eq!(case_at(full, "delete_cmd_denied")["victim_exists"], false);
    assert_eq!(case_at(full, "git_reset_denied")["dirty_survived"], false);
    assert_eq!(full_execution["delete"]["victim_exists"], false);
    assert_eq!(full_execution["reset"]["dirty_survived"], false);

    // Tier 1 compatibility: V2 request-approval is the old Auto, case by case.
    let v1_assisted = arm("v1_assisted");
    let v2_request = arm("v2_request_approval");
    for left in v1_assisted["cases"].as_array().unwrap() {
        let name = left["case"].as_str().unwrap();
        if name == "go_test" && go.is_none() {
            continue;
        }
        assert_eq!(
            left["observed"]["approvals"].as_array().unwrap().len(),
            right_count(v2_request, name),
            "tier-1 compatibility broke for {name}"
        );
    }

    // The runtime's resolved policy kind is the contract mapping, not the
    // caller's mode label.
    for arm in &results {
        let expected = arm["expected_policy_kind"].as_str().unwrap();
        for case in arm["cases"].as_array().unwrap() {
            if let Some(observed) = case["observed"].as_object() {
                assert_eq!(
                    observed["policy_kind"], expected,
                    "{}/{} resolved policy kind",
                    arm["arm"], case["case"]
                );
            }
        }
    }
}

fn case_at<'a>(arm: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    arm["cases"]
        .as_array()
        .unwrap_or_else(|| panic!("arm without cases"))
        .iter()
        .find(|case| case["case"] == name)
        .unwrap_or_else(|| panic!("missing case {name}"))
}

fn right_count(arm: &serde_json::Value, name: &str) -> usize {
    arm["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["case"] == name)
        .and_then(|case| case["observed"]["approvals"].as_array())
        .map(Vec::len)
        .unwrap_or(0)
}

async fn command_case(
    fixture: &Fixture,
    case: &str,
    program: &str,
    args: Vec<String>,
    decision: ApprovalDecision,
) -> Observed {
    run_case(
        fixture,
        PermissionContract::V2,
        PermissionProfile::Assisted,
        case,
        vec![
            sse_call(
                "run_command",
                serde_json::json!({"program": program, "args": args}),
            ),
            sse_text("done"),
        ],
        false,
        move |_| decision,
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "real Windows Permission Model V2 acceptance; run explicitly with --ignored"]
async fn windows_shell_command_matrix() {
    let _env = env_lock().await;
    let Some(python) = python() else {
        panic!("no Python interpreter is available");
    };
    let powershell = powershell();
    let home = tempfile::tempdir().unwrap();
    isolate_global_config(home.path());
    let fixture = Fixture::new().await;
    fixture.reset();

    // Seed the Windows-specific victims after reset().
    for name in [
        "victim-del.txt",
        "victim-force.txt",
        "victim-erase.txt",
        "victim-shell.txt",
    ] {
        std::fs::write(fixture.repo.join(name), "precious\n").unwrap();
    }
    std::fs::write(fixture.repo.join("victim-clear.txt"), "precious\n").unwrap();
    std::fs::create_dir_all(fixture.repo.join("victim-dir").join("inner")).unwrap();
    std::fs::write(
        fixture
            .repo
            .join("victim-dir")
            .join("inner")
            .join("data.txt"),
        "precious\n",
    )
    .unwrap();
    std::fs::create_dir_all(fixture.repo.join("victim-rd").join("inner")).unwrap();
    std::fs::write(
        fixture
            .repo
            .join("victim-rd")
            .join("inner")
            .join("data.txt"),
        "precious\n",
    )
    .unwrap();
    let chinese = fixture.repo.join("验收 目录");
    std::fs::create_dir_all(&chinese).unwrap();
    std::fs::write(chinese.join("victim.txt"), "precious\n").unwrap();

    let mut cases = Vec::new();

    // Ordinary PowerShell and CMD: automatic, real effects.
    let observed = command_case(
        &fixture,
        "ps_get_content",
        &powershell,
        vec![
            "-NoProfile".into(),
            "-Command".into(),
            format!(
                "Get-Content -LiteralPath '{}'",
                fixture.repo.join("victim-del.txt").display()
            ),
        ],
        ApprovalDecision::Deny,
    )
    .await;
    cases.push(serde_json::json!({
        "case": "ps_get_content", "observed": observed_json(&observed),
    }));

    let observed = command_case(
        &fixture,
        "ps_set_content",
        &powershell,
        vec![
            "-NoProfile".into(),
            "-Command".into(),
            "Set-Content -LiteralPath 'ps-new.txt' -Value 'ok'".into(),
        ],
        ApprovalDecision::Deny,
    )
    .await;
    let ps_new = fixture.repo.join("ps-new.txt").exists();
    cases.push(serde_json::json!({
        "case": "ps_set_content", "file": ps_new, "observed": observed_json(&observed),
    }));

    let observed = command_case(
        &fixture,
        "ps_new_item",
        &powershell,
        vec![
            "-NoProfile".into(),
            "-Command".into(),
            "New-Item -ItemType Directory -Path 'ps-new-dir' | Out-Null".into(),
        ],
        ApprovalDecision::Deny,
    )
    .await;
    let ps_dir = fixture.repo.join("ps-new-dir").is_dir();
    cases.push(serde_json::json!({
        "case": "ps_new_item", "dir": ps_dir, "observed": observed_json(&observed),
    }));

    let observed = command_case(
        &fixture,
        "cmd_echo_ordinary",
        "cmd",
        vec!["/C".into(), "echo ok> cmd-new.txt".into()],
        ApprovalDecision::Deny,
    )
    .await;
    let cmd_new = fixture.repo.join("cmd-new.txt").exists();
    cases.push(serde_json::json!({
        "case": "cmd_echo_ordinary", "file": cmd_new, "observed": observed_json(&observed),
    }));

    // Direct irreversible PowerShell deletions: each asks, a denial has no
    // side effect.
    for (case, target, extra) in [
        ("ps_remove_item", "victim-del.txt", "-Force"),
        ("ps_remove_item_recurse", "victim-dir", "-Recurse -Force"),
        ("ps_remove_item_force", "victim-force.txt", "-Force"),
    ] {
        let victim = fixture.repo.join(target);
        let observed = command_case(
            &fixture,
            case,
            &powershell,
            vec![
                "-NoProfile".into(),
                "-Command".into(),
                format!("Remove-Item -LiteralPath '{}' {extra}", victim.display()),
            ],
            ApprovalDecision::Deny,
        )
        .await;
        let exists = victim.exists();
        cases.push(serde_json::json!({
            "case": case, "target_exists": exists, "observed": observed_json(&observed),
        }));
    }

    let observed = command_case(
        &fixture,
        "ps_clear_content",
        &powershell,
        vec![
            "-NoProfile".into(),
            "-Command".into(),
            format!(
                "Clear-Content -LiteralPath '{}'",
                fixture.repo.join("victim-clear.txt").display()
            ),
        ],
        ApprovalDecision::Deny,
    )
    .await;
    let clear_content = std::fs::read_to_string(fixture.repo.join("victim-clear.txt")).ok();
    cases.push(serde_json::json!({
        "case": "ps_clear_content", "content": clear_content,
        "observed": observed_json(&observed),
    }));

    for (case, args) in [
        (
            "cmd_erase",
            vec!["/C".into(), "erase /F /Q victim-erase.txt".into()],
        ),
        ("cmd_rd_s", vec!["/C".into(), "rd /s /q victim-rd".into()]),
    ] {
        let victim = if case == "cmd_erase" {
            fixture.repo.join("victim-erase.txt")
        } else {
            fixture.repo.join("victim-rd")
        };
        let observed = command_case(&fixture, case, "cmd", args, ApprovalDecision::Deny).await;
        cases.push(serde_json::json!({
            "case": case, "target_exists": victim.exists(),
            "observed": observed_json(&observed),
        }));
    }

    // Quoted path with Chinese characters and a space.
    let observed = command_case(
        &fixture,
        "ps_chinese_space_path",
        &powershell,
        vec![
            "-NoProfile".into(),
            "-Command".into(),
            format!(
                "Remove-Item -LiteralPath '{}' -Force",
                chinese.join("victim.txt").display()
            ),
        ],
        ApprovalDecision::Deny,
    )
    .await;
    cases.push(serde_json::json!({
        "case": "ps_chinese_space_path",
        "target_exists": chinese.join("victim.txt").exists(),
        "observed": observed_json(&observed),
    }));

    // A compound PowerShell command keeps the visible deletion.
    let observed = command_case(
        &fixture,
        "ps_compound",
        &powershell,
        vec![
            "-NoProfile".into(),
            "-Command".into(),
            format!(
                "Get-Content -LiteralPath 'victim-clear.txt'; Remove-Item -LiteralPath '{}' -Force",
                fixture.repo.join("victim-force.txt").display()
            ),
        ],
        ApprovalDecision::Deny,
    )
    .await;
    cases.push(serde_json::json!({
        "case": "ps_compound",
        "target_exists": fixture.repo.join("victim-force.txt").exists(),
        "observed": observed_json(&observed),
    }));

    // Approved once: the exact command executes.
    let approved_victim = fixture.repo.join("victim-del.txt");
    let observed = command_case(
        &fixture,
        "ps_remove_item_approved",
        &powershell,
        vec![
            "-NoProfile".into(),
            "-Command".into(),
            format!(
                "Remove-Item -LiteralPath '{}' -Force",
                approved_victim.display()
            ),
        ],
        ApprovalDecision::ApproveOnce,
    )
    .await;
    cases.push(serde_json::json!({
        "case": "ps_remove_item_approved",
        "target_exists": approved_victim.exists(),
        "observed": observed_json(&observed),
    }));

    // The `shell_command` shape on Windows: one line through `cmd /C`.
    let shell_victim = fixture.repo.join("victim-shell.txt");
    let observed = run_case(
        &fixture,
        PermissionContract::V2,
        PermissionProfile::Assisted,
        "shell_command_powershell_delete",
        vec![
            sse_call(
                "shell_command",
                serde_json::json!({
                    "cmd": format!(
                        "powershell -NoProfile -Command \"Remove-Item -LiteralPath '{}' -Force\"",
                        shell_victim.display()
                    )
                }),
            ),
            sse_text("done"),
        ],
        false,
        |_| ApprovalDecision::Deny,
    )
    .await;
    cases.push(serde_json::json!({
        "case": "shell_command_powershell_delete",
        "target_exists": shell_victim.exists(),
        "observed": observed_json(&observed),
    }));

    // A script's own indirect deletion is not inspected.
    let observed = command_case(
        &fixture,
        "python_indirect_delete",
        &python,
        vec!["indirect.py".into()],
        ApprovalDecision::Deny,
    )
    .await;
    cases.push(serde_json::json!({
        "case": "python_indirect_delete",
        "ran": fixture.repo.join("script-ran.txt").exists(),
        "deleted": !fixture.repo.join("disposable.txt").exists(),
        "observed": observed_json(&observed),
    }));

    let report = serde_json::json!({
        "gate": "W3",
        "host": { "os": std::env::consts::OS, "arch": std::env::consts::ARCH },
        "powershell": powershell,
        "cases": cases,
    });
    write_report("windows-permission-v2-shell", &report);

    let case = |name: &str| {
        cases
            .iter()
            .find(|case| case["case"] == name)
            .unwrap_or_else(|| panic!("missing case {name}"))
    };
    let approvals = |name: &str| {
        case(name)["observed"]["approvals"]
            .as_array()
            .unwrap()
            .len()
    };
    for name in [
        "ps_get_content",
        "ps_set_content",
        "ps_new_item",
        "cmd_echo_ordinary",
    ] {
        assert_eq!(approvals(name), 0, "{name} must not ask");
    }
    assert_eq!(case("ps_set_content")["file"], true);
    assert_eq!(case("ps_new_item")["dir"], true);
    assert_eq!(case("cmd_echo_ordinary")["file"], true);

    for (name, code) in [
        ("ps_remove_item", "irreversible.filesystem_delete"),
        ("ps_remove_item_recurse", "irreversible.filesystem_delete"),
        ("ps_remove_item_force", "irreversible.filesystem_delete"),
        ("cmd_erase", "irreversible.filesystem_delete"),
        ("cmd_rd_s", "irreversible.filesystem_delete"),
        ("ps_chinese_space_path", "irreversible.filesystem_delete"),
        ("ps_compound", "irreversible.filesystem_delete"),
        (
            "shell_command_powershell_delete",
            "irreversible.filesystem_delete",
        ),
    ] {
        assert_eq!(approvals(name), 1, "{name} must ask exactly once");
        assert_eq!(
            case(name)["observed"]["approvals"][0]["reason_code"],
            code,
            "{name}"
        );
        assert_eq!(case(name)["target_exists"], true, "{name}");
    }
    assert_eq!(
        approvals("ps_clear_content"),
        1,
        "Clear-Content destroys real data"
    );
    assert_eq!(
        case("ps_clear_content")["observed"]["approvals"][0]["reason_code"],
        "irreversible.filesystem_destroy"
    );
    assert_eq!(
        case("ps_clear_content")["content"],
        "precious\n",
        "a denied Clear-Content must leave the content intact"
    );
    assert_eq!(approvals("ps_remove_item_approved"), 1);
    assert_eq!(
        case("ps_remove_item_approved")["target_exists"],
        false,
        "an approved Remove-Item must execute"
    );
    assert_eq!(approvals("python_indirect_delete"), 0);
    assert_eq!(case("python_indirect_delete")["ran"], true);
    assert_eq!(case("python_indirect_delete")["deleted"], true);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "real Windows Permission Model V2 acceptance; run explicitly with --ignored"]
async fn windows_filesystem_boundaries() {
    let _env = env_lock().await;
    let home = tempfile::tempdir().unwrap();
    isolate_global_config(home.path());
    let fixture = Fixture::new().await;
    fixture.reset();
    let mut cases = Vec::new();

    // Ordinary outside write succeeds under V2 Auto.
    let outside = fixture.outside.join("ordinary.txt");
    let observed = run_case(
        &fixture,
        PermissionContract::V2,
        PermissionProfile::Assisted,
        "outside_write",
        vec![
            sse_call(
                "write_file",
                serde_json::json!({"path": outside.display().to_string(), "content": "ok\n"}),
            ),
            sse_text("done"),
        ],
        false,
        |_| ApprovalDecision::Deny,
    )
    .await;
    cases.push(serde_json::json!({
        "case": "outside_write",
        "file": outside.exists(),
        "observed": observed_json(&observed),
    }));

    // A path the OS does not allow this non-elevated user to write fails at
    // execution; the policy allowance is not reported as success.
    let protected = PathBuf::from(r"C:\Windows\System32")
        .join(format!("leveler-v2-acceptance-{}.txt", std::process::id()));
    let observed = run_case(
        &fixture,
        PermissionContract::V2,
        PermissionProfile::Assisted,
        "protected_write",
        vec![
            sse_call(
                "write_file",
                serde_json::json!({"path": protected.display().to_string(), "content": "x"}),
            ),
            sse_text("done"),
        ],
        false,
        |_| ApprovalDecision::Deny,
    )
    .await;
    cases.push(serde_json::json!({
        "case": "protected_write",
        "file": protected.exists(),
        "observed": observed_json(&observed),
    }));

    // Chinese and space directory inside the workspace.
    let chinese = fixture.repo.join("验收 目录");
    std::fs::create_dir_all(&chinese).unwrap();
    let chinese_file = chinese.join("中文 文件.txt");
    let observed = run_case(
        &fixture,
        PermissionContract::V2,
        PermissionProfile::Assisted,
        "chinese_space_write",
        vec![
            sse_call(
                "write_file",
                serde_json::json!({
                    "path": chinese_file.display().to_string(),
                    "content": "中文\n"
                }),
            ),
            sse_text("done"),
        ],
        false,
        |_| ApprovalDecision::Deny,
    )
    .await;
    cases.push(serde_json::json!({
        "case": "chinese_space_write",
        "file": chinese_file.exists(),
        "observed": observed_json(&observed),
    }));

    // A relative path resolving above the workspace is an ordinary outside
    // write under V2 Auto.
    let escaped = fixture._tmp.path().join("escaped.txt");
    let _ = std::fs::remove_file(&escaped);
    let observed = run_case(
        &fixture,
        PermissionContract::V2,
        PermissionProfile::Assisted,
        "relative_escape",
        vec![
            sse_call(
                "write_file",
                serde_json::json!({"path": "../escaped.txt", "content": "escape\n"}),
            ),
            sse_text("done"),
        ],
        false,
        |_| ApprovalDecision::Deny,
    )
    .await;
    cases.push(serde_json::json!({
        "case": "relative_escape",
        "file": escaped.exists(),
        "observed": observed_json(&observed),
    }));

    // A junction inside the workspace is an ordinary path, not a laundering
    // trick under V2 Auto (the write is allowed; the OS resolves the target).
    let junction = fixture.repo.join("junction-outside");
    let _ = std::fs::remove_dir(&junction);
    let mklink = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "New-Item -ItemType Junction -Path '{}' -Target '{}' | Out-Null",
                junction.display(),
                fixture.outside.display()
            ),
        ])
        .output();
    if mklink.is_ok_and(|output| output.status.success()) {
        let junction_file = fixture.outside.join("through-junction.txt");
        let _ = std::fs::remove_file(&junction_file);
        let observed = run_case(
            &fixture,
            PermissionContract::V2,
            PermissionProfile::Assisted,
            "junction_write",
            vec![
                sse_call(
                    "write_file",
                    serde_json::json!({
                        "path": junction.join("through-junction.txt").display().to_string(),
                        "content": "junction\n"
                    }),
                ),
                sse_text("done"),
            ],
            false,
            |_| ApprovalDecision::Deny,
        )
        .await;
        cases.push(serde_json::json!({
            "case": "junction_write",
            "target_file": junction_file.exists(),
            "observed": observed_json(&observed),
        }));
        let _ = std::fs::remove_dir(&junction);
        let _ = std::fs::remove_file(junction_file);
    } else {
        cases.push(serde_json::json!({
            "case": "junction_write",
            "status": "NOT_VERIFIED: mklink /J is unavailable for this account",
        }));
    }

    // A different drive when the host has one.
    let other_drive = Path::new(r"D:\");
    if other_drive.is_dir() {
        let target = other_drive.join(format!("leveler-v2-acceptance-{}.txt", std::process::id()));
        let observed = run_case(
            &fixture,
            PermissionContract::V2,
            PermissionProfile::Assisted,
            "second_drive_write",
            vec![
                sse_call(
                    "write_file",
                    serde_json::json!({"path": target.display().to_string(), "content": "d\n"}),
                ),
                sse_text("done"),
            ],
            false,
            |_| ApprovalDecision::Deny,
        )
        .await;
        let exists = target.exists();
        let _ = std::fs::remove_file(&target);
        cases.push(serde_json::json!({
            "case": "second_drive_write",
            "file": exists,
            "observed": observed_json(&observed),
        }));
    } else {
        cases.push(serde_json::json!({
            "case": "second_drive_write",
            "status": "NOT_VERIFIED: no second drive",
        }));
    }

    let report = serde_json::json!({
        "gate": "W4",
        "host": { "os": std::env::consts::OS, "arch": std::env::consts::ARCH },
        "cases": cases,
    });
    write_report("windows-permission-v2-filesystem", &report);

    let case = |name: &str| {
        cases
            .iter()
            .find(|case| case["case"] == name)
            .unwrap_or_else(|| panic!("missing case {name}"))
    };
    assert_eq!(
        case("outside_write")["observed"]["approvals"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    assert_eq!(case("outside_write")["file"], true);
    assert_eq!(
        case("protected_write")["file"],
        false,
        "a non-elevated user must not create a file in System32"
    );
    assert!(
        case("protected_write")["observed"]["tool_results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|result| result["tool"] == "write_file" && result["ok"] == false),
        "the protected write must surface an execution error: {}",
        case("protected_write")["observed"]["tool_results"]
    );
    assert_eq!(case("chinese_space_write")["file"], true);
    assert_eq!(case("relative_escape")["file"], true);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "real Windows Permission Model V2 acceptance; run explicitly with --ignored"]
async fn windows_explicit_network_denial() {
    let _env = env_lock().await;
    let Some(python) = python() else {
        panic!("no Python interpreter is available");
    };
    let home = tempfile::tempdir().unwrap();
    isolate_global_config(home.path());
    let fixture = Fixture::new().await;
    fixture.reset();
    let url = format!("http://{}/denied", fixture.http.address);
    let mut cases = Vec::new();

    let before = fixture.http.hits.load(Ordering::SeqCst);
    let observed = run_headless_case(
        &fixture,
        "deny_network_web_fetch",
        vec![
            sse_call(
                "capability",
                serde_json::json!({"action": "enable", "id": "web"}),
            ),
            sse_call("web_fetch", serde_json::json!({"url": url})),
            sse_text("done"),
        ],
        true,
    )
    .await;
    cases.push(serde_json::json!({
        "case": "deny_network_web_fetch",
        "served": fixture.http.hits.load(Ordering::SeqCst) > before,
        "observed": {
            "approvals": observed.approvals,
            "tool_results": observed.tool_results.iter().map(|(tool, ok, preview)| serde_json::json!({
                "tool": tool, "ok": ok, "preview": preview,
            })).collect::<Vec<_>>(),
            "stop": observed.stop,
            "failure": observed.failure,
        },
    }));

    let before = fixture.http.hits.load(Ordering::SeqCst);
    let script =
        format!("import urllib.request\nurllib.request.urlopen('{url}', timeout=5).read()\n");
    let observed = run_headless_case(
        &fixture,
        "deny_network_run_command",
        vec![
            sse_call(
                "run_command",
                serde_json::json!({"program": python, "args": ["-c", script]}),
            ),
            sse_text("done"),
        ],
        true,
    )
    .await;
    cases.push(serde_json::json!({
        "case": "deny_network_run_command",
        "served": fixture.http.hits.load(Ordering::SeqCst) > before,
        "observed": {
            "approvals": observed.approvals,
            "tool_results": observed.tool_results.iter().map(|(tool, ok, preview)| serde_json::json!({
                "tool": tool, "ok": ok, "preview": preview,
            })).collect::<Vec<_>>(),
            "stop": observed.stop,
            "failure": observed.failure,
        },
    }));

    let report = serde_json::json!({
        "gate": "W5",
        "host": { "os": std::env::consts::OS, "arch": std::env::consts::ARCH },
        "url": url,
        "cases": cases,
    });
    write_report("windows-permission-v2-network", &report);

    let case = |name: &str| {
        cases
            .iter()
            .find(|case| case["case"] == name)
            .unwrap_or_else(|| panic!("missing case {name}"))
    };
    for name in ["deny_network_web_fetch", "deny_network_run_command"] {
        assert_eq!(
            case(name)["served"],
            false,
            "{name}: the request reached the server despite an explicit denial"
        );
        assert_eq!(
            case(name)["observed"]["approvals"],
            0,
            "{name}: an enforced denial is a refusal, not a question"
        );
        assert!(
            case(name)["observed"]["tool_results"]
                .as_array()
                .unwrap()
                .iter()
                .any(|result| result["ok"] == false),
            "{name}: the denied call must fail honestly: {}",
            case(name)["observed"]["tool_results"]
        );
    }
}
