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
    // Definition discovery also reads the user roots under `$HOME`
    // (`~/.codex/skills`, `~/.agents/skills`, `~/.claude/skills`). Pin the OS
    // home to the same empty temp directory as `LEVELER_HOME`, or a developer's
    // own skills become pending definitions and change the counts here.
    let out = leveler()
        .env("LEVELER_HOME", home)
        .env("HOME", home)
        .env("USERPROFILE", home)
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

/// A repository's default-mode file goes through the same trust surface: an
/// untrusted `full_access` cannot take effect, and trusting it is an explicit
/// decision on exact bytes.
#[test]
fn the_project_default_mode_file_is_listed_and_trusted() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    write(
        &repo.path().join(".leveler/config.yaml"),
        "mode: full_access\n",
    );

    let (ok, out) = run(home.path(), repo.path(), &["trust", "show"]);
    assert!(ok, "{out}");
    assert!(out.contains("config.yaml"), "{out}");
    assert!(out.to_lowercase().contains("untrusted"), "{out}");
    assert!(
        out.contains("full access"),
        "the purpose must say why it matters: {out}"
    );

    let (ok, out) = run(home.path(), repo.path(), &["trust", "allow", "--yes"]);
    assert!(ok, "{out}");
    let (ok, out) = run(home.path(), repo.path(), &["trust", "show"]);
    assert!(ok, "{out}");
    assert!(
        !out.to_lowercase().contains("untrusted"),
        "should be trusted now: {out}"
    );

    write(
        &repo.path().join(".leveler/config.yaml"),
        "mode: assisted\n",
    );
    let (ok, out) = run(home.path(), repo.path(), &["trust", "show"]);
    assert!(ok, "{out}");
    assert!(
        out.to_lowercase().contains("untrusted"),
        "an edit must drop trust: {out}"
    );
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

#[test]
fn project_definitions_are_inert_until_exact_bytes_are_approved() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    write(&repo.path().join("AGENTS.md"), "RULE-ONE\n");

    let (ok, out) = run(home.path(), repo.path(), &["trust", "show"]);
    assert!(ok, "{out}");
    assert!(out.contains("AGENTS.md"), "{out}");
    assert!(out.contains("new (ignored)"), "{out}");

    let (ok, out) = run(home.path(), repo.path(), &["trust", "allow", "--yes"]);
    assert!(ok, "{out}");
    assert!(out.contains("1 definition(s)"), "{out}");

    let (ok, out) = run(home.path(), repo.path(), &["trust", "show"]);
    assert!(ok, "{out}");
    assert!(out.contains("trusted"), "{out}");

    // A changed file is no longer admitted, and the diff is against the exact
    // bytes the human approved, not against HEAD or the current file.
    write(&repo.path().join("AGENTS.md"), "RULE-TWO\n");
    let (ok, out) = run(home.path(), repo.path(), &["trust", "show"]);
    assert!(ok, "{out}");
    assert!(out.contains("changed since approval"), "{out}");
    let (ok, out) = run(home.path(), repo.path(), &["trust", "diff", "AGENTS.md"]);
    assert!(ok, "{out}");
    assert!(out.contains("- RULE-ONE"), "{out}");
    assert!(out.contains("+ RULE-TWO"), "{out}");

    let (ok, out) = run(
        home.path(),
        repo.path(),
        &["trust", "revoke-definition", "AGENTS.md"],
    );
    assert!(ok, "{out}");
    let (ok, out) = run(home.path(), repo.path(), &["trust", "show"]);
    assert!(ok, "{out}");
    assert!(out.contains("new (ignored)"), "{out}");
}

