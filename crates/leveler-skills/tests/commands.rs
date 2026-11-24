//! The slash-command projection of the one skill registry.

use std::path::Path;

use leveler_definition_trust::DefinitionTrustView;
use leveler_skills::{
    PreferencesState, SkillCommandError, SkillLookupError, SkillRegistry, SkillRoot, SkillRoots,
    SkillScope, SkillSource,
};

fn write_skill(root: &Path, name: &str, extra_frontmatter: &str) {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!(
            "---\nname: {name}\ndescription: {name} procedure.\n{extra_frontmatter}---\n\n{name} BODY\n"
        ),
    )
    .unwrap();
}

fn roots(project: &Path, user: &Path) -> SkillRoots {
    let mut roots = SkillRoots::empty();
    roots.push(SkillRoot {
        scope: SkillScope::Project,
        source: SkillSource::Native,
        dir: project.join(".leveler/skills"),
        containing_root: Some(project.to_path_buf()),
    });
    roots.push(SkillRoot {
        scope: SkillScope::User,
        source: SkillSource::Native,
        dir: user.to_path_buf(),
        containing_root: None,
    });
    roots
}

fn load(roots: &SkillRoots, preferences: &PreferencesState) -> SkillRegistry {
    SkillRegistry::load(
        roots,
        &DefinitionTrustView::trust_everything_for_tests(),
        preferences,
    )
}

fn registry(project: &Path, user: &Path) -> SkillRegistry {
    load(&roots(project, user), &PreferencesState::default())
}

#[test]
fn bare_names_are_advertised_and_reserved_names_are_qualified() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("repo");
    let user = tmp.path().join("home/.leveler/skills");
    write_skill(&project.join(".leveler/skills"), "deploy", "");
    write_skill(&project.join(".leveler/skills"), "help", "");
    let registry = registry(&project, &user);

    let catalog = registry.command_catalog(&["help", "skills"]);
    let deploy = catalog.get("deploy").expect("bare deploy");
    assert_eq!(deploy.advertised, "deploy");
    assert_eq!(deploy.canonical, "project:deploy");
    assert!(!deploy.is_qualified());

    let help = catalog
        .get("project:help")
        .expect("a reserved skill moves to its qualified form");
    assert_eq!(help.name, "help");
    assert!(help.is_qualified());
    assert!(
        catalog.get("help").is_none(),
        "the bare reserved name must not be advertised as a skill"
    );

    assert!(matches!(
        registry.resolve_command("help", &["help"]),
        Err(SkillCommandError::Reserved { .. })
    ));
    assert!(registry.resolve_command("project:help", &["help"]).is_ok());
}

#[test]
fn qualified_names_resolve_and_shadowed_scopes_are_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("repo");
    let user = tmp.path().join("home/.leveler/skills");
    write_skill(&project.join(".leveler/skills"), "dup", "");
    write_skill(&user, "dup", "");
    let registry = registry(&project, &user);

    let project_dup = registry.resolve_command("project:dup", &[]).unwrap();
    assert_eq!(project_dup.scope, SkillScope::Project);
    let user_dup = registry.resolve_command("user:dup", &[]).unwrap_err();
    assert!(
        matches!(user_dup, SkillCommandError::Shadowed { .. }),
        "{user_dup:?}"
    );
}

#[test]
fn disabled_and_untrusted_skills_are_not_commands() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("repo");
    let user = tmp.path().join("home/.leveler/skills");
    write_skill(&project.join(".leveler/skills"), "deploy", "");
    write_skill(&project.join(".leveler/skills"), "evil", "");

    let file = tmp.path().join("state/skills.toml");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(
        &file,
        format!(
            "[projects]\n\"{}\" = [\"deploy\"]\n",
            leveler_skills::project_key(&project)
        ),
    )
    .unwrap();
    let preferences = PreferencesState::load_path(&file, Some(&project));
    let disabled_registry = load(&roots(&project, &user), &preferences);
    let untrusted_registry = SkillRegistry::load(
        &roots(&project, &user),
        &DefinitionTrustView::deny_all(),
        &PreferencesState::default(),
    );

    assert!(
        disabled_registry
            .command_catalog(&[])
            .get("deploy")
            .is_none(),
        "{:?}",
        disabled_registry.command_catalog(&[])
    );
    assert!(
        untrusted_registry
            .command_catalog(&[])
            .get("evil")
            .is_none(),
        "{:?}",
        untrusted_registry.command_catalog(&[])
    );
    let error = disabled_registry
        .resolve_command("deploy", &[])
        .unwrap_err();
    assert!(
        matches!(error, SkillCommandError::Unavailable { ref state, .. } if state.contains("disabled")),
        "{error:?}"
    );
    let error = untrusted_registry.resolve_command("evil", &[]).unwrap_err();
    assert!(
        matches!(error, SkillCommandError::Untrusted { .. }),
        "{error:?}"
    );
}

