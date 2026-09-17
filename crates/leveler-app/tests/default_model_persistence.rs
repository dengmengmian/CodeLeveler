//! The user's explicit model choice — and only that — changes the persisted
//! default model.
//!
//! A session-scoped switch (`ClientCommand::SelectModel`, the shape a runtime
//! fallback or a temporary override would use) must leave the user's config
//! alone; `ClientCommand::SetDefaultModel` (the TUI picker's explicit action)
//! must update both the active session and the persisted default.

use std::sync::Arc;

use leveler_app::{Application, GlobalConfig, InProcessRuntimeClient};
use leveler_client_protocol::{
    ClientCommand, InteractiveRuntimeClient, PermissionProfile as WirePermissionProfile,
};
use leveler_core::SessionId;
use leveler_execution::PermissionProfile;
use leveler_local_transport::{CreateSessionRequest, LocalRuntimeService};
use leveler_model::ModelRef;
use leveler_project::Layout;

/// `LEVELER_HOME` is process-global; point it at one temp home and serialize
/// the tests here, which all read or write `config.toml` under it.
fn isolate_global_config() {
    use std::sync::OnceLock;
    static HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    let dir = HOME.get_or_init(|| tempfile::tempdir().unwrap());
    unsafe {
        std::env::set_var("LEVELER_HOME", dir.path());
    }
}

/// Serializes access to the shared `config.toml` in the isolated home.
async fn config_guard() -> tokio::sync::MutexGuard<'static, ()> {
    use std::sync::OnceLock;
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

fn write_repo_config(root: &std::path::Path) {
    std::fs::create_dir_all(root.join("configs/providers")).unwrap();
    std::fs::create_dir_all(root.join("configs/models")).unwrap();
    std::fs::write(
        root.join("configs/providers/mock.yaml"),
        "id: mock\nprotocol: openai_chat\nbase_url: http://127.0.0.1:9\n\
         retry:\n  max_attempts: 1\n  initial_backoff_ms: 10\n  max_backoff_ms: 10\n",
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

fn write_global_default(text: &str) {
    isolate_global_config();
    let path = GlobalConfig::path().expect("isolated global config path");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    if path.is_dir() {
        std::fs::remove_dir(&path).unwrap();
    }
    std::fs::write(&path, text).unwrap();
}

async fn build(
    tmp: &tempfile::TempDir,
) -> (Arc<Application>, Arc<InProcessRuntimeClient>, SessionId) {
    isolate_global_config();
    write_repo_config(tmp.path());
    let layout = Layout::from_parts(
        tmp.path().to_path_buf(),
        tmp.path().join("configs"),
        tmp.path().join("state"),
    );
    let app = Arc::new(Application::assemble(layout).unwrap());
    let model = ModelRef::new("mock", "m");
    let session_id = app.create_session(&model, "goal").await.unwrap();
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        model,
        PermissionProfile::Assisted,
        false,
    ));
    (app, client, session_id)
}

async fn session_model(client: &InProcessRuntimeClient, session_id: &SessionId) -> ModelRef {
    client
        .snapshot(session_id)
        .await
        .unwrap()
        .model
        .expect("a session always names its model")
}

/// Case 1: the explicit selection moves the active session and persists the
/// default in the same action.
#[tokio::test]
async fn explicit_selection_switches_the_session_and_persists_the_default() {
    let _guard = config_guard().await;
    let tmp = tempfile::tempdir().unwrap();
    write_global_default("default_model = \"old/keep\"\n");
    let (_app, client, session_id) = build(&tmp).await;

    client
        .send(ClientCommand::SetDefaultModel {
            session_id: session_id.clone(),
            model: ModelRef::new("mock", "m"),
        })
        .await
        .unwrap();

    assert_eq!(
        session_model(&client, &session_id).await,
        ModelRef::new("mock", "m")
    );
    assert_eq!(
        GlobalConfig::load().unwrap().default_model.as_deref(),
        Some("mock/m"),
        "the user's explicit choice is the persisted default"
    );
}

