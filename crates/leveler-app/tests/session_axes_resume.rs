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

/// A — a new Coding Session defaults to Goal, durably, without any prompt
/// inspection.
#[tokio::test]
async fn a_new_coding_session_defaults_to_goal() {
    isolate_global_config();
    let tmp = tempfile::tempdir().unwrap();
    let app = Application::assemble(layout(&tmp)).unwrap();
    let id = app
        .create_session(&ModelRef::new("mock", "m"), "fix the login bug")
        .await
        .unwrap();
    let db = app.open_database().await.unwrap();
    let record = SessionRepository::new(&db).get(&id).await.unwrap().unwrap();
    assert_eq!(record.collaboration, "goal");
    assert_eq!(
        app.session_product_axes(&id).await.unwrap(),
        CollaborationMode::Goal
    );
    // Reopening reads the row, not the process default.
    let resumed = Application::assemble(layout(&tmp)).unwrap();
    assert_eq!(
        resumed.session_product_axes(&id).await.unwrap(),
        CollaborationMode::Goal
    );
}

/// B — Chat is an explicit choice and stays chat across a reopen.
#[tokio::test]
async fn an_explicit_chat_application_creates_a_chat_session() {
    isolate_global_config();
    let tmp = tempfile::tempdir().unwrap();
    let app = Application::assemble(layout(&tmp))
        .unwrap()
        .with_collaboration(CollaborationMode::Chat);
    let id = app
        .create_session(&ModelRef::new("mock", "m"), "just chat")
        .await
        .unwrap();
    let db = app.open_database().await.unwrap();
    let record = SessionRepository::new(&db).get(&id).await.unwrap().unwrap();
    assert_eq!(record.collaboration, "chat");
    let resumed = Application::assemble(layout(&tmp)).unwrap();
    assert_eq!(
        resumed.session_product_axes(&id).await.unwrap(),
        CollaborationMode::Chat
    );
}

/// I — no prompt heuristic: a long, multi-phase text is still just a user
/// message. It changes neither an explicit Chat session nor the Goal default.
#[tokio::test]
async fn a_complex_prompt_does_not_change_the_chosen_axis() {
    const COMPLEX: &str = "FINAL ARCHITECTURE CLEANUP + DATABASE BASELINE + \
        CLEAN-ROOM FULL BUSINESS ACCEPTANCE\n\nPART A: refactor, then test, then verify…";
    for (configured, expected) in [
        (CollaborationMode::Chat, "chat"),
        (CollaborationMode::default(), "goal"),
    ] {
        isolate_global_config();
        let tmp = tempfile::tempdir().unwrap();
        let app = Application::assemble(layout(&tmp))
            .unwrap()
            .with_collaboration(configured);
        let id = app
            .create_session(&ModelRef::new("mock", "m"), COMPLEX)
            .await
            .unwrap();
        let db = app.open_database().await.unwrap();
        let record = SessionRepository::new(&db).get(&id).await.unwrap().unwrap();
        assert_eq!(
            record.collaboration, expected,
            "the axis is the user's selection, never inferred from the prompt"
        );
    }
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
    // The explicit Goal entry survives a reopen: the session row is the source
    // of truth, so a later process cannot silently downgrade it to Chat.
    drop(db);
    let resumed = Application::assemble(layout(&tmp)).unwrap();
    assert_eq!(
        resumed.session_product_axes(&id).await.unwrap(),
        CollaborationMode::Goal
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

/// Full access governs how the tools ON the side surface are authorized — not
/// which tools are on it. The surface stays the observe-only one, and Full is
/// still unrestricted INSIDE it (nothing is denied, nothing asks).
#[tokio::test]
async fn full_side_question_keeps_the_observe_only_surface_with_unrestricted_authority() {
    isolate_global_config();
    let tmp = tempfile::tempdir().unwrap();
    let app = Application::assemble(layout(&tmp)).unwrap();
    let (registry, ctx) = app
        .side_question_tools(
            &ModelRef::new("mock", "m"),
            leveler_execution::PermissionProfile::FullAccess,
            true,
            None,
        )
        .await
        .unwrap();
    // Full is not downgraded to an approval flow: this is the permission
    // contract, and narrowing the surface must not touch it.
    assert!(ctx.policy.unrestricted_execution());
    assert!(registry.get("read_file").is_some());
    assert!(registry.get("get_task").is_some());
    // A lifecycle-bound or mutating tool is not part of the side surface, so
    // Full access cannot add it back.
    for name in [
        "apply_patch",
        "write_file",
        "run_command",
        "wait_task",
        "kill_task",
        "update_plan",
        "spawn_agent",
    ] {
        assert!(
            registry.get(name).is_none(),
            "the side surface must not expose {name}, whatever the permission profile"
        );
    }
}