#[test]
fn uppercase_and_dotted_names_are_commands() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("repo");
    let user = tmp.path().join("home/.leveler/skills");
    let skills = project.join(".leveler/skills");
    write_skill(&skills, "UpperSkill", "user-invocable: true\n");
    write_skill(&skills, "MySkill.v2", "");
    let registry = registry(&project, &user);
    let catalog = registry.command_catalog(&[]);

    let upper = catalog
        .get("UpperSkill")
        .expect("uppercase is a valid name");
    assert!(upper.mentionable, "uppercase is mentionable");
    let dotted = catalog.get("MySkill.v2").expect("dotted name is a command");
    assert!(
        !dotted.mentionable,
        "a dotted name is slash-only; `$` cannot spell it"
    );
    assert!(registry.resolve_command("MySkill.v2", &[]).is_ok());
    assert!(registry.resolve_mentions("$UpperSkill").loaded.len() == 1);
}

#[test]
fn leading_slash_requests_load_the_body_and_leave_prose_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("repo");
    let user = tmp.path().join("home/.leveler/skills");
    write_skill(&project.join(".leveler/skills"), "deploy", "");
    let registry = registry(&project, &user);

    let resolved = registry
        .resolve_command_request("/deploy staging", &[])
        .expect("a leading slash command");
    assert_eq!(resolved.token, "/deploy");
    assert!(resolved.is_loaded());
    assert!(resolved.error.is_none());
    assert!(resolved.loaded.unwrap().body.contains("deploy BODY"));

    assert!(
        registry
            .resolve_command_request("use /deploy", &[])
            .is_none()
    );
    assert!(
        registry
            .resolve_command_request("/Users/me/a.txt", &[])
            .is_none(),
        "an absolute path is not a skill command"
    );
    assert!(
        registry.resolve_command_request("/missing", &[]).is_none(),
        "an unknown plausible name is not a known Skill request"
    );
    assert!(
        registry
            .resolve_command_request("/help now", &["help"])
            .is_none(),
        "a built-in command keeps its bare name"
    );
}

#[test]
fn argument_hint_is_projected_into_the_command() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("repo");
    let user = tmp.path().join("home/.leveler/skills");
    write_skill(
        &project.join(".leveler/skills"),
        "deploy",
        "argument-hint: \"<env> [--force]\"\n",
    );
    let registry = registry(&project, &user);
    let command = registry
        .command_catalog(&[])
        .get("deploy")
        .cloned()
        .expect("deploy");
    assert_eq!(command.argument_hint.as_deref(), Some("<env> [--force]"));
}

#[test]
fn user_invocable_false_blocks_explicit_invocation_but_not_model_discovery() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("repo");
    let user = tmp.path().join("home/.leveler/skills");
    write_skill(
        &project.join(".leveler/skills"),
        "no-user",
        "user-invocable: false\n",
    );
    let registry = registry(&project, &user);

    assert!(registry.command_catalog(&[]).get("no-user").is_none());
    assert!(matches!(
        registry.resolve_command("no-user", &[]),
        Err(SkillCommandError::UserInvocationDisabled { .. })
    ));
    assert!(
        registry
            .resolve_mentions("$no-user")
            .invalid
            .iter()
            .any(|invalid| invalid.name == "no-user")
    );
    assert!(
        registry
            .model_summaries()
            .iter()
            .any(|summary| summary.name == "no-user"),
        "the model may still discover it"
    );
    assert!(registry.load_skill("no-user").is_ok());
}

#[test]
fn disable_model_invocation_blocks_model_loading_but_not_explicit_invocation() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("repo");
    let user = tmp.path().join("home/.leveler/skills");
    write_skill(
        &project.join(".leveler/skills"),
        "user-only",
        "disable-model-invocation: true\n",
    );
    let registry = registry(&project, &user);

    assert!(
        registry.command_catalog(&[]).get("user-only").is_some(),
        "explicit invocation stays available"
    );
    assert!(
        !registry
            .model_summaries()
            .iter()
            .any(|summary| summary.name == "user-only"),
        "the model index must not name it"
    );
    assert!(matches!(
        registry.load_skill("user-only"),
        Err(SkillLookupError::ModelInvocationDisabled { .. })
    ));
    assert!(registry.load_skill_explicit("user-only").is_ok());
    assert!(
        registry
            .resolve_command_request("/user-only", &[])
            .is_some_and(|resolved| resolved.is_loaded())
    );
    assert_eq!(registry.resolve_mentions("$user-only").loaded.len(), 1);
}

#[test]
fn both_flags_together_leave_inspection_only() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("repo");
    let user = tmp.path().join("home/.leveler/skills");
    write_skill(
        &project.join(".leveler/skills"),
        "private",
        "user-invocable: false\ndisable-model-invocation: true\n",
    );
    let registry = registry(&project, &user);

    assert!(registry.command_catalog(&[]).get("private").is_none());
    assert!(
        !registry
            .model_summaries()
            .iter()
            .any(|summary| summary.name == "private")
    );
    assert!(matches!(
        registry.load_skill("private"),
        Err(SkillLookupError::ModelInvocationDisabled { .. })
    ));
    assert!(matches!(
        registry.load_skill_explicit("private"),
        Err(SkillLookupError::UserInvocationDisabled { .. })
    ));
    let inspected = registry
        .load_skill_for_inspection("private")
        .expect("the page can still show the package");
    assert!(inspected.body.contains("private BODY"));
}
