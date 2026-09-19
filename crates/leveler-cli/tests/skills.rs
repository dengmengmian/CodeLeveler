//! `leveler skills`: the resolved skill registry without a UI, in a temp
//! `LEVELER_HOME` and `HOME`.

use std::path::Path;
use std::process::Command;

fn skill(root: &Path, name: &str, description: &str, body: &str) {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}\n"),
    )
    .unwrap();
}

fn run(home: &Path, os_home: &Path, repo: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_leveler"))
        .env("LEVELER_HOME", home)
        .env("HOME", os_home)
        .env("USERPROFILE", os_home)
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
    os_home: tempfile::TempDir,
    repo: tempfile::TempDir,
}

fn fixture() -> Fx {
    let home = tempfile::tempdir().unwrap();
    let os_home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let project = repo.path().join(".leveler/skills");
    skill(
        &project,
        "windows-ci-debug",
        "Diagnose intermittent Windows CI failures.",
        "1. Reproduce.\n2. Bisect.",
    );
    // A broken package: invalid YAML must be diagnosable, not fatal.
    let broken = project.join("broken");
    std::fs::create_dir_all(&broken).unwrap();
    std::fs::write(broken.join("SKILL.md"), "---\nname: [oops\n---\nx\n").unwrap();
    // A user-native skill and a Codex-compatible skill of the same name: the
    // native one wins and the other is visible as shadowed.
    skill(
        &home.path().join("skills"),
        "fanui",
        "Native UI skill.",
        "N.",
    );
    skill(
        &os_home.path().join(".codex/skills"),
        "fanui",
        "Codex UI skill.",
        "C.",
    );
    skill(
        &os_home.path().join(".agents/skills"),
        "find-skills",
        "Discover skills.",
        "F.",
    );
    Fx {
        home,
        os_home,
        repo,
    }
}

#[test]
fn list_shows_scope_source_status_and_shadowing() {
    let fx = fixture();
    let (ok, out) = run(
        fx.home.path(),
        fx.os_home.path(),
        fx.repo.path(),
        &["skills", "list"],
    );
    assert!(ok, "{out}");
    assert!(out.contains("windows-ci-debug"), "{out}");
    assert!(
        out.contains("project") && out.contains("CodeLeveler"),
        "{out}"
    );
    assert!(out.contains("broken") && out.contains("invalid"), "{out}");
    assert!(
        out.contains("fanui") && out.contains("shadows user Codex"),
        "{out}"
    );
    assert!(out.contains("find-skills"), "external discovered: {out}");
    assert!(out.contains("skill-creator"), "built-in listed: {out}");
}

#[test]
fn list_json_is_machine_readable() {
    let fx = fixture();
    let (ok, out) = run(
        fx.home.path(),
        fx.os_home.path(),
        fx.repo.path(),
        &["skills", "list", "--json"],
    );
    assert!(ok, "{out}");
    let value: serde_json::Value = serde_json::from_str(&out).expect("json");
    let skills = value["skills"].as_array().expect("skills array");
    let find = |n: &str| {
        skills
            .iter()
            .find(|s| s["name"] == n)
            .unwrap_or_else(|| panic!("{n}"))
    };
    assert_eq!(find("windows-ci-debug")["scope"], "project");
    assert_eq!(find("windows-ci-debug")["source"], "native");
    assert_eq!(find("broken")["status"], "invalid");
    assert_eq!(find("fanui")["source"], "native");
    assert_eq!(find("fanui")["shadowed"][0]["source"], "codex");
    assert_eq!(find("find-skills")["source"], "agent_skills");
}

#[test]
fn show_prints_one_skill_and_refuses_an_unknown_name() {
    let fx = fixture();
    let (ok, out) = run(
        fx.home.path(),
        fx.os_home.path(),
        fx.repo.path(),
        &["skills", "show", "windows-ci-debug"],
    );
    assert!(ok, "{out}");
    assert!(out.contains("1. Reproduce."), "{out}");
    assert!(out.contains("CodeLeveler"), "{out}");
    let (ok, out) = run(
        fx.home.path(),
        fx.os_home.path(),
        fx.repo.path(),
        &["skills", "show", "nope"],
    );
    assert!(!ok, "{out}");
    assert!(out.contains("not found"), "{out}");
}
