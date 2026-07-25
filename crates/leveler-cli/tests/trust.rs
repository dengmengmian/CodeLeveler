//! Integration tests for `leveler trust` and the in-repo config trust gate.
//!
//! The CLI crate is bin-only, so these drive the real `leveler` binary via
//! `CARGO_BIN_EXE_leveler`.

use std::path::Path;
use std::process::Command;

const HOOKS: &str = "hooks:\n  pre_tool_use:\n    - command: [\"/bin/sh\", \"-c\", \"exit 0\"]\n";

fn leveler() -> Command {
    Command::new(env!("CARGO_BIN_EXE_leveler"))
}

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

fn run(home: &Path, repo: &Path, args: &[&str]) -> (bool, String) {
    let out = leveler()
        .env("LEVELER_HOME", home)
        .arg("--repo")
        .arg(repo)
        .args(args)
        .output()
        .expect("spawn leveler");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

#[test]
fn show_reports_an_untrusted_in_repo_hooks_file() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    write(&repo.path().join(".leveler/hooks.yaml"), HOOKS);

    let (ok, out) = run(home.path(), repo.path(), &["trust", "show"]);
    assert!(ok, "{out}");
    assert!(out.contains("hooks.yaml"), "{out}");
    assert!(out.to_lowercase().contains("untrusted"), "{out}");
}

/// The whole gate in one pass: untrusted → `allow --yes` → trusted → an edit
/// drops it back to untrusted.
#[test]
fn allow_yes_trusts_and_a_later_edit_revokes() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let hooks = repo.path().join(".leveler/hooks.yaml");
    write(&hooks, HOOKS);

    let (ok, out) = run(home.path(), repo.path(), &["trust", "allow", "--yes"]);
    assert!(ok, "{out}");

    let (ok, out) = run(home.path(), repo.path(), &["trust", "show"]);
    assert!(ok, "{out}");
    assert!(out.contains("trusted"), "{out}");
    assert!(
        !out.to_lowercase().contains("untrusted"),
        "should be trusted now: {out}"
    );

    write(
        &hooks,
        "hooks:\n  pre_tool_use:\n    - command: [\"/bin/sh\", \"-c\", \"id\"]\n",
    );
    let (ok, out) = run(home.path(), repo.path(), &["trust", "show"]);
    assert!(ok, "{out}");
    assert!(
        out.to_lowercase().contains("untrusted"),
        "an edit must drop trust: {out}"
    );
}

#[test]
fn revoke_drops_trust() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    write(&repo.path().join(".leveler/hooks.yaml"), HOOKS);

    let (ok, out) = run(home.path(), repo.path(), &["trust", "allow", "--yes"]);
    assert!(ok, "{out}");
    let (ok, out) = run(home.path(), repo.path(), &["trust", "revoke"]);
    assert!(ok, "{out}");
    let (ok, out) = run(home.path(), repo.path(), &["trust", "show"]);
    assert!(ok, "{out}");
    assert!(out.to_lowercase().contains("untrusted"), "{out}");
}

/// A trust decision belongs to the human. With no TTY and no `--yes` the
/// command must refuse rather than quietly grant.
#[test]
fn non_interactive_allow_without_yes_refuses() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    write(&repo.path().join(".leveler/hooks.yaml"), HOOKS);

    // `output()` gives the child a piped, non-terminal stdin.
    let (ok, out) = run(home.path(), repo.path(), &["trust", "allow"]);
    assert!(!ok, "must not succeed without confirmation: {out}");

    let (ok, show) = run(home.path(), repo.path(), &["trust", "show"]);
    assert!(ok, "{show}");
    assert!(
        show.to_lowercase().contains("untrusted"),
        "nothing may have been trusted: {show}"
    );
}

/// A silent skip is indistinguishable from "my hooks are broken", so every
/// command must say so on stderr — not only `trust show`.
#[test]
fn any_command_warns_on_stderr_about_ignored_in_repo_config() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    write(&repo.path().join(".leveler/hooks.yaml"), HOOKS);

    let out = leveler()
        .env("LEVELER_HOME", home.path())
        .arg("--repo")
        .arg(repo.path())
        .args(["permissions", "list"])
        .output()
        .expect("spawn leveler");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("hooks.yaml"), "stderr={stderr}");
    assert!(stderr.contains("leveler trust"), "stderr={stderr}");
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("leveler trust"),
        "the notice must stay off stdout"
    );

    // Once trusted, the notice stops.
    let (ok, msg) = run(home.path(), repo.path(), &["trust", "allow", "--yes"]);
    assert!(ok, "{msg}");
    let out = leveler()
        .env("LEVELER_HOME", home.path())
        .arg("--repo")
        .arg(repo.path())
        .args(["permissions", "list"])
        .output()
        .expect("spawn leveler");
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("hooks.yaml"),
        "no notice once trusted"
    );
}

/// Listing rules that are not in force, without saying so, misrepresents the
/// permissions actually applied.
#[test]
fn permissions_list_marks_untrusted_repo_rules_as_not_in_force() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    write(
        &repo.path().join(".leveler/permissions.yaml"),
        "rules:\n  - match: { tool: run_command, command_prefix: \"curl\" }\n    effect: allow\n",
    );

    let (ok, out) = run(home.path(), repo.path(), &["permissions", "list"]);
    assert!(ok, "{out}");
    assert!(out.contains("NOT in force"), "{out}");

    let (ok, msg) = run(home.path(), repo.path(), &["trust", "allow", "--yes"]);
    assert!(ok, "{msg}");
    let (ok, out) = run(home.path(), repo.path(), &["permissions", "list"]);
    assert!(ok, "{out}");
    assert!(
        !out.contains("NOT in force"),
        "trusted rules are in force: {out}"
    );
}

/// With the store inside the repository the gate grants nothing, so `allow`
/// must fail loudly rather than report a write that has no effect.
#[test]
fn allow_refuses_when_the_trust_store_would_live_inside_the_repo() {
    let repo = tempfile::tempdir().unwrap();
    write(&repo.path().join(".leveler/hooks.yaml"), HOOKS);
    let home = repo.path().join(".leveler");

    let (ok, out) = run(&home, repo.path(), &["trust", "allow", "--yes"]);
    assert!(!ok, "must not claim success: {out}");
    assert!(out.contains("inside this repository"), "{out}");
}

#[test]
fn a_repo_with_no_gated_files_has_nothing_to_trust() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();

    let (ok, out) = run(home.path(), repo.path(), &["trust", "show"]);
    assert!(ok, "{out}");
    assert!(out.contains("(no file)"), "{out}");
}
