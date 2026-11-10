//! Project-rule discovery and failure boundaries (Gate A).
//!
//! These tests exercise the production loader directly. They do not build a
//! prompt: discovery, source identity, ordering, scoped selection and the line
//! between "absent" and "present but unreadable" are decided here.

use std::path::{Path, PathBuf};

use leveler_context::{
    RuleDeliveryPolicy, load_rules, load_rules_reported, plan_rule_delivery, read_rule_source,
    read_rule_source_reported, scoped_rule_sources,
};

fn workspace(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("leveler-rules-gate-a-{}-{tag}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

fn sources(root: &Path) -> Vec<String> {
    load_rules(root)
        .into_iter()
        .map(|rule| rule.source)
        .collect()
}

#[test]
fn every_documented_source_is_discovered_in_priority_order() {
    let root = workspace("order");
    write(&root, "AGENTS.md", "# Root\nroot rule\n");
    write(
        &root,
        ".leveler/instructions.md",
        "# Project\nproject rule\n",
    );
    write(&root, ".leveler/rules/b.md", "# B\nb rule\n");
    write(&root, ".leveler/rules/a.md", "# A\na rule\n");
    // A rules-directory file that is not Markdown is not a rule document.
    write(&root, ".leveler/rules/notes.txt", "not a rule\n");

    let rules = load_rules(&root);
    assert_eq!(
        rules.iter().map(|r| r.source.as_str()).collect::<Vec<_>>(),
        vec![
            "AGENTS.md",
            ".leveler/instructions.md",
            ".leveler/rules/a.md",
            ".leveler/rules/b.md",
        ]
    );
    assert_eq!(rules[0].content, "# Root\nroot rule");
    assert_eq!(rules[1].content, "# Project\nproject rule");
    assert_eq!(rules[2].content, "# A\na rule");
    assert_eq!(rules[3].content, "# B\nb rule");

    let (_, issues) = load_rules_reported(&root);
    assert!(issues.is_empty(), "clean workspace: {issues:?}");
}

#[test]
fn an_empty_workspace_has_no_rules_and_no_failure() {
    let root = workspace("empty");
    let (rules, issues) = load_rules_reported(&root);
    assert!(rules.is_empty());
    assert!(
        issues.is_empty(),
        "a missing AGENTS.md is normal, not a failure: {issues:?}"
    );

    let plan = plan_rule_delivery(&root, &RuleDeliveryPolicy::default());
    assert!(plan.documents.is_empty());
    assert!(plan.report.load_issues.is_empty());
}

#[test]
fn changed_content_is_read_again_on_the_next_load() {
    let root = workspace("changed");
    write(&root, "AGENTS.md", "# Rule\nfirst version\n");
    assert_eq!(load_rules(&root)[0].content, "# Rule\nfirst version");

    write(&root, "AGENTS.md", "# Rule\nsecond version\n");
    assert_eq!(load_rules(&root)[0].content, "# Rule\nsecond version");
}

#[test]
fn nested_rules_scope_to_their_directory_and_never_leak_from_siblings() {
    let root = workspace("scoped");
    write(&root, "AGENTS.md", "# Root\nroot rule\n");
    write(&root, "src/AGENTS.md", "# Src\nsrc rule\n");
    write(&root, "src/deep/AGENTS.md", "# Deep\ndeep rule\n");
    write(&root, "other/AGENTS.md", "# Other\nother rule\n");

    let scoped = scoped_rule_sources(&root, &["src/deep/lib.rs".to_string()]);
    assert_eq!(
        scoped.iter().map(|r| r.source.as_str()).collect::<Vec<_>>(),
        vec!["src/AGENTS.md", "src/deep/AGENTS.md"],
        "root and unrelated directories are not scoped rules"
    );

    let sibling = scoped_rule_sources(&root, &["other/lib.rs".to_string()]);
    assert_eq!(
        sibling
            .iter()
            .map(|r| r.source.as_str())
            .collect::<Vec<_>>(),
        vec!["other/AGENTS.md"]
    );
    assert!(
        scoped
            .iter()
            .all(|rule| !rule.content.contains("other rule")),
        "a sibling directory's rules must not be injected"
    );
}

#[test]
fn an_illegal_path_is_a_refusal_not_an_absent_file() {
    let root = workspace("illegal");
    for path in ["../AGENTS.md", "/etc/AGENTS.md", "src/../AGENTS.md"] {
        assert!(
            read_rule_source(&root, path).is_none(),
            "{path} must not be readable"
        );
        assert!(
            read_rule_source_reported(&root, path).is_err(),
            "{path} must be reported as refused, not as missing"
        );
    }
}

#[test]
fn an_unreadable_document_is_reported_not_treated_as_absent() {
    let root = workspace("unreadable");
    // A directory named AGENTS.md exists, so this is a present-but-unreadable
    // document, not a missing one.
    std::fs::create_dir_all(root.join("AGENTS.md")).unwrap();

    let (rules, issues) = load_rules_reported(&root);
    assert!(rules.is_empty(), "nothing was delivered");
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert_eq!(issues[0].source, "AGENTS.md");
    assert!(!issues[0].reason.is_empty());

    let plan = plan_rule_delivery(&root, &RuleDeliveryPolicy::default());
    assert_eq!(
        plan.report.load_issues, issues,
        "the delivery report carries the same failure to the caller"
    );
}

#[test]
fn invalid_utf8_is_reported_not_treated_as_absent() {
    let root = workspace("invalid-utf8");
    std::fs::write(root.join("AGENTS.md"), b"# Broken\n\xff\xfe\n").unwrap();

    let (rules, issues) = load_rules_reported(&root);
    assert!(rules.is_empty());
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert_eq!(issues[0].source, "AGENTS.md");
    assert!(
        issues[0].reason.to_lowercase().contains("utf-8")
            || issues[0].reason.to_lowercase().contains("invalid"),
        "reason must identify an encoding failure: {}",
        issues[0].reason
    );
}

#[test]
fn a_rules_path_that_cannot_be_listed_is_reported() {
    let root = workspace("rules-not-a-dir");
    write(&root, ".leveler/rules", "this is a file, not a directory\n");

    let (rules, issues) = load_rules_reported(&root);
    assert!(rules.is_empty());
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert_eq!(issues[0].source, ".leveler/rules");
}

#[cfg(unix)]
#[test]
fn a_symlink_outside_the_workspace_is_refused_and_reported() {
    let outside = workspace("symlink-outside-target");
    write(&outside, "AGENTS.md", "# Outside\nEXTERNAL_SENTINEL_9f3c\n");
    let root = workspace("symlink-outside-link");
    std::os::unix::fs::symlink(outside.join("AGENTS.md"), root.join("AGENTS.md")).unwrap();

    let (rules, issues) = load_rules_reported(&root);
    assert!(
        rules
            .iter()
            .all(|rule| !rule.content.contains("EXTERNAL_SENTINEL_9f3c")),
        "content outside the workspace must never become project rules"
    );
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert_eq!(issues[0].source, "AGENTS.md");
    assert!(
        issues[0].reason.to_lowercase().contains("outside"),
        "reason must name the containment failure: {}",
        issues[0].reason
    );
}

#[cfg(unix)]
#[test]
fn a_symlink_inside_the_workspace_is_followed() {
    let root = workspace("symlink-inside");
    write(&root, "real/AGENTS.md", "# Real\ninside rule\n");
    std::os::unix::fs::symlink(root.join("real/AGENTS.md"), root.join("AGENTS.md")).unwrap();

    let (rules, issues) = load_rules_reported(&root);
    assert_eq!(rules.len(), 1, "{issues:?}");
    assert_eq!(rules[0].source, "AGENTS.md");
    assert!(rules[0].content.contains("inside rule"));
    assert!(issues.is_empty(), "{issues:?}");
}

#[test]
fn source_identity_is_workspace_relative_and_content_is_trimmed_only_at_the_edges() {
    let root = workspace("identity");
    write(&root, ".leveler/rules/style.md", "\n\n# Style\n\nbody\n\n");

    let rules = load_rules(&root);
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].source, ".leveler/rules/style.md");
    assert_eq!(rules[0].content, "# Style\n\nbody");
    assert_eq!(sources(&root), vec![".leveler/rules/style.md"]);
}
