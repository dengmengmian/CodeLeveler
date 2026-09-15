use std::path::{Path, PathBuf};

use super::*;

struct Fixture {
    _tmp: tempfile::TempDir,
    project: PathBuf,
    user: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("repo");
        let user = tmp.path().join("home").join("agents");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&user).unwrap();
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

    fn load(&self) -> AgentRegistry {
        AgentRegistry::load(&self.roots())
    }

    fn project_agent(&self, name: &str, yaml: &str, instructions: &str) -> PathBuf {
        write_agent(&project_agents_dir(&self.project), name, yaml, instructions)
    }

    fn user_agent(&self, name: &str, yaml: &str, instructions: &str) -> PathBuf {
        write_agent(&self.user, name, yaml, instructions)
    }
}

fn write_agent(root: &Path, name: &str, yaml: &str, instructions: &str) -> PathBuf {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(MANIFEST_FILE), yaml).unwrap();
    std::fs::write(dir.join(INSTRUCTIONS_FILE), instructions).unwrap();
    dir
}

fn yaml(name: &str, extra: &str) -> String {
    format!(
        "version: 1\nname: {name}\ndescription: Does {name} things.\ncapability: read_only\n{extra}"
    )
}

fn invalid_reason(reg: &AgentRegistry, name: &str) -> String {
    match &reg
        .get(name)
        .unwrap_or_else(|| panic!("{name} missing"))
        .state
    {
        AgentState::Invalid(e) => e.clone(),
        AgentState::Valid(_) => panic!("{name} should be invalid"),
    }
}

// ── Discovery & precedence ──────────────────────────────────────────────────

#[test]
fn builtins_are_present_with_no_files() {
    let fx = Fixture::new();
    let reg = fx.load();
    for name in ["default", "explorer", "worker", "reviewer"] {
        let def = reg.get(name).unwrap().definition().unwrap();
        assert_eq!(def.source, AgentSource::Builtin);
        assert!(def.is_structural());
    }
    for name in builtin::persona_names() {
        let def = reg.get(name).unwrap().definition().unwrap();
        assert_eq!(def.source, AgentSource::Builtin);
        assert!(!def.is_structural());
        assert!(!def.instructions.trim().is_empty());
    }
}

#[test]
fn every_shipped_persona_passes_the_user_schema() {
    for (name, manifest, instructions) in builtin::persona_sources() {
        let m = parse_manifest(manifest).unwrap_or_else(|e| panic!("{name}: {e}"));
        resolve_manifest(&m, instructions, name, AgentSource::Builtin, None)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
    }
}

#[test]
fn the_structural_builtins_carry_their_runtime_contract() {
    let reg = Fixture::new().load();
    let cap = |n: &str| reg.get(n).unwrap().definition().unwrap().capability;
    assert_eq!(cap("default"), AgentCapability::Writer);
    assert_eq!(cap("explorer"), AgentCapability::ReadOnly);
    assert_eq!(cap("worker"), AgentCapability::ScopedWriter);
    assert_eq!(cap("reviewer"), AgentCapability::ReadOnly);
    for role in [
        AgentRole::Default,
        AgentRole::Explorer,
        AgentRole::Worker,
        AgentRole::Reviewer,
    ] {
        let def = reg.get(role.label()).unwrap().definition().unwrap();
        assert_eq!(def.child_profile(), ChildProfile::resolve(role));
    }
}

#[test]
fn user_and_project_agents_are_discovered_with_source_and_location() {
    let fx = Fixture::new();
    let p = fx.project_agent(
        "security-reviewer",
        &yaml("security-reviewer", ""),
        "Review.",
    );
    let u = fx.user_agent("rust-explorer", &yaml("rust-explorer", ""), "Explore.");
    let reg = fx.load();
    let project = reg.get("security-reviewer").unwrap();
    assert_eq!(project.source, AgentSource::Project);
    assert_eq!(project.location.as_deref(), Some(p.as_path()));
    assert!(project.definition().is_some());
    let user = reg.get("rust-explorer").unwrap();
    assert_eq!(user.source, AgentSource::User);
    assert_eq!(user.location.as_deref(), Some(u.as_path()));
}

