//! Durable enable/disable preferences: project isolation and fail-closed reads.

use std::path::Path;

use leveler_definition_trust::DefinitionTrustView;
use leveler_skills::{
    PreferencesState, SkillLookupError, SkillPreferences, SkillRegistry, SkillRoot, SkillRoots,
    SkillScope, SkillSource, SkillState, project_key,
};

fn write_skill(root: &Path, name: &str) {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {name} procedure.\n---\n\nBODY\n"),
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

fn write_preferences(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

#[test]
fn the_same_project_skill_name_is_isolated_between_projects() {
    let tmp = tempfile::tempdir().unwrap();
    let project_a = tmp.path().join("project-a");
    let project_b = tmp.path().join("project-b");
    let user = tmp.path().join("home/.leveler/skills");
    std::fs::create_dir_all(&project_a).unwrap();
    std::fs::create_dir_all(&project_b).unwrap();
    write_skill(&project_a.join(".leveler/skills"), "deploy");
    write_skill(&project_b.join(".leveler/skills"), "deploy");

    let key_a = project_key(&project_a);
    let key_b = project_key(&project_b);
    assert_ne!(key_a, key_b);
    let file = tmp.path().join("state/skills.toml");
    write_preferences(&file, &format!("[projects]\n\"{key_a}\" = [\"deploy\"]\n"));

    let prefs_a = PreferencesState::load_path(&file, Some(&project_a));
    let prefs_b = PreferencesState::load_path(&file, Some(&project_b));
    let registry_a = load(&roots(&project_a, &user), &prefs_a);
    let registry_b = load(&roots(&project_b, &user), &prefs_b);

    let entry_a = registry_a.get("deploy").expect("project A entry");
    assert!(
        matches!(entry_a.state, SkillState::Disabled { .. }),
        "{:?}",
        entry_a.state
    );
    assert!(matches!(
        registry_a.load_skill("deploy"),
        Err(SkillLookupError::Disabled { .. })
    ));
    assert!(
        registry_b
            .get("deploy")
            .is_some_and(|entry| entry.is_available()),
        "project B must not inherit project A's disable"
    );
    assert!(registry_b.load_skill("deploy").is_ok());
}

#[test]
fn disabling_a_project_skill_does_not_fall_back_to_the_user_copy() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("repo");
    let user = tmp.path().join("home/.leveler/skills");
    std::fs::create_dir_all(&project).unwrap();
    write_skill(&project.join(".leveler/skills"), "dup");
    write_skill(&user, "dup");

    let file = tmp.path().join("state/skills.toml");
    write_preferences(
        &file,
        &format!("[projects]\n\"{}\" = [\"dup\"]\n", project_key(&project)),
    );
    let prefs = PreferencesState::load_path(&file, Some(&project));
    let registry = load(&roots(&project, &user), &prefs);
    let entry = registry.get("dup").expect("project entry wins");
    assert_eq!(entry.scope, SkillScope::Project);
    assert!(matches!(entry.state, SkillState::Disabled { .. }));
    assert_eq!(entry.shadowed.len(), 1, "the user copy stays shadowed");
    assert!(matches!(
        registry.load_skill("dup"),
        Err(SkillLookupError::Disabled { .. })
    ));
    assert!(
        !registry
            .summaries()
            .iter()
            .any(|summary| summary.name == "dup"),
        "the disabled winner must not appear in any index"
    );
    assert!(
        !registry
            .model_summaries()
            .iter()
            .any(|summary| summary.name == "dup")
    );
    assert!(
        !registry
            .user_summaries()
            .iter()
            .any(|summary| summary.name == "dup")
    );
}

#[test]
fn user_and_builtin_identities_are_global() {
    let tmp = tempfile::tempdir().unwrap();
    let user = tmp.path().join("home/.leveler/skills");
    write_skill(&user, "release-notes");

    let file = tmp.path().join("state/skills.toml");
    write_preferences(
        &file,
        "disabled = [\"user:release-notes\", \"builtin:skill-creator\"]\n",
    );
    let prefs = PreferencesState::load_path(&file, None);
    let registry = load(&roots(&tmp.path().join("no-project"), &user), &prefs);
    assert!(matches!(
        registry.get("release-notes").map(|entry| &entry.state),
        Some(SkillState::Disabled { .. })
    ));
    assert!(matches!(
        registry.get("skill-creator").map(|entry| &entry.state),
        Some(SkillState::Disabled { .. })
    ));
}

#[test]
fn an_unreadable_preferences_file_fails_closed_and_is_reported() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("repo");
    std::fs::create_dir_all(&project).unwrap();
    write_skill(&project.join(".leveler/skills"), "deploy");
    let file = tmp.path().join("state/skills.toml");
    write_preferences(&file, "disabled = [\n");

    let prefs = PreferencesState::load_path(&file, Some(&project));
    assert!(prefs.error().is_some(), "the parse error must be visible");
    let registry = load(&roots(&project, &tmp.path().join("user")), &prefs);
    let entry = registry.get("deploy").expect("the entry stays listed");
    assert!(matches!(entry.state, SkillState::Disabled { .. }));
    assert!(registry.preferences_error().is_some());
    assert!(
        !registry
            .summaries()
            .iter()
            .any(|summary| summary.name == "deploy"),
        "fail-closed means the project entry is not in the index"
    );
    assert!(matches!(
        registry.load_skill("deploy"),
        Err(SkillLookupError::Disabled { reason, .. }) if reason.contains("could not be read")
    ));
}

#[test]
fn a_project_identity_in_the_global_list_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("state/skills.toml");
    write_preferences(&file, "disabled = [\"project:deploy\"]\n");
    let prefs = PreferencesState::load_path(&file, None);
    let error = prefs.error().expect("project: in disabled is ambiguous");
    assert!(error.contains("[projects]"), "{error}");
}

#[test]
fn preferences_round_trip_atomically() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("repo");
    std::fs::create_dir_all(&project).unwrap();
    let file = tmp.path().join("state/skills.toml");

    let mut preferences = SkillPreferences::default();
    preferences
        .set_disabled(SkillScope::User, "release-notes", None, true)
        .unwrap();
    preferences
        .set_disabled(
            SkillScope::Project,
            "deploy",
            Some(&project_key(&project)),
            true,
        )
        .unwrap();
    preferences.save(&file).unwrap();

    let loaded = PreferencesState::load_path(&file, Some(&project));
    assert!(loaded.error().is_none(), "{:?}", loaded.error());
    assert!(loaded.is_disabled(SkillScope::User, "release-notes"));
    assert!(loaded.is_disabled(SkillScope::Project, "deploy"));
    assert!(!loaded.is_disabled(SkillScope::Project, "other"));

    let staged = file.with_extension("toml.tmp");
    assert!(!staged.exists(), "the staged sibling must be renamed away");
}

#[test]
fn a_project_toggle_requires_a_project_key() {
    let mut preferences = SkillPreferences::default();
    assert!(
        preferences
            .set_disabled(SkillScope::Project, "deploy", None, true)
            .is_err(),
        "a project toggle without a project would become a global disable"
    );
    assert!(
        preferences
            .set_disabled(SkillScope::User, "deploy", None, true)
            .is_ok()
    );
}

#[test]
fn a_missing_file_is_an_empty_preference_set() {
    let file = tempfile::tempdir()
        .unwrap()
        .path()
        .join("state/skills.toml");
    let prefs = PreferencesState::load_path(&file, None);
    assert!(prefs.error().is_none());
    assert_eq!(prefs.preferences(), Some(&SkillPreferences::default()));
}