#[test]
fn a_corrupt_definition_store_is_reported_and_nothing_is_approved() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    write(&repo.path().join("AGENTS.md"), "RULE\n");
    write(
        &home.path().join("definition-trust.yaml"),
        "!! not yaml !!\n",
    );

    let (ok, out) = run(home.path(), repo.path(), &["trust"]);
    assert!(!ok, "{out}");
    assert!(
        out.contains("could not be read or admitted"),
        "the unreadable store must be named: {out}"
    );
    assert!(
        out.contains("trust store unavailable"),
        "the typed store failure must be reported: {out}"
    );
    assert!(
        !out.contains("nothing to trust"),
        "a corrupt store is not a reason to report nothing: {out}"
    );
}

/// An approval is scoped to the store root the runtime reads. A CLI run that
/// points at another root can record a real-looking store, but this project's
/// gate must not see it.
#[test]
fn a_different_store_root_cannot_approve_this_projects_definitions() {
    use leveler_definition_trust::{
        DefinitionIdentity, DefinitionKind, DefinitionTrust, DefinitionTrustView,
    };

    let real_home = tempfile::tempdir().unwrap();
    let other_home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    write(&repo.path().join("AGENTS.md"), "RULE\n");

    let (ok, out) = run(other_home.path(), repo.path(), &["trust", "allow", "--yes"]);
    assert!(ok, "{out}");
    assert!(
        out.contains("store:"),
        "the store it wrote must be named: {out}"
    );

    let identity = DefinitionIdentity::project(DefinitionKind::RootAgents, "AGENTS.md").unwrap();
    let digest = DefinitionIdentity::single_digest(b"RULE\n");
    assert_eq!(
        DefinitionTrustView::load(real_home.path(), Some(repo.path())).check(&identity, &digest),
        DefinitionTrust::Unapproved,
        "a store in another root is not this project's approval"
    );
    assert!(
        DefinitionTrustView::load(other_home.path(), Some(repo.path()))
            .check(&identity, &digest)
            .is_trusted(),
        "the approval exists only in the store that recorded it"
    );
}

/// Revocation writes the same control files as approval, so it must fail
/// closed when the store would live inside the repository.
#[test]
fn revoke_refuses_when_the_trust_store_would_live_inside_the_repo() {
    let repo = tempfile::tempdir().unwrap();
    write(&repo.path().join(".leveler/hooks.yaml"), HOOKS);
    let home = repo.path().join(".leveler");

    let (ok, out) = run(&home, repo.path(), &["trust", "revoke"]);
    assert!(!ok, "must not claim success: {out}");
    assert!(out.contains("inside this repository"), "{out}");
    assert!(!home.join("trusted.yaml").exists());
    assert!(!home.join("definition-trust.yaml").exists());
}

/// A corrupt definition store blocks the whole transaction, including the
/// separate executable-config store. Otherwise the command would report a
/// successful hooks approval while every definition was unreadable.
#[test]
fn a_corrupt_definition_store_blocks_config_approval_too() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    write(&repo.path().join(".leveler/hooks.yaml"), HOOKS);
    write(
        &home.path().join("definition-trust.yaml"),
        "!! not yaml !!\n",
    );

    let (ok, out) = run(home.path(), repo.path(), &["trust", "allow", "--yes"]);
    assert!(!ok, "must not claim success: {out}");
    assert!(out.contains("unusable"), "{out}");
    assert!(
        !home.path().join("trusted.yaml").exists(),
        "no config approval may be written beside a corrupt definition store"
    );
}

#[test]
fn revoke_refuses_a_corrupt_definition_store_without_partial_state() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    write(&repo.path().join(".leveler/hooks.yaml"), HOOKS);
    write(
        &home.path().join("definition-trust.yaml"),
        "!! not yaml !!\n",
    );

    let (ok, out) = run(home.path(), repo.path(), &["trust", "revoke"]);
    assert!(!ok, "must not claim a revoke: {out}");
    assert!(out.contains("unusable"), "{out}");
    assert!(!home.path().join("trusted.yaml").exists());
    assert_eq!(
        std::fs::read_to_string(home.path().join("definition-trust.yaml")).unwrap(),
        "!! not yaml !!\n",
        "the corrupt store must not be rewritten"
    );
}
