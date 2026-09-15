use std::path::PathBuf;

use super::*;

struct Fx {
    _tmp: tempfile::TempDir,
    project: PathBuf,
    user: PathBuf,
}

impl Fx {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("repo");
        let user = tmp.path().join("home").join("agents");
        std::fs::create_dir_all(&project).unwrap();
        Self {
            _tmp: tmp,
            project,
            user,
        }
    }

    fn roots(&self) -> AgentRoots {
        AgentRoots {
            project_root: Some(self.project.clone()),
            user_agents_dir: Some(self.user.clone()),
        }
    }

    fn store(&self) -> AgentStore {
        AgentStore::new(self.roots())
    }

    fn registry(&self) -> AgentRegistry {
        AgentRegistry::load(&self.roots())
    }

    fn agents_dir(&self) -> PathBuf {
        project_agents_dir(&self.project)
    }
}

fn manifest(name: &str, capability: AgentCapability) -> AgentManifest {
    AgentManifest {
        version: AGENT_SCHEMA_VERSION,
        name: name.into(),
        description: format!("The {name} agent."),
        capability,
        model: None,
        reasoning_effort: None,
        skills: Vec::new(),
        tools: None,
        workspace: None,
        budget: None,
    }
}

fn listing(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|it| {
            it.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

#[test]
fn create_writes_exactly_the_two_files_and_the_registry_sees_them() {
    let fx = Fx::new();
    let def = fx
        .store()
        .create(
            AgentScope::Project,
            &manifest("security-reviewer", AgentCapability::ReadOnly),
            "Review auth.\n",
        )
        .unwrap();
    assert_eq!(def.source, AgentSource::Project);
    let dir = fx.agents_dir().join("security-reviewer");
    assert_eq!(listing(&dir), vec!["agent.yaml", "instructions.md"]);
    assert_eq!(
        std::fs::read_to_string(dir.join("instructions.md")).unwrap(),
        "Review auth.\n"
    );
    assert_eq!(
        listing(&fx.agents_dir()),
        vec!["security-reviewer"],
        "no staging left"
    );
    let reg = fx.registry();
    assert_eq!(
        reg.resolve("security-reviewer").unwrap().fingerprint,
        def.fingerprint
    );
}

#[test]
fn create_into_the_user_scope_writes_under_the_user_agents_dir() {
    let fx = Fx::new();
    fx.store()
        .create(
            AgentScope::User,
            &manifest("rust-explorer", AgentCapability::ReadOnly),
            "Explore.",
        )
        .unwrap();
    assert!(fx.user.join("rust-explorer").join("agent.yaml").is_file());
    assert_eq!(
        fx.registry().get("rust-explorer").unwrap().source,
        AgentSource::User
    );
}

#[test]
fn an_invalid_definition_writes_nothing() {
    let fx = Fx::new();
    let mut ro_with_roots = manifest("bad", AgentCapability::ReadOnly);
    ro_with_roots.workspace = Some(WorkspaceSpec {
        write_roots: vec!["web".into()],
    });
    let store = fx.store();
    for (m, instructions) in [
        (ro_with_roots, "x"),
        (
            manifest("empty-instructions", AgentCapability::ReadOnly),
            "  ",
        ),
        (manifest("Bad_Name", AgentCapability::ReadOnly), "x"),
        (manifest("worker", AgentCapability::ScopedWriter), "x"),
    ] {
        let err = store
            .create(AgentScope::Project, &m, instructions)
            .unwrap_err();
        assert!(!err.to_string().is_empty());
    }
    assert!(
        listing(&fx.agents_dir()).is_empty(),
        "{:?}",
        listing(&fx.agents_dir())
    );
}

#[test]
fn create_refuses_an_existing_agent_and_leaves_it_intact() {
    let fx = Fx::new();
    let store = fx.store();
    let m = manifest("dup", AgentCapability::ReadOnly);
    store.create(AgentScope::Project, &m, "first").unwrap();
    let err = store.create(AgentScope::Project, &m, "second").unwrap_err();
    assert!(err.to_string().contains("already exists"), "{err}");
    assert_eq!(fx.registry().resolve("dup").unwrap().instructions, "first");
    assert_eq!(listing(&fx.agents_dir()), vec!["dup"]);
}

#[test]
fn an_abandoned_staging_directory_is_never_a_visible_agent() {
    let fx = Fx::new();
    // What a crash between writing the files and the rename leaves behind.
    let staging = fx.agents_dir().join(".half.staging-123");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::write(
        staging.join("agent.yaml"),
        render_manifest(&manifest("half", AgentCapability::ReadOnly)).unwrap(),
    )
    .unwrap();
    let reg = fx.registry();
    assert!(reg.get("half").is_none());
    assert!(reg.problems().is_empty());
}

#[test]
fn update_replaces_both_files_together_and_changes_the_fingerprint() {
    let fx = Fx::new();
    let store = fx.store();
    let before = store
        .create(
            AgentScope::Project,
            &manifest("edit-me", AgentCapability::ReadOnly),
            "v1",
        )
        .unwrap();
    let mut m = manifest("edit-me", AgentCapability::Writer);
    m.description = "Now writes.".into();
    let after = store.update(AgentScope::Project, &m, "v2").unwrap();
    assert_ne!(before.fingerprint, after.fingerprint);
    let def = fx.registry();
    let def = def.resolve("edit-me").unwrap();
    assert_eq!(def.capability, AgentCapability::Writer);
    assert_eq!(def.instructions, "v2");
    assert_eq!(
        listing(&fx.agents_dir()),
        vec!["edit-me"],
        "no staging or backup left"
    );
}

#[test]
fn an_unchanged_manifest_keeps_the_users_yaml_bytes() {
    let fx = Fx::new();
    let store = fx.store();
    let m = manifest("commented", AgentCapability::ReadOnly);
    store.create(AgentScope::Project, &m, "v1").unwrap();
    let yaml_path = fx.agents_dir().join("commented").join("agent.yaml");
    let hand_written = format!(
        "# hand-written comment\n{}",
        std::fs::read_to_string(&yaml_path).unwrap()
    );
    std::fs::write(&yaml_path, &hand_written).unwrap();

    // Only the instructions change: the manifest file is carried over verbatim.
    store.update(AgentScope::Project, &m, "v2").unwrap();
    assert_eq!(std::fs::read_to_string(&yaml_path).unwrap(), hand_written);

    // Nothing changes: nothing is written at all.
    let mtime = std::fs::metadata(&yaml_path).unwrap().modified().unwrap();
    store.update(AgentScope::Project, &m, "v2").unwrap();
    assert_eq!(
        std::fs::metadata(&yaml_path).unwrap().modified().unwrap(),
        mtime
    );
}

#[test]
fn update_requires_the_agent_to_exist_in_that_scope() {
    let fx = Fx::new();
    let store = fx.store();
    store
        .create(
            AgentScope::User,
            &manifest("only-user", AgentCapability::ReadOnly),
            "u",
        )
        .unwrap();
    let err = store
        .update(
            AgentScope::Project,
            &manifest("only-user", AgentCapability::ReadOnly),
            "p",
        )
        .unwrap_err();
    assert!(err.to_string().contains("does not exist"), "{err}");
    let err = store
        .update(
            AgentScope::Project,
            &manifest("code-reviewer", AgentCapability::ReadOnly),
            "p",
        )
        .unwrap_err();
    assert!(err.to_string().contains("built-in"), "{err}");
}

#[test]
fn delete_removes_the_directory_and_lower_precedence_resolves_again() {
    let fx = Fx::new();
    let store = fx.store();
    store
        .create(
            AgentScope::User,
            &manifest("foo", AgentCapability::ReadOnly),
            "user",
        )
        .unwrap();
    store
        .create(
            AgentScope::Project,
            &manifest("foo", AgentCapability::ReadOnly),
            "project",
        )
        .unwrap();
    store.delete(AgentScope::Project, "foo").unwrap();
    assert!(listing(&fx.agents_dir()).is_empty());
    assert_eq!(fx.registry().resolve("foo").unwrap().instructions, "user");
    store.delete(AgentScope::User, "foo").unwrap();
    assert!(fx.registry().get("foo").is_none());
    assert!(
        store
            .delete(AgentScope::User, "foo")
            .unwrap_err()
            .to_string()
            .contains("does not exist")
    );
}

#[test]
fn mutation_refuses_names_that_escape_and_scopes_without_a_root() {
    let fx = Fx::new();
    let store = fx.store();
    for name in ["../x", "..", "a/b", ".hidden"] {
        assert!(store.delete(AgentScope::Project, name).is_err(), "{name}");
    }
    let headless = AgentStore::new(AgentRoots {
        project_root: Some(fx.project.clone()),
        user_agents_dir: None,
    });
    let err = headless
        .create(
            AgentScope::User,
            &manifest("u", AgentCapability::ReadOnly),
            "x",
        )
        .unwrap_err();
    assert!(err.to_string().contains("user"), "{err}");
}

#[cfg(unix)]
#[test]
fn mutation_refuses_a_symlinked_agents_directory_outside_the_project() {
    let fx = Fx::new();
    let outside = fx._tmp.path().join("elsewhere");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::create_dir_all(fx.project.join(".leveler")).unwrap();
    std::os::unix::fs::symlink(&outside, fx.agents_dir()).unwrap();
    let err = fx
        .store()
        .create(
            AgentScope::Project,
            &manifest("x", AgentCapability::ReadOnly),
            "x",
        )
        .unwrap_err();
    assert!(err.to_string().contains("outside the project"), "{err}");
    assert!(listing(&outside).is_empty());
}
