//! Permission-mode persistence and precedence (Release Gate: Full Permission
//! Contract, INVARIANT B).
//!
//! INVARIANT A (authorization) is proven by `full_permission_lifecycle.rs`: an
//! effective Full mode never produces or keeps a CodeLeveler approval.
//!
//! INVARIANT B (persistence) is proven here:
//!
//! ```text
//! user-selected Full
//!     => effective mode remains Full across
//!        create / persist / reconnect / restart / resume
//! unless the user explicitly switches
//! ```
//!
//! Precedence, highest first:
//!
//! 1. an explicit runtime `SetPermissionProfile` (persisted, so it is the mode
//!    later resolves read);
//! 2. an explicit CLI `--permission` (resolved by the CLI and persisted at
//!    create/override time);
//! 3. the session's persisted mode;
//! 4. the project's configured default, else the built-in `assisted`.
//!
//! A break is `FULL_PERMISSION_MODE_DRIFT`.

#[path = "support/observed_command.rs"]
mod observed_command;
use observed_command::ObservedSettings;

use std::sync::Arc;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    ClientCommand, InteractiveRuntimeClient, PermissionProfile as WirePermission,
    UiPendingInteraction,
};
use leveler_core::SessionId;
use leveler_execution::PermissionProfile;
use leveler_local_transport::{
    CreateSessionRequest, CreateWorkspaceSelection, LocalRuntimeService,
};
use leveler_model::ModelRef;
use leveler_project::Layout;

fn isolate_global_config() {
    use std::sync::OnceLock;
    static HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    let dir = HOME.get_or_init(|| tempfile::tempdir().unwrap());
    unsafe {
        std::env::set_var("LEVELER_HOME", dir.path());
    }
}

fn write_config(root: &std::path::Path) {
    isolate_global_config();
    std::fs::create_dir_all(root.join("configs/providers")).unwrap();
    std::fs::create_dir_all(root.join("configs/models")).unwrap();
    std::fs::write(
        root.join("configs/providers/mock.yaml"),
        "id: mock\nprotocol: openai_chat\nbase_url: http://127.0.0.1:1\n",
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

fn layout(root: &std::path::Path) -> Layout {
    Layout::from_parts(root.to_path_buf(), root.join("configs"), root.join("state"))
}

fn assemble_app(root: &std::path::Path) -> Arc<Application> {
    write_config(root);
    Arc::new(Application::assemble(layout(root)).unwrap())
}

fn client(app: &Arc<Application>, default_mode: PermissionProfile) -> Arc<InProcessRuntimeClient> {
    Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        ModelRef::new("mock", "m"),
        default_mode,
        false,
    ))
}

/// The runtime's own snapshot is the effective mode a client would render.
async fn effective_mode(
    client: &Arc<InProcessRuntimeClient>,
    session_id: &SessionId,
) -> WirePermission {
    client.snapshot(session_id).await.unwrap().mode
}

async fn persisted_mode(app: &Application, session_id: &SessionId) -> Option<PermissionProfile> {
    app.persisted_permission_profile(session_id).await.unwrap()
}

// ── TEST 1: embedded create persists the resolved mode ───────────────────────

#[tokio::test]
async fn embedded_create_persists_full_and_the_runtime_reads_it_back() {
    let tmp = tempfile::tempdir().unwrap();
    let app = assemble_app(tmp.path());
    let model = ModelRef::new("mock", "m");

    let id = app
        .create_session_with_mode(&model, "full session", PermissionProfile::FullAccess)
        .await
        .unwrap();

    assert_eq!(
        persisted_mode(&app, &id).await,
        Some(PermissionProfile::FullAccess),
        "the row must carry the resolved mode, not a hardcoded default"
    );

    // The runtime reads the persisted mode, even when the client was built
    // with a deliberately different default (the old double-truth bug).
    let runtime = client(&app, PermissionProfile::Assisted);
    assert_eq!(
        effective_mode(&runtime, &id).await,
        WirePermission::FullAccess,
        "FULL_PERMISSION_MODE_DRIFT: runtime disagreed with the persisted row"
    );

    // And an Auto session persists Auto — the mapping is not one-way.
    let auto = app
        .create_session_with_mode(&model, "auto session", PermissionProfile::Assisted)
        .await
        .unwrap();
    assert_eq!(
        persisted_mode(&app, &auto).await,
        Some(PermissionProfile::Assisted)
    );
    assert_eq!(
        effective_mode(&runtime, &auto).await,
        WirePermission::Assisted
    );
}

