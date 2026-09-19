//! The resolved skill read model: identity, precedence, status and loading.
//!
//! Roots are constructed explicitly (never from the machine's real home) so the
//! tests describe the registry's rules, not the developer's `~`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use leveler_skills::{
    SkillDraft, SkillFile, SkillLookupError, SkillRegistry, SkillRoot, SkillRoots, SkillScope,
    SkillSource, SkillState, SkillStore,
};

fn write_skill(root: &Path, relative: &str, name: &str, description: &str, body: &str) {
    let dir = root.join(relative);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}\n"),
    )
    .unwrap();
}

fn write_raw(root: &Path, relative: &str, contents: &[u8]) {
    let dir = root.join(relative);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), contents).unwrap();
}

fn root(scope: SkillScope, source: SkillSource, dir: PathBuf) -> SkillRoot {
    SkillRoot {
        scope,
        source,
        dir,
        containing_root: None,
    }
}

/// A project-native root plus a user-native root plus a builtin, all isolated.
struct Fx {
    _tmp: tempfile::TempDir,
    project: PathBuf,
    user: PathBuf,
}

impl Fx {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("repo/.leveler/skills");
        let user = tmp.path().join("home/.leveler/skills");
        Self {
            _tmp: tmp,
            project,
            user,
        }
    }

    fn roots(&self) -> SkillRoots {
        let mut roots = SkillRoots::empty();
        roots.push(root(
            SkillScope::Project,
            SkillSource::Native,
            self.project.clone(),
        ));
        roots.push(root(
            SkillScope::User,
            SkillSource::Native,
            self.user.clone(),
        ));
        roots
    }

    fn registry(&self) -> SkillRegistry {
        SkillRegistry::load(&self.roots())
    }
}

#[test]
fn discovers_project_and_user_and_builtin() {
    let fx = Fx::new();
    write_skill(
        &fx.project,
        "deploy",
        "deploy",
        "Ship safely.",
        "Run make deploy.",
    );
    write_skill(&fx.user, "frontend", "frontend", "Design UI.", "Do design.");
    let registry = fx.registry();
    assert_eq!(registry.get("deploy").unwrap().scope, SkillScope::Project);
    assert_eq!(registry.get("frontend").unwrap().scope, SkillScope::User);
    let creator = registry.get("skill-creator").expect("builtin");
    assert_eq!(creator.scope, SkillScope::Builtin);
    assert!(registry.get("deploy").unwrap().is_available());
}

#[test]
fn directory_name_is_the_canonical_identity() {
    let fx = Fx::new();
    write_skill(
        &fx.project,
        "rust-review",
        "rust-review",
        "Review Rust.",
        "Body.",
    );
    let registry = fx.registry();
    let detail = registry.load_skill("rust-review").unwrap();
    assert_eq!(detail.name, "rust-review");
    assert!(registry.load_skill("rust-security-review").is_err());
}

#[test]
fn a_manifest_name_mismatch_is_invalid_and_does_not_load() {
    let fx = Fx::new();
    write_skill(
        &fx.project,
        "rust-review",
        "rust-security-review",
        "Review Rust.",
        "Body.",
    );
    let registry = fx.registry();
    let entry = registry
        .get("rust-review")
        .expect("entry by directory name");
    assert_eq!(
        entry.state,
        SkillState::Invalid(
            "name mismatch: directory `rust-review`, manifest `rust-security-review`".into()
        )
    );
    // The wrong name resolves to nothing, not to the mismatched package.
    assert!(matches!(
        registry.resolve("rust-security-review"),
        Err(SkillLookupError::NotFound { .. })
    ));
    assert!(matches!(
        registry.resolve("rust-review"),
        Err(SkillLookupError::Invalid { .. })
    ));
}

#[test]
fn invalid_higher_precedence_does_not_fall_back_to_a_lower_one() {
    let fx = Fx::new();
    write_skill(
        &fx.project,
        "release",
        "release",
        "Project release.",
        "Project body.",
    );
    write_skill(
        &fx.user,
        "release",
        "release",
        "User release.",
        "User body.",
    );
    // Break the project copy after both exist.
    write_raw(&fx.project, "release", b"---\nname: [broken\n---\n");
    let registry = fx.registry();
    let entry = registry.get("release").unwrap();
    assert_eq!(entry.scope, SkillScope::Project);
    assert!(matches!(entry.state, SkillState::Invalid(_)), "{entry:?}");
    // The user copy is recorded as shadowed, and resolve refuses.
    assert_eq!(entry.shadowed.len(), 1);
    assert_eq!(entry.shadowed[0].scope, SkillScope::User);
    assert!(matches!(
        registry.resolve("release"),
        Err(SkillLookupError::Invalid { .. })
    ));
}

