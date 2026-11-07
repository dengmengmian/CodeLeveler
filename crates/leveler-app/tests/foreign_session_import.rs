//! Cross-agent session import through the interactive runtime.
//!
//! These are the application contracts behind `/resume-claude` and
//! `/resume-codex`: discovery reads only the source store, an import creates a
//! NEW native CodeLeveler session with durable source identity, and the
//! continuation turn carries the bounded briefing plus the read-only history
//! tool. The fixtures are real store shapes — Claude project JSONL and Codex
//! rollout JSONL — written by the test, never by a vendor runtime.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime};

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    ClientCommand, InteractiveRuntimeClient, RuntimeEvent, UiForeignTool, UiMessageKind,
    UiSessionSnapshot,
};
use leveler_execution::PermissionProfile;
use leveler_model::{ModelRef, Role, RuntimeNoticeKind, TranscriptOrigin};
use leveler_project::Layout;
use leveler_storage::{
    ForeignSessionOriginRepository, MessageRepository, SessionRepository, TurnRepository,
};
use leveler_test_support::{MockResponse, MockServer};
use tokio::sync::broadcast;

const CLAUDE_ID: &str = "11111111-1111-4111-8111-111111111111";
const CLAUDE_OLDER_ID: &str = "22222222-2222-4222-8222-222222222222";
const CLAUDE_ELSEWHERE_ID: &str = "33333333-3333-4333-8333-333333333333";
const CLAUDE_EMPTY_ID: &str = "44444444-4444-4444-8444-444444444444";
const CLAUDE_INCOMPATIBLE_ID: &str = "66666666-6666-4666-8666-666666666666";
const CODEX_ID: &str = "55555555-5555-4555-8555-555555555555";
const CLAUDE_UNKNOWN_ID: &str = "99999999-9999-4999-8999-999999999999";

const IMPORTED_REQUEST: &str = "Migrate the widget parser to the new schema.";
const OLDER_REQUEST: &str = "Add a regression test for the empty input path.";
const ELSEWHERE_REQUEST: &str = "Continue the deploy checklist.";
const CODEX_REQUEST: &str = "Fix the failing Codex test.";
const ASSISTANT_CLAIM: &str = "Done: the parser accepts schema v2.";

struct Fixture {
    _root: tempfile::TempDir,
    workspace: PathBuf,
    claude_root: PathBuf,
    codex_root: PathBuf,
}

