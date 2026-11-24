//! Permission Model V2 dogfood (Phase 2D).
//!
//! Four contract/profile arms plus Full, driven through the production runtime
//! (`Application` + `InProcessRuntimeClient`) with real tool execution, real
//! filesystem effects, real process spawning and real network scope checks.
//! The model side is a deterministic mock provider: this gate measures the
//! runtime's permission decisions and their execution consequences, not model
//! quality.
//!
//! ```text
//! v1_request_approval = LegacyRestricted
//! v1_assisted         = LegacyAuto
//! v2_request_approval = LegacyAuto       (tier 1 inherits old Auto)
//! v2_assisted         = EffectAuto       (tier 2)
//! full                = Full
//! ```
//!
//! Run explicitly:
//!
//! ```text
//! PERMISSION_DOGFOOD_REPORT=/tmp/permission-dogfood.json \
//!   cargo test -p leveler-app --test permission_model_v2_dogfood -- --ignored --nocapture
//! ```
//!
//! The JSON report is the machine-readable evidence indexed under
//! `docs/dev/permission-model-v2/`. This harness is macOS/Linux-only: its LAN
//! fixture asks `ipconfig` for a private address and uses a local bare Git
//! remote. Windows results are reported separately as UNVERIFIED.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    ApprovalDecision, ClientCommand, InteractiveRuntimeClient, RuntimeEvent,
};
use leveler_execution::{PermissionContract, PermissionProfile};
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_test_support::{MockResponse, MockServer};

const TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Arm {
    id: &'static str,
    contract: PermissionContract,
    profile: PermissionProfile,
}

const ARMS: [Arm; 5] = [
    Arm {
        id: "v1_request_approval",
        contract: PermissionContract::V1,
        profile: PermissionProfile::RequestApproval,
    },
    Arm {
        id: "v1_assisted",
        contract: PermissionContract::V1,
        profile: PermissionProfile::Assisted,
    },
    Arm {
        id: "v2_request_approval",
        contract: PermissionContract::V2,
        profile: PermissionProfile::RequestApproval,
    },
    Arm {
        id: "v2_assisted",
        contract: PermissionContract::V2,
        profile: PermissionProfile::Assisted,
    },
    Arm {
        id: "full",
        contract: PermissionContract::V2,
        profile: PermissionProfile::FullAccess,
    },
];

#[derive(Debug, Clone)]
struct CaseResult {
    case: String,
    approvals: usize,
    reason_codes: Vec<String>,
    executed: bool,
    tool_ok: Option<bool>,
    tool_preview: String,
    terminal: bool,
    failure: Option<String>,
    detail: String,
}

#[derive(Debug, Clone)]
struct ArmResult {
    arm: Arm,
    cases: Vec<CaseResult>,
}

#[derive(Debug)]
struct Fixture {
    _tmp: tempfile::TempDir,
    repo: PathBuf,
    outside: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let run = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(&repo)
                .env("GIT_AUTHOR_NAME", "dogfood")
                .env("GIT_AUTHOR_EMAIL", "dogfood@example.invalid")
                .env("GIT_COMMITTER_NAME", "dogfood")
                .env("GIT_COMMITTER_EMAIL", "dogfood@example.invalid")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        run(&["init", "-q"]);
        std::fs::write(repo.join("seed.txt"), "seed\n").unwrap();
        std::fs::write(
            repo.join("script.py"),
            "open('script-ran.txt', 'w').write('python\\n')\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("build.sh"),
            "#!/bin/sh\necho built > build-out.txt\n",
        )
        .unwrap();
        std::fs::write(repo.join("real-data.txt"), "precious\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(repo.join("build.sh"), PermissionsExt::from_mode(0o755));
        run(&["add", "-A"]);
        run(&["commit", "-qm", "seed", "--no-gpg-sign"]);
        Self {
            _tmp: tmp,
            repo,
            outside,
        }
    }

