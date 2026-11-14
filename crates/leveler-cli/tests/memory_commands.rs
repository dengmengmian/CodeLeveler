//! `leveler memory` is the user-facing face of the same durable store the
//! agent and the TUI use.
//!
//! These are process-level contracts: the real binary runs against a scratch
//! home, and every assertion is a fact a user can observe (stdout, exit
//! status, and the store's own manifest). No command starts a model turn.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

fn scratch(name: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(1);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    // The product refuses a relative memory root, and "/tmp" has no drive on
    // Windows: the scratch home must come from the platform's own temp root.
    let path = std::env::temp_dir().join(format!("lvl-memory-{}-{n}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(path.join("home/leveler")).unwrap();
    std::fs::create_dir_all(path.join("repo")).unwrap();
    std::fs::write(
        path.join("home/leveler/config.toml"),
        "[memory]\nrollout = \"record_only\"\n",
    )
    .unwrap();
    path
}

fn leveler(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_leveler"))
        .args(args)
        .current_dir(root.join("repo"))
        .env("HOME", root.join("home"))
        .env("LEVELER_HOME", root.join("home/leveler"))
        .env("LEVELER_LANG", "en")
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

/// The observation `remember` wrote, as `status` lists it.
fn saved_observation(root: &Path) -> String {
    let status = json(&leveler(root, &["memory", "status", "--json"]));
    status["global"]["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|file| file["kind"] == "observation")
        .and_then(|file| file["path"].as_str())
        .unwrap_or_else(|| panic!("no observation in {status}"))
        .to_string()
}

#[test]
fn help_lists_the_memory_subcommands() {
    let root = scratch("help");
    let output = leveler(&root, &["memory", "--help"]);
    assert!(
        output.status.success(),
        "memory --help failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let help = String::from_utf8_lossy(&output.stdout);
    for command in ["status", "show", "remember", "forget"] {
        assert!(help.contains(command), "help omits `{command}`:\n{help}");
    }
}

#[test]
fn status_projects_the_configured_rollout() {
    let root = scratch("status");
    let status = json(&leveler(&root, &["memory", "status", "--json"]));
    assert_eq!(status["rollout"], "record_only", "{status}");
    // Path equality, not string equality: the product may render the same
    // directory with the platform's separator while `Path::join` keeps the
    // separator written here.
    let expected_root = root.join("home/leveler/memory/global");
    assert_eq!(
        status["global"]["root"].as_str().map(Path::new),
        Some(expected_root.as_path()),
        "the CLI reads the same home the agent does: {status}"
    );
    assert!(
        status["global"]["files"]
            .as_array()
            .unwrap()
            .iter()
            .all(|file| file["kind"] != "observation"),
        "a fresh store has no observation: {status}"
    );
}

#[test]
fn remember_writes_a_note_that_status_lists_and_show_reads() {
    let root = scratch("remember");
    let note = "Prefer terse answers and exact file paths";
    let remembered = leveler(&root, &["memory", "remember", note]);
    assert!(
        remembered.status.success(),
        "remember failed: {}",
        String::from_utf8_lossy(&remembered.stderr)
    );
    assert!(
        String::from_utf8_lossy(&remembered.stdout).contains("Saved to global memory"),
        "{}",
        String::from_utf8_lossy(&remembered.stdout)
    );

    let relative = saved_observation(&root);
    let shown = json(&leveler(
        &root,
        &["memory", "show", "global", &relative, "--json"],
    ));
    assert_eq!(shown["error"], serde_json::Value::Null, "{shown}");
    assert_eq!(shown["truncated"], false, "{shown}");
    assert!(
        shown["content"].as_str().unwrap().contains(note),
        "show must return the note that was written: {shown}"
    );
    let hash = shown["content_hash"].as_str().unwrap();
    assert_eq!(
        hash.len(),
        64,
        "show returns the BLAKE3 forget binds to: {shown}"
    );
    assert!(hash.bytes().all(|byte| byte.is_ascii_hexdigit()), "{shown}");
}

#[test]
fn forget_is_bound_to_the_previewed_hash() {
    let root = scratch("forget");
    let note = "A note to forget";
    assert!(
        leveler(&root, &["memory", "remember", note])
            .status
            .success()
    );
    let relative = saved_observation(&root);
    let shown = json(&leveler(
        &root,
        &["memory", "show", "global", &relative, "--json"],
    ));
    let hash = shown["content_hash"].as_str().unwrap().to_string();

    let wrong = "0".repeat(64);
    let refused = leveler(
        &root,
        &["memory", "forget", "global", &relative, "--hash", &wrong],
    );
    assert!(
        !refused.status.success(),
        "a hash that does not match the file must refuse: {}",
        String::from_utf8_lossy(&refused.stdout)
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("refused"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );

    let forgotten = leveler(
        &root,
        &["memory", "forget", "global", &relative, "--hash", &hash],
    );
    assert!(
        forgotten.status.success(),
        "forget failed: {}",
        String::from_utf8_lossy(&forgotten.stderr)
    );
    assert!(
        String::from_utf8_lossy(&forgotten.stdout).contains("Forgot"),
        "{}",
        String::from_utf8_lossy(&forgotten.stdout)
    );

    let after = leveler(&root, &["memory", "show", "global", &relative, "--json"]);
    assert!(
        !after.status.success(),
        "a forgotten file must not be readable again: {}",
        String::from_utf8_lossy(&after.stdout)
    );
    assert!(
        String::from_utf8_lossy(&after.stderr).contains("excluded"),
        "{}",
        String::from_utf8_lossy(&after.stderr)
    );
}