fn fixture() -> &'static Fixture {
    static FIXTURE: OnceLock<Fixture> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        fs::create_dir_all(workspace.join("src")).unwrap();
        fs::write(workspace.join("src/lib.rs"), "pub fn parser() {}\n").unwrap();
        fs::write(workspace.join("src/parser.rs"), "pub fn parse() {}\n").unwrap();

        let claude_root = root.path().join("claude");
        let project = claude_root
            .join("projects")
            .join(project_dir_name(&workspace));
        fs::create_dir_all(&project).unwrap();
        let now = SystemTime::now();
        write_claude(
            &project,
            CLAUDE_ID,
            now,
            &[
                claude_user(CLAUDE_ID, &workspace, IMPORTED_REQUEST),
                claude_assistant(
                    CLAUDE_ID,
                    &workspace,
                    Some("I will run the tests."),
                    Some((
                        "toolu_done",
                        "Bash",
                        serde_json::json!({"command": "cargo test"}),
                    )),
                ),
                claude_tool_result(CLAUDE_ID, &workspace, "toolu_done", "test result: ok"),
                claude_assistant(
                    CLAUDE_ID,
                    &workspace,
                    Some(ASSISTANT_CLAIM),
                    Some((
                        "toolu_open",
                        "Edit",
                        serde_json::json!({"file_path": "/tmp/parser.rs"}),
                    )),
                ),
            ],
        );
        write_claude(
            &project,
            CLAUDE_OLDER_ID,
            now - Duration::from_secs(3600),
            &[
                claude_user(CLAUDE_OLDER_ID, &workspace, OLDER_REQUEST),
                claude_assistant(CLAUDE_OLDER_ID, &workspace, Some("Added the test."), None),
            ],
        );
        let elsewhere = root.path().join("elsewhere");
        write_claude(
            &project,
            CLAUDE_ELSEWHERE_ID,
            now - Duration::from_secs(7200),
            &[
                claude_user(CLAUDE_ELSEWHERE_ID, &elsewhere, ELSEWHERE_REQUEST),
                claude_assistant(
                    CLAUDE_ELSEWHERE_ID,
                    &elsewhere,
                    Some("Checked step 3."),
                    None,
                ),
            ],
        );
        fs::write(project.join(format!("{CLAUDE_EMPTY_ID}.jsonl")), "").unwrap();
        fs::write(
            project.join(format!("{CLAUDE_INCOMPATIBLE_ID}.jsonl")),
            "{\"type\":\"something-unknown\"}\n",
        )
        .unwrap();

        let codex_root = root.path().join("codex");
        let day = codex_root.join("sessions/2026/11/05");
        fs::create_dir_all(&day).unwrap();
        fs::write(
            day.join(format!("rollout-2026-11-05T10-00-00-{CODEX_ID}.jsonl")),
            [
                serde_json::json!({
                    "type": "session_meta",
                    "payload": {
                        "id": CODEX_ID,
                        "cwd": workspace.display().to_string(),
                        "git": {"branch": "main"},
                    }
                })
                .to_string(),
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_text", "text": CODEX_REQUEST}],
                    }
                })
                .to_string(),
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": "Fixed the Codex test."}],
                    }
                })
                .to_string(),
            ]
            .join("\n")
                + "\n",
        )
        .unwrap();

        // The runtime snapshots the process environment when it assembles.
        // The stores are per-process fixtures, so every test sees the same
        // roots.
        let leveler_home = root.path().join("leveler-home");
        fs::create_dir_all(&leveler_home).unwrap();
        unsafe {
            std::env::set_var("CLAUDE_CONFIG_DIR", &claude_root);
            std::env::set_var("CODEX_HOME", &codex_root);
            std::env::set_var("LEVELER_HOME", &leveler_home);
        }

        Fixture {
            _root: root,
            workspace,
            claude_root,
            codex_root,
        }
    })
}

fn project_dir_name(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect()
}

fn claude_user(id: &str, cwd: &Path, text: &str) -> String {
    serde_json::json!({
        "type": "user",
        "sessionId": id,
        "cwd": cwd.display().to_string(),
        "gitBranch": "main",
        "timestamp": "2026-11-05T09:00:00.000Z",
        "message": {"role": "user", "content": text},
    })
    .to_string()
}

fn claude_assistant(
    id: &str,
    cwd: &Path,
    text: Option<&str>,
    call: Option<(&str, &str, serde_json::Value)>,
) -> String {
    let mut content = Vec::new();
    if let Some(text) = text {
        content.push(serde_json::json!({"type": "text", "text": text}));
    }
    if let Some((call_id, name, input)) = call {
        content.push(serde_json::json!({
            "type": "tool_use",
            "id": call_id,
            "name": name,
            "input": input,
        }));
    }
    serde_json::json!({
        "type": "assistant",
        "sessionId": id,
        "cwd": cwd.display().to_string(),
        "timestamp": "2026-11-05T09:00:01.000Z",
        "message": {"role": "assistant", "content": content},
    })
    .to_string()
}

fn claude_tool_result(id: &str, cwd: &Path, call_id: &str, text: &str) -> String {
    serde_json::json!({
        "type": "user",
        "sessionId": id,
        "cwd": cwd.display().to_string(),
        "timestamp": "2026-11-05T09:00:02.000Z",
        "message": {
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": call_id,
                "is_error": false,
                "content": text,
            }],
        },
    })
    .to_string()
}

fn write_claude(project: &Path, id: &str, modified: SystemTime, lines: &[String]) {
    let path = project.join(format!("{id}.jsonl"));
    fs::write(&path, lines.join("\n") + "\n").unwrap();
    // Windows refuses SetFileTime on a read-only handle, while Unix accepts
    // it; opening for write is what makes the fixture portable.
    fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(modified)
        .unwrap();
}