#[test]
fn every_consumer_path_resolves_the_same_winner() {
    let fx = Fx::new();
    write_skill(
        &fx.project,
        "rust-review",
        "rust-review",
        "Project review.",
        "PROJECT",
    );
    write_skill(
        &fx.user,
        "rust-review",
        "rust-review",
        "User review.",
        "USER",
    );
    let registry = fx.registry();
    // The index.
    let summary = registry
        .summaries()
        .into_iter()
        .find(|s| s.name == "rust-review")
        .unwrap();
    assert_eq!(summary.description, "Project review.");
    // resolve (what Agent.skills and load_skill both go through).
    assert_eq!(
        registry.resolve("rust-review").unwrap().scope,
        SkillScope::Project
    );
    // load_skill.
    assert_eq!(
        registry.load_skill("rust-review").unwrap().body.trim(),
        "PROJECT"
    );
    // $mention turn injection.
    let resolution = registry.resolve_mentions("do $rust-review now");
    assert_eq!(resolution.loaded.len(), 1);
    assert_eq!(resolution.loaded[0].body.trim(), "PROJECT");
    assert_eq!(resolution.loaded[0].name, summary.name);
}

#[test]
fn project_shadows_user_and_both_are_visible() {
    let fx = Fx::new();
    write_skill(&fx.project, "lint", "lint", "Project lint.", "P.");
    write_skill(&fx.user, "lint", "lint", "User lint.", "U.");
    let registry = fx.registry();
    let entry = registry.get("lint").unwrap();
    assert_eq!(entry.scope, SkillScope::Project);
    assert_eq!(entry.description.as_deref(), Some("Project lint."));
    assert_eq!(entry.shadowed.len(), 1);
    assert_eq!(entry.shadowed[0].scope, SkillScope::User);
    assert!(registry.load_skill("lint").unwrap().body.contains('P'));
}

#[test]
fn malformed_packages_are_invalid_with_a_reason() {
    let fx = Fx::new();
    write_raw(&fx.project, "no-frontmatter", b"# body only\n");
    write_raw(&fx.project, "bad-yaml", b"---\nname: [oops\n---\nbody\n");
    write_raw(
        &fx.project,
        "no-description",
        b"---\nname: no-description\n---\nbody\n",
    );
    write_raw(
        &fx.project,
        "bad-utf8",
        b"---\nname: bad-utf8\n---\n\xff\xfe\n",
    );
    std::fs::create_dir_all(fx.project.join("no-skill-md")).unwrap();
    let registry = fx.registry();
    for name in [
        "no-frontmatter",
        "bad-yaml",
        "no-description",
        "bad-utf8",
        "no-skill-md",
    ] {
        let entry = registry.get(name).expect(name);
        assert!(
            matches!(entry.state, SkillState::Invalid(_)),
            "{name} must be invalid: {entry:?}"
        );
        assert!(!entry.is_available());
    }
    // A broken package never fails discovery as a whole.
    assert!(registry.get("skill-creator").is_some());
}

#[test]
fn an_unreferenceable_directory_name_is_a_problem_not_an_entry() {
    let fx = Fx::new();
    std::fs::create_dir_all(fx.project.join(".hidden")).unwrap();
    std::fs::create_dir_all(fx.project.join("has space")).unwrap();
    let registry = fx.registry();
    assert!(registry.get("has space").is_none());
    assert!(
        registry
            .problems()
            .iter()
            .any(|p| p.error.contains("unusable"))
    );
    // Hidden staging directories are ignored entirely.
    assert!(
        !registry
            .problems()
            .iter()
            .any(|p| p.location.ends_with(".hidden"))
    );
}

#[test]
fn external_roots_are_discovered_from_the_injected_environment() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    write_skill(
        &home.join(".agents/skills"),
        "agents-skill",
        "agents-skill",
        "From agents.",
        "A.",
    );
    write_skill(
        &home.join(".codex/skills"),
        "codex-skill",
        "codex-skill",
        "From codex.",
        "C.",
    );
    write_skill(
        &home.join(".claude/skills"),
        "claude-skill",
        "claude-skill",
        "From claude.",
        "L.",
    );
    write_skill(
        &home.join(".leveler/skills"),
        "native",
        "native",
        "From native.",
        "N.",
    );
    let var = |key: &str| -> Option<OsString> {
        match key {
            "HOME" | "USERPROFILE" => Some(home.clone().into_os_string()),
            _ => None,
        }
    };
    let roots = SkillRoots::for_project_in(tmp.path(), &var);
    let registry = SkillRegistry::load(&roots);
    for (name, source) in [
        ("agents-skill", SkillSource::AgentSkills),
        ("codex-skill", SkillSource::Codex),
        ("claude-skill", SkillSource::Claude),
        ("native", SkillSource::Native),
    ] {
        let entry = registry
            .get(name)
            .unwrap_or_else(|| panic!("{name} missing"));
        assert_eq!(entry.source, source, "{name}");
        assert_eq!(entry.scope, SkillScope::User);
        assert!(entry.is_available());
    }
}

