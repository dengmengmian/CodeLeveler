//! `leveler schedule update` and `grant` as process behaviour.
//!
//! The management protocol, the TUI, the Desktop and the agent tools can
//! already edit a definition and issue an unattended grant without a model.
//! The CLI is the remaining deterministic surface, and these commands must
//! reach the same store instead of growing a second interpretation of a
//! schedule. Every command below runs the real binary against an isolated
//! home; no model, runtime or network is involved.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

fn scratch(name: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(1);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let path = PathBuf::from(format!("/tmp/lvl-sched-{}-{n}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn leveler(home: &Path, workspace: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_leveler"))
        .args(args)
        .current_dir(workspace)
        .env("HOME", home)
        .env("LEVELER_HOME", home.join("leveler"))
        .env("LEVELER_LANG", "zh")
        .env("NO_COLOR", "1")
        .env_remove("LEVELER_CONFIG_DIR")
        .env_remove("DEEPSEEK_API_KEY")
        .env_remove("LEVELER_API_KEY")
        .output()
        .unwrap_or_else(|error| panic!("could not run leveler {args:?}: {error}"))
}

fn json(output: &Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "command failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "not JSON ({error}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn workspace(home: &Path) -> PathBuf {
    let workspace = home.join("ws");
    std::fs::create_dir_all(workspace.join(".leveler")).unwrap();
    std::fs::write(workspace.join(".leveler").join("config.yaml"), "{}\n").unwrap();
    // The store records the canonical path (`/tmp` is a symlink on macOS).
    std::fs::canonicalize(workspace).unwrap()
}

#[test]
fn update_replaces_only_the_given_fields_and_keeps_the_identity() {
    let home = scratch("update");
    let workspace = workspace(&home);
    let workspace_text = workspace.display().to_string();

    let created = json(&leveler(
        &home,
        &workspace,
        &[
            "schedule",
            "add",
            "--workspace",
            &workspace_text,
            "--name",
            "morning report",
            "--daily",
            "09:30",
            "--timezone",
            "Asia/Shanghai",
            "--command",
            "printf a",
            "--json",
        ],
    ));
    let id = created["id"].as_str().unwrap().to_string();

    let updated = json(&leveler(
        &home,
        &workspace,
        &[
            "schedule",
            "update",
            &id,
            "--command",
            "printf b",
            "--daily",
            "10:15",
            "--timezone",
            "Asia/Shanghai",
            "--json",
        ],
    ));
    assert_eq!(updated["id"], id, "update must keep the definition id");
    assert_eq!(updated["name"], "morning report");
    assert_eq!(updated["workspace"], workspace_text);
    assert_eq!(updated["action"]["command"], "printf b");
    assert_eq!(updated["trigger"]["kind"], "daily");
    assert_eq!(updated["trigger"]["time"], "10:15");
    assert_eq!(updated["trigger"]["timezone"], "Asia/Shanghai");

    let shown = json(&leveler(
        &home,
        &workspace,
        &["schedule", "show", &id, "--json"],
    ));
    assert_eq!(shown["action"]["command"], "printf b");
    assert_eq!(shown["trigger"]["time"], "10:15");
    assert!(
        shown["revision"]
            .as_str()
            .is_some_and(|value| !value.is_empty()),
        "show must expose the revision an editor sends back: {shown}"
    );
}

#[test]
fn grant_and_revoke_are_visible_through_show() {
    let home = scratch("grant");
    let workspace = workspace(&home);
    let workspace_text = workspace.display().to_string();
    let created = json(&leveler(
        &home,
        &workspace,
        &[
            "schedule",
            "add",
            "--workspace",
            &workspace_text,
            "--name",
            "nightly",
            "--daily",
            "23:00",
            "--command",
            "printf n",
            "--json",
        ],
    ));
    let id = created["id"].as_str().unwrap().to_string();
    assert!(created["unattended_grant"].is_null());

    let granted = json(&leveler(
        &home,
        &workspace,
        &[
            "schedule",
            "grant",
            &id,
            "--allow-external-side-effects",
            "--json",
        ],
    ));
    assert_eq!(granted["unattended_grant"]["definition_id"], id);
    let shown = json(&leveler(
        &home,
        &workspace,
        &["schedule", "show", &id, "--json"],
    ));
    assert_eq!(shown["unattended_grant"]["definition_id"], id);

    let revoked = leveler(&home, &workspace, &["schedule", "revoke-grant", &id]);
    assert!(revoked.status.success());
    let shown = json(&leveler(
        &home,
        &workspace,
        &["schedule", "show", &id, "--json"],
    ));
    assert!(shown["unattended_grant"].is_null());
}

#[test]
fn a_stale_revision_is_refused_instead_of_overwriting_a_newer_edit() {
    let home = scratch("conflict");
    let workspace = workspace(&home);
    let workspace_text = workspace.display().to_string();
    let created = json(&leveler(
        &home,
        &workspace,
        &[
            "schedule",
            "add",
            "--workspace",
            &workspace_text,
            "--name",
            "conflict",
            "--daily",
            "08:00",
            "--command",
            "printf one",
            "--json",
        ],
    ));
    let id = created["id"].as_str().unwrap().to_string();
    let stale = json(&leveler(
        &home,
        &workspace,
        &["schedule", "show", &id, "--json"],
    ))["revision"]
        .as_str()
        .unwrap()
        .to_string();

    let first = leveler(
        &home,
        &workspace,
        &[
            "schedule",
            "update",
            &id,
            "--command",
            "printf two",
            "--expected-revision",
            &stale,
        ],
    );
    assert!(first.status.success());

    let second = leveler(
        &home,
        &workspace,
        &[
            "schedule",
            "update",
            &id,
            "--command",
            "printf three",
            "--expected-revision",
            &stale,
        ],
    );
    assert!(
        !second.status.success(),
        "a stale revision must be refused, not silently overwrite: {}",
        String::from_utf8_lossy(&second.stdout)
    );
    let shown = json(&leveler(
        &home,
        &workspace,
        &["schedule", "show", &id, "--json"],
    ));
    assert_eq!(
        shown["action"]["command"], "printf two",
        "the newer definition must survive the refused edit"
    );
}

/// Full Access is an interactive admission profile, not a durable unattended
/// authorization. The CLI must not project it as if a grant existed; doing so
/// tells the user the opposite of what the dispatcher will enforce.
#[test]
fn full_access_is_not_projected_as_an_unattended_grant() {
    let home = scratch("full-access");
    let workspace = workspace(&home);
    let workspace_text = workspace.display().to_string();
    let created = json(&leveler(
        &home,
        &workspace,
        &[
            "schedule",
            "add",
            "--workspace",
            &workspace_text,
            "--name",
            "full access agent",
            "--every",
            "3600",
            "--agent-goal",
            "summarize the repository",
            "--permission",
            "full-access",
            "--model",
            "deepseek/deepseek-v4.1-flash-1017",
            "--json",
        ],
    ));
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["mode"], "full_access");
    assert!(
        created["unattended_grant"].is_null(),
        "creating with Full Access must not fabricate a grant: {created}"
    );

    let shown = json(&leveler(
        &home,
        &workspace,
        &["schedule", "show", &id, "--json"],
    ));
    assert!(shown["unattended_grant"].is_null(), "show: {shown}");

    let listed = json(&leveler(&home, &workspace, &["schedule", "list", "--json"]));
    assert!(listed[0]["unattended_grant"].is_null(), "list: {listed}");

    let text = leveler(&home, &workspace, &["schedule", "show", &id]);
    let stdout = String::from_utf8_lossy(&text.stdout);
    assert!(
        !stdout.contains("Full Access (no separate grant)"),
        "text show still equates Full Access with a grant: {stdout}"
    );
}

/// The stored timestamps keep milliseconds; use the same precision so the
/// value read back from SQLite compares equal to the one the test sent.
fn future(offset_secs: i64) -> (chrono::DateTime<chrono::Utc>, String) {
    let millis = chrono::Utc::now().timestamp_millis() + offset_secs * 1000;
    let at = chrono::DateTime::from_timestamp_millis(millis).expect("timestamp");
    (at, at.to_rfc3339())
}

fn next_occurrence(json: &serde_json::Value) -> chrono::DateTime<chrono::Utc> {
    json["next_occurrence"]
        .as_str()
        .expect("the schedule must expose its next occurrence")
        .parse()
        .expect("a projected timestamp")
}

/// A one-off's only slot is its declared instant. Changing that instant must
/// move the slot; before the fix the update deleted the pending occurrence and
/// left the definition with no next run.
#[test]
fn updating_a_one_off_moves_the_next_occurrence() {
    let home = scratch("once-update");
    let workspace = workspace(&home);
    let workspace_text = workspace.display().to_string();
    let (t1, t1_text) = future(3_600);
    let (t2, t2_text) = future(7_200);

    let created = json(&leveler(
        &home,
        &workspace,
        &[
            "schedule",
            "add",
            "--workspace",
            &workspace_text,
            "--name",
            "one off",
            "--at",
            &t1_text,
            "--command",
            "printf once",
            "--json",
        ],
    ));
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(next_occurrence(&created), t1);

    let updated = json(&leveler(
        &home,
        &workspace,
        &["schedule", "update", &id, "--at", &t2_text, "--json"],
    ));
    assert_eq!(updated["trigger"]["kind"], "once");
    assert_eq!(next_occurrence(&updated), t2);

    let shown = json(&leveler(
        &home,
        &workspace,
        &["schedule", "show", &id, "--json"],
    ));
    assert_eq!(
        next_occurrence(&shown),
        t2,
        "another process reading the same store must see the new slot"
    );
}