fn claude_source_path(f: &Fixture, id: &str) -> PathBuf {
    f.claude_root
        .join("projects")
        .join(project_dir_name(&f.workspace))
        .join(format!("{id}.jsonl"))
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

fn answer(text: &str) -> MockResponse {
    sse(vec![
        serde_json::json!({"choices": [{"delta": {"content": text}}]}).to_string(),
        serde_json::json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}).to_string(),
    ])
}

fn write_config(root: &Path, base_url: &str) {
    fs::create_dir_all(root.join("configs/providers")).unwrap();
    fs::create_dir_all(root.join("configs/models")).unwrap();
    fs::write(
        root.join("configs/providers/mock.yaml"),
        format!("id: mock\nprotocol: openai_chat\nbase_url: {base_url}\n"),
    )
    .unwrap();
    fs::write(
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

struct Harness {
    _tmp: tempfile::TempDir,
    app: Arc<Application>,
    client: Arc<InProcessRuntimeClient>,
    requester: leveler_core::SessionId,
    server: MockServer,
}

struct ImportOutcome {
    tool: UiForeignTool,
    native_id: String,
    session_id: leveler_core::SessionId,
    warning: Option<String>,
    opened: Option<UiSessionSnapshot>,
}

impl Harness {
    async fn start() -> Self {
        let f = fixture();
        let server = MockServer::start(vec![answer("工作已接续，我先核对当前工作区。")]).await;
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), &server.base_url());
        let layout = Layout::from_parts(
            f.workspace.clone(),
            tmp.path().join("configs"),
            tmp.path().join("state"),
        );
        let app = Arc::new(Application::assemble(layout).unwrap());
        let model = ModelRef::new("mock", "m");
        let requester = app.create_session(&model, "requester").await.unwrap();
        let client = Arc::new(InProcessRuntimeClient::new(
            app.clone(),
            model,
            PermissionProfile::Assisted,
            false,
        ));
        Self {
            _tmp: tmp,
            app,
            client,
            requester,
            server,
        }
    }

    fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
        self.client.subscribe_session(&self.requester)
    }

    async fn import(&self, tool: UiForeignTool, native_id: &str) -> ImportOutcome {
        let mut events = self.subscribe();
        self.client
            .send(ClientCommand::ImportForeignSession {
                requester_session_id: self.requester.clone(),
                tool,
                native_id: native_id.to_string(),
            })
            .await
            .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let mut imported = None;
        let mut opened = None;
        while imported.is_none() || opened.is_none() {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, events.recv()).await {
                Ok(Ok(RuntimeEvent::ForeignSessionImported {
                    tool,
                    native_id,
                    session_id,
                    warning,
                })) => imported = Some((tool, native_id, session_id, warning)),
                Ok(Ok(RuntimeEvent::ForeignSessionImportFailed { message, .. })) => {
                    panic!("the import failed: {message}")
                }
                Ok(Ok(RuntimeEvent::SessionOpened { session })) => opened = Some(session),
                Ok(Ok(_)) | Ok(Err(broadcast::error::RecvError::Lagged(_))) => {}
                Ok(Err(broadcast::error::RecvError::Closed)) | Err(_) => {
                    panic!("timed out waiting for the import to report")
                }
            }
        }
        let (tool, native_id, session_id, warning) = imported.expect("imported event");
        ImportOutcome {
            tool,
            native_id,
            session_id,
            warning,
            opened,
        }
    }

    async fn import_failure(&self, tool: UiForeignTool, native_id: &str) -> String {
        let mut events = self.subscribe();
        self.client
            .send(ClientCommand::ImportForeignSession {
                requester_session_id: self.requester.clone(),
                tool,
                native_id: native_id.to_string(),
            })
            .await
            .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, events.recv()).await {
                Ok(Ok(RuntimeEvent::ForeignSessionImportFailed { message, .. })) => return message,
                Ok(Ok(RuntimeEvent::ForeignSessionImported { session_id, .. })) => {
                    panic!("a failed import must not create session {session_id}")
                }
                Ok(Ok(_)) | Ok(Err(broadcast::error::RecvError::Lagged(_))) => {}
                Ok(Err(broadcast::error::RecvError::Closed)) | Err(_) => {
                    panic!("timed out waiting for the import failure")
                }
            }
        }
    }

    async fn list(&self, tool: UiForeignTool) -> RuntimeEvent {
        let mut events = self.subscribe();
        self.client
            .send(ClientCommand::ListForeignSessionsFor {
                requester_session_id: self.requester.clone(),
                tool,
            })
            .await
            .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, events.recv()).await {
                Ok(Ok(event @ RuntimeEvent::ForeignSessionList { .. })) => return event,
                Ok(Ok(_)) | Ok(Err(broadcast::error::RecvError::Lagged(_))) => {}
                Ok(Err(broadcast::error::RecvError::Closed)) | Err(_) => {
                    panic!("timed out waiting for the foreign session list")
                }
            }
        }
    }

    async fn wait_for_turn_end(&self, session: &leveler_core::SessionId) {
        let db = self.app.open_database().await.unwrap();
        let repo = TurnRepository::new(&db);
        for _ in 0..600 {
            let durable_idle = repo.list_running(None).await.unwrap().is_empty();
            if durable_idle && !self.client.has_live_turn(session) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("the continuation turn did not settle");
    }

    async fn reopen(&self, session_id: &leveler_core::SessionId) -> UiSessionSnapshot {
        // `OpenSession` answers on the runtime-wide stream; a per-session
        // stream only carries events addressed to that session.
        let mut events = self.client.subscribe();
        self.client
            .send(ClientCommand::OpenSession {
                session_id: session_id.clone(),
            })
            .await
            .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, events.recv()).await {
                Ok(Ok(RuntimeEvent::SessionOpened { session })) if session.id == *session_id => {
                    return session;
                }
                Ok(Ok(_)) | Ok(Err(broadcast::error::RecvError::Lagged(_))) => {}
                Ok(Err(broadcast::error::RecvError::Closed)) | Err(_) => {
                    panic!("timed out reopening the imported session")
                }
            }
        }
    }
}

