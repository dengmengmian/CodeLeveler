//! Runtime configuration is live state, not a boot-time constant.
//!
//! These tests drive the real composition root against scripted HTTP providers
//! and assert on the requests that actually left the process: a reload must
//! change which provider/model the NEXT turn calls, an invalid file must leave
//! the last known good generation serving, and a running turn must keep the
//! generation it started with.

#[path = "support/observed_command.rs"]
mod observed_command;
use observed_command::ObservedSettings;

use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient, ReloadOrigin, ReloadOutcome};
use leveler_client_protocol::{
    ClientCommand, InteractiveRuntimeClient, PermissionProfile as WireProfile,
};
use leveler_execution::PermissionProfile;
use leveler_local_transport::{CreateSessionRequest, LocalRuntimeService};
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_test_support::{MockResponse, MockServer};

/// `LEVELER_HOME` is process-global; point it at the same root `Layout` derives
/// its home from, so the loader, the watcher and the fingerprint all read the
/// one file. Tests using it are serialized below.
fn isolate_global_config(root: &std::path::Path) {
    std::fs::create_dir_all(root).unwrap();
    unsafe {
        std::env::set_var("LEVELER_HOME", root);
    }
}

async fn config_guard() -> tokio::sync::MutexGuard<'static, ()> {
    use std::sync::OnceLock;
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

/// Two providers with one model each. `default_model` names the first.
fn write_global_config(default_model: &str, a: &str, b: &str, extra: &str) {
    write_global_config_with_key(default_model, a, b, extra, None)
}

fn write_global_config_with_key(
    default_model: &str,
    a: &str,
    b: &str,
    extra: &str,
    api_key: Option<&str>,
) {
    let path = leveler_app::GlobalConfig::path().expect("isolated global config path");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let api_key_line = api_key
        .map(|key| format!("api_key = \"{key}\"\n"))
        .unwrap_or_default();
    let text = format!(
        r#"default_model = "{default_model}"
{extra}
[providers.a]
protocol = "openai_chat"
base_url = "{a}"
{api_key_line}

[providers.b]
protocol = "openai_chat"
base_url = "{b}"

[models.alpha]
provider = "a"
model_id = "alpha-model"
context_window = 131072
max_output_tokens = 1024

[models.beta]
provider = "b"
model_id = "beta-model"
context_window = 65536
max_output_tokens = 1024
"#
    );
    std::fs::write(&path, &text).unwrap();
}

