//! Project-rule discovery and failure boundaries (Gate A).
//!
//! These tests exercise the production loader directly. They do not build a
//! prompt: discovery, source identity, ordering, scoped selection and the line
//! between "absent" and "present but unreadable" are decided here.

use std::path::{Path, PathBuf};

use leveler_context::{
    RuleDeliveryPolicy, RuleReadErrorKind, load_rules, plan_rule_delivery, read_rule_source,
    scoped_rule_sources,
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
        .documents
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

    let loaded = load_rules(&root);
    assert_eq!(
        loaded
            .documents
            .iter()
            .map(|r| r.source.as_str())
            .collect::<Vec<_>>(),
        vec![
            "AGENTS.md",
            ".leveler/instructions.md",
            ".leveler/rules/a.md",
            ".leveler/rules/b.md",
        ]
    );
    assert_eq!(loaded.documents[0].content, "# Root\nroot rule");
    assert_eq!(loaded.documents[1].content, "# Project\nproject rule");
    assert_eq!(loaded.documents[2].content, "# A\na rule");
    assert_eq!(loaded.documents[3].content, "# B\nb rule");
    assert!(loaded.empty.is_empty());
    assert!(
        loaded.errors.is_empty(),
        "clean workspace: {:?}",
        loaded.errors
    );
}

#[test]
fn an_empty_workspace_has_no_rules_and_no_failure() {
    let root = workspace("empty");
    let loaded = load_rules(&root);
    assert!(loaded.documents.is_empty());
    assert!(
        loaded.errors.is_empty(),
        "a missing AGENTS.md is normal, not a failure: {:?}",
        loaded.errors
    );

    let plan = plan_rule_delivery(&root, &RuleDeliveryPolicy::default());
    assert!(plan.documents.is_empty());
    assert!(plan.report.read_errors.is_empty());
}

#[test]
fn changed_content_is_read_again_on_the_next_load() {
    let root = workspace("changed");
    write(&root, "AGENTS.md", "# Rule\nfirst version\n");
    assert_eq!(
        load_rules(&root).documents[0].content,
        "# Rule\nfirst version"
    );

    write(&root, "AGENTS.md", "# Rule\nsecond version\n");
    assert_eq!(
        load_rules(&root).documents[0].content,
        "# Rule\nsecond version"
    );
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
        scoped
            .documents
            .iter()
            .map(|r| r.source.as_str())
            .collect::<Vec<_>>(),
        vec!["src/AGENTS.md", "src/deep/AGENTS.md"],
        "root and unrelated directories are not scoped rules"
    );

    let sibling = scoped_rule_sources(&root, &["other/lib.rs".to_string()]);
    assert_eq!(
        sibling
            .documents
            .iter()
            .map(|r| r.source.as_str())
            .collect::<Vec<_>>(),
        vec!["other/AGENTS.md"]
    );
    assert!(
        scoped
            .documents
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
            matches!(
                read_rule_source(&root, path),
                Err(error) if error.kind == RuleReadErrorKind::NotARuleDocument
            ),
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

    let loaded = load_rules(&root);
    assert!(loaded.documents.is_empty(), "nothing was delivered");
    assert_eq!(loaded.errors.len(), 1, "{:?}", loaded.errors);
    assert_eq!(loaded.errors[0].source, "AGENTS.md");
    assert_eq!(loaded.errors[0].kind, RuleReadErrorKind::NotAFile);
    assert!(!loaded.errors[0].detail.is_empty());

    let plan = plan_rule_delivery(&root, &RuleDeliveryPolicy::default());
    assert_eq!(
        plan.report.read_errors, loaded.errors,
        "the delivery report carries the same failure to the caller"
    );
}

#[test]
fn invalid_utf8_is_reported_not_treated_as_absent() {
    let root = workspace("invalid-utf8");
    std::fs::write(root.join("AGENTS.md"), b"# Broken\n\xff\xfe\n").unwrap();

    let loaded = load_rules(&root);
    assert!(loaded.documents.is_empty());
    assert_eq!(loaded.errors.len(), 1, "{:?}", loaded.errors);
    assert_eq!(loaded.errors[0].source, "AGENTS.md");
    assert_eq!(loaded.errors[0].kind, RuleReadErrorKind::InvalidUtf8);
}

#[test]
fn a_rules_path_that_cannot_be_listed_is_reported() {
    let root = workspace("rules-not-a-dir");
    write(&root, ".leveler/rules", "this is a file, not a directory\n");

    let loaded = load_rules(&root);
    assert!(loaded.documents.is_empty());
    assert_eq!(loaded.errors.len(), 1, "{:?}", loaded.errors);
    assert_eq!(loaded.errors[0].source, ".leveler/rules");
    assert_eq!(loaded.errors[0].kind, RuleReadErrorKind::NotAFile);
}

#[cfg(unix)]
#[test]
fn a_symlink_outside_the_workspace_is_refused_and_reported() {
    let outside = workspace("symlink-outside-target");
    write(&outside, "AGENTS.md", "# Outside\nEXTERNAL_SENTINEL_9f3c\n");
    let root = workspace("symlink-outside-link");
    std::os::unix::fs::symlink(outside.join("AGENTS.md"), root.join("AGENTS.md")).unwrap();

    let loaded = load_rules(&root);
    assert!(
        loaded
            .documents
            .iter()
            .all(|rule| !rule.content.contains("EXTERNAL_SENTINEL_9f3c")),
        "content outside the workspace must never become project rules"
    );
    assert_eq!(loaded.errors.len(), 1, "{:?}", loaded.errors);
    assert_eq!(loaded.errors[0].source, "AGENTS.md");
    assert_eq!(loaded.errors[0].kind, RuleReadErrorKind::OutsideWorkspace);
}

#[cfg(unix)]
#[test]
fn a_symlink_inside_the_workspace_is_followed() {
    let root = workspace("symlink-inside");
    write(&root, "real/AGENTS.md", "# Real\ninside rule\n");
    std::os::unix::fs::symlink(root.join("real/AGENTS.md"), root.join("AGENTS.md")).unwrap();

    let loaded = load_rules(&root);
    assert_eq!(loaded.documents.len(), 1, "{:?}", loaded.errors);
    assert_eq!(loaded.documents[0].source, "AGENTS.md");
    assert!(loaded.documents[0].content.contains("inside rule"));
    assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
}

#[cfg(unix)]
#[test]
fn a_dangling_symlink_is_reported_not_treated_as_absent() {
    let root = workspace("symlink-dangling");
    std::os::unix::fs::symlink(root.join("missing.md"), root.join("AGENTS.md")).unwrap();

    let loaded = load_rules(&root);
    assert!(loaded.documents.is_empty());
    assert_eq!(loaded.errors.len(), 1, "{:?}", loaded.errors);
    assert_eq!(loaded.errors[0].source, "AGENTS.md");
    assert_eq!(loaded.errors[0].detail, "dangling symlink");
    assert!(matches!(
        read_rule_source(&root, "AGENTS.md"),
        Err(error) if error.detail == "dangling symlink"
    ));
}

#[test]
fn source_identity_is_workspace_relative_and_content_is_trimmed_only_at_the_edges() {
    let root = workspace("identity");
    write(&root, ".leveler/rules/style.md", "\n\n# Style\n\nbody\n\n");

    let loaded = load_rules(&root);
    assert_eq!(loaded.documents.len(), 1);
    assert_eq!(loaded.documents[0].source, ".leveler/rules/style.md");
    assert_eq!(loaded.documents[0].content, "# Style\n\nbody");
    assert_eq!(sources(&root), vec![".leveler/rules/style.md"]);
}