    fn reset(&self) {
        for name in [
            "script-ran.txt",
            "build-out.txt",
            "inside.txt",
            "real-data.txt",
        ] {
            let _ = std::fs::remove_file(self.repo.join(name));
        }
        let _ = std::fs::write(self.repo.join("real-data.txt"), "precious\n");
        for name in ["written-outside.txt", "deleted-outside.txt"] {
            let _ = std::fs::remove_file(self.outside.join(name));
        }
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

/// Build the isolated runtime for one turn and observe it. Every approval is
/// recorded and denied: the question is the measurement, and a denial proves
/// the operation did not slip through behind it.
async fn run_turn(
    fixture: &Fixture,
    arm: Arm,
    case: &str,
    tool: &str,
    arguments: serde_json::Value,
    enable_web: bool,
) -> CaseResult {
    let mut script = Vec::new();
    if enable_web {
        script.push(sse_call(
            "capability",
            serde_json::json!({"action": "enable", "id": "web"}),
        ));
    }
    script.push(sse_call(tool, arguments));
    script.push(sse_text("done"));
    let server = MockServer::start(script).await;
    let root = fixture._tmp.path().join(format!("app-{}-{case}", arm.id));
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
            "permission dogfood",
            arm.contract,
            arm.profile,
            app.collaboration(),
        )
        .await
        .unwrap();
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        model,
        arm.profile,
        false,
    ));
    let mut events = client.subscribe_session(&session);
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "run the dogfood operation".into(),
            attachments: vec![],
        })
        .await
        .unwrap();

    let mut approvals = 0;
    let mut reason_codes = Vec::new();
    let mut executed = false;
    let mut tool_ok = None;
    let mut tool_preview = String::new();
    let mut terminal = false;
    let mut failure = None;
    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline && !terminal {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Ok(RuntimeEvent::ApprovalRequested { request })) => {
                approvals += 1;
                reason_codes.push(
                    request
                        .reason_code
                        .clone()
                        .unwrap_or_else(|| "<none>".into()),
                );
                let _ = client
                    .send(ClientCommand::ApprovalDecision {
                        request_id: request.id.clone(),
                        decision: ApprovalDecision::Deny,
                    })
                    .await;
            }
            Ok(Ok(RuntimeEvent::ToolCallCompleted { ok, preview, .. })) => {
                executed = true;
                tool_ok = Some(ok);
                tool_preview = preview;
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
    CaseResult {
        case: case.to_string(),
        approvals,
        reason_codes,
        executed,
        tool_ok,
        tool_preview,
        terminal,
        failure,
        detail: String::new(),
    }
}

fn lan_ip() -> Option<String> {
    for service in ["en0", "en1", "en2"] {
        let output = Command::new("ipconfig")
            .args(["getifaddr", service])
            .output()
            .ok()?;
        let ip = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if output.status.success() && !ip.is_empty() {
            return Some(ip);
        }
    }
    None
}