#[test]
fn project_beats_user_beats_builtin_and_the_losers_are_recorded() {
    let fx = Fixture::new();
    fx.user_agent("foo", &yaml("foo", ""), "user foo");
    fx.project_agent("foo", &yaml("foo", ""), "project foo");
    fx.user_agent("code-reviewer", &yaml("code-reviewer", ""), "user reviewer");
    let reg = fx.load();

    let foo = reg.get("foo").unwrap();
    assert_eq!(foo.source, AgentSource::Project);
    assert_eq!(foo.definition().unwrap().instructions, "project foo");
    assert_eq!(foo.shadowed.len(), 1);
    assert_eq!(foo.shadowed[0].source, AgentSource::User);

    let cr = reg.get("code-reviewer").unwrap();
    assert_eq!(cr.source, AgentSource::User);
    assert_eq!(cr.shadowed[0].source, AgentSource::Builtin);

    // Delete the project definition: the user one is what resolves now.
    std::fs::remove_dir_all(project_agents_dir(&fx.project).join("foo")).unwrap();
    let reg = fx.load();
    assert_eq!(reg.get("foo").unwrap().source, AgentSource::User);
    assert_eq!(
        reg.resolve("foo").unwrap().instructions,
        "user foo",
        "refresh after delete falls to the next precedence"
    );
}

#[test]
fn an_invalid_higher_precedence_definition_does_not_fall_back() {
    let fx = Fixture::new();
    fx.user_agent("foo", &yaml("foo", ""), "user foo");
    fx.project_agent("foo", &yaml("foo", "wirte: true\n"), "project foo");
    let reg = fx.load();
    let err = reg.resolve("foo").unwrap_err();
    match &err {
        ResolveError::Invalid { source, error, .. } => {
            assert_eq!(*source, AgentSource::Project);
            assert!(error.contains("wirte"), "{error}");
        }
        other => panic!("expected Invalid, got {other:?}"),
    }
    assert!(err.to_string().contains("project agent"), "{err}");
}

#[test]
fn the_structural_names_are_reserved() {
    let fx = Fixture::new();
    for name in RESERVED_AGENT_NAMES {
        fx.project_agent(name, &yaml(name, ""), "take over");
    }
    let reg = fx.load();
    for name in RESERVED_AGENT_NAMES {
        let entry = reg.get(name).unwrap();
        assert_eq!(
            entry.source,
            AgentSource::Builtin,
            "{name} must stay built-in"
        );
        assert!(entry.definition().unwrap().is_structural());
    }
    assert_eq!(reg.problems().len(), RESERVED_AGENT_NAMES.len());
    assert!(reg.problems()[0].error.contains("cannot be overridden"));
}

#[test]
fn registry_order_is_deterministic_by_name() {
    let fx = Fixture::new();
    for n in ["zeta", "alpha", "mid"] {
        fx.project_agent(n, &yaml(n, ""), "x");
    }
    let names: Vec<String> = fx.load().entries().iter().map(|e| e.name.clone()).collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted);
    assert_eq!(
        names,
        fx.load()
            .entries()
            .iter()
            .map(|e| e.name.clone())
            .collect::<Vec<_>>()
    );
}

#[test]
fn one_bad_agent_does_not_hide_the_others() {
    let fx = Fixture::new();
    fx.project_agent("good", &yaml("good", ""), "fine");
    fx.project_agent("bad", "version: 1\nname: [", "fine");
    let reg = fx.load();
    assert!(reg.resolve("good").is_ok());
    assert!(reg.resolve("bad").is_err());
}

#[test]
fn a_missing_agent_is_not_found_with_suggestions_and_never_defaults() {
    let fx = Fixture::new();
    fx.project_agent("security-reviewer", &yaml("security-reviewer", ""), "x");
    let err = fx.load().resolve("security").unwrap_err();
    let text = err.to_string();
    assert!(text.contains("Agent \"security\" not found."), "{text}");
    assert!(
        text.contains("Matching agents: security-reviewer."),
        "{text}"
    );
    assert!(!matches!(err, ResolveError::Invalid { .. }));
}

