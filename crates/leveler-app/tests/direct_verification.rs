use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use leveler_agent::AutoClarify;
use leveler_app::Application;
use leveler_execution::{AutoApprove, PermissionProfile};
use leveler_model::{ContentPart, ModelRef};
use leveler_project::Layout;
use leveler_test_support::{MockResponse, MockServer};

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

fn tool_call_frame(name: &str, arguments: serde_json::Value) -> String {
    serde_json::json!({
        "choices": [{
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": "call_patch",
                    "function": {
                        "name": name,
                        "arguments": arguments.to_string()
                    }
                }]
            }
        }]
    })
    .to_string()
}

fn finish_frame(reason: &str) -> String {
    serde_json::json!({
        "choices": [{
            "delta": {},
            "finish_reason": reason
        }]
    })
    .to_string()
}

fn text_frame(text: &str) -> String {
    serde_json::json!({
        "choices": [{
            "delta": { "content": text },
            "finish_reason": "stop"
        }]
    })
    .to_string()
}

/// Point `LEVELER_HOME` at an empty dir so `GlobalConfig::load()` yields the
/// default. Tests must not depend on the developer's `~/.leveler/config.toml`
/// (which may legitimately hold real provider entries) for their outcome.
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
        format!(
            r#"
id: mock
protocol: openai_chat
base_url: {base_url}
"#
        ),
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

/// Verify gates must spawn a real program. Unix fixtures shell out to `sh`;
/// Windows runners have no `sh`/`true`/`grep` on PATH, so the same checks go
/// through `cmd /c` there.
fn gate_config(unix_body: &str, windows_body: &str) -> String {
    let (program, flag, body) = if cfg!(windows) {
        ("cmd", "/c", windows_body)
    } else {
        ("sh", "-c", unix_body)
    };
    format!("verify:\n  test: {{ program: \"{program}\", args: [\"{flag}\", \"{body}\"] }}\n")
}

#[tokio::test]
async fn direct_run_reports_checks_failed_when_post_edit_verification_fails() {
    let patch = "*** Begin Patch\n*** Update File: src/lib.rs\n old\n+new\n*** End Patch";
    let server = MockServer::start(vec![
        sse(vec![
            tool_call_frame("apply_patch", serde_json::json!({ "patch": patch })),
            finish_frame("tool_calls"),
        ]),
        // The goal-mode run resolves explicitly; verification then fails.
        sse(vec![
            tool_call_frame(
                "update_goal",
                serde_json::json!({ "status": "complete", "summary": "done" }),
            ),
            finish_frame("tool_calls"),
        ]),
    ])
    .await;

    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(tmp.path().join("src/lib.rs"), "old\n").unwrap();
    std::fs::create_dir_all(tmp.path().join(".leveler")).unwrap();
    std::fs::write(
        tmp.path().join(".leveler/config.yaml"),
        gate_config(
            "echo VERIFY_SENTINEL; exit 1",
            "echo VERIFY_SENTINEL & exit 1",
        ),
    )
    .unwrap();
    write_config(tmp.path(), &server.base_url());

    let layout = Layout::from_parts(
        tmp.path().to_path_buf(),
        tmp.path().join("configs"),
        tmp.path().join("state"),
    );
    let app = Application::assemble(layout).unwrap();
    let session_id = app
        .create_session(&ModelRef::new("mock", "m"), "edit readme")
        .await
        .unwrap();

    let mut events = Vec::new();
    let result = app
        .run_in_session(
            &session_id,
            &ModelRef::new("mock", "m"),
            PermissionProfile::Assisted,
            "edit readme",
            Arc::new(AutoApprove),
            false,
            &mut |event| events.push(event),
            CancellationToken::new(),
        )
        .await;

    // Case 3 at the app seam: the run completed and the project's checks
    // failed. Both facts are reported; the run is not relabelled incomplete.
    let outcome = result.expect("verification failure is a completed run with a failed check");
    assert_eq!(
        outcome.stop_reason,
        leveler_agent::StopReason::CompletedChecksFailed
    );
    let detail = outcome.stop_detail.unwrap_or_default();
    assert!(
        detail.contains("test") && !detail.contains("VERIFY_SENTINEL"),
        "the turn marker must stay concise: {detail}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            leveler_agent::AgentEvent::VerificationCheck { evidence: Some(evidence), .. }
                if evidence.contains("VERIFY_SENTINEL")
        )),
        "full evidence belongs in the structured verification event: {events:?}"
    );
}