fn text_response(content: &str) -> MockResponse {
    let frame = serde_json::json!({
        "choices": [{"delta": {"content": content}, "finish_reason": "stop"}]
    });
    MockResponse::Sse {
        body: format!("data: {frame}\n\ndata: [DONE]\n\n"),
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

fn tool_call_frame(id: &str, name: &str, arguments: serde_json::Value) -> String {
    serde_json::json!({
        "choices": [{"delta": {"tool_calls": [{
            "index": 0, "id": id,
            "function": {"name": name, "arguments": arguments.to_string()}
        }]}}]
    })
    .to_string()
}

fn finish_frame(reason: &str) -> String {
    serde_json::json!({"choices": [{"delta": {}, "finish_reason": reason}]}).to_string()
}

fn assemble(tmp: &tempfile::TempDir) -> Arc<Application> {
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(workspace.join(".leveler")).unwrap();
    // Keep the model-call count in these tests attributable to the turn: no
    // background memory extraction shares the same provider.
    std::fs::write(workspace.join(".leveler/config.yaml"), "").unwrap();
    let layout = Layout::from_parts(
        workspace,
        tmp.path().join("configs"),
        tmp.path().join("state"),
    );
    Arc::new(
        Application::assemble(layout)
            .unwrap()
            .with_collaboration(leveler_agent::CollaborationMode::Chat),
    )
}

/// The UI's default path: a create request that names no model inherits the
/// configured default and follows a later change.
async fn create_inherited(client: &InProcessRuntimeClient) -> leveler_core::SessionId {
    client
        .create_session(CreateSessionRequest {
            request_id: None,
            collaboration: leveler_local_transport::CollaborationMode::Chat,
            workspace: leveler_local_transport::CreateWorkspaceSelection::RuntimeDefault,
            approval_policy: leveler_client_protocol::ApprovalPolicy::AutoApprove,
            goal: "hello".to_string(),
            model: None,
            mode: WireProfile::FullAccess,
        })
        .await
        .unwrap()
        .session
        .id
}

async fn run_turn(app: &Application, session: &leveler_core::SessionId, model: &ModelRef) {
    app.run_in_session(
        session,
        model,
        PermissionProfile::FullAccess,
        "say hello",
        Arc::new(leveler_execution::AutoApprove),
        false,
        &mut |_| {},
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();
}

/// The request body the provider actually received names the model.
async fn models_requested(server: &MockServer) -> Vec<String> {
    server
        .request_bodies()
        .await
        .into_iter()
        .filter_map(|body| {
            serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|value| value["model"].as_str().map(str::to_string))
        })
        .collect()
}

#[tokio::test]
async fn editing_the_default_model_reaches_the_next_real_request() {
    let _guard = config_guard().await;
    let tmp = tempfile::tempdir().unwrap();
    isolate_global_config(tmp.path());
    let first = MockServer::start_one(text_response("from a")).await;
    let second = MockServer::start_one(text_response("from b")).await;
    write_global_config("a/alpha", &first.base_url(), &second.base_url(), "");
    let app = assemble(&tmp);

    let alpha = ModelRef::new("a", "alpha");
    let beta = ModelRef::new("b", "beta");
    assert_eq!(app.config_version(), 1);
    assert_eq!(
        app.config_snapshot().loaded.default_model.as_deref(),
        Some("a/alpha")
    );

    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        alpha.clone(),
        PermissionProfile::FullAccess,
        false,
    ));
    let session = create_inherited(&client).await;
    run_turn(&app, &session, &alpha).await;
    assert!(
        models_requested(&first)
            .await
            .iter()
            .all(|model| model == "alpha-model"),
        "the first turn's requests: {:?}",
        models_requested(&first).await
    );
    assert!(models_requested(&second).await.is_empty());

    // The edit an external editor (or the Desktop settings page) makes.
    write_global_config("b/beta", &first.base_url(), &second.base_url(), "");
    let outcome = app.reload_config(ReloadOrigin::Command, true).await;
    let ReloadOutcome::Applied(applied) = outcome else {
        panic!("a valid edit must apply: {outcome:?}");
    };
    assert_eq!(applied.version, 2);
    assert!(applied.change.default_model);
    assert_eq!(applied.model.as_deref(), Some("b/beta"));
    assert_eq!(
        app.config_snapshot().loaded.default_model.as_deref(),
        Some("b/beta")
    );
    assert!(app.model_refs().contains(&beta));

    // What the runtime now says the session's next turn will use.
    let next = client
        .snapshot(&session)
        .await
        .unwrap()
        .model
        .expect("the session names its model");
    assert_eq!(next, beta, "an unpinned session inherits the new default");
    run_turn(&app, &session, &next).await;
    let requested = models_requested(&second).await;
    assert!(
        !requested.is_empty() && requested.iter().all(|model| model == "beta-model"),
        "the real request must use the new provider and model: {requested:?}"
    );
    assert!(
        models_requested(&first)
            .await
            .iter()
            .all(|model| model == "alpha-model"),
        "the old provider must not receive the second turn"
    );
}