#[test]
fn the_reviewer_resolves_as_harness_only() {
    let reg = Fixture::new().load();
    assert!(matches!(
        reg.resolve("reviewer"),
        Err(ResolveError::HarnessOnly { .. })
    ));
    assert!(!reg.spawnable_names().contains(&"reviewer".to_string()));
}

#[test]
fn hidden_entries_and_plain_files_are_ignored() {
    let fx = Fixture::new();
    let dir = project_agents_dir(&fx.project);
    write_agent(&dir, ".staging-foo", &yaml("foo", ""), "x");
    std::fs::write(dir.join("old-style.md"), "---\nname: old-style\n---\nbody").unwrap();
    let reg = fx.load();
    assert!(reg.get("foo").is_none());
    assert!(reg.get("old-style").is_none());
    assert!(reg.problems().is_empty(), "{:?}", reg.problems());
}

#[test]
fn a_project_copied_to_another_repo_loads_the_same_agent() {
    let a = Fixture::new();
    let b = Fixture::new();
    a.project_agent(
        "portable",
        &yaml("portable", "skills: [feature-dev]\n"),
        "same",
    );
    let src = project_agents_dir(&a.project).join("portable");
    let dst = project_agents_dir(&b.project).join("portable");
    std::fs::create_dir_all(&dst).unwrap();
    for f in [MANIFEST_FILE, INSTRUCTIONS_FILE] {
        std::fs::copy(src.join(f), dst.join(f)).unwrap();
    }
    let fa = a.load().resolve("portable").unwrap().fingerprint.clone();
    let fb = b.load().resolve("portable").unwrap().fingerprint.clone();
    assert_eq!(fa, fb, "no machine path enters the identity");
}

// ── Validation ──────────────────────────────────────────────────────────────

#[test]
fn unknown_fields_fail_closed_at_every_level() {
    let fx = Fixture::new();
    let cases = [
        ("top", yaml("top", "wirte: true\n")),
        (
            "nested-ws",
            "version: 1\nname: nested-ws\ndescription: d\ncapability: writer\nworkspace:\n  wirte_roots: [web]\n"
                .to_string(),
        ),
        ("nested-budget", yaml("nested-budget", "budget:\n  max_round: 3\n")),
        ("secret", yaml("secret", "api_key: sk-123\n")),
    ];
    for (name, text) in &cases {
        fx.project_agent(name, text, "x");
    }
    let reg = fx.load();
    for (name, _) in &cases {
        let reason = invalid_reason(&reg, name);
        assert!(reason.contains("unknown field"), "{name}: {reason}");
    }
}

#[test]
fn structural_validation_refuses_each_bad_field() {
    let fx = Fixture::new();
    let cases: &[(&str, String, &str)] = &[
        ("no-version", "name: no-version\ndescription: d\ncapability: read_only\n".into(), "version"),
        ("v2", "version: 2\nname: v2\ndescription: d\ncapability: read_only\n".into(), "unsupported version"),
        ("mismatch", yaml("other-name", ""), "does not match"),
        ("no-desc", "version: 1\nname: no-desc\ndescription: '  '\ncapability: read_only\n".into(), "description"),
        ("long-desc", format!("version: 1\nname: long-desc\ndescription: {}\ncapability: read_only\n", "x".repeat(201)), "200"),
        ("bad-cap", "version: 1\nname: bad-cap\ndescription: d\ncapability: admin\n".into(), "capability"),
        ("bad-model", yaml("bad-model", "model: gpt5\n"), "provider/model"),
        ("bad-effort", yaml("bad-effort", "reasoning_effort: extreme\n"), "reasoning_effort"),
        ("bad-skill", yaml("bad-skill", "skills: ['../x']\n"), "skill"),
        ("dup-skill", yaml("dup-skill", "skills: [a, a]\n"), "twice"),
        ("unknown-tool", yaml("unknown-tool", "tools: [read_file, teleport]\n"), "unknown tool `teleport`"),
        ("empty-tools", yaml("empty-tools", "tools: []\n"), "empty"),
        ("ro-write-tool", yaml("ro-write-tool", "tools: [apply_patch]\n"), "not read-only"),
        ("ro-command", yaml("ro-command", "tools: [run_command]\n"), "not read-only"),
        ("mcp-tool", "version: 1\nname: mcp-tool\ndescription: d\ncapability: writer\ntools: [mcp__github__push]\n".into(), "MCP"),
        ("ro-roots", yaml("ro-roots", "workspace:\n  write_roots: [web]\n"), "read_only agent cannot"),
        ("zero-rounds", yaml("zero-rounds", "budget:\n  max_rounds: 0\n"), "max_rounds"),
        ("long-duration", yaml("long-duration", "budget:\n  max_duration_secs: 99999\n"), "max_duration_secs"),
    ];
    for (dir, text, _) in cases {
        fx.project_agent(dir, text, "x");
    }
    let reg = fx.load();
    for (dir, _, needle) in cases {
        let reason = invalid_reason(&reg, dir);
        assert!(
            reason.contains(needle),
            "{dir}: expected `{needle}` in `{reason}`"
        );
    }
}