async fn wait_for_request(server: &MockServer) -> String {
    for _ in 0..600 {
        if let Some(body) = server.request_bodies().await.first().cloned() {
            return body;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the continuation turn never reached the provider");
}

fn briefing_message(snapshot: &UiSessionSnapshot) -> &leveler_client_protocol::UiMessage {
    snapshot
        .messages
        .iter()
        .find(|message| {
            message.kind == Some(UiMessageKind::RuntimeNotice)
                && message.text.starts_with("## Imported session briefing")
        })
        .unwrap_or_else(|| {
            panic!(
                "the snapshot must show the import briefing; messages: {:?}",
                snapshot
                    .messages
                    .iter()
                    .map(|message| message.text.chars().take(80).collect::<String>())
                    .collect::<Vec<_>>()
            )
        })
}

#[test]
fn claude_import_creates_a_native_session_and_continues_from_the_source() {
    leveler_test_support::bounded_test(
        "claude_import_creates_a_native_session_and_continues_from_the_source",
        leveler_test_support::DEFAULT_TEST_TIMEOUT,
        claude_import_body,
    );
}

async fn claude_import_body() {
    let f = fixture();
    let h = Harness::start().await;
    let source = claude_source_path(f, CLAUDE_ID);
    let before = fs::read(&source).unwrap();

    let outcome = h.import(UiForeignTool::ClaudeCode, CLAUDE_ID).await;
    assert_eq!(outcome.tool, UiForeignTool::ClaudeCode);
    assert_eq!(outcome.native_id, CLAUDE_ID);
    assert_ne!(
        outcome.session_id, h.requester,
        "an import starts a NEW native session"
    );
    assert!(
        outcome.warning.is_none(),
        "the source and workspace agree: {:?}",
        outcome.warning
    );

    let opened = outcome
        .opened
        .expect("the requester is switched to the imported session");
    assert_eq!(opened.id, outcome.session_id);
    let origin = opened
        .foreign_origin
        .as_ref()
        .expect("the snapshot carries the source identity");
    assert_eq!(origin.tool, UiForeignTool::ClaudeCode);
    assert_eq!(origin.native_id, CLAUDE_ID);
    assert_eq!(origin.source_path, source.display().to_string());
    let briefing = briefing_message(&opened);
    assert!(
        briefing.text.contains(IMPORTED_REQUEST),
        "the original request is quoted"
    );
    assert!(
        briefing.text.contains(ASSISTANT_CLAIM),
        "the last claim is quoted"
    );
    assert!(
        briefing.text.contains("that agent's claim, unverified"),
        "the claim is labeled, not promoted to fact"
    );

    // The continuation turn is a real request carrying the briefing and the
    // read-only history tool.
    let body = wait_for_request(&h.server).await;
    assert!(
        body.contains("foreign_session_history"),
        "the history tool must be on the continuation surface: {body}"
    );
    assert!(
        body.contains("Imported session briefing"),
        "the briefing must be prior context: {body}"
    );
    assert!(body.contains(IMPORTED_REQUEST), "{body}");
    assert!(body.contains(CLAUDE_ID), "{body}");
    assert!(
        body.contains("Continue the work described in the imported-session briefing above"),
        "{body}"
    );
    h.wait_for_turn_end(&outcome.session_id).await;

    let db = h.app.open_database().await.unwrap();
    let record = ForeignSessionOriginRepository::new(&db)
        .get(outcome.session_id.as_str())
        .await
        .unwrap()
        .expect("the origin is durable");
    assert_eq!(record.tool, "claude-code");
    assert_eq!(record.native_id, CLAUDE_ID);
    assert_eq!(
        record.source_cwd.as_deref(),
        Some(f.workspace.to_str().unwrap())
    );

    let sessions = SessionRepository::new(&db).list().await.unwrap();
    let row = sessions
        .iter()
        .find(|session| session.id == outcome.session_id.as_str())
        .expect("the imported session is listed");
    assert_eq!(
        row.collaboration, "chat",
        "imported work is not silently a Goal"
    );
    assert!(
        row.goal.contains("Migrate the widget parser"),
        "{}",
        row.goal
    );

    let payloads = MessageRepository::new(&db)
        .load(&outcome.session_id)
        .await
        .unwrap();
    let message: leveler_model::Message = serde_json::from_str(&payloads[0]).unwrap();
    assert_eq!(message.role, Role::User);
    assert!(matches!(
        message.origin,
        Some(TranscriptOrigin::RuntimeNotice {
            notice: RuntimeNoticeKind::ForeignSessionImport
        })
    ));
    let text = message.text_content();
    assert!(text.contains("Source session id: `"), "{text}");
    assert!(text.contains(CLAUDE_ID), "{text}");
    assert!(text.contains("### Reading the original history"), "{text}");

    // Read-only source: the import and the turn never wrote the file.
    assert_eq!(fs::read(&source).unwrap(), before);

    // Reopening is the native resume path and still knows where it came from.
    let reopened = h.reopen(&outcome.session_id).await;
    assert_eq!(
        reopened
            .foreign_origin
            .as_ref()
            .map(|o| o.native_id.as_str()),
        Some(CLAUDE_ID)
    );

    // A reopened transcript keeps the briefing, not just the live snapshot.
    let (entries, _) = leveler_app::session_history::load_session_history(&db, &outcome.session_id)
        .await
        .unwrap();
    assert!(
        entries.iter().any(|entry| matches!(
            &entry.event,
            RuntimeEvent::UserMessageAdded { message }
                if message.kind == Some(UiMessageKind::RuntimeNotice)
                    && message.text.starts_with("## Imported session briefing")
        )),
        "the source briefing must survive history replay"
    );

    // Native compaction on the imported session is a no-op on a short
    // conversation and must not fail or drop the source identity.
    let mut events = h.client.subscribe_session(&outcome.session_id);
    h.client
        .send(ClientCommand::CompactContext {
            session_id: outcome.session_id.clone(),
        })
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Ok(RuntimeEvent::Notification { message, .. })) => {
                assert!(
                    message.contains("无需压缩"),
                    "a short conversation compacts to a no-op, got {message}"
                );
                break;
            }
            Ok(Ok(RuntimeEvent::TurnFailed { error, .. })) => {
                panic!("native compaction failed on the imported session: {error}")
            }
            Ok(Ok(_)) | Ok(Err(broadcast::error::RecvError::Lagged(_))) => {}
            Ok(Err(broadcast::error::RecvError::Closed)) | Err(_) => {
                panic!("timed out waiting for native compaction")
            }
        }
    }
    assert!(
        ForeignSessionOriginRepository::new(&db)
            .get(outcome.session_id.as_str())
            .await
            .unwrap()
            .is_some(),
        "compaction must not drop the imported source identity"
    );
}

