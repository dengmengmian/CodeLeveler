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
        .join("state")
        .join("projects")
        .join(leveler_project::layout::encode_repo_path(&canonical_repo))
        .join("permissions.yaml");
    let rule = PermissionRule {
        match_: RuleMatch {
            tool: Some("run_command".into()),
            command_prefix: Some("git push".into()),
            command_exact: None,
            write_path_glob: None,
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

/// A grant handle is a stable management key: `grants` lists the exact
/// capability/resource pair, `revoke-grant` removes it, and a second revoke
/// reports that nothing was removed. The identity is host-resolved, so this
/// runs the same on Windows and unix.
#[tokio::test]
async fn resource_grants_are_listed_and_revoked_on_windows() {
    use leveler_core::{Capability, GrantBinding, GrantScope};
    use leveler_project::Layout;

    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let layout = Layout::ephemeral(repo.path().to_path_buf(), None, home.path());
    std::fs::create_dir_all(&layout.state_dir).unwrap();
    let db = leveler_storage::Database::connect(&layout.database_path())
        .await
        .unwrap();

    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("approved.txt");
    std::fs::write(&target, "original").unwrap();
    let resource = leveler_execution::resolve_filesystem_resource(&target).unwrap();
    let binding = GrantBinding {
        capability: Capability::FilesystemWrite,
        resource,
    };

    // The CLI resolves the project from the same environment, minus the
    // inherited editor override that the identity gate refuses.
    let values: Vec<_> = std::env::vars_os()
        .filter(|(key, _)| key != "GIT_EDITOR")
        .collect();
    let environment =
        leveler_core::EnvSnapshot::new(values, repo.path().to_path_buf(), std::env::temp_dir());
    // The CLI composition root installs the same snapshot before resolving
    // the project; a test process starts with the library's empty default.
    let _ = leveler_core::install_environment(environment.clone());
    let project = leveler_execution::resolve_project_identity_with_environment(
        &repo.path().canonicalize().unwrap(),
        &environment,
    )
    .await
    .unwrap();
    leveler_storage::ResourceGrantStore::grant(
        &db,
        &project,
        "session-1",
        GrantScope::Session,
        std::slice::from_ref(&binding),
    )
    .await
    .unwrap();
    drop(db);

    let run = |args: &[&str]| {
        leveler()
            .env("LEVELER_HOME", home.path())
            .env_remove("GIT_EDITOR")
            .args(["--repo"])
            .arg(repo.path())
            .args(args)
            .output()
            .expect("spawn leveler")
    };
    let listed = run(&["permissions", "grants", "--json"]);
    assert!(
        listed.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let rows: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    let rows = rows.as_array().expect("a JSON array");
    assert_eq!(rows.len(), 1, "{rows:?}");
    let id = rows[0]["id"].as_str().expect("grant handle").to_string();
    assert_eq!(rows[0]["capability"], "FilesystemWrite");
    assert!(
        matches!(
            rows[0]["resource"].as_str(),
            Some(summary) if summary.contains("approved.txt")
        ),
        "{rows:?}"
    );

    let revoked = run(&["permissions", "revoke-grant", &id]);
    assert!(
        revoked.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&revoked.stderr)
    );
    let again = run(&["permissions", "revoke-grant", &id]);
    assert_eq!(
        again.status.code(),
        Some(1),
        "a second revoke reports that the handle no longer exists"
    );
    let empty = run(&["permissions", "grants", "--json"]);
    let rows: serde_json::Value = serde_json::from_slice(&empty.stdout).unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 0);
}

#[test]
fn permissions_list_shows_project_rule_then_clear_removes_it() {
    let dir = tempfile::tempdir().unwrap();
    let rule = PermissionRule {
        match_: RuleMatch {
            tool: Some("run_command".into()),
            command_prefix: Some("git push".into()),
            command_exact: None,
            write_path_glob: None,
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

/// The listing is how a user audits what they have granted. `command_exact` —
/// the condition that narrows a rule to ONE verbatim command — was not printed,
/// so the rule the TUI writes for "始终允许" showed as a bare
/// `allow tool=shell_command`: every shell command, to anyone reading it.
#[test]
fn a_rule_listing_shows_the_condition_that_narrows_it() {
    let dir = tempfile::tempdir().unwrap();
    let rule = PermissionRule {
        match_: RuleMatch {
            tool: Some("shell_command".into()),
            command_prefix: None,
            command_exact: Some("rm -rf dist && node -e \"x\"".into()),
            write_path_glob: None,
        },
        effect: RuleEffect::Allow,
    };
    append_project_rule(dir.path(), &rule).unwrap();

    let list = leveler()
        .args(["--repo"])
        .arg(dir.path())
        .args(["permissions", "list"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&list.stdout).to_string();
    assert!(
        text.contains("command_exact"),
        "the narrowing condition must be visible: {text}"
    );
    assert!(
        text.contains("rm -rf dist"),
        "and so must the command it is limited to: {text}"
    );
}
