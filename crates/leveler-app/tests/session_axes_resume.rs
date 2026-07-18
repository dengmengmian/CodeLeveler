//! Resume must re-apply product axes from the session row, not Application defaults.
//! A delivery/economy session resumed after a fresh `assemble()` (balanced default)
//! must still build the engine with the persisted work profile (AC5 / skeptic gap).

use std::str::FromStr;
use std::sync::Arc;

use leveler_agent::{AutoClarify, CollaborationMode, WorkProfile};
use leveler_app::Application;
use leveler_execution::{AutoApprove, PermissionProfile};
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_storage::SessionRepository;
use leveler_tools::{core_registry, full_registry};

fn isolate_global_config() {
    use std::sync::OnceLock;
    static EMPTY_HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    let dir = EMPTY_HOME.get_or_init(|| tempfile::tempdir().unwrap());
    unsafe {
        std::env::set_var("LEVELER_HOME", dir.path());
    }
}

fn layout(tmp: &tempfile::TempDir) -> Layout {
    Layout {
        repo_root: tmp.path().to_path_buf(),
        config_dir: tmp.path().join("configs"),
        state_dir: tmp.path().join("state"),
    }
}

#[tokio::test]
async fn create_session_persists_work_profile_and_collaboration() {
    isolate_global_config();
    let tmp = tempfile::tempdir().unwrap();
    let app = Application::assemble(layout(&tmp))
        .unwrap()
        .with_work_profile(WorkProfile::Delivery)
        .with_collaboration(CollaborationMode::Goal);
    let id = app
        .create_session(&ModelRef::new("mock", "m"), "fix the login bug")
        .await
        .unwrap();
    let db = app.open_database().await.unwrap();
    let record = SessionRepository::new(&db).get(&id).await.unwrap().unwrap();
    assert_eq!(record.work_profile, "delivery");
    assert_eq!(record.collaboration, "goal");
}

#[tokio::test]
async fn session_product_axes_read_from_db_not_app_default() {
    isolate_global_config();
    let tmp = tempfile::tempdir().unwrap();
    let creator = Application::assemble(layout(&tmp))
        .unwrap()
        .with_work_profile(WorkProfile::Economy)
        .with_collaboration(CollaborationMode::Chat);
    let id = creator
        .create_session(&ModelRef::new("mock", "m"), "quick scan")
        .await
        .unwrap();

    // Fresh process-shaped app: defaults balanced/chat.
    let resumer = Application::assemble(layout(&tmp)).unwrap();
    assert_eq!(resumer.work_profile(), WorkProfile::Balanced);
    assert_eq!(resumer.collaboration(), CollaborationMode::Chat);

    let (wp, collab) = resumer.session_product_axes(&id).await.unwrap();
    assert_eq!(wp, WorkProfile::Economy);
    assert_eq!(collab, CollaborationMode::Chat);
}

#[tokio::test]
async fn engine_for_with_profile_uses_session_axes_not_app_default() {
    isolate_global_config();
    let tmp = tempfile::tempdir().unwrap();
    let app = Application::assemble(layout(&tmp)).unwrap();
    assert_eq!(app.work_profile(), WorkProfile::Balanced);

    let engine = app
        .engine_for_with_profile(
            &ModelRef::new("mock", "m"),
            PermissionProfile::Assisted,
            false,
            Arc::new(AutoApprove),
            Arc::new(AutoClarify),
            WorkProfile::Economy,
            false,
        )
        .await
        .unwrap();
    assert_eq!(engine.factory.work_profile, WorkProfile::Economy);
    let n = engine.factory.registry.definitions().len();
    assert_eq!(
        n,
        core_registry().definitions().len(),
        "economy must ship Core surface; got {n}"
    );

    let engine_full = app
        .engine_for_with_profile(
            &ModelRef::new("mock", "m"),
            PermissionProfile::Assisted,
            false,
            Arc::new(AutoApprove),
            Arc::new(AutoClarify),
            WorkProfile::Delivery,
            false,
        )
        .await
        .unwrap();
    assert_eq!(engine_full.factory.work_profile, WorkProfile::Delivery);
    assert_eq!(
        engine_full.factory.registry.definitions().len(),
        full_registry().definitions().len()
    );
}

#[tokio::test]
async fn resume_session_rebuilds_engine_with_persisted_delivery_profile() {
    isolate_global_config();
    let tmp = tempfile::tempdir().unwrap();
    let creator = Application::assemble(layout(&tmp))
        .unwrap()
        .with_work_profile(WorkProfile::Delivery);
    let id = creator
        .create_session(&ModelRef::new("mock", "m"), "fix the auth bug")
        .await
        .unwrap();

    // Simulate CLI resume: fresh assemble without --work-mode.
    let resumer = Application::assemble(layout(&tmp)).unwrap();
    assert_eq!(resumer.work_profile(), WorkProfile::Balanced);

    // The axes resume will apply must come from the session row.
    let (wp, _) = resumer.session_product_axes(&id).await.unwrap();
    assert_eq!(wp, WorkProfile::Delivery);

    // And the engine built the same way resume_session does must carry Delivery.
    let engine = resumer
        .engine_for_with_profile(
            &ModelRef::new("mock", "m"),
            PermissionProfile::Assisted,
            false,
            Arc::new(AutoApprove),
            Arc::new(AutoClarify),
            wp,
            false,
        )
        .await
        .unwrap();
    assert_eq!(engine.factory.work_profile, WorkProfile::Delivery);
    // Same mapping factory.build uses for Goal/Delivery (no live model profile needed).
    let gate = leveler_agent::GateConfig::for_work_profile(engine.factory.work_profile);
    assert!(
        gate.delivery_gate,
        "resumed delivery session must keep delivery_gate on"
    );
}

#[test]
fn axes_wire_strings_parse_round_trip() {
    assert_eq!(
        WorkProfile::from_str("delivery").unwrap(),
        WorkProfile::Delivery
    );
    assert_eq!(
        WorkProfile::from_str("economy").unwrap(),
        WorkProfile::Economy
    );
    assert_eq!(
        CollaborationMode::from_str("chat").unwrap(),
        CollaborationMode::Chat
    );
}