#[tokio::test]
async fn direct_content_run_reports_checks_failed_when_post_edit_verification_fails() {
    let patch = "*** Begin Patch\n*** Update File: src/lib.rs\n old\n+new\n*** End Patch";
    let server = MockServer::start(vec![
        sse(vec![
            tool_call_frame("apply_patch", serde_json::json!({ "patch": patch })),
            finish_frame("tool_calls"),
        ]),
        sse(vec![text_frame("done")]),
    ])
    .await;

    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(tmp.path().join("src/lib.rs"), "old\n").unwrap();
    std::fs::create_dir_all(tmp.path().join(".leveler")).unwrap();
    std::fs::write(
        tmp.path().join(".leveler/config.yaml"),
        gate_config(
            "echo CONTENT_VERIFY_SENTINEL; exit 1",
            "echo CONTENT_VERIFY_SENTINEL & exit 1",
        ),
    )
    .unwrap();
    write_config(tmp.path(), &server.base_url());

    let layout = Layout::from_parts(
        tmp.path().to_path_buf(),
        tmp.path().join("configs"),
        tmp.path().join("state"),
    );
    let app = Application::assemble(layout).unwrap();
    let session_id = app
        .create_session(&ModelRef::new("mock", "m"), "edit readme")
        .await
        .unwrap();

    let mut events = Vec::new();
    let result = app
        .run_in_session_with_content(
            &session_id,
            &ModelRef::new("mock", "m"),
            PermissionProfile::Assisted,
            vec![ContentPart::Text {
                text: "edit readme".to_string(),
            }],
            Arc::new(AutoApprove),
            Arc::new(AutoClarify),
            false,
            &mut |event| events.push(event),
            CancellationToken::new(),
        )
        .await;

    // Case 3 at the app seam: the run completed and the project's checks
    // failed. Both facts are reported; the run is not relabelled incomplete.
    let outcome = result.expect("verification failure is a completed run with a failed check");
    assert_eq!(
        outcome.stop_reason,
        leveler_agent::StopReason::CompletedChecksFailed
    );
    let detail = outcome.stop_detail.unwrap_or_default();
    assert!(
        detail.contains("test") && !detail.contains("CONTENT_VERIFY_SENTINEL"),
        "the turn marker must stay concise: {detail}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            leveler_engine::EngineEvent::VerificationCheck { evidence: Some(evidence), .. }
                if evidence.contains("CONTENT_VERIFY_SENTINEL")
        )),
        "full evidence belongs in the structured verification event: {events:?}"
    );
}

#[tokio::test]
async fn direct_run_succeeds_when_post_edit_verification_passes() {
    let patch = "*** Begin Patch\n*** Update File: README.md\n old\n+new\n*** End Patch";
    let server = MockServer::start(vec![
        sse(vec![
            tool_call_frame("apply_patch", serde_json::json!({ "patch": patch })),
            finish_frame("tool_calls"),
        ]),
        sse(vec![
            tool_call_frame(
                "update_goal",
                serde_json::json!({"status": "complete", "summary": "done"}),
            ),
            finish_frame("tool_calls"),
        ]),
    ])
    .await;

    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("README.md"), "old\n").unwrap();
    std::fs::create_dir_all(tmp.path().join(".leveler")).unwrap();
    std::fs::write(
        tmp.path().join(".leveler/config.yaml"),
        gate_config("exit 0", "exit 0"),
    )
    .unwrap();
    write_config(tmp.path(), &server.base_url());

    let layout = Layout::from_parts(
        tmp.path().to_path_buf(),
        tmp.path().join("configs"),
        tmp.path().join("state"),
    );
    let app = Application::assemble(layout).unwrap();
    let session_id = app
        .create_session(&ModelRef::new("mock", "m"), "edit readme")
        .await
        .unwrap();

    let outcome = app
        .run_in_session(
            &session_id,
            &ModelRef::new("mock", "m"),
            PermissionProfile::Assisted,
            "edit readme",
            Arc::new(AutoApprove),
            false,
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
        .expect("passing verification should allow completion");

    assert_eq!(outcome.modified_files, vec!["README.md"]);
}

