//! Read-only CLI surfaces as process behaviour.
//!
//! These are the commands a person runs to *look* at something: the resolved
//! configuration, the model catalogue, the theme, the agent roster, the skill
//! list, sessions, background tasks, memory, permission rules, the trace and the
//! LSP surface. None of them needs a model, a network or a credential, so none of
//! them has an excuse for having no acceptance: the contract asserted here is
//! what the process prints, whether it exits 0 or refuses, that it never crashes
//! on a fresh install, and that it never prints a secret.
//!
//! Isolation is the same shape as `init_config_contract.rs`: every home-shaped
//! variable points at a short empty directory, so a command that reads the
//! operator's own setup or writes state is visible here. The directory is short
//! on purpose — `background list` talks to a UNIX socket, and a long temp path
//! fails `SUN_LEN` before the command's own logic runs.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

/// A unique short home per call: tests run in parallel, and two commands sharing
/// one home would race over the same state directory (which is exactly how a
/// listing test turns into a storage error). Short, because one of these
/// commands talks to a UNIX socket and a long path fails `SUN_LEN` first.
fn scratch(name: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(1);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let path = PathBuf::from(format!("/tmp/lvl-{}-{n}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn leveler(home: &Path, args: &[&str]) -> Output {
    leveler_in(
        &std::env::current_dir().expect("the test process has a working directory"),
        home,
        args,
    )
}

fn leveler_in(cwd: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_leveler"))
        .args(args)
        .current_dir(cwd)
        .env("HOME", home)
        .env("LEVELER_HOME", home.join("leveler"))
        .env("LEVELER_CONFIG", home.join("leveler/config.toml"))
        .env("LEVELER_LANG", "zh")
        .env("NO_COLOR", "1")
        .env_remove("DEEPSEEK_API_KEY")
        .env_remove("LEVELER_API_KEY")
        .output()
        .unwrap_or_else(|err| panic!("could not run leveler {args:?}: {err}"))
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_no_secret_or_panic(rendered: &str, args: &[&str]) {
    assert!(
        !rendered.contains("sk-"),
        "{args:?} must never print a secret value: {rendered}"
    );
    assert!(
        !rendered.contains("panicked"),
        "{args:?} must not panic on a fresh install: {rendered}"
    );
}

/// A listing that must work on a machine with no configuration at all.
fn assert_lists(args: &[&str], expect: &[&str]) {
    let home = scratch("list");
    let output = leveler(&home, args);
    let rendered = text(&output);
    assert!(
        output.status.success(),
        "{args:?} must succeed with no config and no credential: {rendered}"
    );
    assert_no_secret_or_panic(&rendered, args);
    for needle in expect {
        assert!(
            rendered.contains(needle),
            "{args:?} should print {needle:?}: {rendered}"
        );
    }
    assert!(
        !home.join("leveler/config.toml").exists(),
        "{args:?} created a config file nobody asked for"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn config_show_prints_the_schema_without_a_config_file() {
    assert_lists(&["config", "show"], &["Providers", "Models"]);
}

#[test]
fn models_list_reports_the_catalogue_or_says_it_is_empty() {
    let home = scratch("models");
    let output = leveler(&home, &["models", "list"]);
    let rendered = text(&output);
    assert!(output.status.success(), "{rendered}");
    assert_no_secret_or_panic(&rendered, &["models", "list"]);
    assert!(
        rendered.contains("Models") || rendered.contains("No models"),
        "a catalogue listing says either what it has or that it has nothing: {rendered}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn theme_preview_renders_without_a_configured_theme() {
    assert_lists(&["theme", "preview"], &["THEME"]);
}

#[test]
fn the_agent_roster_lists_the_builtin_agents() {
    assert_lists(&["agents", "list"], &["Agents"]);
}

#[test]
fn the_skill_list_reports_what_is_available() {
    assert_lists(&["skills", "list"], &["Skills"]);
}

#[test]
fn sessions_list_reports_an_empty_history() {
    assert_lists(&["sessions", "list"], &[]);
}

#[test]
fn background_list_reports_no_tasks() {
    assert_lists(&["background", "list"], &[]);
}

#[test]
fn memory_list_reports_an_empty_store() {
    assert_lists(&["memory", "list"], &[]);
}

#[test]
fn memory_status_names_both_switches_and_the_experimental_lane() {
    // No config file: user facts keep the historical default, automatic
    // experience is closed until the project opts in.
    assert_lists(
        &["memory", "status"],
        &[
            "Memory switches",
            "total: on",
            "user facts: on",
            "auto experience: off",
        ],
    );
}

#[test]
fn memory_status_reports_a_closed_lane_as_off() {
    let home = scratch("memory-lane");
    let ws = scratch("memory-lane-ws");
    std::fs::create_dir_all(ws.join(".leveler")).unwrap();
    std::fs::write(
        ws.join(".leveler/config.yaml"),
        "memory:\n  enabled: true\n  user_enabled: false\n  experience_enabled: true\n",
    )
    .unwrap();
    let output = leveler_in(&ws, &home, &["memory", "status"]);
    let rendered = text(&output);
    assert!(
        output.status.success(),
        "memory status must succeed: {rendered}"
    );
    assert_no_secret_or_panic(&rendered, &["memory", "status"]);
    assert!(
        rendered.contains("user facts: off (active 0 · pending 0 · archived 0)"),
        "a closed user lane must be reported as off: {rendered}"
    );
    assert!(
        rendered.contains("auto experience: on (experimental)"),
        "the open experience lane keeps its experimental marker: {rendered}"
    );
}

#[test]
fn permission_rules_list_without_a_config() {
    assert_lists(&["permissions", "list"], &[]);
}

#[test]
fn the_lsp_and_update_surfaces_describe_themselves() {
    for args in [["lsp", "--help"], ["update", "--help"], ["eval", "--help"]] {
        let home = scratch("help");
        let output = leveler(&home, &args);
        let rendered = text(&output);
        assert!(output.status.success(), "{args:?}: {rendered}");
        assert!(
            rendered.contains("Usage") || rendered.contains("用法"),
            "{args:?} has no usage text: {rendered}"
        );
        assert_no_secret_or_panic(&rendered, &args);
        let _ = std::fs::remove_dir_all(&home);
    }
}

#[test]
fn trace_refuses_cleanly_when_this_repository_has_no_sessions() {
    // A refusal is a contract too: it must name the reason and must not be a
    // crash. `trace` with no session is the first thing anybody runs.
    let home = scratch("trace");
    let output = leveler(&home, &["trace"]);
    let rendered = text(&output);
    assert_no_secret_or_panic(&rendered, &["trace"]);
    assert!(
        !rendered.trim().is_empty(),
        "a refusal must say why it refused"
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "trace with no session is a reported refusal, not a crash: {rendered}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_group_command_without_its_subcommand_prints_usage() {
    // `models` and `model` are groups: clap exits 2 with the usage text. That is
    // the contract a script sees, so it is asserted rather than assumed.
    for args in [["models"], ["model"]] {
        let home = scratch("group");
        let output = leveler(&home, &args);
        let rendered = text(&output);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?} must exit 2 and print usage: {rendered}"
        );
        assert!(
            rendered.contains("Usage") || rendered.contains("用法"),
            "{args:?}: {rendered}"
        );
        assert_no_secret_or_panic(&rendered, &args);
        let _ = std::fs::remove_dir_all(&home);
    }
}