#[tokio::test]
async fn an_invalid_config_keeps_the_last_known_good_generation() {
    let _guard = config_guard().await;
    let tmp = tempfile::tempdir().unwrap();
    isolate_global_config(tmp.path());
    let first = MockServer::start_one(text_response("ok")).await;
    let second = MockServer::start_one(text_response("ok")).await;
    write_global_config("a/alpha", &first.base_url(), &second.base_url(), "");
    let app = assemble(&tmp);
    let before = app.config_version();
    assert_eq!(before, 1);

    let path = leveler_app::GlobalConfig::path().unwrap();
    std::fs::write(&path, "default_model = \"b/beta\"\n[providers\n").unwrap();
    let outcome = app.reload_config(ReloadOrigin::Command, true).await;
    let ReloadOutcome::Failed { error, version } = outcome else {
        panic!("a malformed file must not apply: {outcome:?}");
    };
    assert_eq!(version, before, "the generation must not advance");
    assert!(!error.is_empty());
    let snapshot = app.config_snapshot();
    assert_eq!(snapshot.version, before);
    assert_eq!(
        snapshot.loaded.default_model.as_deref(),
        Some("a/alpha"),
        "the last good default stays in force"
    );
    assert!(app.model_refs().contains(&ModelRef::new("a", "alpha")));
    assert!(app.config_manager().last_error().is_some());

    // Restoring the file is applied, and the recorded failure is cleared.
    write_global_config("b/beta", &first.base_url(), &second.base_url(), "");
    let outcome = app.reload_config(ReloadOrigin::Command, true).await;
    assert!(matches!(outcome, ReloadOutcome::Applied(_)), "{outcome:?}");
    assert!(app.config_manager().last_error().is_none());
    assert_eq!(
        app.config_snapshot().loaded.default_model.as_deref(),
        Some("b/beta")
    );
}

#[tokio::test]
async fn a_comment_only_edit_does_not_advance_the_generation() {
    let _guard = config_guard().await;
    let tmp = tempfile::tempdir().unwrap();
    isolate_global_config(tmp.path());
    let first = MockServer::start_one(text_response("ok")).await;
    let second = MockServer::start_one(text_response("ok")).await;
    write_global_config("a/alpha", &first.base_url(), &second.base_url(), "");
    let app = assemble(&tmp);
    let before = app.config_version();
    let fingerprint_before = app.config_snapshot().fingerprint.clone();
    let registry_before = Arc::as_ptr(&app.config_snapshot().registry);

    write_global_config(
        "a/alpha",
        &first.base_url(),
        &second.base_url(),
        "# a comment that changes the file, not the configuration",
    );
    let outcome = app.reload_config(ReloadOrigin::Command, true).await;
    assert!(
        matches!(outcome, ReloadOutcome::Unchanged { version } if version == before),
        "{outcome:?}"
    );
    let snapshot = app.config_snapshot();
    assert_eq!(snapshot.version, before);
    assert_ne!(
        snapshot.fingerprint, fingerprint_before,
        "the source digest follows the file so a client can tell it is current"
    );
    assert_eq!(
        Arc::as_ptr(&snapshot.registry),
        registry_before,
        "an unchanged configuration reuses the provider clients"
    );

    // Reloading the same file repeatedly must not rebuild anything either.
    for _ in 0..3 {
        assert!(matches!(
            app.reload_config(ReloadOrigin::Command, true).await,
            ReloadOutcome::Unchanged { .. }
        ));
    }
    assert_eq!(
        Arc::as_ptr(&app.config_snapshot().registry),
        registry_before,
        "repeated identical loads reuse the same registry"
    );
}

