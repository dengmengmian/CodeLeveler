//! Integration tests for `leveler permissions list|clear`.
//!
//! The CLI crate is bin-only, so these drive the real `leveler` binary via
//! `CARGO_BIN_EXE_leveler` rather than unit-testing private modules.

use std::process::Command;

use leveler_execution::{
    PermissionRule, RuleEffect, RuleMatch, append_project_rule, append_rule_file,
    project_rules_path,
};

fn leveler() -> Command {
    Command::new(env!("CARGO_BIN_EXE_leveler"))
}

/// ApproveAlways persists rules under `~/.leveler/projects/<hash>/`, next to
/// `sessions.db`. `list` must show them and `clear` must delete that file.
#[test]
fn permissions_list_and_clear_cover_the_state_dir_file() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    // Encode the CANONICAL repo path, exactly as the CLI does with `--repo` —
    // on macOS the raw tempdir goes through the `/var` → `/private/var`
    // symlink, and a slug from the raw path points at a directory the CLI
    // never reads.
    let canonical_repo = repo.path().canonicalize().unwrap();
    let state_rules = home
        .path()
        .join("projects")
        .join(leveler_project::layout::encode_repo_path(&canonical_repo))
        .join("permissions.yaml");
    let rule = PermissionRule {
        match_: RuleMatch {
            tool: Some("run_command".into()),
            command_prefix: Some("git push".into()),
            command_exact: None,
            path_glob: None,
        },
        effect: RuleEffect::Allow,
    };
    append_rule_file(&state_rules, &rule).unwrap();

    let list = leveler()
        .env("LEVELER_HOME", home.path())
        .args(["--repo"])
        .arg(repo.path())
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
        stdout.contains("git push"),
        "list should show the state-dir rule: {stdout}"
    );

    let clear = leveler()
        .env("LEVELER_HOME", home.path())
        .args(["--repo"])
        .arg(repo.path())
        .args(["permissions", "clear"])
        .output()
        .expect("spawn leveler");
    assert!(
        clear.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&clear.stderr)
    );
    assert!(
        !state_rules.exists(),
        "clear must delete the state-dir rules file"
    );
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
