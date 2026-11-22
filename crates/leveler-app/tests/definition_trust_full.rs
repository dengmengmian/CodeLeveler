//! FullAccess adopts the definitions that existed before the session's agent
//! could write anything, and nothing else.
//!
//! One test per binary: it installs an isolated `LEVELER_HOME` before the
//! application captures its environment snapshot.

use leveler_app::Application;
use leveler_definition_trust::{DefinitionIdentity, DefinitionKind, DefinitionTrust};
use leveler_execution::PermissionProfile;
use leveler_model::ModelRef;
use leveler_project::Layout;

fn write_skill(repo: &std::path::Path, name: &str, body: &str) -> String {
    let dir = repo.join(".leveler/skills").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let skill = dir.join("SKILL.md");
    std::fs::write(
        &skill,
        format!("---\nname: {name}\ndescription: Demo.\n---\n\n{body}\n"),
    )
    .unwrap();
    leveler_definition_trust::DefinitionIdentity::skill_package_digest(
        &leveler_definition_trust::read_skill_package(&skill).unwrap(),
    )
}

fn digest_of(path: &std::path::Path) -> String {
    let bytes = std::fs::read(path).unwrap();
    DefinitionIdentity::single_digest(&bytes)
}

#[tokio::test]
async fn full_adopts_the_creation_baseline_and_never_promotes_later_edits() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    unsafe {
        std::env::set_var("LEVELER_HOME", &home);
    }
    let agents = repo.join("AGENTS.md");
    std::fs::write(&agents, "ROOT RULES\n").unwrap();
    let agents_digest = digest_of(&agents);
    let skill_digest = write_skill(&repo, "demo", "DEMO BODY");

    let layout = Layout::from_parts(repo.clone(), home.join("configs"), home.join("state"));
    let app = Application::assemble(layout).unwrap();
    let model = ModelRef::new("mock", "m");
    let session = app
        .create_session_with_collaboration(
            &model,
            "trust baseline",
            PermissionProfile::FullAccess,
            leveler_lifecycle::CollaborationMode::Chat,
        )
        .await
        .unwrap();

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
    let agents_id = DefinitionIdentity::project(DefinitionKind::RootAgents, "AGENTS.md").unwrap();
    let skill_id =
        DefinitionIdentity::project(DefinitionKind::Skill, ".leveler/skills/demo").unwrap();
    assert_eq!(
        view.check(&agents_id, &agents_digest),
        DefinitionTrust::FullAdopted
    );
    assert_eq!(
        view.check(&skill_id, &skill_digest),
        DefinitionTrust::FullAdopted
    );
    // A declarative agent is not adopted even in Full: it can start a child
    // with its own tool surface, so it keeps requiring a persistent approval.
    std::fs::create_dir_all(repo.join(".leveler/agents/x")).unwrap();
    std::fs::write(repo.join(".leveler/agents/x/agent.yaml"), "name: x\n").unwrap();
    std::fs::write(repo.join(".leveler/agents/x/instructions.md"), "do\n").unwrap();
    let agent_id = DefinitionIdentity::project(DefinitionKind::Agent, ".leveler/agents/x").unwrap();
    let agent_digest = DefinitionIdentity::agent_digest(b"name: x\n", b"do\n");
    assert_eq!(
        view.check(&agent_id, &agent_digest),
        DefinitionTrust::Unapproved
    );

    // An edit after the baseline is not the adopted content.
    std::fs::write(&agents, "AGENT WROTE THIS\n").unwrap();
    let edited = digest_of(&agents);
    assert_ne!(edited, agents_digest);
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
    assert_eq!(
        (*engine.factory.tool_context.execution.definition_trust).check(&agents_id, &edited),
        DefinitionTrust::Unapproved
    );

    // Leaving Full stops adoption; the persistent store was never written.
    app.set_live_permission_profile(session.as_str(), PermissionProfile::Assisted);
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
    assert_eq!(
        (*engine.factory.tool_context.execution.definition_trust).check(&agents_id, &agents_digest),
        DefinitionTrust::Unapproved
    );
    assert_eq!(
        leveler_definition_trust::DefinitionTrustStore::load(&home).status(),
        &leveler_definition_trust::StoreStatus::Missing
    );

    // A new process resuming the same session has no provenance for the
    // baseline: the agent may have written definitions before the restart, so
    // Full adopts nothing without a persistent approval.
    drop(app);
    let layout = Layout::from_parts(repo.clone(), home.join("configs"), home.join("state"));
    let app = Application::assemble(layout).unwrap();
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
    assert_eq!(
        (*engine.factory.tool_context.execution.definition_trust).check(&skill_id, &skill_digest),
        DefinitionTrust::Unapproved,
        "a resumed session has no credible Full baseline"
    );
}
