//! Legacy work-profile rows do not alter the single product semantics.
use leveler_agent::CollaborationMode;
use leveler_app::Application;
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_storage::{SessionRepository, SessionStore, TaskStore};
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
async fn creation_persists_collaboration_and_single_legacy_marker() {
    isolate_global_config();
    let tmp = tempfile::tempdir().unwrap();
    let app = Application::assemble(layout(&tmp))
        .unwrap()
        .with_collaboration(CollaborationMode::Goal);
    let id = app
        .create_session(&ModelRef::new("mock", "m"), "fix the login bug")
        .await
        .unwrap();
    let db = app.open_database().await.unwrap();
    let record = SessionRepository::new(&db).get(&id).await.unwrap().unwrap();
    assert_eq!(record.work_profile, "single");
    assert_eq!(record.collaboration, "goal");
    assert_eq!(
        TaskStore::task_for_session(&db, &id).await.unwrap(),
        Some(leveler_core::TaskId::new(id.as_str()))
    );
    assert_eq!(
        SessionStore::execution(&db, &id).await.unwrap(),
        Some(("assisted".into(), false, "direct".into(), None))
    );
}

#[tokio::test]
async fn legacy_profiles_do_not_change_resume_collaboration() {
    isolate_global_config();
    let tmp = tempfile::tempdir().unwrap();
    let app = Application::assemble(layout(&tmp))
        .unwrap()
        .with_collaboration(CollaborationMode::Plan);
    let id = app
        .create_session(&ModelRef::new("mock", "m"), "plan it")
        .await
        .unwrap();
    let db = app.open_database().await.unwrap();
    for legacy in ["economy", "core", "balanced", "delivery"] {
        SessionRepository::new(&db)
            .set_axes(&id, "plan", legacy, leveler_core::now())
            .await
            .unwrap();
        let resumer = Application::assemble(layout(&tmp)).unwrap();
        assert_eq!(
            resumer.session_product_axes(&id).await.unwrap(),
            CollaborationMode::Plan
        );
    }
}

#[tokio::test]
async fn available_tools_start_unexposed_and_memory_config_gates_loading() {
    use leveler_agent::AutoClarify;
    use leveler_agent::capability::CapabilityId;
    use leveler_execution::{AutoApprove, PermissionProfile};
    use std::sync::Arc;
    isolate_global_config();
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join(".leveler")).unwrap();
    std::fs::write(
        tmp.path().join(".leveler/config.yaml"),
        "memory:\n  enabled: false\n",
    )
    .unwrap();
    let app = Application::assemble(layout(&tmp)).unwrap();
    let engine = app
        .engine_for_session(
            &ModelRef::new("mock", "m"),
            PermissionProfile::Assisted,
            false,
            Arc::new(AutoApprove),
            Arc::new(AutoClarify),
            false,
            None,
        )
        .await
        .unwrap();
    assert!(
        engine.factory.registry.get("memory").is_some(),
        "availability is independent of permission"
    );
    assert!(engine.factory.registry.get("find_symbol").is_some());
    let state = engine.factory.capabilities.as_ref().unwrap();
    assert!(state.active().is_empty());
    assert!(
        state
            .enable(CapabilityId::Memory)
            .await
            .unwrap_err()
            .contains("not permitted")
    );
    assert!(state.enable(CapabilityId::CodeIntelligence).await.unwrap());
    assert!(state.permits_tool("find_symbol"));
    assert!(!state.permits_tool("save_agent"));
    assert!(state.fresh().active().is_empty());
}

#[tokio::test]
async fn a_side_question_has_only_base_observation_tools() {
    isolate_global_config();
    let tmp = tempfile::tempdir().unwrap();
    let app = Application::assemble(layout(&tmp)).unwrap();
    let (registry, _) = app
        .side_question_tools(
            &ModelRef::new("mock", "m"),
            leveler_execution::PermissionProfile::Assisted,
            false,
            None,
        )
        .await
        .unwrap();
    assert!(registry.get("read_file").is_some());
    assert!(
        registry
            .definitions()
            .iter()
            .all(|tool| leveler_agent::capability::tool_capability(&tool.name).is_none()),
        "bounded side questions must not disclose optional packs implicitly"
    );
}
