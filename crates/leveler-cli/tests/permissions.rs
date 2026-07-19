//! Integration tests for `leveler permissions list|clear`.
//!
//! The CLI crate is bin-only, so these drive the real `leveler` binary via
//! `CARGO_BIN_EXE_leveler` rather than unit-testing private modules.

use std::process::Command;

use leveler_execution::{
    PermissionRule, RuleEffect, RuleMatch, append_project_rule, project_rules_path,
};

fn leveler() -> Command {
    Command::new(env!("CARGO_BIN_EXE_leveler"))
}

#[test]
fn permissions_list_empty_repo_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    let out = leveler()
        .args(["--repo"])
        .arg(dir.path())
        .args(["permissions", "list"])
        .output()
        .expect("spawn leveler");
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Permission rules") || stdout.contains("project"),
        "stdout={stdout}"
    );
}

#[test]
fn permissions_list_shows_project_rule_then_clear_removes_it() {
    let dir = tempfile::tempdir().unwrap();
    let rule = PermissionRule {
        match_: RuleMatch {
            tool: Some("run_command".into()),
            command_prefix: Some("git push".into()),
            command_exact: None,
            path_glob: None,
        },
        effect: RuleEffect::Allow,
    };
    append_project_rule(dir.path(), &rule).unwrap();
    assert!(project_rules_path(dir.path()).is_file());

    let list = leveler()
        .args(["--repo"])
        .arg(dir.path())
        .args(["permissions", "list"])
        .output()
        .expect("spawn leveler");
    assert!(
        list.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&list.stderr)
    );
    let stdout = String::from_utf8_lossy(&list.stdout);
    assert!(
        stdout.contains("git push") || stdout.contains("run_command"),
        "list should show the rule: {stdout}"
    );

    let clear = leveler()
        .args(["--repo"])
        .arg(dir.path())
        .args(["permissions", "clear"])
        .output()
        .expect("spawn leveler");
    assert!(
        clear.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&clear.stderr)
    );
    assert!(
        !project_rules_path(dir.path()).exists(),
        "clear must delete the project rules file"
    );

    // Idempotent second clear.
    let clear2 = leveler()
        .args(["--repo"])
        .arg(dir.path())
        .args(["permissions", "clear"])
        .output()
        .expect("spawn leveler");
    assert!(clear2.status.success());
}
