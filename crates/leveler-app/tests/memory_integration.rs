//! Memory ↔ agent integration, driven through the real in-process runtime.
//!
//! The memory pipeline is opt-in. These tests assemble a real `Application`
//! over a per-test layout whose home also holds the real memory store, point
//! the configured model at a recording mock provider, and assert on what the
//! provider actually received plus what the durable stores actually wrote.
//! Model output is scripted because a provider is the boundary under test;
//! every memory fact, cursor and file assertion reads the runtime's own state.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    ClientCommand, InteractiveRuntimeClient, RuntimeEvent, UiMemoryActionResult,
};
use leveler_core::SessionId;
use leveler_execution::PermissionProfile;
use leveler_memory::{
    CaptureOutcomeDraft, CaptureRange, CaptureStore, ClaimRequest, ManifestBudget, MemoryScope,
    MemoryStorage, ObservationDraft, ObservationType, regenerate_scope_manifest,
};
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_test_support::{MockResponse, MockServer};

/// One config generation for this test binary: memory active, and the two
/// automatic UX calls off so the provider script contains only requests this
/// suite drives. `LEVELER_HOME` is process-wide, so the config is installed
/// once; every test's *memory data* lives under its own `Layout` home.
fn install_config() {
    use std::sync::OnceLock;
    static HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    let dir = HOME.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            r#"
[assist]
prompt_suggestions = false
away_summary = false

[memory]
rollout = "active"
batch_dream_enabled = true
"#,
        )
        .unwrap();
        unsafe {
            std::env::set_var("LEVELER_HOME", dir.path());
        }
        dir
    });
    let _ = dir;
}

fn text_response(content: &str) -> MockResponse {
    let frame = serde_json::json!({
        "choices": [{"delta": {"content": content}, "finish_reason": "stop"}]
    })
    .to_string();
    MockResponse::sse(&[&frame])
}

fn tool_call_response(id: &str, name: &str, arguments: serde_json::Value) -> MockResponse {
    let frame = serde_json::json!({"choices": [{"delta": {"tool_calls": [{
        "index": 0,
        "id": id,
        "function": {"name": name, "arguments": arguments.to_string()}
    }]}}]})
    .to_string();
    let finish =
        serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}).to_string();
    MockResponse::sse(&[&frame, &finish])
}

fn noop_capture() -> MockResponse {
    capture_response(serde_json::json!({"outcome": "noop", "observations": []}))
}

/// A Dream reply is a non-streaming forced tool call, exactly as the host
/// sends it: only `record_memory_plan` crosses the provider boundary.
fn dream_response(operations: serde_json::Value) -> MockResponse {
    forced_tool_json("record_memory_plan", operations)
}

/// Batch Dream answers with raw JSON text, not a tool call.
fn batch_response(plan: serde_json::Value) -> MockResponse {
    MockResponse::SilentThenJson {
        silent_ms: 0,
        body: serde_json::json!({
            "choices": [{
                "message": {"role": "assistant", "content": plan.to_string()},
                "finish_reason": "stop"
            }]
        })
        .to_string(),
    }
}

fn forced_tool_json(name: &str, arguments: serde_json::Value) -> MockResponse {
    MockResponse::SilentThenJson {
        silent_ms: 0,
        body: serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_memory_action",
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": arguments.to_string()
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        })
        .to_string(),
    }
}

fn capture_response(arguments: serde_json::Value) -> MockResponse {
    MockResponse::SilentThenJson {
        silent_ms: 0,
        body: serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_memory_capture",
                        "type": "function",
                        "function": {
                            "name": "record_memory_observations",
                            "arguments": arguments.to_string()
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {
                "prompt_tokens": 120,
                "completion_tokens": 30,
                "prompt_tokens_details": {"cached_tokens": 40}
            }
        })
        .to_string(),
    }
}

fn observation(statement: &str) -> serde_json::Value {
    serde_json::json!({
        "outcome": "observations",
        "observations": [{
            "type": "project",
            "topic_hint": "Testing",
            "statement": statement,
            "keywords": ["fixture"],
            "aliases": [],
            "body": null
        }]
    })
}

struct Harness {
    root: tempfile::TempDir,
    /// The repository root the runtime serves. Usually `root`; a separate
    /// temporary directory for the ephemeral-workspace case.
    workspace: PathBuf,
    /// Keeps a temporary workspace alive for the lifetime of the harness.
    _workspace_keepalive: Option<tempfile::TempDir>,
    servers: Vec<MockServer>,
    app: Arc<Application>,
    client: Arc<InProcessRuntimeClient>,
    session: SessionId,
    models: Vec<ModelRef>,
}

/// One configured provider/model pair pointed at its own recording server.
struct Backend {
    provider: String,
    model: String,
    server: MockServer,
}

async fn harness(responses: Vec<MockResponse>) -> Harness {
    harness_with_backends(vec![Backend {
        provider: "mock".to_owned(),
        model: "m".to_owned(),
        server: MockServer::start(responses).await,
    }])
    .await
}

async fn harness_with_backends(backends: Vec<Backend>) -> Harness {
    let root = fixture_root();
    let workspace = root.path().to_path_buf();
    harness_at(root, workspace, None, backends).await
}

/// A `tempfile::tempdir()` lives under the system temp directory, which the
/// memory store deliberately treats as an ephemeral workspace with no
/// workspace scope. Durable-workspace tests must keep their fixture under
/// Cargo's target tmp directory instead.
fn fixture_root() -> tempfile::TempDir {
    let target_tmp = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(&target_tmp).unwrap();
    tempfile::Builder::new()
        .prefix("memory-integration-")
        .tempdir_in(&target_tmp)
        .unwrap()
}

/// Assemble the real runtime with `root` as home/config/state and `workspace`
/// as the repository root it serves.
async fn harness_at(
    root: tempfile::TempDir,
    workspace: PathBuf,
    workspace_keepalive: Option<tempfile::TempDir>,
    backends: Vec<Backend>,
) -> Harness {
    install_config();
    std::fs::create_dir_all(root.path().join("configs/providers")).unwrap();
    std::fs::create_dir_all(root.path().join("configs/models")).unwrap();
    for backend in &backends {
        std::fs::write(
            root.path()
                .join(format!("configs/providers/{}.yaml", backend.provider)),
            format!(
                "id: {}\nprotocol: openai_chat\nbase_url: {}\n",
                backend.provider,
                backend.server.base_url()
            ),
        )
        .unwrap();
        std::fs::write(
            root.path()
                .join(format!("configs/models/{}.yaml", backend.model)),
            format!(
                r#"
id: {}
provider: {}
model_id: mock-model
protocol: openai_chat
capabilities: {{ streaming: true, tool_calling: true, parallel_tool_calls: false, structured_output: true, reasoning: false, vision: false }}
limits: {{ context_window: 131072, reliable_context: 65536, max_output_tokens: 4096, max_tool_schema_bytes: 65536, max_parallel_tool_calls: 1 }}
compatibility: {{ synthesize_tool_call_ids: true, drop_unsupported_fields: true }}
"#,
                backend.model, backend.provider
            ),
        )
        .unwrap();
    }
    let layout = Layout::from_parts(
        workspace.clone(),
        root.path().join("configs"),
        root.path().join("state"),
    );
    let app = Arc::new(
        Application::assemble(layout)
            .unwrap()
            .with_collaboration(leveler_agent::CollaborationMode::Chat),
    );
    let models: Vec<ModelRef> = backends
        .iter()
        .map(|backend| ModelRef::new(backend.provider.as_str(), backend.model.as_str()))
        .collect();
    let model = primary_model(&backends);
    let session = app
        .create_session(&model, "memory integration")
        .await
        .unwrap();
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        model.clone(),
        PermissionProfile::Assisted,
        false,
    ));
    let servers = backends.into_iter().map(|backend| backend.server).collect();
    Harness {
        root,
        workspace,
        _workspace_keepalive: workspace_keepalive,
        servers,
        app,
        client,
        session,
        models,
    }
}

