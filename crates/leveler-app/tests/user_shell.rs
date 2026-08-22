//! User shell execution (`!command`) hard gates.
//!
//! The two non-negotiable product contracts, proven by counters — not by
//! reading the code:
//! - NO LLM: running a user shell performs ZERO model requests.
//! - NO CONTEXT LEAK: neither the command nor its output ever appears in
//!   what a later agent turn sends to the model.
//!
//! Plus the execution-environment and lifecycle contracts (cwd, streaming
//! shape, cancellation, busy conflict, snapshot restore).

use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::InteractiveRuntimeClient;
use leveler_client_protocol::{ClientCommand, RuntimeEvent};
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_test_support::{MockResponse, MockServer};

fn isolate_global_config() {
    use std::sync::OnceLock;
    static EMPTY_HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    let dir = EMPTY_HOME.get_or_init(|| tempfile::tempdir().unwrap());
    unsafe {
        std::env::set_var("LEVELER_HOME", dir.path());
    }
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
  context_window: 8192
  reliable_context: 4096
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

fn sse_text(text: &str) -> MockResponse {
    let frame = serde_json::json!({
        "choices": [{ "delta": { "content": text }, "finish_reason": "stop" }]
    })
    .to_string();
    MockResponse::Sse {
        body: format!("data: {frame}\n\ndata: [DONE]\n\n"),
    }
}

async fn app_with_repo(server: &MockServer) -> (tempfile::TempDir, Application) {
    let tmp = tempfile::tempdir().unwrap();
    // A real git repo so `!git status` shapes work and snapshots are honest.
    std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(tmp.path())
        .status()
        .unwrap();
    write_config(tmp.path(), &server.base_url());
    let layout = Layout::from_parts(
        tmp.path().to_path_buf(),
        tmp.path().join("configs"),
        tmp.path().join("state"),
    );
    let app = Application::assemble(layout).unwrap();
    (tmp, app)
}

struct Harness {
    _tmp: tempfile::TempDir,
    client: InProcessRuntimeClient,
    session_id: leveler_core::SessionId,
    events: tokio::sync::broadcast::Receiver<RuntimeEvent>,
}

async fn harness(server: &MockServer) -> Harness {
    let (tmp, app) = app_with_repo(server).await;
    let session_id = app
        .create_session(&ModelRef::new("mock", "m"), "user shell test")
        .await
        .unwrap();
    let client = InProcessRuntimeClient::new_with_options(
        std::sync::Arc::new(app),
        ModelRef::new("mock", "m"),
        leveler_execution::PermissionProfile::Assisted,
        /* sandbox */ false,
        /* auto_approve */ true,
    );
    let events = client.subscribe_session(&session_id);
    Harness {
        _tmp: tmp,
        client,
        session_id,
        events,
    }
}

/// Drain events until the user shell exits (or time out loudly).
async fn wait_for_exit(h: &mut Harness) -> Vec<RuntimeEvent> {
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let ev = tokio::time::timeout(remaining, h.events.recv())
            .await
            .expect("user shell events arrived before the deadline")
            .expect("stream open");
        let done = matches!(ev, RuntimeEvent::UserShellExited { .. });
        seen.push(ev);
        if done {
            return seen;
        }
    }
}

fn run_shell(h: &Harness, command: &str) -> ClientCommand {
    ClientCommand::RunUserShell {
        session_id: h.session_id.clone(),
        command: command.to_string(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hard_gate_user_shell_makes_zero_model_requests() {
    let server = MockServer::start(vec![sse_text("unused")]).await;
    let mut h = harness(&server).await;

    h.client.send(run_shell(&h, "echo hello")).await.unwrap();
    let events = wait_for_exit(&mut h).await;

    // Lifecycle shape: started → stdout chunk → exited(success).
    assert!(matches!(
        events.first(),
        Some(RuntimeEvent::UserShellStarted { command, .. }) if command == "echo hello"
    ));
    assert!(events.iter().any(|e| matches!(
        e,
        RuntimeEvent::UserShellOutput { stream, chunk, .. }
            if stream == "stdout" && chunk.contains("hello")
    )));
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::UserShellExited { exit_code: Some(0), status, .. }) if status == "success"
    ));

    // THE hard gate: zero provider requests, proven by the counter.
    assert_eq!(
        server.request_count(),
        0,
        "a user shell must never reach the model"
    );
}