#[tokio::test]
async fn an_explicit_model_choice_is_not_overwritten_by_a_default_change() {
    let _guard = config_guard().await;
    let tmp = tempfile::tempdir().unwrap();
    isolate_global_config(tmp.path());
    let first = MockServer::start_one(text_response("ok")).await;
    let second = MockServer::start_one(text_response("ok")).await;
    write_global_config("a/alpha", &first.base_url(), &second.base_url(), "");
    let app = assemble(&tmp);
    let alpha = ModelRef::new("a", "alpha");
    let beta = ModelRef::new("b", "beta");
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        alpha.clone(),
        PermissionProfile::Assisted,
        false,
    ));
    let pinned = create_inherited(&client).await;
    let inherited = create_inherited(&client).await;

    // The user's explicit choice (`/model`) pins this session.
    client
        .send_observed(ClientCommand::SelectModel {
            session_id: pinned.clone(),
            model: beta.clone(),
        })
        .await
        .unwrap();

    write_global_config("b/beta", &first.base_url(), &second.base_url(), "");
    assert!(matches!(
        app.reload_config(ReloadOrigin::Command, true).await,
        ReloadOutcome::Applied(_)
    ));

    assert_eq!(
        client.snapshot(&pinned).await.unwrap().model,
        Some(beta.clone()),
        "an explicit choice is durable and wins over the new default"
    );
    assert_eq!(
        client.snapshot(&inherited).await.unwrap().model,
        Some(beta),
        "a session that never chose inherits the new default"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_running_turn_keeps_the_generation_it_started_with() {
    let _guard = config_guard().await;
    let tmp = tempfile::tempdir().unwrap();
    isolate_global_config(tmp.path());
    // Provider A holds the turn open with a real command, then answers.
    let (program, args) = sleep_command(2);
    let first = MockServer::start(vec![
        sse(vec![
            tool_call_frame(
                "c-sleep",
                "run_command",
                serde_json::json!({ "program": program, "args": args }),
            ),
            finish_frame("tool_calls"),
        ]),
        text_response("from a"),
    ])
    .await;
    let second = MockServer::start_one(text_response("from b")).await;
    write_global_config("a/alpha", &first.base_url(), &second.base_url(), "");
    let app = assemble(&tmp);
    let alpha = ModelRef::new("a", "alpha");
    let beta = ModelRef::new("b", "beta");
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        alpha.clone(),
        PermissionProfile::FullAccess,
        false,
    ));
    let session = create_inherited(&client).await;
    let mut events = client.subscribe();
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "say hello".into(),
            attachments: vec![],
        })
        .await
        .unwrap();

    // Wait until the turn really reached provider A and is executing its tool.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while first.request_count() == 0 {
        assert!(tokio::time::Instant::now() < deadline, "turn never started");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    write_global_config("b/beta", &first.base_url(), &second.base_url(), "");
    let outcome = app.reload_config(ReloadOrigin::Command, true).await;
    assert!(matches!(outcome, ReloadOutcome::Applied(_)), "{outcome:?}");
    // While the turn runs, the client must keep naming the model doing the work.
    assert_eq!(
        client.snapshot(&session).await.unwrap().model,
        Some(alpha.clone()),
        "a live turn is not relabelled by a reload"
    );

    wait_terminal(&mut events).await;
    let requested = models_requested(&first).await;
    assert!(
        !requested.is_empty() && requested.iter().all(|model| model == "alpha-model"),
        "every request of the running turn stays on its generation: {requested:?}"
    );
    assert!(
        models_requested(&second).await.is_empty(),
        "the new provider must not be touched mid-turn"
    );

    // The next turn uses the new default.
    let next = client.snapshot(&session).await.unwrap().model.unwrap();
    assert_eq!(next, beta);
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "say hello again".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    wait_terminal(&mut events).await;
    let requested = models_requested(&second).await;
    assert!(
        !requested.is_empty() && requested.iter().all(|model| model == "beta-model"),
        "the next turn calls the new provider: {requested:?}"
    );
}

/// Drain events until the turn reaches a terminal state.
async fn wait_terminal(
    rx: &mut tokio::sync::broadcast::Receiver<leveler_client_protocol::RuntimeEvent>,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Ok(event)) => match event {
                leveler_client_protocol::RuntimeEvent::TurnCompleted
                | leveler_client_protocol::RuntimeEvent::TurnCompletedWithWarnings { .. }
                | leveler_client_protocol::RuntimeEvent::TurnAnswered
                | leveler_client_protocol::RuntimeEvent::TurnTruncated { .. }
                | leveler_client_protocol::RuntimeEvent::TurnIncomplete { .. }
                | leveler_client_protocol::RuntimeEvent::TurnFailed { .. }
                | leveler_client_protocol::RuntimeEvent::TurnCancelled
                | leveler_client_protocol::RuntimeEvent::TaskCancelled => return,
                _ => continue,
            },
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            _ => panic!("the turn never settled"),
        }
    }
}

