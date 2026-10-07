//! Full / Auto permission acceptance matrix (Release Gate, always-on).
//!
//! These are the six product-contract cases stated as a permanent regression,
//! run through the production runtime (`Application` + `InProcessRuntimeClient`)
//! and observed on the client event stream:
//!
//! ```text
//! Full + rm -rf          => no permission approval
//! Full + git reset --hard=> no permission approval
//! Full + process kill    => no permission approval
//! Full + network         => no permission approval
//! Auto + dangerous       => permission approval
//! Auto + benign          => no permission approval
//! ```
//!
//! `full_permission_dogfood.rs` is the deeper, `#[ignore]`d release gate; this
//! file is the fast, always-on half so a regression that reintroduces a Full
//! prompt fails in the ordinary `cargo test` run instead of only in the gate.

use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    ApprovalDecision, ClientCommand, InteractiveRuntimeClient, RuntimeEvent, UiApprovalRequest,
};
use leveler_execution::PermissionProfile;
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_test_support::{MockResponse, MockServer};

const TIMEOUT: Duration = Duration::from_secs(30);

fn isolate_global_config() {
    use std::sync::OnceLock;
    static EMPTY_HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    let dir = EMPTY_HOME.get_or_init(|| tempfile::tempdir().unwrap());
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

fn tool_call_sse(name: &str, arguments: serde_json::Value) -> MockResponse {
    let call = serde_json::json!({
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
    .to_string();
    let finish =
        serde_json::json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]})
            .to_string();
    sse(vec![call, finish])
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

fn init_git(root: &std::path::Path) {
    let run = |args: &[&str]| {
        let _ = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_AUTHOR_NAME", "acceptance")
            .env("GIT_AUTHOR_EMAIL", "acceptance@example.invalid")
            .env("GIT_COMMITTER_NAME", "acceptance")
            .env("GIT_COMMITTER_EMAIL", "acceptance@example.invalid")
            .output();
    };
    run(&["init", "-q"]);
    std::fs::write(root.join("tracked.txt"), "x\n").unwrap();
    run(&["add", "tracked.txt"]);
    run(&["commit", "-q", "-m", "init", "--no-gpg-sign"]);
}

/// What one case observed on the client event stream.
struct Observed {
    approval: Option<UiApprovalRequest>,
    tool_completed: bool,
    terminal: bool,
    failure: Option<String>,
}

/// Run one operation under `mode` and observe the runtime's permission behaviour.
async fn run_case(
    mode: PermissionProfile,
    tool: &str,
    arguments: serde_json::Value,
    git: bool,
) -> (tempfile::TempDir, Observed) {
    let server = MockServer::start(vec![tool_call_sse(tool, arguments), text("done")]).await;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_config(root, &server.base_url());
    if git {
        init_git(root);
    }
    let app = Arc::new(
        Application::assemble(Layout::from_parts(
            root.to_path_buf(),
            root.join("configs"),
            root.join("state"),
        ))
        .unwrap(),
    );
    let model = ModelRef::new("mock", "m");
    let session_id = app
        .create_session_with_mode(&model, "acceptance", mode)
        .await
        .unwrap();
    let client = Arc::new(InProcessRuntimeClient::new(app.clone(), model, mode, false));
    let mut events = client.subscribe_session(&session_id);
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session_id.clone(),
            content: "run the acceptance operation".to_string(),
            attachments: vec![],
        })
        .await
        .unwrap();

    let mut observed = Observed {
        approval: None,
        tool_completed: false,
        terminal: false,
        failure: None,
    };
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while tokio::time::Instant::now() < deadline && !observed.terminal {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Ok(RuntimeEvent::ApprovalRequested { request })) => {
                if observed.approval.is_none() {
                    observed.approval = Some(request);
                }
                // The approval is the observation; deny it so the turn settles.
                let _ = client
                    .send(ClientCommand::ApprovalDecision {
                        request_id: observed.approval.as_ref().unwrap().id.clone(),
                        decision: ApprovalDecision::Deny,
                    })
                    .await;
            }
            Ok(Ok(RuntimeEvent::ToolCallCompleted { .. })) => observed.tool_completed = true,
            Ok(Ok(RuntimeEvent::TurnCompleted))
            | Ok(Ok(RuntimeEvent::TurnCompletedWithWarnings { .. }))
            | Ok(Ok(RuntimeEvent::TurnAnswered))
            | Ok(Ok(RuntimeEvent::TurnCancelled))
            | Ok(Ok(RuntimeEvent::TaskCancelled))
            | Ok(Ok(RuntimeEvent::TurnIncomplete { .. }))
            | Ok(Ok(RuntimeEvent::TurnTruncated { .. })) => observed.terminal = true,
            Ok(Ok(RuntimeEvent::TurnFailed { error, .. })) => {
                observed.failure = Some(error);
                observed.terminal = true;
            }
            Ok(Ok(_)) => {}
            Ok(Err(_)) => break,
            Err(_) => break,
        }
    }
    (tmp, observed)
}