#[tokio::test]
async fn direct_run_without_gating_verification_is_completed_unverified() {
    let patch = "*** Begin Patch\n*** Update File: README.md\n old\n+new\n*** End Patch";
    let server = MockServer::start(vec![
        sse(vec![
            tool_call_frame("apply_patch", serde_json::json!({ "patch": patch })),
            finish_frame("tool_calls"),
        ]),
        sse(vec![
            tool_call_frame(
                "update_goal",
                serde_json::json!({"status":"complete","summary":"done"}),
            ),
            finish_frame("tool_calls"),
        ]),
    ])
    .await;

    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("README.md"), "old\n").unwrap();
    write_config(tmp.path(), &server.base_url());

    let layout = Layout::from_parts(
        tmp.path().to_path_buf(),
        tmp.path().join("configs"),
        tmp.path().join("state"),
    );
    let app = Application::assemble(layout).unwrap();
    let session_id = app
        .create_session(&ModelRef::new("mock", "m"), "edit readme")
        .await
        .unwrap();

    let outcome = app
        .run_in_session(
            &session_id,
            &ModelRef::new("mock", "m"),
            PermissionProfile::Assisted,
            "edit readme",
            Arc::new(AutoApprove),
            false,
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
        .expect("an unverified run still returns its work for inspection");

    assert_eq!(outcome.modified_files, vec!["README.md"]);
    // No gating check ran, so the work is done but leveler cannot claim it
    // verified — a distinct state from a genuinely-incomplete run.
    assert_eq!(
        outcome.stop_reason,
        leveler_agent::StopReason::CompletedUnverified
    );
}

/// §13 B: the durable check status is the canonical spelling, not a lowercased
/// `Debug`. `CheckStatus::ToolMissing` used to reach the log as `toolmissing`,
/// which no reader's vocabulary contained — the app projection and the eval
/// reader each filed it under their fallback, so a tool that was not installed
/// was reported as a check that was deliberately skipped.
#[tokio::test]
async fn a_verification_tool_that_is_missing_is_written_as_tool_missing() {
    let patch = "*** Begin Patch\n*** Update File: src/lib.rs\n old\n+new\n*** End Patch";
    let server = MockServer::start(vec![
        sse(vec![
            tool_call_frame("apply_patch", serde_json::json!({ "patch": patch })),
            finish_frame("tool_calls"),
        ]),
        // Chat path: the model ends with prose, and the gate is what decides
        // the run's verdict.
        sse(vec![text_frame("done")]),
    ])
    .await;

    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(tmp.path().join("src/lib.rs"), "old\n").unwrap();
    std::fs::create_dir_all(tmp.path().join(".leveler")).unwrap();
    std::fs::write(
        tmp.path().join(".leveler/config.yaml"),
        "verify:\n  test: { program: \"leveler-definitely-not-a-real-program-xyz\", args: [] }\n",
    )
    .unwrap();
    write_config(tmp.path(), &server.base_url());

    let layout = Layout::from_parts(
        tmp.path().to_path_buf(),
        tmp.path().join("configs"),
        tmp.path().join("state"),
    );
    let app = Application::assemble(layout).unwrap();
    let session_id = app
        .create_session(&ModelRef::new("mock", "m"), "edit lib")
        .await
        .unwrap();

    let mut statuses: Vec<String> = Vec::new();
    app.run_in_session_with_content(
        &session_id,
        &ModelRef::new("mock", "m"),
        PermissionProfile::Assisted,
        vec![ContentPart::Text {
            text: "edit lib".to_string(),
        }],
        Arc::new(AutoApprove),
        Arc::new(AutoClarify),
        false,
        &mut |event| {
            if let leveler_engine::EngineEvent::VerificationCheck { status, .. } = event {
                statuses.push(status);
            }
        },
        CancellationToken::new(),
    )
    .await
    .expect("a missing tool does not fail the run");

    assert_eq!(
        statuses,
        vec!["tool_missing".to_string()],
        "the durable vocabulary is canonical, never a lowercased Debug"
    );
}