#[cfg(unix)]
fn sleep_command(seconds: u64) -> (String, Vec<String>) {
    ("sleep".to_string(), vec![seconds.to_string()])
}

#[cfg(not(unix))]
fn sleep_command(seconds: u64) -> (String, Vec<String>) {
    (
        "ping".to_string(),
        vec![
            "-n".to_string(),
            (seconds + 1).to_string(),
            "127.0.0.1".to_string(),
        ],
    )
}

/// `/reload` answers through the real command path and reports what happened.
#[tokio::test]
async fn the_reload_command_answers_from_the_runtime_state() {
    let _guard = config_guard().await;
    let tmp = tempfile::tempdir().unwrap();
    isolate_global_config(tmp.path());
    let first = MockServer::start_one(text_response("ok")).await;
    let second = MockServer::start_one(text_response("ok")).await;
    write_global_config("a/alpha", &first.base_url(), &second.base_url(), "");
    let app = assemble(&tmp);
    let alpha = ModelRef::new("a", "alpha");
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        alpha,
        PermissionProfile::Assisted,
        false,
    ));
    let session = create_inherited(&client).await;
    let mut events = client.subscribe();

    // No change → the runtime says so, and does not advance the version.
    let before = app.config_version();
    client
        .send(ClientCommand::ReloadConfig { query_id: None })
        .await
        .unwrap();
    let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("a reload event arrives")
        .expect("stream alive");
    match event {
        leveler_client_protocol::RuntimeEvent::ConfigReloaded {
            outcome, version, ..
        } => {
            assert_eq!(outcome, "unchanged");
            assert_eq!(version, before);
        }
        other => panic!("expected ConfigReloaded, got {other:?}"),
    }

    write_global_config("b/beta", &first.base_url(), &second.base_url(), "");
    client
        .send(ClientCommand::ReloadConfig { query_id: None })
        .await
        .unwrap();
    let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("a reload event arrives")
        .expect("stream alive");
    match event {
        leveler_client_protocol::RuntimeEvent::ConfigReloaded {
            outcome,
            version,
            model,
            deferred_to_next_turn,
            ..
        } => {
            assert_eq!(outcome, "applied");
            assert_eq!(version, before + 1);
            assert_eq!(model.as_deref(), Some("b/beta"));
            assert!(!deferred_to_next_turn, "idle runtime: nothing is deferred");
        }
        other => panic!("expected ConfigReloaded, got {other:?}"),
    }
    assert_eq!(
        client.snapshot(&session).await.unwrap().model,
        Some(ModelRef::new("b", "beta"))
    );
}

#[tokio::test]
async fn a_provider_credential_change_reaches_the_next_request() {
    let _guard = config_guard().await;
    let tmp = tempfile::tempdir().unwrap();
    isolate_global_config(tmp.path());
    let first = MockServer::start_one(text_response("ok")).await;
    let second = MockServer::start_one(text_response("ok")).await;
    write_global_config_with_key(
        "a/alpha",
        &first.base_url(),
        &second.base_url(),
        "",
        Some("key-one"),
    );
    let app = assemble(&tmp);
    let alpha = ModelRef::new("a", "alpha");
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        alpha.clone(),
        PermissionProfile::FullAccess,
        false,
    ));
    let session = create_inherited(&client).await;
    run_turn(&app, &session, &alpha).await;
    let heads = first.request_heads().await;
    assert!(
        heads.iter().any(|head| head
            .to_ascii_lowercase()
            .contains("authorization: bearer key-one")),
        "the configured key must reach the wire: {heads:?}"
    );

    write_global_config_with_key(
        "a/alpha",
        &first.base_url(),
        &second.base_url(),
        "",
        Some("key-two"),
    );
    assert!(matches!(
        app.reload_config(ReloadOrigin::Command, true).await,
        ReloadOutcome::Applied(_)
    ));
    // The same session is pinned to `a/alpha` by `create_session(&model)`, so
    // this proves the CREDENTIAL changed, not the model.
    run_turn(&app, &session, &alpha).await;
    let heads = first.request_heads().await;
    assert_eq!(heads.len(), 2, "both turns reached the same provider");
    assert!(
        heads[1]
            .to_ascii_lowercase()
            .contains("authorization: bearer key-two"),
        "the reloaded key must reach the next request: {:?}",
        heads[1]
    );
    assert!(
        !heads[1].contains("key-one"),
        "the previous credential must not be reused"
    );
}