/// The model ref a backend's config files define.
fn primary_model(backends: &[Backend]) -> ModelRef {
    let backend = backends.first().expect("at least one backend");
    ModelRef::new(backend.provider.as_str(), backend.model.as_str())
}

impl Harness {
    fn server(&self) -> &MockServer {
        &self.servers[0]
    }
    fn workspace(&self) -> &Path {
        &self.workspace
    }

    fn memory_storage(&self) -> MemoryStorage {
        MemoryStorage::for_workspace(self.workspace(), &self.root.path().join("memory"))
    }

    fn workspace_memory(&self) -> PathBuf {
        self.memory_storage().workspace_dir().to_path_buf()
    }

    /// Inbox note paths, sorted; pending observations only.
    fn inbox_notes(&self) -> Vec<PathBuf> {
        let inbox = self.workspace_memory().join("observations/_inbox");
        let Ok(entries) = std::fs::read_dir(inbox) else {
            return Vec::new();
        };
        let mut notes: Vec<_> = entries.map(|entry| entry.unwrap().path()).collect();
        notes.sort();
        notes
    }

    /// The deterministic inbox path a capture of `from..=through` uses.
    fn observation_path(&self, from: u32, through: u32, ordinal: u32) -> String {
        format!(
            "observations/_inbox/{}__t{from:06}-{through:06}__n{ordinal:03}.md",
            self.session.as_str().to_ascii_lowercase()
        )
    }

    async fn submit(&self, content: &str) -> RuntimeEvent {
        self.submit_for(&self.session, content).await
    }

    /// Submit through the client that owns `session` and wait for its
    /// terminal. The in-process client routes by the session id in the
    /// command, so one runtime can drive several configured models.
    async fn submit_for(&self, session: &SessionId, content: &str) -> RuntimeEvent {
        let mut rx = self.client.subscribe();
        self.client
            .send(ClientCommand::SubmitMessage {
                session_id: session.clone(),
                content: content.to_owned(),
                attachments: vec![],
            })
            .await
            .unwrap();
        wait_for_turn_end(&mut rx).await
    }

    fn read(&self, relative: &str) -> String {
        std::fs::read_to_string(self.workspace_memory().join(relative)).unwrap_or_else(|error| {
            panic!("could not read {relative}: {error}");
        })
    }

    /// Wait until `predicate` holds on this session's activity.
    async fn wait_for_activity(
        &self,
        session: &SessionId,
        predicate: impl Fn(&leveler_app::memory::MemoryActivity) -> bool,
    ) -> leveler_app::memory::MemoryActivity {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let activity = self.app.memory().activity(session.as_str());
            if predicate(&activity) {
                return activity;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "activity never matched: {activity:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Wait until the session's capture worker has settled its first range.
    async fn wait_for_capture(&self) {
        self.wait_for_capture_of(&self.session).await;
    }

    /// Wait until the session's activity reports this exact settled range.
    /// `wait_for_capture` is enough for a session's first turn; later turns
    /// need the range, because the previous range remains visible.
    async fn wait_for_capture_range(&self, session: &SessionId, range: (u32, u32)) {
        self.wait_for_activity(session, |activity| {
            activity.last_captured_range == Some(range) && !activity.active
        })
        .await;
    }

    async fn wait_for_capture_of(&self, session: &SessionId) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let activity = self.app.memory().activity(session.as_str());
            if activity.last_captured_range.is_some() && !activity.active {
                return;
            }
            if activity.kind == leveler_app::memory::MemoryActivityKind::Failed && !activity.active
            {
                panic!("capture failed: {:?}", activity.last_error);
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("capture never settled: {activity:?}");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Wait for a memory management action and return its typed result.
    async fn action(&self, command: ClientCommand) -> UiMemoryActionResult {
        let mut rx = self.client.subscribe();
        self.client.send(command).await.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(left, rx.recv()).await {
                Ok(Ok(RuntimeEvent::MemoryActionFinished { result, .. })) => return result,
                Ok(Ok(_)) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
                Ok(Err(_)) => panic!("memory action stream closed"),
                Err(_) => panic!("memory action never finished"),
            }
        }
    }

    /// Send a command and wait for the first event matching `predicate`.
    async fn command_event(
        &self,
        command: ClientCommand,
        predicate: impl Fn(&RuntimeEvent) -> bool,
    ) -> RuntimeEvent {
        let mut rx = self.client.subscribe();
        self.client.send(command).await.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(left, rx.recv()).await {
                Ok(Ok(event)) => {
                    if predicate(&event) {
                        return event;
                    }
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
                Ok(Err(_)) => panic!("event stream closed"),
                Err(_) => panic!("the expected event never arrived"),
            }
        }
    }

    /// Fork the primary session and return the new session id from the
    /// runtime's own session list.
    async fn fork(&self) -> SessionId {
        let original = self.session.clone();
        let event = self
            .command_event(
                ClientCommand::ForkSession {
                    session_id: original.clone(),
                },
                |event| matches!(event, RuntimeEvent::SessionList { .. }),
            )
            .await;
        let RuntimeEvent::SessionList { sessions } = event else {
            unreachable!("the predicate selected a session list");
        };
        let fork = sessions
            .iter()
            .find(|summary| summary.id != original)
            .expect("the fork appears in the runtime's listing");
        assert!(
            fork.goal.contains("分叉"),
            "the fork names its origin: {}",
            fork.goal
        );
        fork.id.clone()
    }

    async fn main_bodies(&self) -> Vec<serde_json::Value> {
        self.server()
            .request_bodies()
            .await
            .into_iter()
            .filter(|body| body.contains("Cross-session memory"))
            .map(|body| serde_json::from_str(&body).unwrap())
            .collect()
    }

    async fn capture_bodies(&self) -> Vec<serde_json::Value> {
        self.server()
            .request_bodies()
            .await
            .into_iter()
            .filter(|body| body.contains("record_memory_observations"))
            .map(|body| serde_json::from_str(&body).unwrap())
            .collect()
    }
}

async fn wait_for_turn_end(
    rx: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>,
) -> RuntimeEvent {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Ok(event)) => match event {
                RuntimeEvent::TurnCompleted
                | RuntimeEvent::TurnCompletedWithWarnings { .. }
                | RuntimeEvent::TurnAnswered
                | RuntimeEvent::TurnTruncated { .. }
                | RuntimeEvent::TurnIncomplete { .. }
                | RuntimeEvent::TurnFailed { .. }
                | RuntimeEvent::TurnCancelled => return event,
                _ => continue,
            },
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            _ => panic!("the turn never settled"),
        }
    }
}