#[test]
fn codex_import_continues_and_can_be_reopened() {
    leveler_test_support::bounded_test(
        "codex_import_continues_and_can_be_reopened",
        leveler_test_support::DEFAULT_TEST_TIMEOUT,
        codex_import_body,
    );
}

async fn codex_import_body() {
    let f = fixture();
    let h = Harness::start().await;
    let source = f
        .codex_root
        .join("sessions/2026/11/05")
        .join(format!("rollout-2026-11-05T10-00-00-{CODEX_ID}.jsonl"));
    let before = fs::read(&source).unwrap();

    let outcome = h.import(UiForeignTool::Codex, CODEX_ID).await;
    assert_eq!(outcome.tool, UiForeignTool::Codex);
    assert!(outcome.warning.is_none(), "{:?}", outcome.warning);
    let opened = outcome.opened.expect("the imported session is opened");
    let origin = opened.foreign_origin.as_ref().expect("source identity");
    assert_eq!(origin.tool, UiForeignTool::Codex);
    assert_eq!(origin.native_id, CODEX_ID);
    assert!(
        briefing_message(&opened).text.contains(CODEX_REQUEST),
        "the Codex request is quoted"
    );

    let body = wait_for_request(&h.server).await;
    assert!(body.contains("foreign_session_history"), "{body}");
    assert!(body.contains(CODEX_REQUEST), "{body}");
    assert!(body.contains("Source branch: `main`"), "{body}");
    h.wait_for_turn_end(&outcome.session_id).await;

    let db = h.app.open_database().await.unwrap();
    let record = ForeignSessionOriginRepository::new(&db)
        .get(outcome.session_id.as_str())
        .await
        .unwrap()
        .expect("origin");
    assert_eq!(record.tool, "codex");
    assert_eq!(record.native_id, CODEX_ID);

    assert_eq!(fs::read(&source).unwrap(), before);

    let reopened = h.reopen(&outcome.session_id).await;
    assert_eq!(
        reopened.foreign_origin.as_ref().map(|o| o.tool),
        Some(UiForeignTool::Codex)
    );
}