#[tokio::test]
async fn a_startup_only_setting_is_reported_as_requiring_a_restart() {
    let _guard = config_guard().await;
    let tmp = tempfile::tempdir().unwrap();
    isolate_global_config(tmp.path());
    let first = MockServer::start_one(text_response("ok")).await;
    let second = MockServer::start_one(text_response("ok")).await;
    write_global_config(
        "a/alpha",
        &first.base_url(),
        &second.base_url(),
        "lang = \"zh\"",
    );
    let app = assemble(&tmp);
    let alpha = ModelRef::new("a", "alpha");
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        alpha.clone(),
        PermissionProfile::Assisted,
        false,
    ));
    let _session = create_inherited(&client).await;
    let mut events = client.subscribe();

    write_global_config(
        "a/alpha",
        &first.base_url(),
        &second.base_url(),
        "lang = \"en\"",
    );
    client
        .send(ClientCommand::ReloadConfig { query_id: None })
        .await
        .unwrap();
    let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("a reload event arrives")
        .expect("stream alive");
    match event {
        leveler_client_protocol::RuntimeEvent::ConfigReloaded {
            outcome,
            restart_required,
            ..
        } => {
            assert_eq!(outcome, "applied");
            assert!(
                restart_required.iter().any(|item| item == "ui_language"),
                "a startup-only setting must be named, not reported as applied: {restart_required:?}"
            );
        }
        other => panic!("expected ConfigReloaded, got {other:?}"),
    }

    // The repository policy file is a separate source: a change there is not
    // "no change", and the boot-frozen part is named.
    std::fs::write(
        tmp.path().join("workspace/.leveler/config.yaml"),
        "agents:\n  delegation: false\n",
    )
    .unwrap();
    client
        .send(ClientCommand::ReloadConfig { query_id: None })
        .await
        .unwrap();
    let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("a reload event arrives")
        .expect("stream alive");
    match event {
        leveler_client_protocol::RuntimeEvent::ConfigReloaded {
            outcome,
            changed,
            restart_required,
            ..
        } => {
            assert_eq!(outcome, "applied");
            assert!(changed.iter().any(|item| item == "project"), "{changed:?}");
            assert!(restart_required.is_empty(), "{restart_required:?}");
        }
        other => panic!("expected ConfigReloaded, got {other:?}"),
    }
}

/// Concurrent reloads are serialized: one file state publishes one
/// generation, and no caller observes a half-applied mix.
#[tokio::test]
async fn concurrent_reloads_publish_exactly_one_generation() {
    let _guard = config_guard().await;
    let tmp = tempfile::tempdir().unwrap();
    isolate_global_config(tmp.path());
    let first = MockServer::start_one(text_response("ok")).await;
    let second = MockServer::start_one(text_response("ok")).await;
    write_global_config("a/alpha", &first.base_url(), &second.base_url(), "");
    let app = assemble(&tmp);
    let before = app.config_version();

    write_global_config("b/beta", &first.base_url(), &second.base_url(), "");
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let app = app.clone();
        tasks.push(tokio::spawn(async move {
            app.reload_config(ReloadOrigin::Command, true).await
        }));
    }
    let mut applied = 0;
    for task in tasks {
        match task.await.unwrap() {
            ReloadOutcome::Applied(_) => applied += 1,
            ReloadOutcome::Unchanged { .. } => {}
            ReloadOutcome::Failed { error, .. } => panic!("no reload may fail: {error}"),
        }
    }
    assert_eq!(applied, 1, "one file state is one generation");
    assert_eq!(app.config_version(), before + 1);
    let snapshot = app.config_snapshot();
    assert_eq!(snapshot.loaded.default_model.as_deref(), Some("b/beta"));
    assert!(
        snapshot
            .registry
            .model_refs()
            .contains(&ModelRef::new("b", "beta"))
    );
}