#[test]
fn user_native_wins_a_same_scope_collision_deterministically() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    write_skill(
        &home.join(".leveler/skills"),
        "fanui",
        "fanui",
        "Native.",
        "N.",
    );
    write_skill(
        &home.join(".codex/skills"),
        "fanui",
        "fanui",
        "Codex.",
        "C.",
    );
    write_skill(
        &home.join(".agents/skills"),
        "fanui",
        "fanui",
        "Agents.",
        "A.",
    );
    let registry = SkillRegistry::load(&SkillRoots::for_project_in(
        &tmp.path().join("repo"),
        &|key: &str| match key {
            "HOME" | "USERPROFILE" => Some(home.clone().into_os_string()),
            _ => None,
        },
    ));
    let entry = registry.get("fanui").unwrap();
    assert_eq!(entry.source, SkillSource::Native);
    assert_eq!(entry.shadowed.len(), 2);
    let shadowed: Vec<SkillSource> = entry.shadowed.iter().map(|s| s.source).collect();
    assert_eq!(shadowed, vec![SkillSource::Codex, SkillSource::AgentSkills]);
}

#[test]
fn index_carries_no_body_and_load_does() {
    let fx = Fx::new();
    write_skill(&fx.project, "pack", "pack", "Pack it.", "UNIQUE_BODY_42");
    let registry = fx.registry();
    let index = leveler_skills::render_index(&registry.summaries());
    assert!(index.contains("pack"));
    assert!(!index.contains("UNIQUE_BODY_42"), "{index}");
    let loaded = leveler_skills::render_skill_package(&registry.load_skill("pack").unwrap());
    assert!(loaded.contains("UNIQUE_BODY_42"));
}

#[test]
fn mentions_resolve_through_the_registry() {
    let fx = Fx::new();
    write_skill(&fx.project, "deploy", "deploy", "Ship.", "DEPLOY_BODY");
    let registry = fx.registry();
    let resolution = registry.resolve_mentions("please use $deploy and $missing");
    assert_eq!(resolution.loaded.len(), 1);
    assert_eq!(resolution.loaded[0].name, "deploy");
    assert_eq!(resolution.unknown, vec!["missing"]);
}

#[test]
fn mentions_of_an_invalid_skill_are_reported_separately() {
    let fx = Fx::new();
    write_raw(&fx.project, "broken", b"---\nname: [oops\n---\nx\n");
    let resolution = fx.registry().resolve_mentions("$broken");
    assert!(resolution.loaded.is_empty());
    assert!(resolution.unknown.is_empty());
    assert_eq!(resolution.invalid.len(), 1);
    assert_eq!(resolution.invalid[0].0, "broken");
}

#[test]
fn bundled_files_are_classified_relative_to_the_package() {
    let fx = Fx::new();
    write_skill(&fx.project, "pack", "pack", "Pack it.", "Body.");
    let dir = fx.project.join("pack");
    std::fs::create_dir_all(dir.join("scripts")).unwrap();
    std::fs::create_dir_all(dir.join("references")).unwrap();
    std::fs::write(dir.join("scripts/run.sh"), "echo hi\n").unwrap();
    std::fs::write(dir.join("references/note.md"), "note\n").unwrap();
    std::fs::write(dir.join("extra.txt"), "x\n").unwrap();
    let detail = fx.registry().load_skill("pack").unwrap();
    assert_eq!(detail.scripts, vec!["scripts/run.sh".to_string()]);
    assert_eq!(detail.references, vec!["references/note.md".to_string()]);
    assert_eq!(detail.other_files, vec!["extra.txt".to_string()]);
}

#[test]
fn a_project_root_that_escapes_the_project_loads_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let outside = tmp.path().join("outside");
    write_skill(&outside, "escape", "escape", "Escaped.", "Body.");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let mut roots = SkillRoots::empty();
    roots.push(SkillRoot {
        scope: SkillScope::Project,
        source: SkillSource::Native,
        dir: outside.clone(),
        containing_root: Some(repo),
    });
    let registry = SkillRegistry::load(&roots);
    assert!(registry.get("escape").is_none());
    assert!(
        registry
            .problems()
            .iter()
            .any(|p| p.error.contains("outside the project")),
        "{:?}",
        registry.problems()
    );
}

