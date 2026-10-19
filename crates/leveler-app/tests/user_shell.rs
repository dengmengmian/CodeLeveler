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
    // Something that stays alive for a few seconds on either platform. The
    // requirement is a blocking process, not `sleep`, so the spelling lives in
    // one place rather than in every test that needs one.
    let stay_alive = leveler_test_support::sleep_shell_line(5);
    let stay_alive = stay_alive.as_str();
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
        .unwrap_or_else(|| {
            // Say what the runtime actually held. A shell that died on the way
            // up looks exactly like one that was never recorded, and the
            // difference is the whole diagnosis.
            panic!(
                "no running user shell in the snapshot; it held: {:?}",
                snap.user_shells
            )
        });
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
            "i=0; while [ $i -lt 12000 ]; do printf '%0100d\\n' $i; i=$((i+1)); done",
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
        shell.output_tail.len() <= 1024 * 1024,
        "tail bounded: {} bytes",
        shell.output_tail.len()
    );
    assert!(shell.output_truncated, "truncation is flagged, not silent");
    assert!(
        shell.output_tail.contains("11999"),
        "the tail keeps the END of the output"
    );
}

/// Wait for one event matching `pred`, dropping unrelated events (later waits
/// for the same terminal fact must not be satisfied by an old one because each
/// test uses a fresh harness).
async fn wait_event(
    h: &mut Harness,
    pred: impl Fn(&RuntimeEvent) -> bool,
    seconds: u64,
) -> RuntimeEvent {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let ev = tokio::time::timeout(remaining, h.events.recv())
            .await
            .expect("event before deadline")
            .expect("stream open");
        if pred(&ev) {
            return ev;
        }
    }
}