/// The automatic watcher reaches the same validated path: an external write
/// becomes a new generation with no command and no restart.
#[tokio::test]
async fn the_watcher_applies_an_external_edit_without_a_command() {
    let _guard = config_guard().await;
    let tmp = tempfile::tempdir().unwrap();
    isolate_global_config(tmp.path());
    let first = MockServer::start_one(text_response("ok")).await;
    let second = MockServer::start_one(text_response("ok")).await;
    write_global_config("a/alpha", &first.base_url(), &second.base_url(), "");
    let app = assemble(&tmp);
    assert_eq!(
        app.layout.home().config_file(),
        leveler_app::GlobalConfig::path().unwrap(),
        "loader, watcher and fingerprint must read the same file"
    );
    app.start_config_watcher();

    write_global_config("b/beta", &first.base_url(), &second.base_url(), "");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if app.config_snapshot().loaded.default_model.as_deref() == Some("b/beta") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the watcher did not apply the edit: version={} last_error={:?}",
            app.config_version(),
            app.config_manager().last_error()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(app.config_version(), 2);

    // A file that cannot be parsed is reported and does not advance anything.
    std::fs::write(leveler_app::GlobalConfig::path().unwrap(), "not = [valid").unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if app.config_manager().last_error().is_some() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the watcher did not report the broken file"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        app.config_version(),
        2,
        "a failed load keeps the last good one"
    );
    assert_eq!(
        app.config_snapshot().loaded.default_model.as_deref(),
        Some("b/beta")
    );
}

/// A memory-only edit is the case that made `/memory` show "not enabled" to a
/// user whose config said `active`: the reload classified the change as a
/// comment-only edit and republished the old bundle. Both the snapshot and the
/// memory runtime read by the UI must move to the new rollout.
#[tokio::test]
async fn a_memory_only_edit_reaches_the_running_runtime() {
    use leveler_memory::config::MemoryRollout;

    let _guard = config_guard().await;
    let tmp = tempfile::tempdir().unwrap();
    isolate_global_config(tmp.path());
    let first = MockServer::start_one(text_response("ok")).await;
    let second = MockServer::start_one(text_response("ok")).await;
    write_global_config(
        "a/alpha",
        &first.base_url(),
        &second.base_url(),
        "[memory]\nrollout = \"off\"\n",
    );
    let app = assemble(&tmp);
    assert_eq!(
        app.config_snapshot().loaded.memory.rollout,
        MemoryRollout::Off
    );
    assert_eq!(app.memory().config().rollout, MemoryRollout::Off);

    write_global_config(
        "a/alpha",
        &first.base_url(),
        &second.base_url(),
        "[memory]\nrollout = \"active\"\n",
    );
    let outcome = app.reload_config(ReloadOrigin::Command, true).await;
    let ReloadOutcome::Applied(applied) = outcome else {
        panic!("a memory-only edit must apply, not be read as a comment: {outcome:?}");
    };
    assert!(applied.change.labels().contains(&"memory"));
    assert_eq!(
        app.config_snapshot().loaded.memory.rollout,
        MemoryRollout::Active,
        "the published snapshot must carry the loaded bundle, not the old one"
    );
    assert_eq!(
        app.memory().config().rollout,
        MemoryRollout::Active,
        "the memory runtime the UI queries reads the new generation"
    );
}