#[test]
fn external_skills_are_read_but_the_store_never_touches_them() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    write_skill(
        &home.join(".agents/skills"),
        "external",
        "external",
        "External.",
        "Body.",
    );
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let roots = SkillRoots::for_project_in(&repo, &|key: &str| match key {
        "HOME" | "USERPROFILE" => Some(home.clone().into_os_string()),
        _ => None,
    });
    let registry = SkillRegistry::load(&roots);
    assert!(registry.load_skill("external").is_ok());

    // The store only manages the native store: deleting the external name fails
    // and leaves the file in place.
    let store = SkillStore::new(Some(repo.clone()), Some(home.join(".leveler/skills")));
    assert!(store.delete(SkillScope::User, "external").is_err());
    assert!(home.join(".agents/skills/external/SKILL.md").is_file());
}

#[test]
fn store_create_update_delete_round_trips() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let user = tmp.path().join("home/.leveler/skills");
    let store = SkillStore::new(Some(repo.clone()), Some(user.clone()));

    let draft = SkillDraft {
        name: "windows-ci-debug".into(),
        description: "Diagnose intermittent Windows CI failures.".into(),
        body: "1. Reproduce.\n2. Bisect.".into(),
        files: vec![SkillFile {
            path: "references/checklist.md".into(),
            content: "checklist\n".into(),
        }],
    };
    let dir = store.create(SkillScope::Project, &draft).unwrap();
    assert!(dir.join("SKILL.md").is_file());
    assert!(dir.join("references/checklist.md").is_file());
    // No staging leftover.
    let entries: Vec<String> = std::fs::read_dir(repo.join(".leveler/skills"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries, vec!["windows-ci-debug"]);

    // Duplicate create is refused.
    assert!(store.create(SkillScope::Project, &draft).is_err());

    let updated = SkillDraft {
        body: "1. Reproduce.\n2. Bisect.\n3. Report.".into(),
        files: Vec::new(),
        ..draft.clone()
    };
    store.update(SkillScope::Project, &updated).unwrap();
    let registry = SkillRegistry::load(&{
        let mut roots = SkillRoots::empty();
        roots.push(root(
            SkillScope::Project,
            SkillSource::Native,
            repo.join(".leveler/skills"),
        ));
        roots.push(root(SkillScope::User, SkillSource::Native, user.clone()));
        roots
    });
    let body = &registry.load_skill("windows-ci-debug").unwrap().body;
    assert!(body.contains("3. Report."));
    assert!(
        !dir.join("references/checklist.md").exists(),
        "replace removes files not in the draft"
    );

    store
        .delete(SkillScope::Project, "windows-ci-debug")
        .unwrap();
    assert!(!dir.exists());
}

#[test]
fn store_refuses_invalid_and_out_of_scope_proposals() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let store = SkillStore::new(Some(repo.clone()), None);
    let base = SkillDraft {
        name: "ok".into(),
        description: "desc".into(),
        body: "body".into(),
        files: Vec::new(),
    };
    for (draft, why) in [
        (
            SkillDraft {
                name: "Bad Name".into(),
                ..base.clone()
            },
            "name",
        ),
        (
            SkillDraft {
                description: String::new(),
                ..base.clone()
            },
            "description",
        ),
        (
            SkillDraft {
                description: "two\nlines".into(),
                ..base.clone()
            },
            "single line",
        ),
        (
            SkillDraft {
                body: "  ".into(),
                ..base.clone()
            },
            "body",
        ),
        (
            SkillDraft {
                files: vec![SkillFile {
                    path: "../escape".into(),
                    content: "x".into(),
                }],
                ..base.clone()
            },
            "stay inside",
        ),
        (
            SkillDraft {
                files: vec![SkillFile {
                    path: "/abs".into(),
                    content: "x".into(),
                }],
                ..base.clone()
            },
            "relative",
        ),
        (
            SkillDraft {
                files: vec![SkillFile {
                    path: "SKILL.md".into(),
                    content: "x".into(),
                }],
                ..base.clone()
            },
            "SKILL.md",
        ),
    ] {
        let error = store.create(SkillScope::Project, &draft).unwrap_err();
        assert!(error.to_string().contains(why), "wanted {why:?} in {error}");
    }
    // Nothing was written by any refused proposal.
    assert!(!repo.join(".leveler/skills/ok").exists());
}

#[test]
fn store_user_scope_writes_under_the_user_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let user = tmp.path().join("home/.leveler/skills");
    let store = SkillStore::new(Some(repo), Some(user.clone()));
    store
        .create(
            SkillScope::User,
            &SkillDraft {
                name: "personal".into(),
                description: "Personal workflow.".into(),
                body: "Do it.".into(),
                files: Vec::new(),
            },
        )
        .unwrap();
    assert!(user.join("personal/SKILL.md").is_file());
}
