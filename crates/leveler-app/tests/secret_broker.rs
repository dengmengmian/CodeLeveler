//! End-to-end custody proof for the Secret Broker.
//!
//! These tests drive the real in-process runtime, the real provider transport
//! and the real SQLite store against a loopback provider that records the
//! bytes it receives. That is the only way to prove the promise the product
//! makes: a captured value reaches neither the model request nor the ordinary
//! durable stores.
//!
//! A synthetic canary is used throughout. It is never a real credential.

use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::secret::{SecretCapture, SecretValue};
use leveler_client_protocol::{ClientCommand, InteractiveRuntimeClient, RuntimeEvent};
use leveler_core::{CommandId, SessionId};
use leveler_execution::PermissionProfile;
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_test_support::{MockResponse, MockServer};

const CANARY: &str = "sk-CANARY-SECRET-DO-NOT-EXPOSE-001";

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
            "id: mock\nprotocol: openai_chat\nbase_url: {base_url}\n\
             timeouts:\n  connect_seconds: 5\n  request_seconds: 30\n  \
             idle_stream_seconds: 30\nretry:\n  max_attempts: 1\n  \
             initial_backoff_ms: 10\n  max_backoff_ms: 10\n"
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

fn text_response(text: &str) -> MockResponse {
    MockResponse::sse(&[
        &format!(r#"{{"choices":[{{"delta":{{"content":{text:?}}}}}]}}"#),
        r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":1}}"#,
    ])
}

struct Fixture {
    _tmp: tempfile::TempDir,
    app: Arc<Application>,
    client: Arc<InProcessRuntimeClient>,
    session: SessionId,
    server: MockServer,
}

async fn fixture(responses: Vec<MockResponse>) -> Fixture {
    let server = MockServer::start(responses).await;
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), &server.base_url());
    let layout = Layout::from_parts(
        tmp.path().to_path_buf(),
        tmp.path().join("configs"),
        tmp.path().join("state"),
    );
    let app = Arc::new(Application::assemble(layout).unwrap());
    let model = ModelRef::new("mock", "m");
    let session = app
        .create_session_with_collaboration(
            &model,
            "secret broker",
            PermissionProfile::Assisted,
            leveler_lifecycle::CollaborationMode::Chat,
        )
        .await
        .unwrap();
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        model,
        PermissionProfile::Assisted,
        false,
    ));
    Fixture {
        _tmp: tmp,
        app,
        client,
        session,
        server,
    }
}

impl Fixture {
    async fn wait_for_turn_end(
        &self,
        rx: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>,
    ) -> Vec<RuntimeEvent> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        let mut seen: Vec<String> = Vec::new();
        let mut events: Vec<RuntimeEvent> = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(
                    RuntimeEvent::TurnCompleted
                    | RuntimeEvent::TurnAnswered
                    | RuntimeEvent::TurnCompletedPartiallyUnverified { .. }
                    | RuntimeEvent::TurnCompletedWithWarnings { .. },
                )) => return events,
                Ok(Ok(
                    RuntimeEvent::TurnIncomplete { reason, .. }
                    | RuntimeEvent::TurnBlocked { reason, .. },
                )) => panic!("turn ended without completing: {reason}"),
                Ok(Ok(RuntimeEvent::TurnTruncated { error, .. })) => {
                    panic!("turn truncated: {error}")
                }
                Ok(Ok(RuntimeEvent::TurnPaused { .. })) => {
                    panic!("turn paused instead of completing")
                }
                Ok(Ok(RuntimeEvent::TurnFailed { error, .. })) => {
                    panic!("turn failed: {error}")
                }
                Ok(Ok(other)) => {
                    seen.push(format!("{other:?}").chars().take(160).collect());
                    events.push(other);
                }
                Ok(Err(error)) => panic!("event stream closed: {error}; seen={seen:#?}"),
                Err(_) => panic!("turn did not settle before the deadline; seen={seen:#?}"),
            }
        }
    }
}

/// Every file under the app's temp root, including the SQLite database and its
/// WAL. Reading raw bytes is deliberate: the claim is about the stored bytes,
/// not about what a repository API happens to project.
fn all_files(root: &std::path::Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(bytes) = std::fs::read(&path) {
                out.push((path, bytes));
            }
        }
    }
    out
}

fn assert_canary_absent_from_disk(root: &std::path::Path) {
    assert_canary_absent_except(root, &[]);
}