// ── TEST 2: embedded restart/resume keeps Full ───────────────────────────────

#[tokio::test]
async fn embedded_restart_resume_keeps_full() {
    let tmp = tempfile::tempdir().unwrap();
    let model = ModelRef::new("mock", "m");
    let app = assemble_app(tmp.path());
    let id = app
        .create_session_with_mode(&model, "full session", PermissionProfile::FullAccess)
        .await
        .unwrap();
    drop(app);

    // Fresh process: re-assemble from the same state dir.
    let restarted = assemble_app(tmp.path());
    let runtime = client(&restarted, PermissionProfile::Assisted);
    assert_eq!(
        effective_mode(&runtime, &id).await,
        WirePermission::FullAccess,
        "FULL_PERMISSION_MODE_DRIFT: Full did not survive an embedded restart"
    );
}

// ── TEST 3: daemon create + restart keeps Full ───────────────────────────────

#[tokio::test]
async fn daemon_create_and_restart_keeps_full() {
    let tmp = tempfile::tempdir().unwrap();
    let app1 = assemble_app(tmp.path());
    let model = ModelRef::new("mock", "m");
    let daemon1 = client(&app1, PermissionProfile::Assisted);
    let bootstrap = daemon1
        .create_session(CreateSessionRequest {
            request_id: None,
            collaboration: leveler_local_transport::CollaborationMode::Chat,
            workspace: CreateWorkspaceSelection::RuntimeDefault,
            goal: "daemon full".to_string(),
            model: Some(model.clone()),
            mode: WirePermission::FullAccess,
            approval_policy: leveler_client_protocol::ApprovalPolicy::Interactive,
        })
        .await
        .unwrap();
    let id = bootstrap.session.id.clone();
    assert_eq!(bootstrap.session.mode, WirePermission::FullAccess);
    assert_eq!(
        persisted_mode(&app1, &id).await,
        Some(PermissionProfile::FullAccess)
    );
    drop(daemon1);
    drop(app1);

    let app2 = assemble_app(tmp.path());
    let daemon2 = client(&app2, PermissionProfile::Assisted);
    assert_eq!(
        effective_mode(&daemon2, &id).await,
        WirePermission::FullAccess,
        "FULL_PERMISSION_MODE_DRIFT: Full did not survive a daemon restart"
    );
}

// ── TEST 4: persisted Auto + explicit override → Full ────────────────────────

#[tokio::test]
async fn explicit_full_override_on_a_persisted_auto_session_is_effective_and_persisted() {
    let tmp = tempfile::tempdir().unwrap();
    let model = ModelRef::new("mock", "m");
    let app = assemble_app(tmp.path());
    let id = app
        .create_session_with_mode(&model, "auto session", PermissionProfile::Assisted)
        .await
        .unwrap();
    assert_eq!(
        persisted_mode(&app, &id).await,
        Some(PermissionProfile::Assisted)
    );

    // The interactive resume override: `SetPermissionProfile` is the same path
    // `--permission full` drives.
    let runtime = client(&app, PermissionProfile::Assisted);
    runtime
        .send_observed(ClientCommand::SetPermissionProfile {
            session_id: id.clone(),
            mode: WirePermission::FullAccess,
        })
        .await
        .unwrap();

    assert_eq!(
        effective_mode(&runtime, &id).await,
        WirePermission::FullAccess
    );
    assert_eq!(
        persisted_mode(&app, &id).await,
        Some(PermissionProfile::FullAccess),
        "an explicit override must be persisted, not memory-only"
    );
    let snapshot = runtime.snapshot(&id).await.unwrap();
    assert!(
        snapshot
            .pending_interactions
            .iter()
            .all(|item| !matches!(item, UiPendingInteraction::Approval(_))),
        "no permission approval may exist under Full"
    );

    // And it survives a restart.
    drop(runtime);
    drop(app);
    let restarted = assemble_app(tmp.path());
    assert_eq!(
        effective_mode(&client(&restarted, PermissionProfile::Assisted), &id).await,
        WirePermission::FullAccess
    );
}