#[test]
fn discovery_lists_the_current_workspace_only_and_newest_first() {
    leveler_test_support::bounded_test(
        "discovery_lists_the_current_workspace_only_and_newest_first",
        leveler_test_support::DEFAULT_TEST_TIMEOUT,
        discovery_body,
    );
}

async fn discovery_body() {
    let f = fixture();
    let h = Harness::start().await;

    let RuntimeEvent::ForeignSessionList {
        tool,
        sessions,
        error,
    } = h.list(UiForeignTool::ClaudeCode).await
    else {
        unreachable!()
    };
    assert_eq!(tool, UiForeignTool::ClaudeCode);
    assert!(error.is_none(), "{error:?}");
    let ids: Vec<&str> = sessions
        .iter()
        .map(|session| session.native_id.as_str())
        .collect();
    assert_eq!(
        ids,
        vec![CLAUDE_ID, CLAUDE_OLDER_ID],
        "newest first, current workspace only"
    );
    for session in &sessions {
        assert_eq!(session.tool, UiForeignTool::ClaudeCode);
        assert_eq!(session.cwd.as_deref(), Some(f.workspace.to_str().unwrap()));
        assert!(session.size_bytes > 0);
        assert!(!session.title.is_empty());
    }
    assert!(
        !ids.contains(&CLAUDE_ELSEWHERE_ID),
        "a session from another directory is not this workspace's picker entry"
    );
    assert!(
        !ids.contains(&CLAUDE_EMPTY_ID) && !ids.contains(&CLAUDE_INCOMPATIBLE_ID),
        "unreadable files are not offered"
    );

    let RuntimeEvent::ForeignSessionList {
        tool,
        sessions,
        error,
    } = h.list(UiForeignTool::Codex).await
    else {
        unreachable!()
    };
    assert_eq!(tool, UiForeignTool::Codex);
    assert!(error.is_none(), "{error:?}");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].native_id, CODEX_ID);
    assert!(
        sessions[0].title.contains("Fix the failing Codex test"),
        "{}",
        sessions[0].title
    );
    assert_eq!(sessions[0].branch.as_deref(), Some("main"));
}