#[test]
fn write_roots_must_be_bounded_repository_relative_paths() {
    for (root, ok) in [
        ("web", true),
        ("web/src/", true),
        ("crates/leveler-web/web", true),
        ("", false),
        (".", false),
        ("/etc", false),
        ("../outside", false),
        ("web/../../x", false),
        ("web//src", false),
        ("C:/x", false),
        ("web\\src", false),
        ("web/**", false),
        ("src/*.rs", false),
    ] {
        let m = parse_manifest(&format!(
            "version: 1\nname: w\ndescription: d\ncapability: writer\nworkspace:\n  write_roots: ['{root}']\n"
        ))
        .unwrap();
        assert_eq!(m.check("w").is_ok(), ok, "write_root `{root}`");
    }
}

#[test]
fn instructions_are_required_bounded_and_text() {
    let fx = Fixture::new();
    let dir = project_agents_dir(&fx.project);
    fx.project_agent("empty", &yaml("empty", ""), "  \n");
    fx.project_agent(
        "huge",
        &yaml("huge", ""),
        &"x".repeat(MAX_INSTRUCTIONS_BYTES + 1),
    );
    fx.project_agent("binary", &yaml("binary", ""), "a\0b");
    fx.project_agent(
        "at-limit",
        &yaml("at-limit", ""),
        &"x".repeat(MAX_INSTRUCTIONS_BYTES),
    );
    std::fs::create_dir_all(dir.join("missing")).unwrap();
    std::fs::write(dir.join("missing").join(MANIFEST_FILE), yaml("missing", "")).unwrap();
    let reg = fx.load();
    assert!(invalid_reason(&reg, "empty").contains("empty"));
    assert!(invalid_reason(&reg, "huge").contains("limit"));
    assert!(invalid_reason(&reg, "binary").contains("binary"));
    assert!(invalid_reason(&reg, "missing").contains("instructions.md is missing"));
    assert!(reg.resolve("at-limit").is_ok());
}

#[test]
fn a_complete_manifest_resolves_every_field() {
    let fx = Fixture::new();
    fx.project_agent(
        "frontend-worker",
        "version: 1\nname: frontend-worker\ndescription: React changes under web.\ncapability: scoped_writer\nmodel: deepseek/deepseek-v4-flash\nreasoning_effort: high\nskills: [feature-dev]\ntools: [read_file, grep, apply_patch, run_command]\nworkspace:\n  write_roots: [web/]\nbudget:\n  max_rounds: 40\n  max_duration_secs: 600\n",
        "Work on the frontend.",
    );
    let reg = fx.load();
    let def = reg.resolve("frontend-worker").unwrap();
    assert_eq!(def.capability, AgentCapability::ScopedWriter);
    assert_eq!(
        def.model,
        Some(ModelRef::new("deepseek", "deepseek-v4-flash"))
    );
    assert_eq!(def.reasoning_effort, Some(ReasoningEffort::High));
    assert_eq!(def.skills, vec!["feature-dev"]);
    assert_eq!(def.tools.as_deref().unwrap().len(), 4);
    assert_eq!(def.write_roots, vec!["web"]);
    let profile = def.child_profile();
    assert_eq!(profile.id, "frontend-worker");
    assert_eq!(profile.role, AgentRole::Worker);
    assert_eq!(profile.max_rounds(), Some(40));
    assert_eq!(profile.budget_policy.max_duration_secs, 600);
    profile.validate().unwrap();
    assert!(def.fingerprint.starts_with("sha256:") && def.fingerprint.len() == 71);
}