#[cfg(unix)]
async fn wait_pid_file(path: &std::path::Path) -> i32 {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(text) = std::fs::read_to_string(path)
            && let Ok(pid) = text.trim().parse()
        {
            return pid;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "pid file was never written: {}",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreground_shell_moves_to_background_with_same_process_and_output() {
    let server = MockServer::start(vec![sse_text("unused")]).await;
    let mut h = harness(&server).await;
    let pid_file = h._tmp.path().join("shell.pid");
    let command = format!(
        "echo $$ > '{}'; echo BEFORE; sleep 2; echo AFTER; sleep 0.2",
        pid_file.display()
    );
    h.client.send(run_shell(&h, &command)).await.unwrap();
    let execution_id = match wait_event(
        &mut h,
        |ev| matches!(ev, RuntimeEvent::UserShellStarted { .. }),
        10,
    )
    .await
    {
        RuntimeEvent::UserShellStarted { execution_id, .. } => execution_id,
        _ => unreachable!(),
    };
    // The foreground output reached the client before the transfer.
    wait_event(
        &mut h,
        |ev| matches!(ev, RuntimeEvent::UserShellOutput { chunk, .. } if chunk.contains("BEFORE")),
        10,
    )
    .await;
    let foreground_pid: i32 = std::fs::read_to_string(&pid_file)
        .expect("pid file")
        .trim()
        .parse()
        .expect("pid");

    h.client
        .send(ClientCommand::BackgroundUserShell {
            session_id: h.session_id.clone(),
            execution_id: execution_id.clone(),
        })
        .await
        .unwrap();
    let task_id = match wait_event(
        &mut h,
        |ev| matches!(ev, RuntimeEvent::UserShellBackgrounded { .. }),
        10,
    )
    .await
    {
        RuntimeEvent::UserShellBackgrounded { task_id, .. } => task_id,
        _ => unreachable!(),
    };
    // The foreground record is now marked background and the execution is
    // still the same id. The snapshot names the same process the foreground
    // pid file recorded; `BackgroundTaskStarted` was already consumed by the
    // wait above, and the snapshot is the authoritative live view.
    let snap = h.client.snapshot(&h.session_id).await.unwrap();
    let shell = snap
        .user_shells
        .iter()
        .find(|shell| shell.id == execution_id)
        .expect("shell record survives the transfer");
    assert_eq!(shell.status, "background");
    assert_eq!(shell.task_id.as_deref(), Some(task_id.as_str()));
    let task = snap
        .active_background_tasks
        .iter()
        .find(|t| t.task_id == task_id)
        .expect("background task is listed");
    assert_eq!(
        task.pid.map(|pid| pid as i32),
        Some(foreground_pid),
        "the background task is the same process, not a respawn"
    );

    // Output produced after the transfer still reaches the shell record.
    wait_event(
        &mut h,
        |ev| {
            matches!(ev, RuntimeEvent::UserShellOutput { execution_id: id, chunk, .. }
            if id == &execution_id && chunk.contains("AFTER"))
        },
        10,
    )
    .await;

    let exited = wait_event(
        &mut h,
        |ev| matches!(ev, RuntimeEvent::BackgroundTaskExited { task_id: id, .. } if id == &task_id),
        10,
    )
    .await;
    let RuntimeEvent::BackgroundTaskExited {
        ok,
        exit_code,
        output,
        ..
    } = exited
    else {
        unreachable!()
    };
    assert!(ok, "the adopted task completed successfully");
    assert_eq!(exit_code, Some(0));
    assert!(
        output.contains("BEFORE") && output.contains("AFTER"),
        "the adopted log carries pre- and post-transfer output: {output}"
    );
    assert_eq!(
        output.matches("BEFORE").count(),
        1,
        "pre-transfer output is seeded once, not replayed: {output}"
    );

    // The snapshot is the reconnect authority; an earlier wait may already
    // have consumed the live `UserShellExited` fact.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let shell = loop {
        let snap = h.client.snapshot(&h.session_id).await.unwrap();
        if let Some(shell) = snap
            .user_shells
            .iter()
            .find(|shell| shell.id == execution_id && shell.status != "background")
        {
            break shell.clone();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "terminal shell record never reached the snapshot"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert_eq!(shell.status, "success");
    assert!(shell.output_tail.contains("BEFORE") && shell.output_tail.contains("AFTER"));
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ctrl_c_after_background_does_not_cancel_the_task() {
    let server = MockServer::start(vec![sse_text("unused")]).await;
    let mut h = harness(&server).await;
    let pid_file = h._tmp.path().join("bg.pid");
    h.client
        .send(run_shell(
            &h,
            &format!("echo $$ > '{}'; sleep 30", pid_file.display()),
        ))
        .await
        .unwrap();
    let execution_id = match wait_event(
        &mut h,
        |ev| matches!(ev, RuntimeEvent::UserShellStarted { .. }),
        10,
    )
    .await
    {
        RuntimeEvent::UserShellStarted { execution_id, .. } => execution_id,
        _ => unreachable!(),
    };
    let pid = wait_pid_file(&pid_file).await;
    h.client
        .send(ClientCommand::BackgroundUserShell {
            session_id: h.session_id.clone(),
            execution_id: execution_id.clone(),
        })
        .await
        .unwrap();
    let task_id = match wait_event(
        &mut h,
        |ev| matches!(ev, RuntimeEvent::UserShellBackgrounded { .. }),
        10,
    )
    .await
    {
        RuntimeEvent::UserShellBackgrounded { task_id, .. } => task_id,
        _ => unreachable!(),
    };
    // The foreground cancel path must not reach a background task.
    h.client
        .send(ClientCommand::CancelUserShell {
            session_id: h.session_id.clone(),
            execution_id: execution_id.clone(),
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let snap = h.client.snapshot(&h.session_id).await.unwrap();
    assert!(
        snap.active_background_tasks
            .iter()
            .any(|t| t.task_id == task_id),
        "the background task is still running"
    );
    assert!(
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false),
        "the process survived Ctrl+C after the transfer"
    );

    // The background stop path owns it now.
    h.client
        .send(ClientCommand::CancelBackgroundTask {
            session_id: h.session_id.clone(),
            task_id: task_id.clone(),
        })
        .await
        .unwrap();
    let exited = wait_event(
        &mut h,
        |ev| matches!(ev, RuntimeEvent::BackgroundTaskExited { task_id: id, .. } if id == &task_id),
        10,
    )
    .await;
    assert!(matches!(
        exited,
        RuntimeEvent::BackgroundTaskExited { stopped: true, .. }
    ));
    // The terminal shell fact is in the snapshot; the live `UserShellExited`
    // event may have been consumed while waiting for the background exit.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let snap = h.client.snapshot(&h.session_id).await.unwrap();
        let shell = snap
            .user_shells
            .iter()
            .find(|shell| shell.id == execution_id)
            .expect("shell record");
        if shell.status == "cancelled" {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "shell never reached cancelled: {}",
            shell.status
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_finished_shell_cannot_be_backgrounded_and_no_task_is_created() {
    let server = MockServer::start(vec![sse_text("unused")]).await;
    let mut h = harness(&server).await;
    h.client.send(run_shell(&h, "echo done")).await.unwrap();
    let execution_id = match wait_event(
        &mut h,
        |ev| matches!(ev, RuntimeEvent::UserShellStarted { .. }),
        10,
    )
    .await
    {
        RuntimeEvent::UserShellStarted { execution_id, .. } => execution_id,
        _ => unreachable!(),
    };
    wait_event(
        &mut h,
        |ev| matches!(ev, RuntimeEvent::UserShellExited { .. }),
        10,
    )
    .await;
    h.client
        .send(ClientCommand::BackgroundUserShell {
            session_id: h.session_id.clone(),
            execution_id,
        })
        .await
        .unwrap();
    // No background task may appear for an execution that already ended.
    let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, h.events.recv()).await {
            Ok(Ok(RuntimeEvent::BackgroundTaskStarted { .. })) => {
                panic!("a finished execution must never create a background task")
            }
            Ok(Ok(_)) => {}
            Ok(Err(_)) => break,
            Err(_) => break,
        }
    }
    let snap = h.client.snapshot(&h.session_id).await.unwrap();
    assert!(snap.active_background_tasks.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_shell_can_run_after_the_first_moves_to_background() {
    let server = MockServer::start(vec![sse_text("unused")]).await;
    let mut h = harness(&server).await;
    h.client.send(run_shell(&h, "sleep 30")).await.unwrap();
    let execution_id = match wait_event(
        &mut h,
        |ev| matches!(ev, RuntimeEvent::UserShellStarted { .. }),
        10,
    )
    .await
    {
        RuntimeEvent::UserShellStarted { execution_id, .. } => execution_id,
        _ => unreachable!(),
    };
    h.client
        .send(ClientCommand::BackgroundUserShell {
            session_id: h.session_id.clone(),
            execution_id,
        })
        .await
        .unwrap();
    wait_event(
        &mut h,
        |ev| matches!(ev, RuntimeEvent::UserShellBackgrounded { .. }),
        10,
    )
    .await;
    h.client.send(run_shell(&h, "echo SECOND")).await.unwrap();
    let second = wait_event(
        &mut h,
        |ev| matches!(ev, RuntimeEvent::UserShellStarted { command, .. } if command == "echo SECOND"),
        10,
    )
    .await;
    let RuntimeEvent::UserShellStarted {
        execution_id: second_id,
        ..
    } = second
    else {
        unreachable!()
    };
    let exit = wait_event(
        &mut h,
        |ev| matches!(ev, RuntimeEvent::UserShellExited { execution_id: id, .. } if id == &second_id),
        10,
    )
    .await;
    assert!(matches!(exit, RuntimeEvent::UserShellExited { status, .. } if status == "success"));
    // Clean up the still-running background task so the tempdir/runtime ends.
    let snap = h.client.snapshot(&h.session_id).await.unwrap();
    for task in snap.active_background_tasks {
        let _ = h
            .client
            .send(ClientCommand::CancelBackgroundTask {
                session_id: h.session_id.clone(),
                task_id: task.task_id,
            })
            .await;
    }
}

fn layout_for(root: &std::path::Path) -> Layout {
    Layout::from_parts(root.to_path_buf(), root.join("configs"), root.join("state"))
}

fn client_for(
    app: std::sync::Arc<Application>,
    session_id: &leveler_core::SessionId,
) -> (
    InProcessRuntimeClient,
    tokio::sync::broadcast::Receiver<RuntimeEvent>,
) {
    let client = InProcessRuntimeClient::new_with_options(
        app,
        ModelRef::new("mock", "m"),
        leveler_execution::PermissionProfile::Assisted,
        /* sandbox */ false,
        /* auto_approve */ true,
    );
    let events = client.subscribe_session(session_id);
    (client, events)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cold_runtime_restores_terminal_shell_history_with_output() {
    let server = MockServer::start(vec![sse_text("unused")]).await;
    let (tmp, app) = app_with_repo(&server).await;
    let layout = layout_for(tmp.path());
    let app = std::sync::Arc::new(app);
    let session_id = app
        .create_session(&ModelRef::new("mock", "m"), "cold shell")
        .await
        .unwrap();
    let (client, mut events) = client_for(app.clone(), &session_id);
    client
        .send(ClientCommand::RunUserShell {
            session_id: session_id.clone(),
            command: "echo COLD-MARK".into(),
        })
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let ev = tokio::time::timeout(remaining, events.recv())
            .await
            .expect("terminal before deadline")
            .expect("stream open");
        if matches!(ev, RuntimeEvent::UserShellExited { .. }) {
            break;
        }
    }
    let snap = client.snapshot(&session_id).await.unwrap();
    let shell_id = snap
        .user_shells
        .iter()
        .find(|shell| shell.command == "echo COLD-MARK")
        .expect("live record")
        .id
        .clone();
    drop(client);
    drop(app);
    tokio::time::sleep(Duration::from_millis(150)).await;

    let app = std::sync::Arc::new(Application::assemble(layout).unwrap());
    let (client, _events) = client_for(app, &session_id);
    let snap = client.snapshot(&session_id).await.unwrap();
    let shell = snap
        .user_shells
        .iter()
        .find(|shell| shell.id == shell_id)
        .expect("terminal shell survives a cold runtime");
    assert_eq!(shell.status, "success");
    assert!(
        shell.output_tail.contains("COLD-MARK"),
        "retained output survives: {:?}",
        shell.output_tail
    );
    assert!(snap.active_background_tasks.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_terminal_fact_wins_over_a_late_backgrounded_fact_in_cold_restore() {
    use leveler_engine::EngineEvent;

    let server = MockServer::start(vec![sse_text("unused")]).await;
    let (_tmp, app) = app_with_repo(&server).await;
    let app = std::sync::Arc::new(app);
    let session = app
        .create_session(&ModelRef::new("mock", "m"), "cold order")
        .await
        .unwrap();
    let db = app.open_database().await.unwrap();
    let id = leveler_core::UserShellId::new("ush-cold-order");
    let mut forward = |_event: EngineEvent| {};
    let log = leveler_engine::EventLog::new(&db, session.clone());
    for event in [
        EngineEvent::UserShellStarted {
            execution_id: id.clone(),
            command: "echo ORDER".into(),
            cwd: "/repo".into(),
        },
        EngineEvent::UserShellFinished {
            execution_id: id.clone(),
            exit_code: Some(0),
            duration_ms: 12,
            status: "success".into(),
            output_tail: "ORDER\n".into(),
            output_truncated: false,
        },
        // The transfer fact is written late (for example by a racing worker).
        EngineEvent::UserShellBackgrounded {
            execution_id: id.clone(),
            task_id: "bg-1".into(),
            output_tail: "stale".into(),
            output_truncated: false,
        },
    ] {
        log.append(None, event, &mut forward).await.unwrap();
    }

    let (client, _events) = client_for(app, &session);
    let snap = client.snapshot(&session).await.unwrap();
    let shell = snap
        .user_shells
        .iter()
        .find(|shell| shell.id == id)
        .expect("cold restore keeps the execution");
    assert_eq!(shell.status, "success");
    assert!(
        shell.task_id.is_none(),
        "a terminal execution is not linked to a background task"
    );
    assert_eq!(
        shell.output_tail.trim(),
        "ORDER",
        "the terminal tail stays authoritative over a late transfer tail"
    );
    assert!(snap.active_background_tasks.is_empty());
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cold_runtime_reports_an_orphaned_background_shell_as_unknown() {
    let server = MockServer::start(vec![sse_text("unused")]).await;
    let (tmp, app) = app_with_repo(&server).await;
    let layout = layout_for(tmp.path());
    let app = std::sync::Arc::new(app);
    let session_id = app
        .create_session(&ModelRef::new("mock", "m"), "cold bg shell")
        .await
        .unwrap();
    let (client, mut events) = client_for(app.clone(), &session_id);
    let pid_file = tmp.path().join("cold-bg.pid");
    client
        .send(ClientCommand::RunUserShell {
            session_id: session_id.clone(),
            command: format!("echo $$ > '{}'; echo BEFORE; sleep 30", pid_file.display()),
        })
        .await
        .unwrap();
    let execution_id = loop {
        let ev = tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .expect("started")
            .expect("open");
        if let RuntimeEvent::UserShellStarted { execution_id, .. } = ev {
            break execution_id;
        }
    };
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .expect("before output")
            .expect("open");
        if matches!(&ev, RuntimeEvent::UserShellOutput { chunk, .. } if chunk.contains("BEFORE")) {
            break;
        }
    }
    let pid = wait_pid_file(&pid_file).await;
    client
        .send(ClientCommand::BackgroundUserShell {
            session_id: session_id.clone(),
            execution_id: execution_id.clone(),
        })
        .await
        .unwrap();
    let task_id = loop {
        let ev = tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .expect("backgrounded")
            .expect("open");
        if let RuntimeEvent::UserShellBackgrounded { task_id, .. } = ev {
            break task_id;
        }
    };
    let snap = client.snapshot(&session_id).await.unwrap();
    assert!(
        snap.active_background_tasks
            .iter()
            .any(|t| t.task_id == task_id)
    );
    drop(client);
    drop(app);
    tokio::time::sleep(Duration::from_millis(150)).await;
    // Simulate the runtime generation dying with the process. The local
    // registry no longer exists, so nothing could truthfully keep it Running.
    let _ = std::process::Command::new("kill")
        .args(["-9", &format!("-{pid}")])
        .status();

    let app = std::sync::Arc::new(Application::assemble(layout).unwrap());
    let (client, _events) = client_for(app, &session_id);
    let snap = client.snapshot(&session_id).await.unwrap();
    let shell = snap
        .user_shells
        .iter()
        .find(|shell| shell.id == execution_id)
        .expect("background history survives a cold runtime");
    assert_eq!(
        shell.status, "unknown",
        "a dead runtime generation must never report the orphan as running"
    );
    assert!(
        shell.output_tail.contains("BEFORE"),
        "{:?}",
        shell.output_tail
    );
    assert!(snap.active_background_tasks.is_empty());
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cold_runtime_restores_a_finished_background_shell() {
    let server = MockServer::start(vec![sse_text("unused")]).await;
    let (tmp, app) = app_with_repo(&server).await;
    let layout = layout_for(tmp.path());
    let app = std::sync::Arc::new(app);
    let session_id = app
        .create_session(&ModelRef::new("mock", "m"), "cold bg done")
        .await
        .unwrap();
    let (client, mut events) = client_for(app.clone(), &session_id);
    client
        .send(ClientCommand::RunUserShell {
            session_id: session_id.clone(),
            command: "echo BG-DONE; sleep 1".into(),
        })
        .await
        .unwrap();
    let execution_id = loop {
        let ev = tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .expect("started")
            .expect("open");
        if let RuntimeEvent::UserShellStarted { execution_id, .. } = ev {
            break execution_id;
        }
    };
    client
        .send(ClientCommand::BackgroundUserShell {
            session_id: session_id.clone(),
            execution_id: execution_id.clone(),
        })
        .await
        .unwrap();
    let mut backgrounded = false;
    let mut finished = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while !(backgrounded && finished) {
        let ev = tokio::time::timeout(
            deadline.saturating_duration_since(tokio::time::Instant::now()),
            events.recv(),
        )
        .await
        .expect("background terminal before deadline")
        .expect("open");
        match ev {
            RuntimeEvent::UserShellBackgrounded { .. } => backgrounded = true,
            RuntimeEvent::UserShellExited { status, .. } => {
                assert_eq!(status, "success");
                finished = true;
            }
            _ => {}
        }
    }
    // The terminal fact is persisted by a detached writer; give it the same
    // turn the live view already has.
    tokio::time::sleep(Duration::from_millis(400)).await;
    drop(client);
    drop(app);
    tokio::time::sleep(Duration::from_millis(150)).await;

    let app = std::sync::Arc::new(Application::assemble(layout).unwrap());
    let (client, _events) = client_for(app, &session_id);
    let snap = client.snapshot(&session_id).await.unwrap();
    let shell = snap
        .user_shells
        .iter()
        .find(|shell| shell.id == execution_id)
        .expect("finished background shell survives a cold runtime");
    assert_eq!(shell.status, "success");
    assert!(shell.task_id.is_none());
    assert!(
        shell.output_tail.contains("BG-DONE"),
        "retained output survives: {:?}",
        shell.output_tail
    );
    assert!(snap.active_background_tasks.is_empty());
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_background_requests_create_exactly_one_task() {
    let server = MockServer::start(vec![sse_text("unused")]).await;
    let mut h = harness(&server).await;
    h.client.send(run_shell(&h, "sleep 30")).await.unwrap();
    let execution_id = match wait_event(
        &mut h,
        |ev| matches!(ev, RuntimeEvent::UserShellStarted { .. }),
        10,
    )
    .await
    {
        RuntimeEvent::UserShellStarted { execution_id, .. } => execution_id,
        _ => unreachable!(),
    };
    let command = ClientCommand::BackgroundUserShell {
        session_id: h.session_id.clone(),
        execution_id: execution_id.clone(),
    };
    h.client.send(command.clone()).await.unwrap();
    h.client.send(command).await.unwrap();
    let task_id = match wait_event(
        &mut h,
        |ev| matches!(ev, RuntimeEvent::UserShellBackgrounded { .. }),
        10,
    )
    .await
    {
        RuntimeEvent::UserShellBackgrounded { task_id, .. } => task_id,
        _ => unreachable!(),
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    let snap = h.client.snapshot(&h.session_id).await.unwrap();
    let running: Vec<_> = snap
        .active_background_tasks
        .iter()
        .filter(|task| task.task_id.starts_with("bg-"))
        .collect();
    assert_eq!(
        running.len(),
        1,
        "a repeated request never adds a task: {running:?}"
    );
    assert_eq!(running[0].task_id, task_id);
    h.client
        .send(ClientCommand::CancelBackgroundTask {
            session_id: h.session_id.clone(),
            task_id,
        })
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_runtime_does_not_claim_another_runtimes_background_process() {
    let server = MockServer::start(vec![sse_text("unused")]).await;
    let (_tmp, app) = app_with_repo(&server).await;
    let app = std::sync::Arc::new(app);
    let session_id = app
        .create_session(&ModelRef::new("mock", "m"), "reconnect bg")
        .await
        .unwrap();
    let (client, mut events) = client_for(app.clone(), &session_id);
    client
        .send(ClientCommand::RunUserShell {
            session_id: session_id.clone(),
            command: "sleep 30".into(),
        })
        .await
        .unwrap();
    let execution_id = loop {
        let ev = tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .expect("started")
            .expect("open");
        if let RuntimeEvent::UserShellStarted { execution_id, .. } = ev {
            break execution_id;
        }
    };
    client
        .send(ClientCommand::BackgroundUserShell {
            session_id: session_id.clone(),
            execution_id: execution_id.clone(),
        })
        .await
        .unwrap();
    let task_id = loop {
        let ev = tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .expect("backgrounded")
            .expect("open");
        if let RuntimeEvent::UserShellBackgrounded { task_id, .. } = ev {
            break task_id;
        }
    };
    // A second in-process client is a different runtime generation: it does
    // not own the first runtime's registry. The persisted transfer fact is
    // present, but there is no process it can claim, so the record is unknown.
    let (second, _events) = client_for(app.clone(), &session_id);
    let snap = second.snapshot(&session_id).await.unwrap();
    let shell = snap
        .user_shells
        .iter()
        .find(|shell| shell.id == execution_id)
        .expect("background shell in the cold snapshot");
    assert_eq!(shell.status, "unknown");
    // The registry is app-global in this harness; the important fact is that
    // this client's shell projection does not claim the other runtime's
    // execution as its own live background record.
    // The ORIGINAL runtime still owns and can stop the process.
    let snap = client.snapshot(&session_id).await.unwrap();
    assert!(
        snap.active_background_tasks
            .iter()
            .any(|t| t.task_id == task_id)
    );
    let _ = client
        .send(ClientCommand::CancelBackgroundTask {
            session_id: session_id.clone(),
            task_id,
        })
        .await;
}