fn assert_no_approval(case: &str, observed: &Observed) {
    assert!(
        observed.approval.is_none(),
        "{case}: FULL PERMISSION CONTRACT VIOLATION — a permission approval was requested: {:?}",
        observed.approval
    );
    assert!(
        observed.terminal,
        "{case}: the operation never reached a turn terminal (failure: {:?})",
        observed.failure
    );
}

fn assert_approval(case: &str, observed: &Observed) {
    assert!(
        observed.approval.is_some(),
        "{case}: Auto must ask before the dangerous operation runs"
    );
}

// ── Full: dangerous operations never ask ────────────────────────────────────

#[tokio::test]
async fn full_rm_rf_does_not_ask() {
    let tmp = tempfile::tempdir().unwrap();
    let victim = tmp.path().join("victim.txt");
    std::fs::write(&victim, "x").unwrap();
    let (_root, observed) = run_case(
        PermissionProfile::FullAccess,
        "run_command",
        serde_json::json!({"program": "rm", "args": ["-rf", victim.display().to_string()]}),
        false,
    )
    .await;
    assert_no_approval("Full rm -rf", &observed);
    assert!(
        !victim.exists(),
        "Full rm -rf must actually execute, not merely be authorized"
    );
}

#[tokio::test]
async fn full_git_reset_hard_does_not_ask() {
    let (_root, observed) = run_case(
        PermissionProfile::FullAccess,
        "run_command",
        serde_json::json!({"program": "git", "args": ["reset", "--hard"]}),
        true,
    )
    .await;
    assert_no_approval("Full git reset --hard", &observed);
}

#[cfg(unix)]
#[tokio::test]
async fn full_process_kill_does_not_ask() {
    use std::process::{Command, Stdio};

    /// Kill and reap the throwaway child even if an assertion panics first, so
    /// a failing run never leaks a process either.
    struct ChildGuard(std::process::Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    // This test's OWN throwaway child. It never names, signals or inspects any
    // system or unrelated process.
    let mut child = ChildGuard(
        Command::new("sleep")
            .arg("60")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn a throwaway sleep child"),
    );
    let pid = child.0.id();
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "child is alive before the kill"
    );

    let (_root, observed) = run_case(
        PermissionProfile::FullAccess,
        "run_command",
        serde_json::json!({"program": "kill", "args": ["-TERM", pid.to_string()]}),
        false,
    )
    .await;

    assert_no_approval("Full process kill", &observed);
    assert!(
        observed.tool_completed,
        "Full kill must actually execute the command, not merely be authorized"
    );

    // The child must really have received the signal and exited: the strongest
    // evidence that the command ran rather than being short-circuited.
    let mut exited = false;
    for _ in 0..150 {
        if child.0.try_wait().unwrap().is_some() {
            exited = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        exited,
        "the Full kill command must have terminated this test's own child process"
    );
}

#[tokio::test]
async fn full_network_does_not_ask() {
    let (_root, observed) = run_case(
        PermissionProfile::FullAccess,
        "web_fetch",
        serde_json::json!({"url": "http://127.0.0.1:1/"}),
        false,
    )
    .await;
    // The fetch itself may fail (nothing listens on :1); the permission verdict
    // is what this asserts: Full authorizes it without a prompt.
    assert_no_approval("Full network", &observed);
}

// ── Auto: the same gate still asks, and only where it should ────────────────

#[tokio::test]
async fn auto_dangerous_command_asks() {
    let tmp = tempfile::tempdir().unwrap();
    let victim = tmp.path().join("victim.txt");
    std::fs::write(&victim, "x").unwrap();
    let (_root, observed) = run_case(
        PermissionProfile::Assisted,
        "run_command",
        serde_json::json!({"program": "rm", "args": ["-rf", victim.display().to_string()]}),
        false,
    )
    .await;
    assert_approval("Auto rm -rf", &observed);
    assert!(victim.exists(), "a denied Auto operation must not have run");
}

#[tokio::test]
async fn auto_benign_command_does_not_ask() {
    let (_root, observed) = run_case(
        PermissionProfile::Assisted,
        "run_command",
        serde_json::json!({"program": "echo", "args": ["benign"]}),
        false,
    )
    .await;
    assert_no_approval("Auto benign command", &observed);
    assert!(
        observed.tool_completed,
        "the benign command must have executed under Auto"
    );
}
