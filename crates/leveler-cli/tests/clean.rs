//! `leveler clean` safety and bilingual regression tests.
//!
//! Every test drives the real binary against a throwaway `LEVELER_HOME`; none
//! touches the developer's real home.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_leveler"))
}

struct Sandbox {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    repo: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let repo = tmp.path().join("repo");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&repo).unwrap();
        Self {
            _tmp: tmp,
            home,
            repo,
        }
    }

    fn run(&self, args: &[&str], lang: &str) -> std::process::Output {
        let mut command = bin();
        command
            .arg("--repo")
            .arg(&self.repo)
            .args(["clean"])
            .args(args)
            .env("LEVELER_HOME", &self.home)
            .env("LEVELER_LANG", lang);
        command.output().expect("run leveler clean")
    }

    fn project_state(&self, id: &str, owner: &str) -> PathBuf {
        let dir = self.home.join("state/projects").join(id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(".repository-root"), owner).unwrap();
        dir
    }

    fn cache_entry(&self, name: &str, bytes: usize) -> PathBuf {
        let dir = self.home.join("cache/tools").join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("blob"), vec![0u8; bytes]).unwrap();
        dir
    }

    fn write_budget(&self, bytes: u64) {
        fs::write(
            self.home.join("config.toml"),
            format!("[storage]\ntool_cache_max_bytes = {bytes}\n"),
        )
        .unwrap();
    }
}

// C1 — dry run reports and deletes nothing.
#[test]
fn c1_dry_run_deletes_nothing() {
    let sandbox = Sandbox::new();
    let automation = sandbox.project_state(
        "-tmp-leveler-eval-rust-case-abcd0123456789abcdef",
        "/tmp/leveler-eval-rust-case",
    );
    let out = sandbox.run(&[], "zh");
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("可释放空间"), "{text}");
    assert!(text.contains("默认不删除任何内容"), "{text}");
    assert!(automation.is_dir(), "dry run must delete nothing");
}

// C2 — `--safe` removes safe items only; C3 — durable state remains.
#[test]
fn c2_c3_safe_removes_safe_and_keeps_durable() {
    let sandbox = Sandbox::new();
    let automation = sandbox.project_state(
        "-tmp-leveler-eval-rust-case-abcd0123456789abcdef",
        "/tmp/leveler-eval-rust-case",
    );
    let deleted = sandbox.project_state("-Users-me-gone-0123456789abcdef", "/Users/me/gone");
    fs::write(deleted.join("sessions.db"), b"durable history").unwrap();

    let out = sandbox.run(&["--safe"], "zh");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!automation.exists(), "safe item must be reclaimed");
    assert!(deleted.is_dir(), "deleted-project data must survive --safe");
    assert!(deleted.join("sessions.db").is_file());
}

// C3b — only `--needs-confirmation` touches confirmation-only items.
#[test]
fn confirmation_is_required_for_deleted_project_data() {
    let sandbox = Sandbox::new();
    let deleted = sandbox.project_state("-Users-me-gone-0123456789abcdef", "/Users/me/gone");
    let out = sandbox.run(&["--safe"], "en");
    assert!(out.status.success());
    assert!(
        deleted.is_dir(),
        "safe must never remove durable-adjacent data"
    );
    let out = sandbox.run(&["--needs-confirmation"], "en");
    assert!(out.status.success());
    assert!(!deleted.exists(), "explicit confirmation may remove it");
}

// C7 — both languages render.
#[test]
fn c7_bilingual_render() {
    let sandbox = Sandbox::new();
    sandbox.cache_entry("old", 1024);
    let zh = String::from_utf8_lossy(&sandbox.run(&[], "zh").stdout).to_string();
    assert!(zh.contains("可释放空间"), "{zh}");
    assert!(zh.contains("安全可清理总计"), "{zh}");
    let en = String::from_utf8_lossy(&sandbox.run(&[], "en").stdout).to_string();
    assert!(en.contains("Reclaimable storage"), "{en}");
    assert!(en.contains("Safe to reclaim"), "{en}");
}

// GC2/GC3 end-to-end: over budget reclaims the oldest entry and stops.
#[test]
fn gc_over_budget_reclaims_oldest_entry() {
    let sandbox = Sandbox::new();
    let old = sandbox.cache_entry("old", 6 * 1024 * 1024);
    let new = sandbox.cache_entry("new", 6 * 1024 * 1024);
    // Age `old` well past the idle TTL by touching its marker in the past.
    let marker = old.join(".leveler-last-used");
    fs::write(&marker, b"").unwrap();
    set_old_mtime(&marker);
    sandbox.write_budget(8 * 1024 * 1024);

    let out = sandbox.run(&["--cache", "--manifest", "/dev/null"], "en");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!old.exists(), "oldest idle cache must be reclaimed");
    assert!(new.exists(), "recent cache must be preserved");
}

// JSON plan is machine readable and mirrors the human plan.
#[test]
fn json_plan_lists_entries() {
    let sandbox = Sandbox::new();
    sandbox.project_state(
        "-tmp-leveler-eval-x-abcd0123456789abcdef",
        "/tmp/leveler-eval-x",
    );
    let out = sandbox.run(&["--json"], "en");
    assert!(out.status.success());
    let plan: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid json");
    let entries = plan["entries"].as_array().expect("entries array");
    assert!(
        entries.iter().any(|e| e["kind"] == "historical_automation"),
        "{plan}"
    );
}

fn set_old_mtime(path: &Path) {
    // No unsafe: `touch -t` is available on the Unix CI hosts this test runs on.
    let _ = Command::new("touch")
        .args(["-t", "202001010000", &path.display().to_string()])
        .status();
}