/// Case 2: the persisted default reloads from disk exactly as written.
#[tokio::test]
async fn persisted_default_survives_a_reload() {
    let _guard = config_guard().await;
    let tmp = tempfile::tempdir().unwrap();
    write_global_default("# keep\nlang = \"zh\"\ndefault_model = \"old/keep\"\n");
    let (_app, client, session_id) = build(&tmp).await;

    client
        .send(ClientCommand::SetDefaultModel {
            session_id,
            model: ModelRef::new("mock", "m"),
        })
        .await
        .unwrap();

    let reloaded = GlobalConfig::load().unwrap();
    assert_eq!(reloaded.default_model.as_deref(), Some("mock/m"));
    assert_eq!(
        reloaded.lang.as_deref(),
        Some("zh"),
        "unrelated keys survive"
    );
    let text = std::fs::read_to_string(GlobalConfig::path().unwrap()).unwrap();
    assert!(text.contains("# keep"), "comments survive: {text}");
}

/// Case 3: a fresh runtime built from the persisted default starts new sessions
/// on it (the restart-equivalent path).
#[tokio::test]
async fn a_new_session_starts_on_the_persisted_default() {
    let _guard = config_guard().await;
    let tmp = tempfile::tempdir().unwrap();
    write_global_default("default_model = \"old/keep\"\n");
    let (app, client, session_id) = build(&tmp).await;

    client
        .send(ClientCommand::SetDefaultModel {
            session_id,
            model: ModelRef::new("mock", "m"),
        })
        .await
        .unwrap();

    // Simulate a restart: the new process resolves its default from the config
    // it just persisted, exactly as `resolve_model` does.
    let resolved = ModelRef::parse(&GlobalConfig::load().unwrap().default_model.unwrap()).unwrap();
    let restarted =
        InProcessRuntimeClient::new(app.clone(), resolved, PermissionProfile::Assisted, false);
    let bootstrap = restarted
        .create_session(CreateSessionRequest {
            goal: "restarted".to_string(),
            model: None,
            mode: WirePermissionProfile::Assisted,
            approval_policy: Default::default(),
        })
        .await
        .unwrap();
    assert_eq!(
        bootstrap.session.model,
        Some(ModelRef::new("mock", "m")),
        "a new session uses the persisted default"
    );
}

/// Case 4/5: a session-scoped switch — the shape a runtime fallback or a
/// temporary override uses — never rewrites the default.
#[tokio::test]
async fn a_session_scoped_switch_leaves_the_default_alone() {
    let _guard = config_guard().await;
    let tmp = tempfile::tempdir().unwrap();
    write_global_default("default_model = \"old/keep\"\n");
    let (_app, client, session_id) = build(&tmp).await;

    client
        .send(ClientCommand::SelectModel {
            session_id: session_id.clone(),
            model: ModelRef::new("mock", "m"),
        })
        .await
        .unwrap();

    assert_eq!(
        session_model(&client, &session_id).await,
        ModelRef::new("mock", "m")
    );
    assert_eq!(
        GlobalConfig::load().unwrap().default_model.as_deref(),
        Some("old/keep"),
        "a session switch is not a user default change"
    );
}

/// Case 6: the session switch and the config write are independent outcomes.
/// When the write fails the switch still stands, but the failure is returned —
/// never swallowed and never reported as a full success.
#[tokio::test]
async fn a_config_write_failure_is_reported_as_a_partial_failure() {
    let _guard = config_guard().await;
    let tmp = tempfile::tempdir().unwrap();
    write_global_default("default_model = \"old/keep\"\n");
    let (_app, client, session_id) = build(&tmp).await;

    // Make the config file unwritable: a directory where it belongs.
    let path = GlobalConfig::path().unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();

    let error = client
        .send(ClientCommand::SetDefaultModel {
            session_id: session_id.clone(),
            model: ModelRef::new("mock", "m"),
        })
        .await
        .unwrap_err();

    let message = error.to_string();
    assert!(
        message.contains("已切换") && message.contains("默认模型保存失败"),
        "the failure must name both outcomes: {message}"
    );
    assert_eq!(
        session_model(&client, &session_id).await,
        ModelRef::new("mock", "m"),
        "the session switch happened before the failed write"
    );

    std::fs::remove_dir(&path).unwrap();
}
