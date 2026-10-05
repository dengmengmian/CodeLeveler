//! Full permission lifecycle (Release Gate: `FULL PERMISSION CONTRACT`).
//!
//! The invariant this file protects is a RUNTIME lifecycle one, not merely a
//! policy one:
//!
//! ```text
//! effective_permission_mode == Full
//!     => CodeLeveler-owned permission approval count == 0
//! ```
//!
//! That covers three things a policy-only test misses:
//!
//! * Full never produces a new `Ask` (already covered by
//!   `full_product_contract_bypasses_every_representative_permission_gate`);
//! * switching to Full supersedes a question that was already waiting, so the
//!   blocked call is re-admitted under Full and allowed;
//! * a superseded request can no longer be answered (no stale Approve/Deny),
//!   and never appears again in a snapshot or a reconnect.
//!
//! `request_user_input` is a clarification, not a permission approval, and is
//! deliberately untouched: Full does not take away the model's ability to ask
//! the user a question.

use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    ApprovalDecision, ClientCommand, InteractiveRuntimeClient, PermissionProfile as WirePermission,
    RuntimeEvent, UiApprovalRequest, UiPendingInteraction,
};
use leveler_core::SessionId;
use leveler_execution::PermissionProfile;
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_test_support::{MockResponse, MockServer};
use tokio::sync::broadcast;

/// Point `LEVELER_HOME` at an empty dir so `GlobalConfig::load()` yields the
/// default. Tests must not depend on the developer's `~/.leveler/config.toml`.
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

/// Build an application + client in `mode` over a fresh repo, submit `content`,
/// and hand back the live session-event receiver.
async fn session(
    root: &std::path::Path,
    base_url: &str,
    mode: PermissionProfile,
    content: &str,
) -> (
    Arc<Application>,
    Arc<InProcessRuntimeClient>,
    SessionId,
    broadcast::Receiver<RuntimeEvent>,
) {
    write_config(root, base_url);
    let layout = Layout::from_parts(root.to_path_buf(), root.join("configs"), root.join("state"));
    let app = Arc::new(Application::assemble(layout).unwrap());
    let model = ModelRef::new("mock", "m");
    let session_id = app.create_session(&model, content).await.unwrap();
    let client = Arc::new(InProcessRuntimeClient::new(app.clone(), model, mode, false));
    let events = client.subscribe_session(&session_id);
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session_id.clone(),
            content: content.to_string(),
            attachments: vec![],
        })
        .await
        .unwrap();
    (app, client, session_id, events)
}

/// Wait for the next `ApprovalRequested`, skipping unrelated events.
async fn await_approval(events: &mut broadcast::Receiver<RuntimeEvent>) -> UiApprovalRequest {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match events.recv().await {
                Ok(RuntimeEvent::ApprovalRequested { request }) => return request,
                Ok(RuntimeEvent::TurnFailed { error, .. }) => {
                    panic!("turn failed before asking: {error}")
                }
                Ok(_) => continue,
                Err(error) => panic!("session event stream closed: {error}"),
            }
        }
    })
    .await
    .expect("the runtime should reach an approval within 30s")
}

/// Wait for `SessionUpdated` to report `mode`.
async fn await_mode(events: &mut broadcast::Receiver<RuntimeEvent>, mode: WirePermission) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match events.recv().await {
                Ok(RuntimeEvent::SessionUpdated { session }) if session.mode == mode => return,
                Ok(_) => continue,
                Err(error) => panic!("session event stream closed: {error}"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("runtime never reported mode {mode:?}"));
}

async fn switch(
    client: &Arc<InProcessRuntimeClient>,
    session_id: &SessionId,
    mode: WirePermission,
) {
    client
        .send(ClientCommand::SetPermissionProfile {
            session_id: session_id.clone(),
            mode,
        })
        .await
        .unwrap();
}

/// The lifecycle invariant itself, asserted against the runtime snapshot.
fn assert_no_permission_approval(snapshot: &leveler_client_protocol::UiSessionSnapshot, ctx: &str) {
    let pending: Vec<_> = snapshot
        .pending_interactions
        .iter()
        .filter(|item| matches!(item, UiPendingInteraction::Approval(_)))
        .collect();
    assert!(
        pending.is_empty(),
        "{ctx}: FULL PERMISSION CONTRACT VIOLATION — a CodeLeveler permission approval is live \
         under {:?}: {pending:?}",
        snapshot.mode
    );
}

/// Wait until `path` is gone, proving a re-admitted call really executed.
async fn await_file_gone(path: &std::path::Path) {
    tokio::time::timeout(Duration::from_secs(30), async {
        while path.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "{} still exists: the blocked call never re-admitted",
            path.display()
        )
    });
}