/// The session must be free the instant the shell says it finished.
///
/// A client reacts to `UserShellExited` by enabling its composer, and a user
/// who types immediately submits within microseconds. If the runtime publishes
/// the terminal event before it releases the session's turn slot, that submit
/// is rejected with "session ... already has an active turn" — a real user
/// seeing a finished command and being told the agent is busy.
///
/// Ten rounds because the window is small: on a loaded machine (CI) it opens
/// wide enough to fail, on an idle laptop it can stay shut all day. After the
/// release-before-publish ordering it is not a window at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_session_is_free_the_moment_the_shell_reports_exit() {
    use leveler_local_transport::LocalRuntimeService;

    let server = MockServer::start(vec![sse_text("unused")]).await;
    let mut h = harness(&server).await;

    for round in 0..10 {
        h.client
            .send(run_shell(&h, &format!("echo round-{round}")))
            .await
            .unwrap();
        wait_for_exit(&mut h).await;
        let info = h.client.runtime_info().await.expect("runtime info");
        assert_eq!(
            info.health.active_turns, 0,
            "round {round}: the shell reported exit while the session was still \
             holding its turn slot — the next user message would be refused"
        );
    }

    assert_eq!(
        server.request_count(),
        0,
        "this test must not reach the model at all"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hard_gate_shell_output_never_enters_model_context() {
    let server = MockServer::start(vec![sse_text("model answer")]).await;
    let mut h = harness(&server).await;

    h.client
        .send(run_shell(&h, "echo SECRET_TEST_VALUE_XYZZY"))
        .await
        .unwrap();
    wait_for_exit(&mut h).await;

    // Now a NORMAL agent message: whatever the model receives must not
    // contain the shell command or its output.
    h.client
        .send(ClientCommand::SubmitMessage {
            session_id: h.session_id.clone(),
            content: "say hi".into(),
            attachments: Vec::new(),
        })
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let ev = tokio::time::timeout(remaining, h.events.recv())
            .await
            .expect("agent turn finished")
            .expect("stream open");
        if matches!(
            ev,
            RuntimeEvent::TurnCompleted
                | RuntimeEvent::TurnAnswered
                | RuntimeEvent::TurnFailed { .. }
                | RuntimeEvent::TurnIncomplete { .. }
                | RuntimeEvent::TurnCompletedUnverified { .. }
        ) {
            break;
        }
    }
    assert!(server.request_count() >= 1, "the agent turn used the model");
    for body in server.request_bodies().await {
        assert!(
            !body.contains("SECRET_TEST_VALUE_XYZZY"),
            "shell output leaked into the model request: {body}"
        );
        assert!(
            !body.contains("!echo"),
            "the shell command leaked into the model request: {body}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_runs_in_the_repository_root() {
    let server = MockServer::start(vec![sse_text("unused")]).await;
    let mut h = harness(&server).await;
    // `pwd` is not a command on Windows; `cd` with no argument is how cmd
    // prints the working directory. The property is the same either way.
    let print_cwd = if cfg!(windows) { "cd" } else { "pwd" };
    h.client.send(run_shell(&h, print_cwd)).await.unwrap();
    let events = wait_for_exit(&mut h).await;
    let repo = h._tmp.path().canonicalize().unwrap();
    let output: String = events
        .iter()
        .filter_map(|e| match e {
            RuntimeEvent::UserShellOutput { chunk, .. } => Some(chunk.as_str()),
            _ => None,
        })
        .collect();
    let printed = std::path::Path::new(output.trim())
        .canonicalize()
        .expect("pwd printed a real path");
    assert_eq!(printed, repo, "user shell cwd is the repository root");
    assert_eq!(server.request_count(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_stops_exactly_the_named_execution() {
    let server = MockServer::start(vec![sse_text("unused")]).await;
    let mut h = harness(&server).await;
    h.client
        .send(run_shell(&h, "echo begin; sleep 30"))
        .await
        .unwrap();
    // Wait for the started event to learn the execution id.
    let id = loop {
        let ev = tokio::time::timeout(Duration::from_secs(10), h.events.recv())
            .await
            .expect("started")
            .expect("open");
        if let RuntimeEvent::UserShellStarted { execution_id, .. } = ev {
            break execution_id;
        }
    };
    // A stale/wrong id must not kill it.
    h.client
        .send(ClientCommand::CancelUserShell {
            session_id: h.session_id.clone(),
            execution_id: leveler_core::UserShellId::new("ush-does-not-exist"),
        })
        .await
        .unwrap();
    // The right id does.
    h.client
        .send(ClientCommand::CancelUserShell {
            session_id: h.session_id.clone(),
            execution_id: id,
        })
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let status = loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let ev = tokio::time::timeout(remaining, h.events.recv())
            .await
            .expect("exited after cancel")
            .expect("open");
        if let RuntimeEvent::UserShellExited { status, .. } = ev {
            break status;
        }
    };
    assert_eq!(status, "cancelled");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn busy_agent_turn_rejects_a_user_shell_without_side_effects() {
    // A slow model response keeps the agent turn running while we try `!`.
    let server = MockServer::start(vec![MockResponse::Sse {
        body: {
            let frame = serde_json::json!({
                "choices": [{ "delta": { "content": "thinking" }, "finish_reason": "stop" }]
            })
            .to_string();
            format!("data: {frame}\n\ndata: [DONE]\n\n")
        },
    }])
    .await;
    let mut h = harness(&server).await;
    h.client
        .send(ClientCommand::SubmitMessage {
            session_id: h.session_id.clone(),
            content: "hello".into(),
            attachments: Vec::new(),
        })
        .await
        .unwrap();
    // Immediately race a shell that would create a file.
    let marker = h._tmp.path().join("conflict.txt");
    h.client
        .send(run_shell(&h, "touch conflict.txt"))
        .await
        .unwrap();
    // Either the turn was still admitted (busy → rejected with a
    // notification and no side effect) or the turn already finished and the
    // shell ran. Both are legal serializations; the invariant is: if it was
    // rejected, the file must not exist.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut rejected = false;
    let mut shell_ran = false;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let Ok(Ok(ev)) = tokio::time::timeout(remaining, h.events.recv()).await else {
            break;
        };
        match ev {
            RuntimeEvent::Notification { message, .. } if message.contains("Agent 正在运行") => {
                rejected = true;
                break;
            }
            RuntimeEvent::UserShellExited { .. } => {
                shell_ran = true;
                break;
            }
            _ => {}
        }
    }
    assert!(rejected || shell_ran, "one outcome must be observable");
    if rejected {
        assert!(
            !marker.exists(),
            "a rejected shell must not have touched the workspace"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_restores_active_shell_with_elapsed() {
    let server = MockServer::start(vec![sse_text("unused")]).await;
    let mut h = harness(&server).await;
    // Something that stays alive for a few seconds on either platform. cmd has
    // no `sleep`, and `ping` needs a network the confined profile does not
    // grant; `waitfor` blocks on a signal that never arrives and needs neither
    // a console nor a socket.
    let stay_alive = if cfg!(windows) {
        "waitfor /t 5 LevelerUserShellTest"
    } else {
        "sleep 5"
    };
    h.client.send(run_shell(&h, stay_alive)).await.unwrap();
    // Started…
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(10), h.events.recv())
            .await
            .expect("started")
            .expect("open");
        if matches!(ev, RuntimeEvent::UserShellStarted { .. }) {
            break;
        }
    }
    tokio::time::sleep(Duration::from_millis(1200)).await;
    // A reconnecting client discovers the running shell via snapshot.
    let snap = h.client.snapshot(&h.session_id).await.unwrap();
    let shell = snap
        .user_shells
        .iter()
        .find(|s| s.status == "running")
        .expect("active user shell rides the snapshot");
    assert_eq!(shell.command, stay_alive);
    assert!(shell.elapsed_secs >= 1, "elapsed does not reset");
    // Clean up: cancel it.
    h.client
        .send(ClientCommand::CancelUserShell {
            session_id: h.session_id.clone(),
            execution_id: shell.id.clone(),
        })
        .await
        .unwrap();
    let _ = wait_for_exit(&mut h).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn shell_syntax_pipes_env_and_failure_exit() {
    let server = MockServer::start(vec![sse_text("unused")]).await;

    // Pipe: `echo hello | wc -c` → 6 (5 chars + newline).
    let mut h = harness(&server).await;
    h.client
        .send(run_shell(&h, "echo hello | wc -c"))
        .await
        .unwrap();
    let events = wait_for_exit(&mut h).await;
    let out: String = events
        .iter()
        .filter_map(|e| match e {
            RuntimeEvent::UserShellOutput { chunk, .. } => Some(chunk.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(out.trim(), "6", "pipe semantics preserved: {out:?}");

    // Environment assignment inside the shell string.
    let mut h = harness(&server).await;
    h.client
        .send(run_shell(&h, "FOO=bar sh -c 'echo \"$FOO\"'"))
        .await
        .unwrap();
    let events = wait_for_exit(&mut h).await;
    let out: String = events
        .iter()
        .filter_map(|e| match e {
            RuntimeEvent::UserShellOutput { chunk, .. } => Some(chunk.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(out.trim(), "bar", "{out:?}");

    // stderr is tagged; non-zero exit is failed with its code.
    let mut h = harness(&server).await;
    h.client
        .send(run_shell(&h, "echo oops 1>&2; false"))
        .await
        .unwrap();
    let events = wait_for_exit(&mut h).await;
    assert!(events.iter().any(|e| matches!(
        e,
        RuntimeEvent::UserShellOutput { stream, chunk, .. }
            if stream == "stderr" && chunk.contains("oops")
    )));
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::UserShellExited { exit_code: Some(1), status, .. })
            if status == "failed"
    ));
    assert_eq!(server.request_count(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn large_output_stays_bounded_with_a_truncation_flag() {
    let server = MockServer::start(vec![sse_text("unused")]).await;
    let mut h = harness(&server).await;
    // ~200 KiB >> the 64 KiB tail cap.
    h.client
        .send(run_shell(
            &h,
            "i=0; while [ $i -lt 2000 ]; do printf '%0100d\\n' $i; i=$((i+1)); done",
        ))
        .await
        .unwrap();
    let events = wait_for_exit(&mut h).await;
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::UserShellExited { status, .. }) if status == "success"
    ));
    let snap = h.client.snapshot(&h.session_id).await.unwrap();
    let shell = snap.user_shells.last().expect("in history");
    assert!(
        shell.output_tail.len() <= 64 * 1024,
        "tail bounded: {} bytes",
        shell.output_tail.len()
    );
    assert!(shell.output_truncated, "truncation is flagged, not silent");
    assert!(
        shell.output_tail.contains("1999"),
        "the tail keeps the END of the output"
    );
}
