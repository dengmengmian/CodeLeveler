use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use leveler_agent::{AgentEvent, AgentVerificationStatus, AutoClarify};
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
  middleware: []
  synthesize_tool_call_ids: true
  drop_unsupported_fields: true
"#,
    )
    .unwrap();
}

#[tokio::test]
async fn direct_run_fails_when_post_edit_verification_fails() {
    let patch = "*** Begin Patch\n*** Update File: README.md\n old\n+new\n*** End Patch";
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
    std::fs::write(tmp.path().join("README.md"), "old\n").unwrap();
    std::fs::create_dir_all(tmp.path().join(".leveler")).unwrap();
    std::fs::write(
        tmp.path().join(".leveler/config.yaml"),
        r#"
verify:
  test: { program: "sh", args: ["-c", "echo VERIFY_SENTINEL; exit 1"] }
"#,
    )
    .unwrap();
    write_config(tmp.path(), &server.base_url());

    let layout = Layout {
        repo_root: tmp.path().to_path_buf(),
        config_dir: tmp.path().join("configs"),
        state_dir: tmp.path().join("state"),
    };
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

    let outcome = result.expect("verification failure is a completed run with an unmet gate");
    assert_eq!(outcome.stop_reason, leveler_agent::StopReason::Incomplete);
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
async fn direct_content_run_fails_when_post_edit_verification_fails() {
    let patch = "*** Begin Patch\n*** Update File: README.md\n old\n+new\n*** End Patch";
    let server = MockServer::start(vec![
        sse(vec![
            tool_call_frame("apply_patch", serde_json::json!({ "patch": patch })),
            finish_frame("tool_calls"),
        ]),
        sse(vec![text_frame("done")]),
    ])
    .await;

    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("README.md"), "old\n").unwrap();
    std::fs::create_dir_all(tmp.path().join(".leveler")).unwrap();
    std::fs::write(
        tmp.path().join(".leveler/config.yaml"),
        r#"
verify:
  test: { program: "sh", args: ["-c", "echo CONTENT_VERIFY_SENTINEL; exit 1"] }
"#,
    )
    .unwrap();
    write_config(tmp.path(), &server.base_url());

    let layout = Layout {
        repo_root: tmp.path().to_path_buf(),
        config_dir: tmp.path().join("configs"),
        state_dir: tmp.path().join("state"),
    };
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

    let outcome = result.expect("verification failure is a completed run with an unmet gate");
    assert_eq!(outcome.stop_reason, leveler_agent::StopReason::Incomplete);
    let detail = outcome.stop_detail.unwrap_or_default();
    assert!(
        detail.contains("test") && !detail.contains("CONTENT_VERIFY_SENTINEL"),
        "the turn marker must stay concise: {detail}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            leveler_agent::AgentEvent::VerificationCheck { evidence: Some(evidence), .. }
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
        r#"
verify:
  test: { program: "true", args: [] }
"#,
    )
    .unwrap();
    write_config(tmp.path(), &server.base_url());

    let layout = Layout {
        repo_root: tmp.path().to_path_buf(),
        config_dir: tmp.path().join("configs"),
        state_dir: tmp.path().join("state"),
    };
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

    let layout = Layout {
        repo_root: tmp.path().to_path_buf(),
        config_dir: tmp.path().join("configs"),
        state_dir: tmp.path().join("state"),
    };
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

#[tokio::test]
async fn direct_content_run_emits_verification_events() {
    let patch = "*** Begin Patch\n*** Update File: README.md\n old\n+new\n*** End Patch";
    let server = MockServer::start(vec![
        sse(vec![
            tool_call_frame("apply_patch", serde_json::json!({ "patch": patch })),
            finish_frame("tool_calls"),
        ]),
        sse(vec![text_frame("done")]),
    ])
    .await;

    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("README.md"), "old\n").unwrap();
    std::fs::create_dir_all(tmp.path().join(".leveler")).unwrap();
    std::fs::write(
        tmp.path().join(".leveler/config.yaml"),
        r#"
verify:
  test: { program: "true", args: [] }
"#,
    )
    .unwrap();
    write_config(tmp.path(), &server.base_url());

    let layout = Layout {
        repo_root: tmp.path().to_path_buf(),
        config_dir: tmp.path().join("configs"),
        state_dir: tmp.path().join("state"),
    };
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
            .any(|event| matches!(event, AgentEvent::VerificationStarted))
    );
    assert!(events.iter().any(|event| {
        matches!(
            event,
            AgentEvent::VerificationCheck {
                name,
                status: AgentVerificationStatus::Passed,
                ..
            } if name == "test"
        )
    }));
    assert!(
        events
            .iter()
            .any(|event| { matches!(event, AgentEvent::VerificationFinished { passed: true }) })
    );
}

#[tokio::test]
async fn direct_run_repairs_once_after_failed_verification() {
    let first_patch = "*** Begin Patch\n*** Update File: README.md\n old\n+bad\n*** End Patch";
    let repair_patch =
        "*** Begin Patch\n*** Update File: README.md\n old\n-bad\n+fixed\n*** End Patch";
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
        sse(vec![
            tool_call_frame("apply_patch", serde_json::json!({ "patch": repair_patch })),
            finish_frame("tool_calls"),
        ]),
        sse(vec![text_frame("repaired")]),
    ])
    .await;

    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("README.md"), "old\n").unwrap();
    std::fs::create_dir_all(tmp.path().join(".leveler")).unwrap();
    std::fs::write(
        tmp.path().join(".leveler/config.yaml"),
        r#"
verify:
  test: { program: "sh", args: ["-c", "grep fixed README.md"] }
"#,
    )
    .unwrap();
    write_config(tmp.path(), &server.base_url());

    let layout = Layout {
        repo_root: tmp.path().to_path_buf(),
        config_dir: tmp.path().join("configs"),
        state_dir: tmp.path().join("state"),
    };
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
        .expect("repair should make verification pass");

    assert_eq!(outcome.modified_files, vec!["README.md"]);
    assert!(
        std::fs::read_to_string(tmp.path().join("README.md"))
            .unwrap()
            .contains("fixed")
    );
}