#[test]
fn a_source_workspace_mismatch_is_reported_before_it_is_trusted() {
    leveler_test_support::bounded_test(
        "a_source_workspace_mismatch_is_reported_before_it_is_trusted",
        leveler_test_support::DEFAULT_TEST_TIMEOUT,
        mismatch_body,
    );
}

async fn mismatch_body() {
    let f = fixture();
    let h = Harness::start().await;
    let outcome = h
        .import(UiForeignTool::ClaudeCode, CLAUDE_ELSEWHERE_ID)
        .await;
    let warning = outcome
        .warning
        .expect("a source from another directory must warn");
    assert!(warning.contains("来源会话的工作目录是"), "{warning}");
    assert!(
        warning.contains(&f.workspace.display().to_string()),
        "{warning}"
    );

    let opened = outcome.opened.expect("the session is still created");
    let briefing = briefing_message(&opened);
    assert!(
        briefing.text.contains("### Workspace mismatch"),
        "{}",
        briefing.text
    );
    assert!(
        briefing.text.contains(ELSEWHERE_REQUEST),
        "{}",
        briefing.text
    );

    let body = wait_for_request(&h.server).await;
    assert!(
        body.contains("Workspace mismatch"),
        "the model is told about the mismatch too: {body}"
    );
    h.wait_for_turn_end(&outcome.session_id).await;
}

#[test]
fn missing_corrupt_and_incompatible_ids_fail_without_creating_a_session() {
    leveler_test_support::bounded_test(
        "missing_corrupt_and_incompatible_ids_fail_without_creating_a_session",
        leveler_test_support::DEFAULT_TEST_TIMEOUT,
        failure_body,
    );
}

async fn failure_body() {
    let h = Harness::start().await;
    let db = h.app.open_database().await.unwrap();
    let list = SessionRepository::new(&db);
    let before = list.list().await.unwrap().len();

    for (id, expected) in [
        (CLAUDE_UNKNOWN_ID, "找不到会话"),
        (CLAUDE_EMPTY_ID, "会话文件已损坏"),
        (CLAUDE_INCOMPATIBLE_ID, "不支持的会话格式"),
        ("../../etc/passwd", "会话 ID 无效"),
    ] {
        let message = h.import_failure(UiForeignTool::ClaudeCode, id).await;
        assert!(
            message.contains(expected),
            "failure for {id} should say {expected:?}, got {message:?}"
        );
        assert!(
            message.contains(id) || id.starts_with(".."),
            "failure names the id it could not use: {message:?}"
        );
    }

    let after = list.list().await.unwrap().len();
    assert_eq!(after, before, "a failed import creates no session");
}

#[test]
fn the_same_source_can_be_imported_twice_as_two_native_sessions() {
    leveler_test_support::bounded_test(
        "the_same_source_can_be_imported_twice_as_two_native_sessions",
        leveler_test_support::DEFAULT_TEST_TIMEOUT,
        reimport_body,
    );
}

async fn reimport_body() {
    let h = Harness::start().await;
    let first = h.import(UiForeignTool::ClaudeCode, CLAUDE_ID).await;
    h.wait_for_turn_end(&first.session_id).await;
    let second = h.import(UiForeignTool::ClaudeCode, CLAUDE_ID).await;
    h.wait_for_turn_end(&second.session_id).await;
    assert_ne!(
        first.session_id, second.session_id,
        "each import is its own native session"
    );

    let db = h.app.open_database().await.unwrap();
    let repo = ForeignSessionOriginRepository::new(&db);
    for id in [&first.session_id, &second.session_id] {
        let record = repo
            .get(id.as_str())
            .await
            .unwrap()
            .expect("each native session keeps its own origin row");
        assert_eq!(record.native_id, CLAUDE_ID);
    }
}
