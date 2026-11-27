//! Switching an existing session into FullAccess keeps the creation baseline
//! it was given while Assisted. This is the desktop path: a session is created
//! by the New Session button (Assisted) and only later switched to Full.

use leveler_app::Application;
use leveler_definition_trust::{DefinitionIdentity, DefinitionKind, DefinitionTrust};
use leveler_execution::PermissionProfile;
use leveler_model::ModelRef;
use leveler_project::Layout;

#[tokio::test]
async fn switching_to_full_adopts_the_creation_baseline() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    unsafe {
        std::env::set_var("LEVELER_HOME", &home);
    }
    let dir = repo.join(".leveler/skills/demo");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        "---\nname: demo\ndescription: Demo.\n---\n\nDEMO BODY\n",
    )
    .unwrap();

    let layout = Layout::from_parts(repo.clone(), home.join("configs"), home.join("state"));
    let app = Application::assemble(layout).unwrap();
    let model = ModelRef::new("mock", "m");
    let session = app
        .create_session_with_collaboration(
            &model,
            "switch to full",
            PermissionProfile::Assisted,
            leveler_lifecycle::CollaborationMode::Chat,
        )
        .await
        .unwrap();
    // A warm-up turn in Assisted creates the live profile cell the Desktop's
    // permission picker updates.
    let engine = app
        .engine_for_session(
            &model,
            PermissionProfile::Assisted,
            false,
            std::sync::Arc::new(leveler_execution::AutoApprove),
            std::sync::Arc::new(leveler_agent::AutoClarify),
            false,
            Some(session.as_str()),
        )
        .await
        .unwrap();
    drop(engine);
    assert!(
        app.set_live_permission_profile(session.as_str(), PermissionProfile::FullAccess),
        "the session must have a live profile cell"
    );
    let engine = app
        .engine_for_session(
            &model,
            PermissionProfile::FullAccess,
            false,
            std::sync::Arc::new(leveler_execution::AutoApprove),
            std::sync::Arc::new(leveler_agent::AutoClarify),
            false,
            Some(session.as_str()),
        )
        .await
        .unwrap();
    let view = (*engine.factory.tool_context.execution.definition_trust).clone();
    let identity = DefinitionIdentity::project(DefinitionKind::Skill, ".leveler/skills/demo")
        .expect("identity");
    let package = leveler_definition_trust::read_skill_package(&dir.join("SKILL.md")).unwrap();
    let digest = DefinitionIdentity::skill_package_digest(&package);
    assert_eq!(
        view.check(&identity, &digest),
        DefinitionTrust::FullAdopted,
        "a session created under Assisted must keep its creation baseline when switched to Full"
    );
}
