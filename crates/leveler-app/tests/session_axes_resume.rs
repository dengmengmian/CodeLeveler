//! Resume must re-apply product axes from the session row, not Application defaults.
//! An economy session resumed after a fresh `assemble()` (balanced default)
//! must still build the engine with the persisted work profile (AC5 / skeptic gap).

use std::str::FromStr;
use std::sync::Arc;

use leveler_agent::{AutoClarify, CollaborationMode, WorkProfile};
use leveler_app::Application;
use leveler_execution::{AutoApprove, PermissionProfile};
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_storage::{SessionRepository, SessionStore, TaskStore};
use leveler_tools::core_surface;

/// In-process capability handles: the composition under test is about which
/// tools exist, not about which host services back them.
fn caps() -> leveler_tools::Capabilities {
    leveler_tools::Capabilities::in_process(std::sync::Arc::new(
        leveler_core::environment().clone(),
    ))
}

/// What an Economy turn must look like: the core primitives plus the harness
/// controls, and nothing optional. Composed here from the same two halves the
/// application composes, so the assertion cannot drift from the production
/// composition by counting a hardcoded number.
fn economy_surface_size() -> usize {
    let mut registry = core_surface(&caps());
    leveler_agent::register_harness_controls(&mut registry);
    registry.definitions().len()
}

fn isolate_global_config() {
    use std::sync::OnceLock;
    static EMPTY_HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    let dir = EMPTY_HOME.get_or_init(|| tempfile::tempdir().unwrap());
    unsafe {
        std::env::set_var("LEVELER_HOME", dir.path());
    }
}

fn layout(tmp: &tempfile::TempDir) -> Layout {
    Layout::from_parts(
        tmp.path().to_path_buf(),
        tmp.path().join("configs"),
        tmp.path().join("state"),
    )
}

#[tokio::test]
async fn create_session_persists_work_profile_and_collaboration() {
    isolate_global_config();
    let tmp = tempfile::tempdir().unwrap();
    let app = Application::assemble(layout(&tmp))
        .unwrap()
        .with_work_profile(WorkProfile::Economy)
        .with_collaboration(CollaborationMode::Goal);
    let id = app
        .create_session(&ModelRef::new("mock", "m"), "fix the login bug")
        .await
        .unwrap();
    let db = app.open_database().await.unwrap();
    let record = SessionRepository::new(&db).get(&id).await.unwrap().unwrap();
    assert_eq!(record.work_profile, "economy");
    assert_eq!(record.collaboration, "goal");
    assert_eq!(
        TaskStore::task_for_session(&db, &id).await.unwrap(),
        Some(leveler_core::TaskId::new(id.as_str())),
        "session creation must return only after the engine has created its task"
    );
    assert_eq!(
        SessionStore::execution(&db, &id).await.unwrap(),
        Some(("assisted".into(), false, "direct".into(), None)),
        "routing creation through the engine must preserve the initial execution config"
    );
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
            None,
        )
        .await
        .unwrap();
    let n = engine.factory.registry.definitions().len();
    assert_eq!(
        n,
        economy_surface_size(),
        "economy composes no optional pack: the primitives and the protocol; got {n}"
    );

    let engine_full = app
        .engine_for_with_profile(
            &ModelRef::new("mock", "m"),
            PermissionProfile::Assisted,
            false,
            Arc::new(AutoApprove),
            Arc::new(AutoClarify),
            WorkProfile::Balanced,
            false,
            None,
        )
        .await
        .unwrap();
    // `balanced` composes whatever packs THIS host can offer, which depends on
    // the machine (a browser runtime, a search key, a vision model). The
    // invariant that does not depend on the machine is that it is a superset
    // of the core surface.
    assert!(
        engine_full.factory.registry.definitions().len()
            >= core_surface(&caps()).definitions().len(),
        "a full profile is never smaller than the core surface"
    );
}

#[tokio::test]
async fn a_legacy_delivery_row_resumes_as_balanced() {
    isolate_global_config();
    let tmp = tempfile::tempdir().unwrap();
    let creator = Application::assemble(layout(&tmp)).unwrap();
    let id = creator
        .create_session(&ModelRef::new("mock", "m"), "fix the auth bug")
        .await
        .unwrap();

    // Simulate an OLD row written before `delivery` was retired from the product.
    {
        let db = creator.open_database().await.unwrap();
        SessionRepository::new(&db)
            .set_axes(&id, "chat", "delivery", leveler_core::now())
            .await
            .unwrap();
    }

    // A fresh process-shaped app resumes; the legacy value must read as `balanced`.
    let resumer = Application::assemble(layout(&tmp)).unwrap();
    let (wp, _) = resumer.session_product_axes(&id).await.unwrap();
    assert_eq!(wp, WorkProfile::Balanced);
}

#[test]
fn axes_wire_strings_parse_round_trip() {
    // `delivery` is no longer a user value; it is only a legacy persisted alias.
    assert!(WorkProfile::from_str("delivery").is_err());
    assert_eq!(
        WorkProfile::from_persisted("delivery"),
        WorkProfile::Balanced
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