/// The authorized `.env` is the documented exception: the host really writes
/// the value there. Every OTHER file must still be free of it.
fn assert_canary_absent_except(root: &std::path::Path, allowed: &[&str]) {
    for (path, bytes) in all_files(root) {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        if allowed.iter().any(|allowed| *allowed == name) {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains(CANARY),
            "the canary reached {}",
            path.display()
        );
    }
}

/// The reference the runtime announced for the captured value.
fn announced_reference(seen: &[RuntimeEvent]) -> String {
    for event in seen {
        if let RuntimeEvent::UserMessageAdded { message } = event
            && let Some(start) = message.text.find("[secret:")
        {
            let rest = &message.text[start + "[secret:".len()..];
            if let Some(end) = rest.find(']') {
                return rest[..end].to_string();
            }
        }
    }
    panic!("the runtime never announced the reference: {seen:#?}");
}

#[test]
fn a_captured_credential_reaches_neither_the_provider_nor_the_database() {
    leveler_test_support::bounded_test(
        "a_captured_credential_reaches_neither_the_provider_nor_the_database",
        Duration::from_secs(60),
        || async {
            let f = fixture(vec![text_response("configured")]).await;
            let mut rx = f.client.subscribe();
            let capture = SecretCapture::new("{{capture-0}}", CANARY)
                .with_key_hint(Some("DEEPSEEK_API_KEY".into()));
            f.client
                .send(ClientCommand::SubmitUserInput {
                    session_id: f.session.clone(),
                    content: "帮我把 {{capture-0}} 配置到 .env".to_string(),
                    attachments: Vec::new(),
                    secret_captures: vec![capture],
                    plaintext_secret_ack: false,
                })
                .await
                .expect("a captured message is admitted");
            let events = f.wait_for_turn_end(&mut rx).await;
            let reference = announced_reference(&events);

            let bodies = f.server.request_bodies().await;
            assert!(!bodies.is_empty(), "the model was called");
            for body in &bodies {
                assert!(
                    !body.contains(CANARY),
                    "the captured value reached the provider request: {body}"
                );
                assert!(
                    body.contains("[secret:sec_"),
                    "the model did not receive the reference token: {body}"
                );
            }
            assert_canary_absent_from_disk(f._tmp.path());
            assert_eq!(
                f.app.secret_vault().available_count(f.session.as_str()),
                1,
                "the host kept exactly one capture"
            );

            // The captured value is usable, and only through the broker: the
            // tool writes the real value into the authorized .env while its
            // own result stays value-free.
            let workspace = leveler_execution::Workspace::new(f._tmp.path()).unwrap();
            let context = leveler_tools::ToolContext::new(workspace, PermissionProfile::Assisted)
                .with_session_scope(f.session.as_str())
                .with_secret_vault(f.app.secret_vault().clone());
            let output = leveler_tools::Tool::execute(
                &leveler_tools::tools::SecretApplyTool,
                serde_json::json!({
                    "secret_ref": reference,
                    "field": "DEEPSEEK_API_KEY",
                    "target": ".env",
                }),
                context,
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
            assert!(!output.is_error, "{}", output.content);
            assert!(!output.content.contains(CANARY), "{}", output.content);
            assert_eq!(
                std::fs::read_to_string(f._tmp.path().join(".env")).unwrap(),
                format!("DEEPSEEK_API_KEY={CANARY}\n")
            );
            assert_canary_absent_except(f._tmp.path(), &[".env"]);
        },
    );
}

#[test]
fn an_unacknowledged_credential_is_refused_before_any_turn_or_write() {
    leveler_test_support::bounded_test(
        "an_unacknowledged_credential_is_refused_before_any_turn_or_write",
        Duration::from_secs(30),
        || async {
            let f = fixture(vec![text_response("should not run")]).await;
            let error = f
                .client
                .send(ClientCommand::SubmitMessage {
                    session_id: f.session.clone(),
                    content: format!("DEEPSEEK_API_KEY={CANARY}"),
                    attachments: Vec::new(),
                })
                .await
                .expect_err("a high-confidence credential is refused");
            let message = error.to_string();
            assert!(message.contains("凭据"), "{message}");
            assert!(!message.contains(CANARY), "the refusal echoed the value");
            assert!(
                f.server.request_bodies().await.is_empty(),
                "no provider request was made"
            );
            assert_canary_absent_from_disk(f._tmp.path());
            assert_eq!(f.app.secret_vault().available_count(f.session.as_str()), 0);
        },
    );
}

#[test]
fn the_explicit_plaintext_choice_sends_the_value_and_says_so() {
    leveler_test_support::bounded_test(
        "the_explicit_plaintext_choice_sends_the_value_and_says_so",
        Duration::from_secs(60),
        || async {
            let f = fixture(vec![text_response("ok")]).await;
            let mut rx = f.client.subscribe();
            f.client
                .send(ClientCommand::SubmitUserInput {
                    session_id: f.session.clone(),
                    content: format!("DEEPSEEK_API_KEY={CANARY}"),
                    attachments: Vec::new(),
                    secret_captures: Vec::new(),
                    plaintext_secret_ack: true,
                })
                .await
                .expect("the person's explicit choice is honoured");
            f.wait_for_turn_end(&mut rx).await;
            assert!(
                f.server
                    .request_bodies()
                    .await
                    .iter()
                    .any(|body| body.contains(CANARY)),
                "the acknowledged plaintext reaches the provider, as chosen"
            );
        },
    );
}

#[test]
fn capture_secret_answers_with_a_reference_the_vault_can_resolve() {
    leveler_test_support::bounded_test(
        "capture_secret_answers_with_a_reference_the_vault_can_resolve",
        Duration::from_secs(30),
        || async {
            let f = fixture(Vec::new()).await;
            let mut rx = f.client.subscribe();
            let request_id = CommandId::generate();
            f.client
                .send(ClientCommand::CaptureSecret {
                    session_id: f.session.clone(),
                    request_id: request_id.clone(),
                    value: SecretValue::new(CANARY),
                    key_hint: Some("DEEPSEEK_API_KEY".into()),
                })
                .await
                .unwrap();
            let reference = loop {
                match tokio::time::timeout(Duration::from_secs(10), rx.recv()).await {
                    Ok(Ok(RuntimeEvent::SecretCaptured {
                        request_id: answered,
                        secret_ref,
                        masked,
                        ..
                    })) if answered == request_id => {
                        assert!(!masked.contains(CANARY), "{masked}");
                        break secret_ref;
                    }
                    Ok(Ok(_)) => {}
                    other => panic!("no capture answer: {other:?}"),
                }
            };
            let reference = leveler_execution::secret_vault::SecretRef::parse(&reference).unwrap();
            let resolved = f
                .app
                .secret_vault()
                .with_value(f.session.as_str(), &reference, |value| value.to_string())
                .unwrap();
            assert_eq!(resolved, CANARY);
            assert_canary_absent_from_disk(f._tmp.path());
        },
    );
}

#[test]
fn revoking_a_reference_stops_it_from_resolving() {
    leveler_test_support::bounded_test(
        "revoking_a_reference_stops_it_from_resolving",
        Duration::from_secs(30),
        || async {
            let f = fixture(Vec::new()).await;
            let mut rx = f.client.subscribe();
            let request_id = CommandId::generate();
            f.client
                .send(ClientCommand::CaptureSecret {
                    session_id: f.session.clone(),
                    request_id: request_id.clone(),
                    value: SecretValue::new(CANARY),
                    key_hint: None,
                })
                .await
                .unwrap();
            let reference = loop {
                match tokio::time::timeout(Duration::from_secs(10), rx.recv()).await {
                    Ok(Ok(RuntimeEvent::SecretCaptured {
                        request_id: answered,
                        secret_ref,
                        ..
                    })) if answered == request_id => break secret_ref,
                    Ok(Ok(_)) => {}
                    other => panic!("no capture answer: {other:?}"),
                }
            };
            f.client
                .send(ClientCommand::RevokeSecret {
                    session_id: f.session.clone(),
                    secret_ref: reference.clone(),
                })
                .await
                .unwrap();
            let reference = leveler_execution::secret_vault::SecretRef::parse(&reference).unwrap();
            assert!(matches!(
                f.app
                    .secret_vault()
                    .with_value(f.session.as_str(), &reference, |value| value.to_string()),
                Err(leveler_execution::secret_vault::VaultError::Revoked)
            ));
            assert_eq!(f.app.secret_vault().available_count(f.session.as_str()), 0);
            assert_canary_absent_from_disk(f._tmp.path());
        },
    );
}