// ── TEST 5: persisted Full + resume without an explicit flag ────────────────

#[tokio::test]
async fn resume_without_an_explicit_flag_keeps_the_persisted_full_mode() {
    let tmp = tempfile::tempdir().unwrap();
    let model = ModelRef::new("mock", "m");
    let app = assemble_app(tmp.path());
    let id = app
        .create_session_with_mode(&model, "full session", PermissionProfile::FullAccess)
        .await
        .unwrap();
    drop(app);

    // A resume that passes no `--permission` uses the persisted mode; the
    // client's own default must not leak in.
    let resumed = assemble_app(tmp.path());
    let runtime = client(&resumed, PermissionProfile::RequestApproval);
    assert_eq!(
        effective_mode(&runtime, &id).await,
        WirePermission::FullAccess,
        "FULL_PERMISSION_MODE_DRIFT: resume used a default instead of the persisted mode"
    );
}

// ── TEST 6: explicit override can downgrade a persisted Full session ────────

#[tokio::test]
async fn explicit_assisted_override_downgrades_a_persisted_full_session() {
    let tmp = tempfile::tempdir().unwrap();
    let model = ModelRef::new("mock", "m");
    let app = assemble_app(tmp.path());
    let id = app
        .create_session_with_mode(&model, "full session", PermissionProfile::FullAccess)
        .await
        .unwrap();
    let runtime = client(&app, PermissionProfile::FullAccess);
    runtime
        .send_observed(ClientCommand::SetPermissionProfile {
            session_id: id.clone(),
            mode: WirePermission::Assisted,
        })
        .await
        .unwrap();

    assert_eq!(
        effective_mode(&runtime, &id).await,
        WirePermission::Assisted
    );
    assert_eq!(
        persisted_mode(&app, &id).await,
        Some(PermissionProfile::Assisted),
        "an explicit downgrade must be persisted too"
    );
}

// ── TEST 7: the headless (`leveler run --resume`) override ───────────────────

#[tokio::test]
async fn headless_override_persists_and_is_read_back_in_both_directions() {
    let tmp = tempfile::tempdir().unwrap();
    let model = ModelRef::new("mock", "m");
    let app = assemble_app(tmp.path());

    // persisted Auto + headless --permission full
    let auto = app
        .create_session_with_mode(&model, "auto", PermissionProfile::Assisted)
        .await
        .unwrap();
    app.set_persisted_permission_profile(&auto, PermissionProfile::FullAccess)
        .await
        .unwrap();
    assert_eq!(
        persisted_mode(&app, &auto).await,
        Some(PermissionProfile::FullAccess)
    );

    // persisted Full + headless --permission auto
    let full = app
        .create_session_with_mode(&model, "full", PermissionProfile::FullAccess)
        .await
        .unwrap();
    app.set_persisted_permission_profile(&full, PermissionProfile::Assisted)
        .await
        .unwrap();
    assert_eq!(
        persisted_mode(&app, &full).await,
        Some(PermissionProfile::Assisted)
    );

    let runtime = client(&app, PermissionProfile::Assisted);
    assert_eq!(
        effective_mode(&runtime, &auto).await,
        WirePermission::FullAccess
    );
    assert_eq!(
        effective_mode(&runtime, &full).await,
        WirePermission::Assisted
    );
}