#[test]
fn a_new_agent_maps_onto_an_existing_class_not_a_new_role() {
    for (cap, role, read_only) in [
        ("read_only", AgentRole::Explorer, true),
        ("writer", AgentRole::Default, false),
        ("scoped_writer", AgentRole::Worker, false),
    ] {
        let m = parse_manifest(&format!(
            "version: 1\nname: any-agent\ndescription: d\ncapability: {cap}\n"
        ))
        .unwrap();
        let def = resolve_manifest(&m, "i", "any-agent", AgentSource::Project, None).unwrap();
        let profile = def.child_profile();
        assert_eq!(profile.role, role);
        assert_eq!(profile.read_only(), read_only);
        assert_eq!(profile.trace_fields().0, "any-agent");
    }
}

#[test]
fn the_fingerprint_moves_with_behaviour_not_with_location() {
    let m = parse_manifest(&yaml("fp", "")).unwrap();
    let a = resolve_manifest(&m, "v1", "fp", AgentSource::Project, Some("/a".into())).unwrap();
    let b = resolve_manifest(&m, "v1", "fp", AgentSource::User, Some("/b".into())).unwrap();
    let c = resolve_manifest(&m, "v2", "fp", AgentSource::Project, None).unwrap();
    assert_eq!(a.fingerprint, b.fingerprint);
    assert_ne!(a.fingerprint, c.fingerprint);
}

#[test]
fn render_then_parse_is_identity_and_field_order_is_stable() {
    let m = parse_manifest(
        "budget:\n  max_rounds: 3\ncapability: writer\nversion: 1\nname: order\ndescription: d\nskills: [s]\n",
    )
    .unwrap();
    let text = render_manifest(&m).unwrap();
    assert_eq!(parse_manifest(&text).unwrap(), m);
    let keys: Vec<&str> = text
        .lines()
        .filter(|l| !l.starts_with(' ') && !l.starts_with('-'))
        .filter_map(|l| l.split(':').next())
        .collect();
    assert_eq!(
        keys,
        [
            "version",
            "name",
            "description",
            "capability",
            "skills",
            "budget"
        ]
    );
    assert_eq!(render_manifest(&m).unwrap(), text, "deterministic");
}

// ── Names ───────────────────────────────────────────────────────────────────

#[test]
fn agent_name_boundaries() {
    let max = format!("a{}", "b".repeat(63));
    let over = format!("a{}", "b".repeat(64));
    for (name, ok) in [
        ("security-reviewer", true),
        ("a", true),
        ("rust2-expert", true),
        (max.as_str(), true),
        (over.as_str(), false),
        ("", false),
        ("..", false),
        ("../x", false),
        ("/x", false),
        (".foo", false),
        ("foo/", false),
        ("foo-", false),
        ("-foo", false),
        ("1foo", false),
        ("Foo", false),
        ("foo_bar", false),
        ("foo.bar", false),
        ("fоo", false), // Cyrillic о
        ("安全", false),
        ("con", false),
        ("lpt1", false),
    ] {
        assert_eq!(validate_agent_name(name).is_ok(), ok, "`{name}`");
    }
}

#[test]
fn an_invalid_directory_name_is_reported_not_loaded() {
    let fx = Fixture::new();
    fx.project_agent("Bad_Name", &yaml("Bad_Name", ""), "x");
    let reg = fx.load();
    assert!(reg.get("Bad_Name").is_none());
    assert_eq!(reg.problems().len(), 1);
    assert!(reg.problems()[0].error.contains("invalid"));
}

// ── Path security ───────────────────────────────────────────────────────────