#[tokio::test]
async fn direct_content_run_emits_verification_events() {
    let patch = "*** Begin Patch\n*** Update File: src/lib.rs\n old\n+new\n*** End Patch";
    let server = MockServer::start(vec![
        sse(vec![
            tool_call_frame("apply_patch", serde_json::json!({ "patch": patch })),
            finish_frame("tool_calls"),
        ]),
        sse(vec![text_frame("done")]),
    ])
    .await;

    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(tmp.path().join("src/lib.rs"), "old\n").unwrap();
    std::fs::create_dir_all(tmp.path().join(".leveler")).unwrap();
    std::fs::write(
        tmp.path().join(".leveler/config.yaml"),
        gate_config("exit 0", "exit 0"),
    )
    .unwrap();
    write_config(tmp.path(), &server.base_url());

    let layout = Layout::from_parts(
        tmp.path().to_path_buf(),
        tmp.path().join("configs"),
        tmp.path().join("state"),
    );
    let app = Application::assemble(layout).unwrap();
    let session_id = app
        .create_session(&ModelRef::new("mock", "m"), "edit readme")
        .await
        .unwrap();

    let mut events = Vec::new();
    let outcome = app
        .run_in_session_with_content(
            &session_id,
            &ModelRef::new("mock", "m"),
            PermissionProfile::Assisted,
            vec![ContentPart::Text {
                text: "edit readme".to_string(),
            }],
            Arc::new(AutoApprove),
            Arc::new(AutoClarify),
            false,
            &mut |event| events.push(event),
            CancellationToken::new(),
        )
        .await
        .expect("passing verification should allow completion");

    // Chat path: the model edited and ended with prose (no update_goal), but
    // leveler's gate passed on real work — the outcome must read as completed,
    // not a bare "answered" that hides the verification.
    assert_eq!(outcome.stop_reason, leveler_agent::StopReason::Completed);

    assert!(
        events
            .iter()
            .any(|event| matches!(event, leveler_engine::EngineEvent::VerificationStarted))
    );
    assert!(events.iter().any(|event| {
        matches!(
            event,
            leveler_engine::EngineEvent::VerificationCheck {
                name,
                status,
                ..
            } if name == "test" && status == "passed"
        )
    }));
    assert!(events.iter().any(|event| {
        matches!(
            event,
            leveler_engine::EngineEvent::VerificationFinished {
                passed: true,
                verification: Some(leveler_lifecycle::VerificationStatus::Passed),
            }
        )
    }));
}

/// Case 3: a failed post-edit check is reported, never repaired on the
/// model's behalf. The script offers a repair patch that the engine must
/// never request: the run ends after the model's own completion claim.
#[tokio::test]
async fn direct_run_does_not_open_a_repair_turn_after_failed_verification() {
    let first_patch = "*** Begin Patch\n*** Update File: code.rs\n old\n+bad\n*** End Patch";
    let repair_patch =
        "*** Begin Patch\n*** Update File: code.rs\n old\n-bad\n+fixed\n*** End Patch";
    let server = MockServer::start(vec![
        sse(vec![
            tool_call_frame("apply_patch", serde_json::json!({ "patch": first_patch })),
            finish_frame("tool_calls"),
        ]),
        sse(vec![
            tool_call_frame(
                "update_goal",
                serde_json::json!({"status": "complete", "summary": "done"}),
            ),
            finish_frame("tool_calls"),
        ]),
        // Would only be consumed by an automatic repair turn.
        sse(vec![
            tool_call_frame("apply_patch", serde_json::json!({ "patch": repair_patch })),
            finish_frame("tool_calls"),
        ]),
        sse(vec![text_frame("repaired")]),
    ])
    .await;

    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("code.rs"), "old\n").unwrap();
    std::fs::create_dir_all(tmp.path().join(".leveler")).unwrap();
    std::fs::write(
        tmp.path().join(".leveler/config.yaml"),
        gate_config("grep fixed code.rs", "findstr fixed code.rs"),
    )
    .unwrap();
    write_config(tmp.path(), &server.base_url());

    let layout = Layout::from_parts(
        tmp.path().to_path_buf(),
        tmp.path().join("configs"),
        tmp.path().join("state"),
    );
    let app = Application::assemble(layout).unwrap();
    let session_id = app
        .create_session(&ModelRef::new("mock", "m"), "edit readme")
        .await
        .unwrap();

    let outcome = app
        .run_in_session(
            &session_id,
            &ModelRef::new("mock", "m"),
            PermissionProfile::Assisted,
            "edit readme",
            Arc::new(AutoApprove),
            false,
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
        .expect("a failed check is a reported fact, not an error");

    assert_eq!(
        outcome.stop_reason,
        leveler_agent::StopReason::CompletedChecksFailed
    );
    assert_eq!(outcome.modified_files, vec!["code.rs"]);
    assert!(
        std::fs::read_to_string(tmp.path().join("code.rs"))
            .unwrap()
            .contains("bad"),
        "no repair turn may edit on the model's behalf"
    );
    assert_eq!(
        server.request_count(),
        2,
        "exactly the model's own two rounds — no hidden repair request"
    );
}
