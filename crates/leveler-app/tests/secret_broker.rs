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
use leveler_client_protocol::{
    ClientCommand, InteractiveRuntimeClient, MessageId, RuntimeEvent, SteerAction,
};
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

fn tool_call_response(id: &str, name: &str, arguments: serde_json::Value) -> MockResponse {
    MockResponse::sse(&[
        &serde_json::json!({
            "choices": [{"delta": {"tool_calls": [{
                "index": 0, "id": id,
                "function": {"name": name, "arguments": arguments.to_string()}
            }]}}]
        })
        .to_string(),
        r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
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

fn assert_value_absent_from_disk(root: &std::path::Path, value: &str) {
    for (path, bytes) in all_files(root) {
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains(value),
            "the canary reached {}",
            path.display()
        );
    }
}

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
fn a_plaintext_acknowledgement_is_one_command_and_confers_no_custody() {
    leveler_test_support::bounded_test(
        "a_plaintext_acknowledgement_is_one_command_and_confers_no_custody",
        Duration::from_secs(60),
        || async {
            let f = fixture(vec![text_response("acknowledged")]).await;
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
                .expect("the person's explicit plaintext choice is admitted");
            f.wait_for_turn_end(&mut rx).await;

            // What the person chose is a real disclosure: the provider sees it.
            // Local persistence is a separate boundary, and the ordinary
            // model-visible sanitizer still keeps the value out of the session
            // store. Plaintext means "the provider may see it", not "the disk
            // now holds it".
            let bodies = f.server.request_bodies().await;
            assert!(
                bodies.iter().any(|body| body.contains(CANARY)),
                "plaintext was not sent even though the person chose it"
            );
            assert_eq!(
                f.app.secret_vault().available_count(f.session.as_str()),
                0,
                "plaintext acknowledgement must not create custody"
            );
            assert_canary_absent_from_disk(f._tmp.path());

            // The acknowledgement belongs to THAT command. A later command
            // carrying the same value is refused before any provider request.
            let error = f
                .client
                .send(ClientCommand::SubmitMessage {
                    session_id: f.session.clone(),
                    content: format!("再发一次 DEEPSEEK_API_KEY={CANARY}"),
                    attachments: Vec::new(),
                })
                .await
                .expect_err("a plaintext acknowledgement is not reusable");
            assert!(!error.to_string().contains(CANARY), "{error}");
            assert_eq!(
                f.server.request_bodies().await.len(),
                bodies.len(),
                "the refused command started no turn"
            );
        },
    );
}

#[test]
fn an_unreviewed_ambiguous_value_is_refused_on_a_legacy_command() {
    leveler_test_support::bounded_test(
        "an_unreviewed_ambiguous_value_is_refused_on_a_legacy_command",
        Duration::from_secs(60),
        || async {
            let f = fixture(vec![text_response("should not run")]).await;
            let mut _rx = f.client.subscribe();
            let error = f
                .client
                .send(ClientCommand::SubmitMessage {
                    session_id: f.session.clone(),
                    content: "API_KEY=abc123def".to_string(),
                    attachments: Vec::new(),
                })
                .await
                .expect_err("a client with no review surface cannot silently send it");
            assert!(!error.to_string().contains("abc123def"), "{error}");
            assert!(
                f.server.request_bodies().await.is_empty(),
                "the refused command started no provider request"
            );
            assert_eq!(
                f.app.secret_vault().available_count(f.session.as_str()),
                0,
                "nothing was captured"
            );
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

/// Gate F (deterministic half): an interrupted turn does not end the runtime
/// or the vault. The captured reference stays usable for the continued work,
/// and the model never sees the value on either side of the interruption.
#[test]
fn a_captured_reference_survives_an_interrupt_and_finishes_the_task() {
    leveler_test_support::bounded_test(
        "a_captured_reference_survives_an_interrupt_and_finishes_the_task",
        Duration::from_secs(60),
        || async {
            let f = fixture(vec![text_response("captured")]).await;
            let mut rx = f.client.subscribe();
            f.client
                .send(ClientCommand::SubmitUserInput {
                    session_id: f.session.clone(),
                    content: "帮我把 {{capture-0}} 配置到 .env".to_string(),
                    attachments: Vec::new(),
                    secret_captures: vec![
                        SecretCapture::new("{{capture-0}}", CANARY)
                            .with_key_hint(Some("DEEPSEEK_API_KEY".into())),
                    ],
                    plaintext_secret_ack: false,
                })
                .await
                .expect("a captured message is admitted");
            let events = f.wait_for_turn_end(&mut rx).await;
            let reference = announced_reference(&events);

            // The next turn hangs inside the provider stream so the person can
            // interrupt it. Nothing about the capture is involved in the
            // interruption itself.
            let release = Arc::new(tokio::sync::Notify::new());
            f.server.clear().await;
            f.server
                .push(MockResponse::GatedSse {
                    first: "data: {\"choices\":[{\"delta\":{\"content\":\"working\"}}]}\n\n"
                        .to_string(),
                    tail: "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"
                        .to_string(),
                    release: release.clone(),
                })
                .await;
            let before = f.server.request_count();
            f.client
                .send(ClientCommand::SubmitUserInput {
                    session_id: f.session.clone(),
                    content: "继续处理，稍后把这次捕获的值配置到 .env".to_string(),
                    attachments: Vec::new(),
                    secret_captures: Vec::new(),
                    plaintext_secret_ack: false,
                })
                .await
                .unwrap();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            while f.server.request_count() <= before {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the interrupted turn never reached the provider"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            f.client
                .send(ClientCommand::CancelCurrentTurn {
                    session_id: f.session.clone(),
                })
                .await
                .unwrap();
            loop {
                let remaining = Duration::from_secs(10);
                match tokio::time::timeout(remaining, rx.recv()).await {
                    Ok(Ok(RuntimeEvent::TurnIncomplete { .. }))
                    | Ok(Ok(RuntimeEvent::TurnCancelled))
                    | Ok(Ok(RuntimeEvent::TurnInterrupted)) => break,
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => panic!("event stream closed: {error}"),
                    Err(_) => panic!("the interrupted turn did not settle"),
                }
            }
            release.notify_one();

            // The vault is runtime-scoped, not turn-scoped: the authorized use
            // still resolves after the interruption.
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
            assert_eq!(
                std::fs::read_to_string(f._tmp.path().join(".env")).unwrap(),
                format!("DEEPSEEK_API_KEY={CANARY}\n")
            );

            // Continue in the same runtime. The model is answered with the
            // broker call for a new target the person names in this message.
            f.server.clear().await;
            f.server
                .push(tool_call_response(
                    "f-apply",
                    "secret_apply",
                    serde_json::json!({
                        "secret_ref": reference,
                        "field": "DEMO_API_KEY",
                        "target": ".env.local",
                    }),
                ))
                .await;
            // The mock repeats its last response; the tool result must be
            // followed by a terminal text answer or the model would call the
            // same tool forever.
            f.server.push(text_response("done")).await;
            f.client
                .send(ClientCommand::SubmitUserInput {
                    session_id: f.session.clone(),
                    content: format!(
                        "继续：把 [secret:{reference}] 写到 .env.local\nDEMO_API_KEY="
                    ),
                    attachments: Vec::new(),
                    secret_captures: Vec::new(),
                    plaintext_secret_ack: false,
                })
                .await
                .unwrap();
            f.wait_for_turn_end(&mut rx).await;
            assert_eq!(
                std::fs::read_to_string(f._tmp.path().join(".env.local")).unwrap(),
                format!("DEMO_API_KEY={CANARY}\n")
            );

            for body in f.server.request_bodies().await {
                assert!(
                    !body.contains(CANARY),
                    "the captured value reached the provider: {body}"
                );
            }
            assert_canary_absent_except(f._tmp.path(), &[".env", ".env.local"]);
        },
    );
}
/// A gated stream: the opening delta lets the test wait for the turn to be
/// genuinely running, then release the tail and settle.
fn gated_response(text: &str, release: Arc<tokio::sync::Notify>) -> MockResponse {
    MockResponse::GatedSse {
        first: format!("data: {{\"choices\":[{{\"delta\":{{\"content\":{text:?}}}}}]}}\n\n"),
        tail: "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
               data: [DONE]\n\n"
            .into(),
        release,
    }
}

async fn wait_for_stream_start(rx: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !matches!(
            rx.recv().await.expect("event stream"),
            RuntimeEvent::AssistantTextDelta { .. }
        ) {}
    })
    .await
    .expect("the final answer started streaming");
}

/// The documented plaintext choice is per command and real: an admitted steer
/// the person explicitly disclosed reaches the provider, while the journal,
/// the transcript and every file on disk keep the redacted projection. Once the
/// input is consumed, a later turn does not inherit the decision.
#[test]
fn an_acknowledged_plaintext_steer_is_delivered_once_and_never_persisted() {
    leveler_test_support::bounded_test(
        "an_acknowledged_plaintext_steer_is_delivered_once_and_never_persisted",
        Duration::from_secs(90),
        || async {
            let release = Arc::new(tokio::sync::Notify::new());
            let f = fixture(vec![
                gated_response("first answer", release.clone()),
                text_response("turn done"),
                text_response("follow up done"),
            ])
            .await;
            let mut rx = f.client.subscribe();
            f.client
                .send(ClientCommand::SubmitUserInput {
                    session_id: f.session.clone(),
                    content: "先做一个普通任务".to_string(),
                    attachments: Vec::new(),
                    secret_captures: Vec::new(),
                    plaintext_secret_ack: false,
                })
                .await
                .unwrap();
            wait_for_stream_start(&mut rx).await;

            // A value registered earlier (for example from a tool result) is
            // exactly what the acknowledgement must be able to override for
            // this one input.
            leveler_core::register_session_secrets(
                f.session.as_str(),
                &[leveler_core::DetectedSecret {
                    value: CANARY.to_string(),
                    key: "key".to_string(),
                }],
            );
            let id = MessageId::new("steer-plaintext-ack");
            f.client
                .send(ClientCommand::SteerUserInput {
                    session_id: f.session.clone(),
                    content: format!("请继续，并使用 key:{CANARY} 再测一次"),
                    secret_captures: Vec::new(),
                    plaintext_secret_ack: true,
                    message_id: Some(id.clone()),
                })
                .await
                .expect("the explicit plaintext steer is admitted");
            release.notify_one();
            f.wait_for_turn_end(&mut rx).await;

            let bodies = f.server.request_bodies().await;
            assert!(
                bodies.iter().any(|body| body.contains(CANARY)),
                "the acknowledged plaintext must reach the provider: {bodies:#?}"
            );
            assert!(
                bodies
                    .iter()
                    .any(|body| body.contains("请继续") || body.contains("再测一次")),
                "the steer itself must reach the provider: {bodies:#?}"
            );
            let db = f.app.open_database().await.unwrap();
            let input = leveler_storage::SessionInputRepository::new(&db)
                .load(&f.session, id.as_str())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(input.state, leveler_storage::SessionInputState::Consumed);
            assert!(
                !input.content.contains(CANARY),
                "the journal stores the redacted projection: {}",
                input.content
            );
            let messages = leveler_storage::MessageRepository::new(&db)
                .load(&f.session)
                .await
                .unwrap();
            assert!(
                messages.iter().all(|payload| !payload.contains(CANARY)),
                "the transcript must not persist the disclosed value: {messages:#?}"
            );

            // A later, unacknowledged turn starts from the durable transcript
            // and must not inherit the permit.
            let before = bodies.len();
            f.client
                .send(ClientCommand::SubmitUserInput {
                    session_id: f.session.clone(),
                    content: "继续普通工作".to_string(),
                    attachments: Vec::new(),
                    secret_captures: Vec::new(),
                    plaintext_secret_ack: false,
                })
                .await
                .unwrap();
            f.wait_for_turn_end(&mut rx).await;
            let after = f.server.request_bodies().await;
            assert!(
                after[before..].iter().all(|body| !body.contains(CANARY)),
                "a later turn inherited the plaintext decision: {after:#?}"
            );
            assert_canary_absent_from_disk(f._tmp.path());
            leveler_core::clear_session_secrets(f.session.as_str());
        },
    );
}

/// A seed explicitly acknowledged as plaintext reaches the provider even when
/// the same value was already registered (the ordinary projection would scrub
/// it), and the durable stores still keep it out.
#[test]
fn an_acknowledged_seed_overrides_a_registered_value_only_for_that_command() {
    leveler_test_support::bounded_test(
        "an_acknowledged_seed_overrides_a_registered_value_only_for_that_command",
        Duration::from_secs(60),
        || async {
            let f = fixture(vec![text_response("acknowledged seed")]).await;
            let mut rx = f.client.subscribe();
            leveler_core::register_session_secrets(
                f.session.as_str(),
                &[leveler_core::DetectedSecret {
                    value: CANARY.to_string(),
                    key: "key".to_string(),
                }],
            );
            f.client
                .send(ClientCommand::SubmitUserInput {
                    session_id: f.session.clone(),
                    content: format!("配置 key:{CANARY}，这次按明文发送"),
                    attachments: Vec::new(),
                    secret_captures: Vec::new(),
                    plaintext_secret_ack: true,
                })
                .await
                .expect("the explicit plaintext seed is admitted");
            f.wait_for_turn_end(&mut rx).await;
            let bodies = f.server.request_bodies().await;
            assert!(
                bodies.iter().any(|body| body.contains(CANARY)),
                "the explicit seed disclosure reaches the provider: {bodies:#?}"
            );
            assert_canary_absent_from_disk(f._tmp.path());

            // The acknowledgement belonged to that command. A later ordinary
            // command carrying the same shape is refused before any request.
            let requests = bodies.len();
            let error = f
                .client
                .send(ClientCommand::SubmitMessage {
                    session_id: f.session.clone(),
                    content: format!("再发一次 key:{CANARY}"),
                    attachments: Vec::new(),
                })
                .await
                .expect_err("plaintext is not a reusable permission");
            assert!(!error.to_string().contains(CANARY), "{error}");
            assert_eq!(
                f.server.request_bodies().await.len(),
                requests,
                "the refused command started no provider request"
            );
            leveler_core::clear_session_secrets(f.session.as_str());
        },
    );
}

/// Withdrawing a pending steer settles its row as cancelled; the plaintext
/// decision dies with it and no provider request carries the value.
#[test]
fn a_withdrawn_plaintext_steer_never_reaches_the_provider() {
    leveler_test_support::bounded_test(
        "a_withdrawn_plaintext_steer_never_reaches_the_provider",
        Duration::from_secs(90),
        || async {
            let release = Arc::new(tokio::sync::Notify::new());
            let f = fixture(vec![
                gated_response("first answer", release.clone()),
                text_response("turn done"),
            ])
            .await;
            let mut rx = f.client.subscribe();
            f.client
                .send(ClientCommand::SubmitUserInput {
                    session_id: f.session.clone(),
                    content: "先做一个普通任务".to_string(),
                    attachments: Vec::new(),
                    secret_captures: Vec::new(),
                    plaintext_secret_ack: false,
                })
                .await
                .unwrap();
            wait_for_stream_start(&mut rx).await;
            let id = MessageId::new("steer-plaintext-withdraw");
            f.client
                .send(ClientCommand::SteerUserInput {
                    session_id: f.session.clone(),
                    content: format!("请使用 key:{CANARY}"),
                    secret_captures: Vec::new(),
                    plaintext_secret_ack: true,
                    message_id: Some(id.clone()),
                })
                .await
                .unwrap();
            f.client
                .send(ClientCommand::ManageSteer {
                    session_id: f.session.clone(),
                    action: SteerAction::Withdraw {
                        message_id: id.clone(),
                        revision: 0,
                    },
                })
                .await
                .expect("the pending steer is withdrawn");
            release.notify_one();
            f.wait_for_turn_end(&mut rx).await;
            assert!(
                f.server
                    .request_bodies()
                    .await
                    .iter()
                    .all(|body| !body.contains(CANARY)),
                "a withdrawn input was delivered"
            );
            let db = f.app.open_database().await.unwrap();
            let input = leveler_storage::SessionInputRepository::new(&db)
                .load(&f.session, id.as_str())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(input.state, leveler_storage::SessionInputState::Cancelled);
            assert_canary_absent_from_disk(f._tmp.path());
            leveler_core::clear_session_secrets(f.session.as_str());
        },
    );
}

/// Editing a pending steer changes the text and its revision. The old
/// plaintext decision is not applied to the new text, and the edited text is
/// delivered normally.
#[test]
fn an_edited_plaintext_steer_does_not_reuse_the_old_decision() {
    leveler_test_support::bounded_test(
        "an_edited_plaintext_steer_does_not_reuse_the_old_decision",
        Duration::from_secs(90),
        || async {
            let release = Arc::new(tokio::sync::Notify::new());
            let f = fixture(vec![
                gated_response("first answer", release.clone()),
                text_response("turn done"),
            ])
            .await;
            let mut rx = f.client.subscribe();
            f.client
                .send(ClientCommand::SubmitUserInput {
                    session_id: f.session.clone(),
                    content: "先做一个普通任务".to_string(),
                    attachments: Vec::new(),
                    secret_captures: Vec::new(),
                    plaintext_secret_ack: false,
                })
                .await
                .unwrap();
            wait_for_stream_start(&mut rx).await;
            let id = MessageId::new("steer-plaintext-edit");
            f.client
                .send(ClientCommand::SteerUserInput {
                    session_id: f.session.clone(),
                    content: format!("请使用 key:{CANARY}"),
                    secret_captures: Vec::new(),
                    plaintext_secret_ack: true,
                    message_id: Some(id.clone()),
                })
                .await
                .unwrap();
            f.client
                .send(ClientCommand::ManageSteer {
                    session_id: f.session.clone(),
                    action: SteerAction::BeginEdit {
                        message_id: id.clone(),
                        revision: 0,
                        lease_id: "00000000-0000-0000-0000-000000000001".to_string(),
                    },
                })
                .await
                .expect("the edit lease opens");
            f.client
                .send(ClientCommand::ManageSteer {
                    session_id: f.session.clone(),
                    action: SteerAction::SaveEdit {
                        message_id: id.clone(),
                        revision: 1,
                        lease_id: "00000000-0000-0000-0000-000000000001".to_string(),
                        content: "改为先跑单元测试".to_string(),
                    },
                })
                .await
                .expect("the edit is saved");
            release.notify_one();
            f.wait_for_turn_end(&mut rx).await;
            let bodies = f.server.request_bodies().await;
            assert!(
                bodies.iter().any(|body| body.contains("改为先跑单元测试")),
                "the edited text must be delivered: {bodies:#?}"
            );
            assert!(
                bodies.iter().all(|body| !body.contains(CANARY)),
                "the edited input reused the old plaintext decision: {bodies:#?}"
            );
            assert_canary_absent_from_disk(f._tmp.path());
            leveler_core::clear_session_secrets(f.session.as_str());
        },
    );
}

/// Prioritize changes only the scheduling priority, not the text or its
/// revision, so the person's explicit decision still applies to that delivery.
#[test]
fn a_prioritized_plaintext_steer_keeps_its_decision() {
    leveler_test_support::bounded_test(
        "a_prioritized_plaintext_steer_keeps_its_decision",
        Duration::from_secs(90),
        || async {
            let release = Arc::new(tokio::sync::Notify::new());
            let f = fixture(vec![
                gated_response("first answer", release.clone()),
                text_response("turn done"),
            ])
            .await;
            let mut rx = f.client.subscribe();
            f.client
                .send(ClientCommand::SubmitUserInput {
                    session_id: f.session.clone(),
                    content: "先做一个普通任务".to_string(),
                    attachments: Vec::new(),
                    secret_captures: Vec::new(),
                    plaintext_secret_ack: false,
                })
                .await
                .unwrap();
            wait_for_stream_start(&mut rx).await;
            let id = MessageId::new("steer-plaintext-prioritize");
            f.client
                .send(ClientCommand::SteerUserInput {
                    session_id: f.session.clone(),
                    content: format!("请立即改用 key:{CANARY}"),
                    secret_captures: Vec::new(),
                    plaintext_secret_ack: true,
                    message_id: Some(id.clone()),
                })
                .await
                .unwrap();
            f.client
                .send(ClientCommand::ManageSteer {
                    session_id: f.session.clone(),
                    action: SteerAction::Prioritize {
                        message_id: id.clone(),
                        revision: 0,
                    },
                })
                .await
                .expect("the steer is prioritized");
            release.notify_one();
            f.wait_for_turn_end(&mut rx).await;
            assert!(
                f.server
                    .request_bodies()
                    .await
                    .iter()
                    .any(|body| body.contains(CANARY)),
                "prioritize changed neither the text nor the decision"
            );
            assert_canary_absent_from_disk(f._tmp.path());
            leveler_core::clear_session_secrets(f.session.as_str());
        },
    );
}

/// Regression #1: an ordinary seed repeating a value the session already
/// registered is projected without it, even though the value has no detectable
/// shape left (a hash-like token in prose). The person did not acknowledge
/// anything here.
#[test]
fn a_registered_value_is_scrubbed_from_an_ordinary_seed() {
    leveler_test_support::bounded_test(
        "a_registered_value_is_scrubbed_from_an_ordinary_seed",
        Duration::from_secs(60),
        || async {
            let f = fixture(vec![text_response("ok")]).await;
            let mut rx = f.client.subscribe();
            // A hash-like value: no key/value shape and recognised as a hash,
            // so only the session registry can catch it.
            let value = "7f3a9c2e5b8d4f6a1c0e8b7d4e5f60718293a4b5";
            leveler_core::register_session_secrets(
                f.session.as_str(),
                &[leveler_core::DetectedSecret {
                    value: value.to_string(),
                    key: "token".to_string(),
                }],
            );
            f.client
                .send(ClientCommand::SubmitUserInput {
                    session_id: f.session.clone(),
                    content: format!("请继续任务，使用 {value} 这个值"),
                    attachments: Vec::new(),
                    secret_captures: Vec::new(),
                    plaintext_secret_ack: false,
                })
                .await
                .expect("an ordinary message without a detectable shape is admitted");
            f.wait_for_turn_end(&mut rx).await;
            let bodies = f.server.request_bodies().await;
            assert!(
                bodies.iter().all(|body| !body.contains(value)),
                "a registered value reached the provider: {bodies:#?}"
            );
            assert!(
                bodies.iter().any(|body| body.contains("[REDACTED]")),
                "the model request kept the scrubbed projection: {bodies:#?}"
            );
            let db = f.app.open_database().await.unwrap();
            let messages = leveler_storage::MessageRepository::new(&db)
                .load(&f.session)
                .await
                .unwrap();
            assert!(messages.iter().all(|payload| !payload.contains(value)));
            assert_canary_absent_from_disk(f._tmp.path());
            leveler_core::clear_session_secrets(f.session.as_str());
        },
    );
}

/// The headless entry's explicit `--allow-plaintext-secrets` is a one-command
/// disclosure: the provider sees the bytes, the durable session stores keep
/// the redacted projection. This is the `leveler run` path, not a mock.
#[test]
fn a_headless_plaintext_choice_reaches_the_provider_and_not_the_stores() {
    leveler_test_support::bounded_test(
        "a_headless_plaintext_choice_reaches_the_provider_and_not_the_stores",
        Duration::from_secs(60),
        || async {
            let canary = "mgzk1gr8k7bfm6z6t8ft6tdxd8ydmm5t";
            let server = MockServer::start(vec![text_response("ok")]).await;
            let tmp = tempfile::tempdir().unwrap();
            write_config(tmp.path(), &server.base_url());
            let layout = Layout::from_parts(
                tmp.path().to_path_buf(),
                tmp.path().join("configs"),
                tmp.path().join("state"),
            );
            let app = Arc::new(
                Application::assemble(layout)
                    .unwrap()
                    .with_collaboration(leveler_lifecycle::CollaborationMode::Chat)
                    .with_plaintext_seed_ack(true),
            );
            let model = ModelRef::new("mock", "m");
            let task = format!("请配置 key:{canary}，然后回答 ok");
            let session = app.create_session(&model, &task).await.unwrap();
            app.run_in_session(
                &session,
                &model,
                PermissionProfile::Assisted,
                &task,
                Arc::new(leveler_execution::AutoApprove),
                false,
                &mut |_| {},
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
            let bodies = server.request_bodies().await;
            assert!(
                bodies.iter().any(|body| body.contains(canary)),
                "the explicit headless choice reaches the provider: {bodies:#?}"
            );
            assert_value_absent_from_disk(tmp.path(), canary);
            leveler_core::clear_session_secrets(session.as_str());
        },
    );
}