/// Fail if a second `ApprovalRequested` arrives while the profile is Full.
async fn assert_no_approval_for(events: &mut broadcast::Receiver<RuntimeEvent>, within: Duration) {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return;
        }
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Ok(RuntimeEvent::ApprovalRequested { request })) => {
                panic!(
                    "FULL PERMISSION CONTRACT VIOLATION: a new approval was requested: {request:?}"
                )
            }
            Ok(Ok(_)) => continue,
            Ok(Err(_)) => return,
            Err(_) => return,
        }
    }
}

async fn cancel(client: &Arc<InProcessRuntimeClient>, session_id: &SessionId) {
    let _ = client
        .send(ClientCommand::CancelTask {
            session_id: session_id.clone(),
        })
        .await;
}

// ── TEST 1: the primary regression ───────────────────────────────────────────

/// Auto pending → switch Full → superseded → re-admitted → executed under Full,
/// with no second question and no pending approval left behind.
#[tokio::test]
async fn switching_to_full_supersedes_the_pending_and_re_admits_under_full() {
    let server = MockServer::start(vec![
        tool_call_sse(
            "run_command",
            serde_json::json!({"program": "rm", "args": ["scratch.txt"], "reason": "cleanup"}),
        ),
        text("done"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let victim = tmp.path().join("scratch.txt");
    std::fs::write(&victim, "x").unwrap();

    let (_app, client, session_id, mut events) = session(
        tmp.path(),
        &server.base_url(),
        PermissionProfile::Assisted,
        "clean up",
    )
    .await;

    let approval = await_approval(&mut events).await;
    let stale_id = approval.id.clone();

    // The question is really live before the switch.
    let before = client.snapshot(&session_id).await.unwrap();
    assert!(
        before
            .pending_interactions
            .iter()
            .any(|item| matches!(item, UiPendingInteraction::Approval(_))),
        "the Auto question must be pending before the switch"
    );

    switch(&client, &session_id, WirePermission::FullAccess).await;
    await_mode(&mut events, WirePermission::FullAccess).await;

    // 1. runtime snapshot carries no permission approval any more.
    let after = client.snapshot(&session_id).await.unwrap();
    assert_eq!(after.mode, WirePermission::FullAccess);
    assert_no_permission_approval(&after, "after switching to Full");

    // 2. the blocked call re-admitted under Full and really executed.
    await_file_gone(&victim).await;

    // 3. the superseded request cannot be answered afterwards — a late Deny
    //    must not turn into a Full-mode refusal of anything.
    let stale = client
        .send(ClientCommand::ApprovalDecision {
            request_id: stale_id,
            decision: ApprovalDecision::Deny,
        })
        .await;
    assert!(
        stale.is_err(),
        "a superseded approval must not be answerable: {stale:?}"
    );

    // 4. Full never asks again for this call.
    assert_no_approval_for(&mut events, Duration::from_secs(2)).await;
    cancel(&client, &session_id).await;
}

// ── TEST 2: Full never produces a Deny from a stale request ──────────────────

#[tokio::test]
async fn a_stale_deny_after_the_switch_cannot_reach_the_runtime() {
    let server = MockServer::start(vec![
        tool_call_sse(
            "run_command",
            serde_json::json!({"program": "rm", "args": ["scratch.txt"]}),
        ),
        text("done"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let victim = tmp.path().join("scratch.txt");
    std::fs::write(&victim, "x").unwrap();

    let (_app, client, session_id, mut events) = session(
        tmp.path(),
        &server.base_url(),
        PermissionProfile::Assisted,
        "clean up",
    )
    .await;
    let approval = await_approval(&mut events).await;

    switch(&client, &session_id, WirePermission::FullAccess).await;
    await_mode(&mut events, WirePermission::FullAccess).await;

    // Both late answers are rejected as stale and change nothing.
    for decision in [ApprovalDecision::Deny, ApprovalDecision::ApproveOnce] {
        let outcome = client
            .send(ClientCommand::ApprovalDecision {
                request_id: approval.id.clone(),
                decision,
            })
            .await;
        assert!(outcome.is_err(), "stale {decision:?} must be refused");
    }

    // The operation still runs under Full: no Deny leaked through.
    await_file_gone(&victim).await;
    assert_no_permission_approval(
        &client.snapshot(&session_id).await.unwrap(),
        "after stale answers",
    );
    cancel(&client, &session_id).await;
}

// ── TEST 3: snapshot / reconnect never restores the superseded question ──────

#[tokio::test]
async fn a_reconnect_after_the_switch_restores_no_approval() {
    let server = MockServer::start(vec![
        tool_call_sse(
            "run_command",
            serde_json::json!({"program": "rm", "args": ["scratch.txt"]}),
        ),
        text("done"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let victim = tmp.path().join("scratch.txt");
    std::fs::write(&victim, "x").unwrap();

    let (_app, client, session_id, mut events) = session(
        tmp.path(),
        &server.base_url(),
        PermissionProfile::Assisted,
        "clean up",
    )
    .await;
    await_approval(&mut events).await;
    switch(&client, &session_id, WirePermission::FullAccess).await;
    await_mode(&mut events, WirePermission::FullAccess).await;
    await_file_gone(&victim).await;

    // A fresh subscriber + snapshot is what a reconnecting TUI renders.
    let mut reconnected = client.subscribe_session(&session_id);
    let restored = client.snapshot(&session_id).await.unwrap();
    assert_no_permission_approval(&restored, "after reconnect");
    assert_no_approval_for(&mut reconnected, Duration::from_millis(500)).await;
    cancel(&client, &session_id).await;
}

// ── TEST 4: flapping profile stays deterministic and leaks nothing ───────────

#[tokio::test]
async fn flapping_between_auto_and_full_leaves_no_orphan_waiter() {
    let server = MockServer::start(vec![
        tool_call_sse(
            "run_command",
            serde_json::json!({"program": "rm", "args": ["scratch.txt"]}),
        ),
        text("done"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let victim = tmp.path().join("scratch.txt");
    std::fs::write(&victim, "x").unwrap();

    let (_app, client, session_id, mut events) = session(
        tmp.path(),
        &server.base_url(),
        PermissionProfile::Assisted,
        "clean up",
    )
    .await;
    await_approval(&mut events).await;

    for mode in [
        WirePermission::FullAccess,
        WirePermission::Assisted,
        WirePermission::FullAccess,
    ] {
        switch(&client, &session_id, mode).await;
        await_mode(&mut events, mode).await;
        let snapshot = client.snapshot(&session_id).await.unwrap();
        assert_eq!(snapshot.mode, mode);
        if mode == WirePermission::FullAccess {
            assert_no_permission_approval(&snapshot, "during a flap");
        }
    }

    await_file_gone(&victim).await;
    assert_no_permission_approval(
        &client.snapshot(&session_id).await.unwrap(),
        "after flapping",
    );
    cancel(&client, &session_id).await;
}

// ── TEST 5: clarification is NOT a permission approval ───────────────────────

#[tokio::test]
async fn switching_to_full_leaves_a_clarification_intact() {
    let server = MockServer::start(vec![
        tool_call_sse(
            "capability",
            serde_json::json!({"action": "enable", "id": "host_interaction"}),
        ),
        tool_call_sse(
            "request_user_input",
            serde_json::json!({"question": "要简洁还是详细？", "options": ["简洁", "详细"]}),
        ),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();

    let (_app, client, session_id, mut events) = session(
        tmp.path(),
        &server.base_url(),
        PermissionProfile::Assisted,
        "帮我改文字",
    )
    .await;

    let question = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match events.recv().await {
                Ok(RuntimeEvent::ClarificationRequested { request }) => return request,
                Ok(RuntimeEvent::TurnFailed { error, .. }) => panic!("turn failed: {error}"),
                Ok(_) => continue,
                Err(error) => panic!("session event stream closed: {error}"),
            }
        }
    })
    .await
    .expect("the runtime should ask a clarifying question");

    switch(&client, &session_id, WirePermission::FullAccess).await;
    await_mode(&mut events, WirePermission::FullAccess).await;

    // The clarification survives: Full changes permission approvals only.
    let snapshot = client.snapshot(&session_id).await.unwrap();
    assert!(
        snapshot.pending_interactions.iter().any(|item| matches!(
            item,
            UiPendingInteraction::Clarification(request) if request.id == question.id
        )),
        "Full must not drop a clarification: {:?}",
        snapshot.pending_interactions
    );
    client
        .send(ClientCommand::AnswerClarification {
            request_id: question.id,
            answer: "简洁".to_string(),
        })
        .await
        .unwrap();
    cancel(&client, &session_id).await;
}

// ── TEST 6: Auto contract is not weakened by the Full fix ────────────────────

#[tokio::test]
async fn auto_still_asks_for_dangerous_git_push() {
    let server = MockServer::start(vec![tool_call_sse(
        "run_command",
        serde_json::json!({"program": "git", "args": ["push", "origin", "HEAD"]}),
    )])
    .await;
    let tmp = tempfile::tempdir().unwrap();

    let (_app, client, session_id, mut events) = session(
        tmp.path(),
        &server.base_url(),
        PermissionProfile::Assisted,
        "push",
    )
    .await;

    let approval = await_approval(&mut events).await;
    assert!(
        approval
            .command
            .as_deref()
            .unwrap_or_default()
            .contains("push"),
        "Auto must still ask before git push: {approval:?}"
    );
    // Deny it: Auto's refusal path still works.
    client
        .send(ClientCommand::ApprovalDecision {
            request_id: approval.id,
            decision: ApprovalDecision::Deny,
        })
        .await
        .unwrap();
    cancel(&client, &session_id).await;
}