#[cfg(unix)]
#[test]
fn symlinks_are_not_followed_anywhere_in_a_definition() {
    use std::os::unix::fs::symlink;
    let fx = Fixture::new();
    let outside = fx._tmp.path().join("outside");
    write_agent(&outside, "evil", &yaml("evil", ""), "secret instructions");
    std::fs::write(outside.join("stolen.md"), "stolen").unwrap();
    let agents = project_agents_dir(&fx.project);
    std::fs::create_dir_all(&agents).unwrap();

    // The directory itself is a symlink.
    symlink(outside.join("evil"), agents.join("evil")).unwrap();
    // A regular directory whose instructions are a symlink.
    let dir = agents.join("linked-file");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(MANIFEST_FILE), yaml("linked-file", "")).unwrap();
    symlink(outside.join("stolen.md"), dir.join(INSTRUCTIONS_FILE)).unwrap();

    let reg = fx.load();
    assert!(invalid_reason(&reg, "evil").contains("symlink"));
    assert!(invalid_reason(&reg, "linked-file").contains("symlinks are not followed"));
}

#[cfg(unix)]
#[test]
fn a_project_agents_directory_outside_the_project_loads_nothing() {
    use std::os::unix::fs::symlink;
    let fx = Fixture::new();
    let outside = fx._tmp.path().join("elsewhere");
    write_agent(&outside, "escapee", &yaml("escapee", ""), "x");
    std::fs::create_dir_all(fx.project.join(".leveler")).unwrap();
    symlink(&outside, project_agents_dir(&fx.project)).unwrap();
    let reg = fx.load();
    assert!(reg.get("escapee").is_none());
    assert!(reg.problems()[0].error.contains("outside the project"));
}

// ── Availability ────────────────────────────────────────────────────────────

struct Env {
    models: Vec<ModelRef>,
    skills: Vec<&'static str>,
}

impl AgentEnvironment for Env {
    fn model_unavailable(&self, model: &ModelRef) -> Option<String> {
        (!self.models.contains(model)).then(|| "not configured".into())
    }
    fn effort_unsupported(&self, _: Option<&ModelRef>, e: ReasoningEffort) -> Option<String> {
        (e == ReasoningEffort::Minimal).then(|| "not supported".into())
    }
    fn skill_exists(&self, name: &str) -> bool {
        self.skills.contains(&name)
    }
}

#[test]
fn a_valid_definition_can_still_be_unavailable_here() {
    let fx = Fixture::new();
    fx.project_agent("m", &yaml("m", "model: acme/big\n"), "x");
    fx.project_agent("s", &yaml("s", "skills: [absent]\n"), "x");
    fx.project_agent("e", &yaml("e", "reasoning_effort: minimal\n"), "x");
    fx.project_agent(
        "ok",
        &yaml("ok", "model: acme/small\nskills: [present]\n"),
        "x",
    );
    let reg = fx.load();
    let env = Env {
        models: vec![ModelRef::new("acme", "small")],
        skills: vec!["present"],
    };
    let avail = |n: &str| reg.resolve(n).unwrap().availability(&env);
    assert!(
        matches!(avail("m"), AgentAvailability::Unavailable(r) if r.contains("not configured"))
    );
    assert!(matches!(avail("s"), AgentAvailability::Unavailable(r) if r.contains("absent")));
    assert!(matches!(avail("e"), AgentAvailability::Unavailable(r) if r.contains("minimal")));
    assert_eq!(avail("ok"), AgentAvailability::Available);
}

/// The tool description advertises agents; if the schema stops accepting the
/// parameter the feature silently degrades to an agentless spawn that still
/// "succeeds".
#[test]
fn the_spawn_tool_advertises_the_agent_parameter() {
    let def = crate::injected_tools::spawn_agent_tool_definition();
    let props = def.input_schema.get("properties").expect("properties");
    assert!(props.get("agent").is_some(), "{:?}", def.input_schema);
    assert!(def.description.contains("agent='<name>'"));
    assert!(
        !def.description.contains(".md"),
        "the single-file format is gone"
    );
}
