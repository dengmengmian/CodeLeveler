//! `leveler agents` and the agent line of `leveler doctor`: a mechanical way to
//! see the resolved registry without a UI, in a temp LEVELER_HOME.

use std::path::Path;
use std::process::Command;

fn agent(root: &Path, name: &str, yaml_body: &str, instructions: &str) {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("agent.yaml"),
        format!("version: 1\nname: {name}\ndescription: The {name} agent.\n{yaml_body}"),
    )
    .unwrap();
    std::fs::write(dir.join("instructions.md"), instructions).unwrap();
}

fn run(home: &Path, repo: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_leveler"))
        .env("LEVELER_HOME", home)
        .arg("--repo")
        .arg(repo)
        .args(args)
        .output()
        .expect("spawn leveler");
    (
        out.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

struct Fx {
    home: tempfile::TempDir,
    repo: tempfile::TempDir,
}

fn fixture() -> Fx {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let project = repo.path().join(".leveler/agents");
    agent(
        &project,
        "security-reviewer",
        "capability: read_only\n",
        "Only exploitable issues.",
    );
    agent(
        &project,
        "broken",
        "capability: read_only\nwirte: true\n",
        "x",
    );
    agent(
        &project,
        "rust-explorer",
        "capability: read_only\n",
        "Project explorer.",
    );
    agent(
        &home.path().join("agents"),
        "rust-explorer",
        "capability: read_only\n",
        "User explorer.",
    );
    Fx { home, repo }
}

#[test]
fn list_shows_sources_status_and_shadowing() {
    let fx = fixture();
    let (ok, out) = run(fx.home.path(), fx.repo.path(), &["agents", "list"]);
    assert!(ok, "{out}");
    assert!(out.contains("security-reviewer"), "{out}");
    assert!(out.contains("broken") && out.contains("wirte"), "{out}");
    assert!(
        out.contains("rust-explorer") && out.contains("shadows user"),
        "{out}"
    );
    assert!(out.contains("code-explorer"), "built-ins are listed: {out}");
}

#[test]
fn list_json_is_machine_readable() {
    let fx = fixture();
    let (ok, out) = run(
        fx.home.path(),
        fx.repo.path(),
        &["agents", "list", "--json"],
    );
    assert!(ok, "{out}");
    let value: serde_json::Value = serde_json::from_str(&out).expect("json");
    let agents = value["agents"].as_array().expect("agents array");
    let find = |n: &str| {
        agents
            .iter()
            .find(|a| a["name"] == n)
            .unwrap_or_else(|| panic!("{n}"))
    };
    assert_eq!(find("security-reviewer")["source"], "project");
    assert_eq!(find("broken")["status"], "invalid");
    assert_eq!(find("rust-explorer")["shadowed"][0]["source"], "user");
}

#[test]
fn show_prints_one_definition_and_refuses_an_unknown_name() {
    let fx = fixture();
    let (ok, out) = run(
        fx.home.path(),
        fx.repo.path(),
        &["agents", "show", "security-reviewer"],
    );
    assert!(ok, "{out}");
    assert!(out.contains("Only exploitable issues."), "{out}");
    assert!(out.contains("read_only"), "{out}");
    let (ok, out) = run(
        fx.home.path(),
        fx.repo.path(),
        &["agents", "show", "security"],
    );
    assert!(!ok, "{out}");
    assert!(out.contains("not found"), "{out}");
}

#[test]
fn doctor_reports_agent_definitions() {
    let fx = fixture();
    let (_, out) = run(fx.home.path(), fx.repo.path(), &["doctor"]);
    let line = out
        .lines()
        .find(|l| l.contains("agents"))
        .unwrap_or_else(|| panic!("no agents line: {out}"));
    assert!(line.contains("1 invalid"), "{line}");
    assert!(line.contains("broken"), "{line}");
}