struct LanServer {
    address: std::net::SocketAddr,
    hits: Arc<AtomicUsize>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for LanServer {
    fn drop(&mut self) {
        if let Some(sender) = self.shutdown.take() {
            let _ = sender.send(());
        }
        self.handle.abort();
    }
}

async fn start_lan_server() -> Option<LanServer> {
    let ip = lan_ip()?;
    let listener = tokio::net::TcpListener::bind((ip.as_str(), 0)).await.ok()?;
    let address = listener.local_addr().ok()?;
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    let (shutdown, mut stop) = tokio::sync::oneshot::channel();
    let handle = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut stop => break,
                accepted = listener.accept() => {
                    let Ok((mut stream, _)) = accepted else { break };
                    use tokio::io::AsyncWriteExt;
                    counter.fetch_add(1, Ordering::SeqCst);
                    let _ = stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                        .await;
                }
            }
        }
    });
    Some(LanServer {
        address,
        hits,
        shutdown: Some(shutdown),
        handle,
    })
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "Phase 2D dogfood; run explicitly with --ignored"]
async fn permission_model_v2_dogfood() {
    let fixture = Fixture::new();
    let lan = start_lan_server().await;
    let lan_url = lan
        .as_ref()
        .map(|server| format!("http://{}/probe", server.address));

    let mut results = Vec::new();
    for arm in ARMS {
        fixture.reset();
        let mut cases = Vec::new();

        let result = run_turn(
            &fixture,
            arm,
            "write_inside",
            "write_file",
            serde_json::json!({"path": "inside.txt", "content": "inside\n"}),
            false,
        )
        .await;
        cases.push(CaseResult {
            detail: format!(
                "approvals={} executed={} file={}",
                result.approvals,
                result.executed,
                fixture.repo.join("inside.txt").exists()
            ),
            ..result
        });

        let outside_path = fixture.outside.join("written-outside.txt");
        let result = run_turn(
            &fixture,
            arm,
            "write_outside",
            "write_file",
            serde_json::json!({
                "path": outside_path.display().to_string(),
                "content": "outside\n"
            }),
            false,
        )
        .await;
        cases.push(CaseResult {
            detail: format!(
                "approvals={} executed={} file={}",
                result.approvals,
                result.executed,
                outside_path.exists()
            ),
            ..result
        });

        let result = run_turn(
            &fixture,
            arm,
            "python_script",
            "run_command",
            serde_json::json!({"program": "python3", "args": ["script.py"]}),
            false,
        )
        .await;
        cases.push(CaseResult {
            detail: format!(
                "approvals={} executed={} ran={}",
                result.approvals,
                result.executed,
                fixture.repo.join("script-ran.txt").exists()
            ),
            ..result
        });

        let result = run_turn(
            &fixture,
            arm,
            "shell_build",
            "run_command",
            serde_json::json!({"program": "sh", "args": ["build.sh"]}),
            false,
        )
        .await;
        cases.push(CaseResult {
            detail: format!(
                "approvals={} executed={} built={}",
                result.approvals,
                result.executed,
                fixture.repo.join("build-out.txt").exists()
            ),
            ..result
        });

        // Ordinary Git: a local push is ordinary development. A fresh bare
        // remote per arm keeps the observation independent of the previous one.
        let remote = fixture._tmp.path().join(format!("origin-{}.git", arm.id));
        let _ = std::fs::remove_dir_all(&remote);
        let _ = Command::new("git")
            .args(["init", "--bare", "-q", remote.to_str().unwrap()])
            .output();
        let set_url = Command::new("git")
            .args(["remote", "set-url", "origin", remote.to_str().unwrap()])
            .current_dir(&fixture.repo)
            .output()
            .unwrap();
        if !set_url.status.success() {
            let _ = Command::new("git")
                .args(["remote", "add", "origin", remote.to_str().unwrap()])
                .current_dir(&fixture.repo)
                .output();
        }
        let result = run_turn(
            &fixture,
            arm,
            "git_push_local",
            "run_command",
            serde_json::json!({"program": "git", "args": ["push", "origin", "HEAD:refs/heads/main"]}),
            false,
        )
        .await;
        let pushed = Command::new("git")
            .args([
                "--git-dir",
                remote.to_str().unwrap(),
                "rev-parse",
                "refs/heads/main",
            ])
            .output()
            .is_ok_and(|output| output.status.success());
        cases.push(CaseResult {
            detail: format!(
                "approvals={} executed={} pushed={}",
                result.approvals, result.executed, pushed
            ),
            ..result
        });

        // A direct deletion of real data: the one ordinary tier-2 ask.
        fixture.reset();
        let victim = fixture.repo.join("real-data.txt");
        let result = run_turn(
            &fixture,
            arm,
            "delete_real_file",
            "run_command",
            serde_json::json!({
                "program": "rm",
                "args": ["-rf", victim.display().to_string()]
            }),
            false,
        )
        .await;
        cases.push(CaseResult {
            detail: format!(
                "approvals={} executed={} victim_exists={}",
                result.approvals,
                result.executed,
                victim.exists()
            ),
            ..result
        });

        // Git history loss: make the working tree dirty so `reset --hard`
        // would really discard a change when it runs.
        std::fs::write(fixture.repo.join("seed.txt"), "dirty\n").unwrap();
        let head_before = git_head(&fixture.repo);
        let result = run_turn(
            &fixture,
            arm,
            "git_reset_hard",
            "run_command",
            serde_json::json!({"program": "git", "args": ["reset", "--hard"]}),
            false,
        )
        .await;
        let head_after = git_head(&fixture.repo);
        let dirty_survived = std::fs::read_to_string(fixture.repo.join("seed.txt"))
            .is_ok_and(|content| content == "dirty\n");
        cases.push(CaseResult {
            detail: format!(
                "approvals={} executed={} head_changed={} dirty_survived={}",
                result.approvals,
                result.executed,
                head_before != head_after,
                dirty_survived
            ),
            ..result
        });

        // Private/LAN network: ordinary under EffectAuto.
        if let (Some(url), Some(server)) = (&lan_url, lan.as_ref()) {
            let before = server.hits.load(Ordering::SeqCst);
            let result = run_turn(
                &fixture,
                arm,
                "private_network",
                "web_fetch",
                serde_json::json!({"url": url}),
                true,
            )
            .await;
            let served = server.hits.load(Ordering::SeqCst) > before;
            cases.push(CaseResult {
                detail: format!(
                    "approvals={} executed={} served={}",
                    result.approvals, result.executed, served
                ),
                ..result
            });
        } else {
            cases.push(CaseResult {
                case: "private_network".into(),
                approvals: 0,
                reason_codes: vec![],
                executed: false,
                tool_ok: None,
                tool_preview: String::new(),
                terminal: false,
                failure: None,
                detail: "NOT_VERIFIED: no LAN address on this host".into(),
            });
        }

        results.push(ArmResult { arm, cases });
    }

    let mut failures = Vec::new();
    for result in &results {
        for case in &result.cases {
            if case.detail.starts_with("NOT_VERIFIED") {
                continue;
            }
            if !case.terminal {
                failures.push(format!(
                    "{}/{}: turn never reached a terminal ({:?})",
                    result.arm.id, case.case, case.failure
                ));
            }
        }
    }

    let get = |arm: &str, case: &str| -> Option<&CaseResult> {
        results
            .iter()
            .find(|result| result.arm.id == arm)?
            .cases
            .iter()
            .find(|candidate| candidate.case == case)
    };
    // Tier 2: ordinary work asks nothing, direct irreversible destruction asks.
    for case in [
        "write_inside",
        "write_outside",
        "python_script",
        "shell_build",
        "git_push_local",
        "private_network",
    ] {
        if let Some(observed) = get("v2_assisted", case)
            && !observed.detail.starts_with("NOT_VERIFIED")
            && observed.approvals != 0
        {
            failures.push(format!(
                "v2_assisted/{case}: expected 0 approvals, got {} ({:?})",
                observed.approvals, observed.reason_codes
            ));
        }
    }
    if let Some(observed) = get("v2_assisted", "private_network")
        && !observed.detail.starts_with("NOT_VERIFIED")
        && !observed.detail.contains("served=true")
    {
        failures.push(format!(
            "v2_assisted/private_network: the LAN request never reached the server: {} (tool={:?} {:?})",
            observed.detail, observed.tool_ok, observed.tool_preview
        ));
    }
    for case in ["delete_real_file", "git_reset_hard"] {
        if let Some(observed) = get("v2_assisted", case)
            && observed.approvals == 0
        {
            failures.push(format!(
                "v2_assisted/{case}: a directly identifiable irreversible effect must ask"
            ));
        }
    }
    if let Some(delete) = get("v2_assisted", "delete_real_file")
        && delete.detail.contains("victim_exists=false")
    {
        failures.push(format!(
            "v2_assisted/delete_real_file: the denied deletion removed the file: {}",
            delete.detail
        ));
    }
    if let Some(reset) = get("v2_assisted", "git_reset_hard")
        && !reset.detail.contains("dirty_survived=true")
    {
        failures.push(format!(
            "v2_assisted/git_reset_hard: the denied reset discarded the change: {}",
            reset.detail
        ));
    }
    // Full executes the reset: the dirty change must actually be gone, which
    // proves Full is not merely authorized but running.
    if let Some(reset) = get("full", "git_reset_hard")
        && !reset.detail.contains("dirty_survived=false")
    {
        failures.push(format!(
            "full/git_reset_hard: the full-access reset did not execute: {}",
            reset.detail
        ));
    }
    // Ordinary Git actually reaches the remote under tier 2 and Full.
    for arm in ["v2_assisted", "full"] {
        if let Some(observed) = get(arm, "git_push_local")
            && !observed.detail.contains("pushed=true")
        {
            failures.push(format!(
                "{arm}/git_push_local: the local push did not land: {} ({:?})",
                observed.detail, observed.tool_preview
            ));
        }
    }
    // Tier 2 executes the ordinary cases it allows.
    for case in [
        "write_inside",
        "python_script",
        "shell_build",
        "git_push_local",
    ] {
        if let Some(observed) = get("v2_assisted", case)
            && !observed.detail.contains("executed=true")
        {
            failures.push(format!(
                "v2_assisted/{case}: policy allowed it but it did not execute: {}",
                observed.detail
            ));
        }
    }
    // Full never asks, for any operation this harness runs.
    for case in &results
        .iter()
        .find(|result| result.arm.id == "full")
        .unwrap()
        .cases
    {
        if case.approvals != 0 {
            failures.push(format!(
                "full/{}: Full must never ask, got {} ({:?})",
                case.case, case.approvals, case.reason_codes
            ));
        }
    }
    // Tier-1 compatibility: V2 request-approval is the old Auto, so the two
    // arms must decide every case the same way.
    let left = results
        .iter()
        .find(|result| result.arm.id == "v1_assisted")
        .unwrap();
    let right = results
        .iter()
        .find(|result| result.arm.id == "v2_request_approval")
        .unwrap();
    for lc in &left.cases {
        let Some(rc) = right.cases.iter().find(|case| case.case == lc.case) else {
            continue;
        };
        if lc.detail.starts_with("NOT_VERIFIED") || rc.detail.starts_with("NOT_VERIFIED") {
            continue;
        }
        if lc.approvals != rc.approvals {
            failures.push(format!(
                "tier-compat/{}: v1_assisted approvals={} but v2_request_approval approvals={}",
                lc.case, lc.approvals, rc.approvals
            ));
        }
    }

    let report = serde_json::json!({
        "generated_at": leveler_core::now().to_rfc3339(),
        "host": {
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
        },
        "provider": "deterministic mock model (real runtime, real tools)",
        "lan": lan_url,
        "arms": results.iter().map(|result| serde_json::json!({
            "arm": result.arm.id,
            "contract": result.arm.contract.as_str(),
            "profile": result.arm.profile.as_str(),
            "cases": result.cases.iter().map(|case| serde_json::json!({
                "case": case.case,
                "approvals": case.approvals,
                "reason_codes": case.reason_codes,
                "executed": case.executed,
                "tool_ok": case.tool_ok,
                "tool_preview": case.tool_preview,
                "terminal": case.terminal,
                "failure": case.failure,
                "detail": case.detail,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "failures": failures,
        "status": if failures.is_empty() { "PASS" } else { "FAIL" },
    });
    let report_path = std::env::var("PERMISSION_DOGFOOD_REPORT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("target/permission-model-v2-dogfood.json"));
    if let Some(parent) = report_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&report_path, serde_json::to_string_pretty(&report).unwrap()).unwrap();
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
    println!("report: {}", report_path.display());
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

fn git_head(repo: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}