fn serialized(body: &serde_json::Value) -> String {
    serde_json::to_string(body).unwrap()
}

fn seed_topic(harness: &Harness, relative: &str, content: &str) {
    let storage = harness.memory_storage();
    storage.ensure_initialized().unwrap();
    let path = storage.workspace_dir().join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, content).unwrap();
    regenerate_scope_manifest(
        storage.workspace_dir(),
        MemoryScope::Workspace,
        ManifestBudget::default(),
    )
    .unwrap();
}

/// Commit one observation straight into the pending inbox and return its
/// workspace-relative path. This drives the Dream boundary deterministically
/// without depending on a scripted capture call.
fn seed_observation(
    harness: &Harness,
    session: &str,
    range: CaptureRange,
    statement: &str,
    created_at: i64,
) -> String {
    let storage = harness.memory_storage();
    storage.ensure_initialized().unwrap();
    let store = CaptureStore::open(storage.workspace_dir(), MemoryScope::Workspace).unwrap();
    store.enqueue(session, range).unwrap();
    let lease = store
        .claim_for_session(
            session,
            &ClaimRequest {
                owner: "seed".to_owned(),
                now: created_at,
                duration: Duration::from_secs(3600),
            },
        )
        .unwrap()
        .expect("the seeded job is claimable");
    let committed = store
        .commit(
            &lease,
            &CaptureOutcomeDraft::Observations(vec![ObservationDraft {
                observation_type: ObservationType::Project,
                topic_hint: Some("Testing".to_owned()),
                statement: statement.to_owned(),
                keywords: vec!["fixture".to_owned()],
                aliases: vec![],
                extraction_model: "seed".to_owned(),
                prompt_version: "memory-v2-capture-1".to_owned(),
                created_at,
                body: None,
            }]),
            created_at + 1,
        )
        .unwrap();
    committed.files[0]
        .strip_prefix(storage.workspace_dir())
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| committed.files[0].clone())
        .to_string_lossy()
        .replace('\\', "/")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn active_memory_injects_a_stable_index_once_per_turn() {
    let harness = harness(vec![
        text_response("first answer"),
        noop_capture(),
        text_response("second answer"),
        noop_capture(),
    ])
    .await;
    harness.submit("first question").await;
    harness.wait_for_capture().await;
    harness.submit("second question").await;
    harness.wait_for_capture().await;

    let bodies = harness.main_bodies().await;
    assert_eq!(bodies.len(), 2, "each turn sends one main request");
    let mut indexes = Vec::new();
    for body in &bodies {
        let text = serialized(body);
        assert_eq!(
            text.matches("## Cross-session memory").count(),
            1,
            "the bounded memory index is injected exactly once per turn"
        );
        let start = text.find("## Cross-session memory").unwrap();
        let end = text[start..]
            .find("### Workspace memory")
            .map(|offset| start + offset)
            .expect("workspace section");
        // The preamble is byte-stable; the two store bodies are stable too
        // because the capture no-ops changed no memory.
        indexes.push(text[start..end].to_owned());
    }
    assert_eq!(
        indexes[0], indexes[1],
        "an unchanged store must send identical index bytes on the next turn"
    );
    assert!(
        serialized(&bodies[1]).contains("### Global memory"),
        "both scopes are advertised"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_seeded_topic_is_listed_and_readable_by_the_model() {
    let harness = harness(vec![]).await;
    let topic = "# Build workflow\n\nHow this repository is verified.\n\n## Commands\n- Run `cargo test -p fixture` before committing.\n";
    seed_topic(&harness, "topics/build-workflow.md", topic);
    let path = harness.workspace_memory().join("topics/build-workflow.md");
    harness
        .server()
        .push(tool_call_response(
            "call_read_topic",
            "read_file",
            serde_json::json!({"path": path}),
        ))
        .await;
    harness.server().push(text_response("used the topic")).await;
    harness.server().push(noop_capture()).await;

    harness.submit("how do I verify this repository?").await;
    harness.wait_for_capture().await;

    let bodies = harness.main_bodies().await;
    let first = serialized(&bodies[0]);
    assert!(
        first.contains("topics/build-workflow.md"),
        "the manifest lists the seeded topic: {first}"
    );
    assert!(
        first.contains("How this repository is verified."),
        "the index carries the topic description: {first}"
    );
    let last = serialized(bodies.last().unwrap());
    assert!(
        last.contains("Run `cargo test -p fixture` before committing."),
        "the read_file result returned the real topic body: {last}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_remember_survives_into_a_new_session() {
    let harness = harness(vec![text_response("first"), noop_capture()]).await;
    let result = harness
        .action(ClientCommand::RememberMemory {
            session_id: harness.session.clone(),
            query_id: None,
            content: "The team ships on Fridays.".into(),
        })
        .await;
    assert!(result.ok, "remember failed: {result:?}");
    let global = harness.memory_storage().global_dir().to_path_buf();
    let notes: Vec<_> = std::fs::read_dir(global.join("observations/_inbox"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(notes.len(), 1, "one explicit note was published");
    let note = std::fs::read_to_string(&notes[0]).unwrap();
    assert!(note.contains("The team ships on Fridays."));

    // A fresh session asks a related question and is told nothing directly.
    let second = harness
        .app
        .create_session(&harness.models[0], "second session")
        .await
        .unwrap();
    let client = Arc::new(InProcessRuntimeClient::new(
        harness.app.clone(),
        harness.models[0].clone(),
        PermissionProfile::Assisted,
        false,
    ));
    let mut rx = client.subscribe();
    client
        .send(ClientCommand::SubmitMessage {
            session_id: second.clone(),
            content: "when do shipments go out?".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    let _ = wait_for_turn_end(&mut rx).await;
    let bodies = harness.main_bodies().await;
    let newest = serialized(bodies.last().expect("the second session's request"));
    assert!(
        newest.contains("The team ships on Fridays."),
        "the explicit note is available to a later session through the index: {newest}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_settled_turn_is_captured_into_a_durable_observation() {
    let harness = harness(vec![
        text_response("the fixture uses cargo"),
        capture_response(observation("The fixture verifies with cargo test.")),
    ])
    .await;
    harness.submit("which tool verifies the fixture?").await;
    harness.wait_for_capture().await;

    let storage = harness.memory_storage();
    let inbox = storage.workspace_dir().join("observations/_inbox");
    let notes: Vec<_> = std::fs::read_dir(&inbox)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(notes.len(), 1, "one observation was committed");
    let rendered = std::fs::read_to_string(&notes[0]).unwrap();
    assert!(rendered.contains("schema_version: 2"));
    assert!(rendered.contains(&format!("session_id: \"{}\"", harness.session.as_str())));
    assert!(rendered.contains("from_turn: 1"));
    assert!(rendered.contains("through_turn: 1"));
    assert!(rendered.contains("extraction_model: \"m\""));
    assert!(rendered.contains("# The fixture verifies with cargo test."));
    assert!(
        harness.workspace_memory().join("MEMORY.md").exists(),
        "the capture commit keeps the manifest in step"
    );
    assert!(
        !harness.capture_bodies().await.is_empty(),
        "capture went through the real provider"
    );
    let records =
        leveler_storage::ModelRequestRepository::new(&harness.app.open_database().await.unwrap())
            .load_for_session(&harness.session)
            .await
            .unwrap();
    let capture = records
        .iter()
        .find(|record| record.kind == leveler_storage::ModelCallKind::MemoryCapture)
        .expect("capture usage is recorded under its own call kind");
    assert_eq!(capture.input_tokens, 120);
    assert_eq!(capture.output_tokens, 30);
    assert_eq!(capture.cached_input_tokens, Some(40));
    assert_eq!(capture.agent_id, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_manual_dream_consolidates_captured_observations_into_a_topic() {
    // The inbox path is deterministic from the session id and turn range, so
    // the Dream evidence can be scripted before the capture runs.
    let harness = harness(vec![]).await;
    let evidence = harness.observation_path(1, 1, 0);
    for response in [
        text_response("the fixture uses cargo"),
        capture_response(observation("The fixture verifies with cargo test.")),
        dream_response(serde_json::json!({"operations": [{
            "op": "create",
            "path": "topics/testing.md",
            "content": "# Testing\nHow the fixture is verified.\n\n## Commands\n- Run `cargo test -p fixture`.",
            "evidence": [evidence]
        }]})),
        text_response("second answer"),
        noop_capture(),
    ] {
        harness.server().push(response).await;
    }

    harness.submit("which tool verifies the fixture?").await;
    harness.wait_for_capture().await;
    assert_eq!(
        harness.inbox_notes().len(),
        1,
        "one observation is pending before the Dream"
    );
    assert_eq!(
        harness.inbox_notes()[0]
            .file_name()
            .unwrap()
            .to_string_lossy(),
        evidence.rsplit('/').next().unwrap(),
        "the scripted evidence names the observation the capture wrote"
    );
    let result = harness
        .action(ClientCommand::RunMemoryDream {
            session_id: harness.session.clone(),
            query_id: None,
            batch: false,
        })
        .await;
    assert!(result.ok, "manual Dream failed: {result:?}");
    assert!(
        result.message.contains("Dream consolidated 1 observations"),
        "the outcome reports the mechanical result: {result:?}"
    );
    let topic = harness.workspace_memory().join("topics/testing.md");
    let rendered = std::fs::read_to_string(&topic).expect("the Dream created the topic");
    assert!(rendered.contains("# Testing"));
    assert!(rendered.contains("cargo test -p fixture"));
    assert!(
        harness.inbox_notes().is_empty(),
        "consolidated observations leave the inbox"
    );

    // The next turn's index carries the consolidated topic.
    harness.submit("what verifies the fixture?").await;
    harness.wait_for_capture().await;
    let bodies = harness.main_bodies().await;
    let last = serialized(bodies.last().unwrap());
    assert!(
        last.contains("topics/testing.md"),
        "the consolidated topic is listed in the next turn's index: {last}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_dream_settles_each_note_with_typed_edits() {
    let harness = harness(vec![]).await;
    for response in [
        text_response("the fixture uses cargo"),
        capture_response(observation("The fixture verifies with cargo test.")),
        batch_response(serde_json::json!({
            "actions": null,
            "plan": {
                "edits": [{
                    "kind": "create",
                    "id": "E1",
                    "path": "topics/testing.md",
                    "content": "# Testing\nHow the fixture is verified.\n\n## Commands\n- Run `cargo test -p fixture`."
                }],
                "outcomes": [{
                    "outcome": "applied",
                    "note": "N1",
                    "edits": ["E1"]
                }]
            }
        })),
        text_response("second answer"),
        noop_capture(),
    ] {
        harness.server().push(response).await;
    }

    harness.submit("which tool verifies the fixture?").await;
    harness.wait_for_capture().await;
    assert_eq!(harness.inbox_notes().len(), 1, "one note is pending");
    let result = harness
        .action(ClientCommand::RunMemoryDream {
            session_id: harness.session.clone(),
            query_id: None,
            batch: true,
        })
        .await;
    assert!(result.ok, "batch Dream failed: {result:?}");
    assert!(
        result.message.contains("Batch Dream settled 1 notes"),
        "the outcome reports the settled notes: {result:?}"
    );
    let rendered = std::fs::read_to_string(harness.workspace_memory().join("topics/testing.md"))
        .expect("the batch plan created the topic");
    assert!(rendered.contains("# Testing"));
    assert!(
        harness.inbox_notes().is_empty(),
        "settled notes leave the inbox"
    );

    harness.submit("what verifies the fixture?").await;
    harness.wait_for_capture().await;
    let bodies = harness.main_bodies().await;
    assert!(
        serialized(bodies.last().unwrap()).contains("topics/testing.md"),
        "the next turn's index carries the batch-created topic"
    );
}

/// Poll until the path exists (an automatic Dream runs off the turn path).
async fn wait_for_file(path: &Path) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !path.exists() {
        if tokio::time::Instant::now() >= deadline {
            panic!("{} was never created", path.display());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_automatic_dream_runs_when_the_oldest_pending_observation_is_old() {
    let harness = harness(vec![]).await;
    // Seed one observation committed more than a day ago under another
    // session. The next capture's follow-up sees the age threshold satisfied.
    let storage = harness.memory_storage();
    storage.ensure_initialized().unwrap();
    let store = CaptureStore::open(storage.workspace_dir(), MemoryScope::Workspace).unwrap();
    let seed_session = leveler_core::new_uuid_string();
    let old_now = leveler_core::now().timestamp() - 25 * 60 * 60;
    store
        .enqueue(&seed_session, CaptureRange::try_new(1, 1).unwrap())
        .unwrap();
    let lease = store
        .claim_for_session(
            &seed_session,
            &ClaimRequest {
                owner: "seed".to_owned(),
                now: old_now,
                duration: Duration::from_secs(3600),
            },
        )
        .unwrap()
        .expect("the seeded job is claimable");
    let committed = store
        .commit(
            &lease,
            &CaptureOutcomeDraft::Observations(vec![ObservationDraft {
                observation_type: ObservationType::Project,
                topic_hint: Some("Testing".to_owned()),
                statement: "An older note about the fixture.".to_owned(),
                keywords: vec!["older".to_owned()],
                aliases: vec![],
                extraction_model: "seed".to_owned(),
                prompt_version: "memory-v2-capture-1".to_owned(),
                created_at: old_now,
                body: None,
            }]),
            old_now + 1,
        )
        .unwrap();
    let old_evidence = committed.files[0]
        .strip_prefix(storage.workspace_dir())
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| committed.files[0].clone())
        .to_string_lossy()
        .replace('\\', "/");

    for response in [
        text_response("the fixture uses cargo"),
        capture_response(observation("The fixture verifies with cargo test.")),
        dream_response(serde_json::json!({"operations": [{
            "op": "create",
            "path": "topics/testing.md",
            "content": "# Testing\nHow the fixture is verified.",
            "evidence": [old_evidence]
        }]})),
    ] {
        harness.server().push(response).await;
    }
    harness.submit("which tool verifies the fixture?").await;
    // The capture worker's follow-up runs the automatic Dream before it
    // settles its activity; the topic is the durable evidence either way.
    let topic = harness.workspace_memory().join("topics/testing.md");
    wait_for_file(&topic).await;
    assert!(
        std::fs::read_to_string(&topic)
            .unwrap()
            .contains("How the fixture is verified."),
        "the automatic Dream committed the planned topic"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_capture_provider_failure_is_retried_and_never_breaks_the_turn() {
    let harness = harness(vec![
        text_response("the answer stands"),
        MockResponse::Status {
            code: 400,
            body: r#"{"error":{"message":"bad request"}}"#.to_owned(),
        },
        noop_capture(),
    ])
    .await;
    let _ = harness.submit("answer anyway").await;
    // The first extraction fails and the same job is retried; wait for the
    // settled range instead of treating the intermediate failure as final.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let activity = harness.app.memory().activity(harness.session.as_str());
        if activity.last_captured_range.is_some() && !activity.active {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "capture never settled after the retry: {activity:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let bodies = harness.capture_bodies().await;
    assert_eq!(
        bodies.len(),
        2,
        "the failed extraction was retried once: {}",
        bodies.len()
    );
    assert!(
        harness.inbox_notes().is_empty(),
        "the retry settled on a no-op, not a fabricated observation"
    );
    let activity = harness.app.memory().activity(harness.session.as_str());
    assert!(
        activity.last_error.is_some(),
        "the provider failure stays visible in the activity: {activity:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dream_provider_failure_keeps_the_observations_pending() {
    let harness = harness(vec![
        text_response("the fixture uses cargo"),
        capture_response(observation("The fixture verifies with cargo test.")),
        MockResponse::Status {
            code: 400,
            body: r#"{"error":{"message":"bad request"}}"#.to_owned(),
        },
    ])
    .await;
    harness.submit("which tool verifies the fixture?").await;
    harness.wait_for_capture().await;
    let result = harness
        .action(ClientCommand::RunMemoryDream {
            session_id: harness.session.clone(),
            query_id: None,
            batch: false,
        })
        .await;
    assert!(
        !result.ok,
        "a provider failure cannot report success: {result:?}"
    );
    assert!(
        result.message.contains("Dream failed"),
        "the failure names the Dream: {result:?}"
    );
    assert_eq!(
        harness.inbox_notes().len(),
        1,
        "the unconsolidated observation stays pending for a retry"
    );
    assert!(
        !harness
            .workspace_memory()
            .join("topics/testing.md")
            .exists(),
        "a failed Dream commits nothing"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_corrupt_memory_database_degrades_the_agent_without_breaking_it() {
    let harness = harness(vec![text_response("the answer stands")]).await;
    let storage = harness.memory_storage();
    storage.ensure_initialized().unwrap();
    let state = storage.workspace_dir().join("memory_state.sqlite");
    for suffix in ["-wal", "-shm"] {
        let sidecar = state.with_file_name(format!("memory_state.sqlite{suffix}"));
        let _ = std::fs::remove_file(sidecar);
    }
    std::fs::write(&state, b"this is not a sqlite database").unwrap();
    assert!(
        CaptureStore::open(storage.workspace_dir(), MemoryScope::Workspace).is_err(),
        "the fixture corruption is visible to the store"
    );
    let reinit = storage.ensure_initialized();
    assert!(
        reinit.is_err(),
        "a corrupt state database cannot be re-initialized: {reinit:?}"
    );
    assert!(
        harness.app.memory().config().can_capture(),
        "the fixture rollout captures"
    );

    let terminal = harness.submit("answer anyway").await;
    let bodies = harness.server().request_bodies().await;
    assert_eq!(
        bodies.len(),
        1,
        "the turn still reached the model; terminal event: {terminal:?}"
    );
    assert!(
        !bodies[0].contains("## Cross-session memory"),
        "a store that cannot be read is not advertised as memory"
    );
    // Memory runs after the turn's terminal event; wait for its settled
    // activity instead of assuming it already recorded the failure.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let activity = harness.app.memory().activity(harness.session.as_str());
        if activity.kind == leveler_app::memory::MemoryActivityKind::Failed && !activity.active {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the store failure was never surfaced: {activity:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        harness.server().request_count(),
        1,
        "no capture call is made against an unreadable store"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_conflicting_fact_is_resolved_only_through_claimed_evidence() {
    let harness = harness(vec![]).await;
    seed_topic(
        &harness,
        "topics/build.md",
        "# Build\nHow the fixture is built.\n\n## Commands\n- Run `cargo build --release`.\n",
    );
    let evidence = seed_observation(
        &harness,
        "conflict-seed",
        CaptureRange::try_new(1, 1).unwrap(),
        "The fixture now builds with `just build`.",
        leveler_core::now().timestamp(),
    );

    // A plan that claims a re-verification it never performed cites an inbox
    // path the claim does not own. The host refuses it and changes no topic.
    for response in [
        dream_response(serde_json::json!({"operations": [{
            "op": "update",
            "path": "topics/build.md",
            "content": "# Build\nHow the fixture is built.\n\n## Commands\n- Run `just build`.\n",
            "evidence": [evidence.clone(), "observations/_inbox/not-claimed.md"]
        }]})),
        dream_response(serde_json::json!({"operations": [{
            "op": "update",
            "path": "topics/build.md",
            "content": "# Build\nHow the fixture is built.\n\n## Commands\n- Run `just build`.\n",
            "evidence": [evidence]
        }]})),
    ] {
        harness.server().push(response).await;
    }

    let rejected = harness
        .action(ClientCommand::RunMemoryDream {
            session_id: harness.session.clone(),
            query_id: None,
            batch: false,
        })
        .await;
    assert!(
        !rejected.ok,
        "unclaimed evidence must not authorize a topic rewrite: {rejected:?}"
    );
    assert!(
        rejected.message.contains("Dream failed"),
        "the rejection is reported: {rejected:?}"
    );
    assert!(
        harness
            .read("topics/build.md")
            .contains("cargo build --release"),
        "a rejected plan leaves the existing fact untouched"
    );
    assert_eq!(
        harness.inbox_notes().len(),
        1,
        "the observation stays pending after the rejected plan"
    );

    // The same resolution citing the real claimed observation is committed.
    let applied = harness
        .action(ClientCommand::RunMemoryDream {
            session_id: harness.session.clone(),
            query_id: None,
            batch: false,
        })
        .await;
    assert!(applied.ok, "the valid plan failed: {applied:?}");
    let rendered = harness.read("topics/build.md");
    assert!(rendered.contains("just build"));
    assert!(
        !rendered.contains("cargo build --release"),
        "the contradicted fact is gone: {rendered}"
    );
    assert!(
        harness.inbox_notes().is_empty(),
        "the consolidated observation leaves the inbox"
    );
}

/// Reassemble an application over the same layout and session database, as a
/// process restart would. The caller must have dropped the previous app so
/// its workers cannot keep writing.
fn assemble_again(
    root: &Path,
    model: &ModelRef,
) -> (Arc<Application>, Arc<InProcessRuntimeClient>) {
    let layout = Layout::from_parts(root.to_path_buf(), root.join("configs"), root.join("state"));
    let app = Arc::new(
        Application::assemble(layout)
            .unwrap()
            .with_collaboration(leveler_agent::CollaborationMode::Chat),
    );
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        model.clone(),
        PermissionProfile::Assisted,
        false,
    ));
    (app, client)
}

/// Drop the application and wait until its cancelled capture worker released
/// the durable job it was holding.
fn stop_runtime(app: Arc<Application>, client: Arc<InProcessRuntimeClient>) {
    drop(client);
    let app = Arc::try_unwrap(app)
        .ok()
        .expect("the test owns the only application handle");
    drop(app);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restarted_runtime_resumes_the_capture_cursor_without_recapturing() {
    let harness = harness(vec![
        text_response("first answer"),
        capture_response(observation("The fixture verifies with cargo test.")),
        text_response("second answer"),
        noop_capture(),
    ])
    .await;
    harness.submit("first question").await;
    harness.wait_for_capture().await;
    let first_note = harness.inbox_notes()[0]
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert!(
        first_note.contains("__t000001-000001__"),
        "the first turn was captured exactly once: {first_note}"
    );

    let Harness {
        root,
        servers,
        app,
        client,
        session,
        models,
        ..
    } = harness;
    stop_runtime(app, client);
    servers[0].clear().await;
    servers[0].push(text_response("answer after restart")).await;
    servers[0]
        .push(capture_response(observation(
            "The restarted session verified with cargo test.",
        )))
        .await;

    let (app, client) = assemble_again(root.path(), &models[0]);
    let mut rx = client.subscribe();
    client
        .send(ClientCommand::ResumeTask {
            session_id: session.clone(),
            content: "continue after restart".to_owned(),
        })
        .await
        .unwrap();
    let terminal = wait_for_turn_end(&mut rx).await;
    assert!(
        matches!(
            terminal,
            RuntimeEvent::TurnAnswered | RuntimeEvent::TurnCompleted
        ),
        "the resumed session runs: {terminal:?}"
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let activity = app.memory().activity(session.as_str());
        if activity.last_captured_range == Some((2, 2)) && !activity.active {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the post-restart capture never settled: {activity:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let memory = MemoryStorage::for_workspace(root.path(), &root.path().join("memory"));
    let inbox = memory.workspace_dir().join("observations/_inbox");
    let mut notes: Vec<String> = std::fs::read_dir(&inbox)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    notes.sort();
    assert_eq!(
        notes.len(),
        2,
        "one note per captured range, with no re-capture of turn 1: {notes:?}"
    );
    assert!(notes[0].contains("__t000001-000001__"));
    assert!(notes[1].contains("__t000002-000002__"));
    let bodies = servers[0].request_bodies().await;
    let main = bodies
        .iter()
        .filter(|body| body.contains("Cross-session memory"))
        .count();
    assert_eq!(
        main, 2,
        "the restarted runtime rebuilds the memory index for its first turn"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_capture_interrupted_by_restart_is_recovered_from_its_durable_job() {
    let harness = harness(vec![
        text_response("first answer"),
        MockResponse::NeverResponds,
        text_response("second answer"),
    ])
    .await;
    harness.submit("first question").await;
    // The capture is in flight, blocked on a provider that never answers.
    harness
        .wait_for_activity(&harness.session, |activity| {
            activity.kind == leveler_app::memory::MemoryActivityKind::Capturing && activity.active
        })
        .await;

    let Harness {
        root,
        servers,
        app,
        client,
        session,
        models,
        ..
    } = harness;
    stop_runtime(app, client);
    // Wait until the cancelled worker released its lease; otherwise the new
    // runtime sees a live lease and legitimately waits for it to expire.
    let storage = MemoryStorage::for_workspace(root.path(), &root.path().join("memory"));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let store = CaptureStore::open(storage.workspace_dir(), MemoryScope::Workspace).unwrap();
        let work = store.work_state(session.as_str(), 1).unwrap();
        if work.running == 0 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the interrupted capture lease was never released: {work:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    servers[0].clear().await;
    servers[0].push(text_response("answer after restart")).await;
    // The new turn's range is pending and is claimed first; the failed range
    // from the interrupted process is claimed next.
    servers[0].push(noop_capture()).await;
    servers[0]
        .push(capture_response(observation(
            "Recovered the observation from the interrupted capture.",
        )))
        .await;

    let (app, client) = assemble_again(root.path(), &models[0]);
    let mut rx = client.subscribe();
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "second question".to_owned(),
            attachments: vec![],
        })
        .await
        .unwrap();
    let terminal = wait_for_turn_end(&mut rx).await;
    assert!(
        !matches!(terminal, RuntimeEvent::TurnFailed { .. }),
        "{terminal:?}"
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let activity = app.memory().activity(session.as_str());
        let work = {
            let store =
                CaptureStore::open(storage.workspace_dir(), MemoryScope::Workspace).unwrap();
            store.work_state(session.as_str(), 2).unwrap()
        };
        if !activity.active && work.pending == 0 && work.running == 0 && work.failed == 0 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "recovery never settled: activity={activity:?} work={work:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let inbox = storage.workspace_dir().join("observations/_inbox");
    let mut notes: Vec<String> = std::fs::read_dir(&inbox)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    notes.sort();
    assert_eq!(
        notes.len(),
        1,
        "the interrupted range produced its observation exactly once: {notes:?}"
    );
    assert!(
        notes[0].contains("__t000001-000001__"),
        "the recovered note belongs to the interrupted range: {notes:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forked_session_keeps_memory_but_captures_only_its_own_turns() {
    let harness = harness(vec![
        text_response("first answer"),
        capture_response(observation("The fixture verifies with cargo test.")),
        text_response("fork answer"),
        capture_response(observation("The fork verified with cargo test.")),
    ])
    .await;
    seed_topic(
        &harness,
        "topics/testing.md",
        "# Testing\nHow the fixture is verified.\n\n## Commands\n- Run `cargo test`.\n",
    );
    harness.submit("first question").await;
    harness.wait_for_capture().await;
    let original_notes = harness.inbox_notes().len();
    assert_eq!(original_notes, 1);

    let fork = harness.fork().await;
    harness.submit_for(&fork, "fork question").await;
    harness.wait_for_capture_of(&fork).await;

    let fork_bodies = harness.main_bodies().await;
    let fork_body = serialized(fork_bodies.last().unwrap());
    assert!(
        fork_body.contains("topics/testing.md"),
        "the fork's first turn is served workspace memory: {fork_body}"
    );

    let inbox = harness.workspace_memory().join("observations/_inbox");
    let mut notes: Vec<String> = std::fs::read_dir(&inbox)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    notes.sort();
    assert_eq!(notes.len(), 2, "the fork captures its own turn: {notes:?}");
    let fork_slug = fork.as_str().to_ascii_lowercase();
    assert!(
        notes
            .iter()
            .any(|note| note.contains(&fork_slug) && note.contains("__t000001-000001__")),
        "the fork starts its own cursor at its first turn: {notes:?}"
    );
    assert!(
        notes
            .iter()
            .any(|note| note.contains("__t000001-000001__") && !note.contains(&fork_slug)),
        "the original session's note is untouched: {notes:?}"
    );

    let original = harness.app.memory().activity(harness.session.as_str());
    assert_eq!(original.last_captured_range, Some((1, 1)));
    let forked = harness.app.memory().activity(fork.as_str());
    assert_eq!(forked.last_captured_range, Some((1, 1)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compaction_does_not_reset_memory_injection_or_capture_progress() {
    let summary = MockResponse::SilentThenJson {
        silent_ms: 0,
        body: serde_json::json!({
            "choices": [{
                "message": {"role": "assistant", "content": "Summary of the fixture conversation."},
                "finish_reason": "stop"
            }]
        })
        .to_string(),
    };
    let harness = harness(vec![
        text_response("first answer"),
        capture_response(observation("The fixture verifies with cargo test.")),
        text_response("second answer"),
        capture_response(observation("The second turn verified with cargo test.")),
        summary,
        text_response("third answer"),
        capture_response(observation("The third turn verified with cargo test.")),
    ])
    .await;
    seed_topic(
        &harness,
        "topics/testing.md",
        "# Testing\nHow the fixture is verified.\n\n## Commands\n- Run `cargo test`.\n",
    );
    harness.submit("first question").await;
    harness.wait_for_capture().await;
    harness.submit("second question").await;
    harness
        .wait_for_capture_range(&harness.session, (2, 2))
        .await;

    let compacted = harness
        .command_event(
            ClientCommand::CompactContext {
                session_id: harness.session.clone(),
            },
            |event| {
                matches!(
                    event,
                    RuntimeEvent::ContextCompacted { .. }
                        | RuntimeEvent::TurnFailed { .. }
                        | RuntimeEvent::Notification { .. }
                )
            },
        )
        .await;
    assert!(
        matches!(compacted, RuntimeEvent::ContextCompacted { .. }),
        "{compacted:?}"
    );

    harness.submit("third question").await;
    harness
        .wait_for_capture_range(&harness.session, (3, 3))
        .await;

    let bodies = harness.main_bodies().await;
    let last = serialized(bodies.last().unwrap());
    assert!(
        last.contains("topics/testing.md"),
        "memory survives the transcript rewrite: {last}"
    );
    let activity = harness.app.memory().activity(harness.session.as_str());
    assert_eq!(
        activity.last_captured_range,
        Some((3, 3)),
        "compaction does not move the capture cursor backwards"
    );
    let inbox = harness.workspace_memory().join("observations/_inbox");
    let mut notes: Vec<String> = std::fs::read_dir(&inbox)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    notes.sort();
    assert_eq!(
        notes.len(),
        3,
        "each turn is captured once across the compaction: {notes:?}"
    );
    assert!(notes[0].contains("__t000001-000001__"));
    assert!(notes[1].contains("__t000002-000002__"));
    assert!(notes[2].contains("__t000003-000003__"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_interrupted_turn_settles_its_capture_range_without_inventing_memory() {
    let harness = harness(vec![
        text_response("first answer"),
        capture_response(observation("The fixture verifies with cargo test.")),
        MockResponse::NeverResponds,
        noop_capture(),
        text_response("third answer"),
        noop_capture(),
    ])
    .await;
    harness.submit("first question").await;
    harness.wait_for_capture().await;

    let mut rx = harness.client.subscribe();
    harness
        .client
        .send(ClientCommand::SubmitMessage {
            session_id: harness.session.clone(),
            content: "this turn will be interrupted".to_owned(),
            attachments: vec![],
        })
        .await
        .unwrap();
    {
        let db = harness.app.open_database().await.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let turns = leveler_storage::TurnRepository::new(&db)
                .list(&harness.session)
                .await
                .unwrap();
            // The cancel must interrupt a turn whose request already reached
            // the provider; otherwise the scripted hang is still queued for
            // the capture that follows the cancelled turn.
            if turns.iter().any(|turn| turn.status == "running")
                && harness.server().request_count() >= 3
            {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the second turn never reached the provider: {turns:?} requests={}",
                harness.server().request_count()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    harness
        .client
        .send(ClientCommand::CancelCurrentTurn {
            session_id: harness.session.clone(),
        })
        .await
        .unwrap();
    let terminal = wait_for_turn_end(&mut rx).await;
    assert!(
        matches!(terminal, RuntimeEvent::TurnCancelled),
        "the interrupted turn settles as cancelled: {terminal:?}"
    );

    let activity = harness
        .wait_for_activity(&harness.session, |activity| {
            activity.last_captured_range == Some((2, 2)) && !activity.active
        })
        .await;
    assert!(
        activity.last_error.is_none(),
        "settling the interrupted range is not an error: {activity:?}"
    );
    let notes = harness.inbox_notes();
    assert_eq!(
        notes.len(),
        1,
        "the interrupted turn's no-op capture adds no observation: {notes:?}"
    );
    assert!(
        notes[0]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains("__t000001-000001__")
    );

    harness.submit("third question").await;
    harness
        .wait_for_capture_range(&harness.session, (3, 3))
        .await;
    let bodies = harness.main_bodies().await;
    let last = serialized(bodies.last().unwrap());
    assert!(
        last.contains("Cross-session memory"),
        "the next turn still receives memory after the interrupt: {last}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_sessions_capture_into_one_store_without_losing_each_other() {
    let harness = harness_with_backends(vec![
        Backend {
            provider: "mock-a".to_owned(),
            model: "a".to_owned(),
            server: MockServer::start(vec![
                text_response("answer a"),
                capture_response(observation("Session A verified with cargo test.")),
            ])
            .await,
        },
        Backend {
            provider: "mock-b".to_owned(),
            model: "b".to_owned(),
            server: MockServer::start(vec![
                text_response("answer b"),
                capture_response(observation("Session B verified with cargo test.")),
            ])
            .await,
        },
    ])
    .await;
    let session_b = harness
        .app
        .create_session(&harness.models[1], "second session")
        .await
        .unwrap();
    let (a, b) = tokio::join!(
        harness.submit_for(&harness.session, "question a"),
        harness.submit_for(&session_b, "question b")
    );
    assert!(!matches!(a, RuntimeEvent::TurnFailed { .. }), "{a:?}");
    assert!(!matches!(b, RuntimeEvent::TurnFailed { .. }), "{b:?}");
    harness.wait_for_capture().await;
    harness.wait_for_capture_of(&session_b).await;

    let inbox = harness.workspace_memory().join("observations/_inbox");
    let mut notes: Vec<String> = std::fs::read_dir(&inbox)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    notes.sort();
    assert_eq!(
        notes.len(),
        2,
        "each session's range commits its own observation: {notes:?}"
    );
    let mut slugs = [
        harness.session.as_str().to_ascii_lowercase(),
        session_b.as_str().to_ascii_lowercase(),
    ];
    slugs.sort();
    assert!(notes.iter().any(|note| note.contains(&slugs[0])));
    assert!(notes.iter().any(|note| note.contains(&slugs[1])));
    for server in &harness.servers {
        assert_eq!(
            server
                .request_bodies()
                .await
                .iter()
                .filter(|body| body.contains("record_memory_observations"))
                .count(),
            1,
            "one capture call per session"
        );
    }
    assert_eq!(
        harness
            .app
            .memory()
            .activity(session_b.as_str())
            .last_captured_range,
        Some((1, 1))
    );
}

/// A temporary working directory has no durable workspace identity, and the
/// memory store deliberately gives it no workspace scope. The automatic
/// pipeline must respect that: capture, retention and Dream all target the
/// workspace and would otherwise create the very state the storage layer
/// refuses to initialize. Global memory — the user's `/remember` notes — stays
/// available and is the only scope the prompt advertises.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_temporary_workspace_keeps_the_automatic_pipeline_off() {
    let root = fixture_root();
    let workspace = tempfile::tempdir().unwrap();
    let storage = MemoryStorage::for_workspace(workspace.path(), &root.path().join("memory"));
    assert!(
        storage.is_ephemeral(),
        "the fixture must exercise the ephemeral workspace path"
    );
    let backend = Backend {
        provider: "mock".to_owned(),
        model: "m".to_owned(),
        server: MockServer::start(vec![text_response("an answer")]).await,
    };
    let harness = harness_at(
        root,
        workspace.path().to_path_buf(),
        Some(workspace),
        vec![backend],
    )
    .await;

    let terminal = harness.submit("answer in a throwaway directory").await;
    assert!(
        matches!(
            terminal,
            RuntimeEvent::TurnAnswered | RuntimeEvent::TurnCompleted
        ),
        "{terminal:?}"
    );

    // The terminal is published when the engine settles; the post-turn hook
    // runs immediately after it in the same task. Observe a bounded quiet
    // window so a capture enqueued after the terminal cannot slip past.
    let workspace_memory = harness.workspace_memory();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        for forbidden in ["memory_state.sqlite", "topics", "observations"] {
            assert!(
                !workspace_memory.join(forbidden).exists(),
                "a temporary workspace must never gain workspace memory at {workspace_memory:?}"
            );
        }
        assert_eq!(
            harness.server().request_count(),
            1,
            "no capture call is made in a temporary workspace"
        );
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // The model is told the truth: global memory exists, workspace memory does
    // not, and no scope root it cannot use is advertised.
    let bodies = harness.server().request_bodies().await;
    assert_eq!(bodies.len(), 1);
    assert!(bodies[0].contains("Cross-session memory"), "{}", bodies[0]);
    assert!(
        bodies[0].contains("temporary working directory"),
        "the prompt states why workspace memory is absent: {}",
        bodies[0]
    );
    assert!(
        !bodies[0].contains(&format!("Scope root: `{}`", workspace_memory.display())),
        "the prompt must not advertise a workspace scope that cannot exist"
    );

    let status = harness.app.memory().ui_status(Some(&harness.session)).await;
    assert!(status.workspace_ephemeral);
    assert!(!status.capture_enabled, "capture is unavailable");
    assert!(!status.automatic_dream_enabled);
    assert!(!status.manual_dream_enabled);
    assert!(!status.batch_dream_enabled);
    assert!(status.workspace.files.is_empty());
    assert_eq!(
        status.session.as_ref().map(|session| session.requested),
        Some(0),
        "no turn was queued for capture"
    );

    for (batch, action) in [(false, "dream"), (true, "batch dream")] {
        let outcome = harness
            .action(ClientCommand::RunMemoryDream {
                session_id: harness.session.clone(),
                query_id: None,
                batch,
            })
            .await;
        assert!(
            !outcome.ok,
            "{action} reports the unavailable workspace: {outcome:?}"
        );
        assert!(
            outcome.message.contains("temporary working directory"),
            "{action} explains why: {outcome:?}"
        );
    }
    let flush = harness
        .action(ClientCommand::FlushMemory {
            session_id: harness.session.clone(),
            query_id: None,
        })
        .await;
    assert!(!flush.ok, "{flush:?}");
    assert!(
        flush.message.contains("temporary working directory"),
        "{flush:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_session_toggle_pauses_memory_for_this_session_only() {
    let harness = harness(vec![
        text_response("first"),
        noop_capture(),
        text_response("second"),
        text_response("third"),
        noop_capture(),
    ])
    .await;

    // Turn 1: memory is on, so the prompt advertises it and capture runs.
    harness.submit("first turn").await;
    harness.wait_for_capture().await;
    let bodies = harness.server().request_bodies().await;
    assert_eq!(bodies.len(), 2, "{bodies:#?}");
    assert!(
        bodies[0].contains("## Cross-session memory"),
        "an enabled session is told about memory: {}",
        bodies[0]
    );

    // Pause through the same command the page sends.
    let paused = harness
        .action(ClientCommand::ToggleMemory {
            session_id: harness.session.clone(),
            query_id: None,
            enabled: false,
        })
        .await;
    assert!(paused.ok, "{paused:?}");
    assert_eq!(paused.action, "toggle");
    let status = harness.app.memory().ui_status(Some(&harness.session)).await;
    assert!(!status.enabled, "the page reports the pause");
    assert_eq!(status.rollout, "active", "the configuration is unchanged");
    assert!(!status.capture_enabled);
    assert_eq!(
        harness
            .app
            .memory()
            .config_for(Some("another-session"))
            .rollout
            .as_str(),
        "active",
        "the pause is scoped to the session that asked for it"
    );

    // Turn 2: no memory block reaches the provider and no capture job runs.
    harness.submit("second turn").await;
    let bodies = harness.server().request_bodies().await;
    assert_eq!(
        harness.server().request_count(),
        3,
        "only the turn's own request: {bodies:#?}"
    );
    assert!(
        !bodies[2].contains("## Cross-session memory"),
        "a paused session sends no memory block: {}",
        bodies[2]
    );

    // Resume: the next turn carries memory again and capture settles once.
    let resumed = harness
        .action(ClientCommand::ToggleMemory {
            session_id: harness.session.clone(),
            query_id: None,
            enabled: true,
        })
        .await;
    assert!(resumed.ok, "{resumed:?}");
    harness.submit("third turn").await;
    // The paused turn is not a capture gap. Only the turn that settled after
    // resume is queued, and its transcript does not include the paused turn.
    harness
        .wait_for_capture_range(&harness.session, (3, 3))
        .await;
    let bodies = harness.server().request_bodies().await;
    assert_eq!(harness.server().request_count(), 5, "{bodies:#?}");
    assert!(
        bodies[3].contains("## Cross-session memory"),
        "a resumed session is told about memory again: {}",
        bodies[3]
    );
    assert!(
        bodies[4].contains("record_memory_observations"),
        "capture runs after the resume: {}",
        bodies[4]
    );
    assert!(
        bodies[4].contains("third turn"),
        "the resumed turn is what capture sees: {}",
        bodies[4]
    );
    assert!(
        !bodies[4].contains("second turn"),
        "the paused turn is not backfilled into the capture transcript: {}",
        bodies[4]
    );
    let store = CaptureStore::open(harness.workspace_memory(), MemoryScope::Workspace).unwrap();
    let cursors = store.cursors(harness.session.as_str()).unwrap();
    assert_eq!(
        (cursors.requested, cursors.captured),
        (3, 3),
        "skipping the paused turn must not leave the cursor behind"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quitting_releases_the_capture_lease_this_process_owns() {
    let harness = harness(vec![text_response("first"), MockResponse::NeverResponds]).await;
    harness.submit("first turn").await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let store = CaptureStore::open(harness.workspace_memory(), MemoryScope::Workspace).unwrap();
        if store
            .work_state(harness.session.as_str(), 1)
            .is_ok_and(|state| state.running == 1)
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "capture never claimed a lease"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    harness.client.send(ClientCommand::Quit).await.unwrap();
    let store = CaptureStore::open(harness.workspace_memory(), MemoryScope::Workspace).unwrap();
    let state = store.work_state(harness.session.as_str(), 1).unwrap();
    assert_eq!(
        state.running, 0,
        "a normal quit must release this process's lease: {state:?}"
    );
    assert_eq!(state.failed, 1, "the job stays retryable: {state:?}");
    assert!(
        state
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("cancelled")),
        "the release records the cancel, not a stolen lease: {state:?}"
    );
}
