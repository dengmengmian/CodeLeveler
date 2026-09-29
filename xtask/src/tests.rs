use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::cli::{Outcome, dispatch_with};
use crate::exec::{CommandSpec, FakeHost, ProcessOut, SystemHost};
use crate::git::Snapshot;
use crate::qualify::{self, dogfood_exit};
use crate::root::{self, Env, GateOpts};
use crate::version::{self, Level};

const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const BASELINE: &str = "0123456789abcdef0123456789abcdef01234567";
const RUN: &str = "eval/release/20260101-000000-aaaaaaaaaaaa";

struct Tmp {
    path: PathBuf,
}

impl Tmp {
    fn new() -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(1);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "xtask-dev-{}-{}-{}",
            std::process::id(),
            n,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

fn is_cmd(spec: &CommandSpec, program: &str, expected: &[&str]) -> bool {
    spec.program == program
        && spec
            .args
            .iter()
            .map(String::as_str)
            .eq(expected.iter().copied())
}

fn arg_after(spec: &CommandSpec, flag: &str) -> Option<String> {
    spec.args
        .windows(2)
        .find(|pair| pair[0] == flag)
        .map(|pair| pair[1].clone())
}

fn touch(path: &Path) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, b"x").unwrap();
}

fn git(repo: &Path, git_args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(git_args)
        .current_dir(repo)
        .status()
        .unwrap_or_else(|err| panic!("git {git_args:?}: {err}"));
    assert!(status.success(), "git {git_args:?}");
}

fn git_commit(repo: &Path) {
    let status = std::process::Command::new("git")
        .args([
            "-c",
            "user.email=dev@example.com",
            "-c",
            "user.name=dev",
            "commit",
            "-m",
            "init",
        ])
        .current_dir(repo)
        .status()
        .unwrap();
    assert!(status.success(), "git commit");
}

fn write_versions(repo: &Path, version: &str) {
    std::fs::write(
        repo.join("Cargo.toml"),
        format!(
            "[workspace.package]\nversion = \"{version}\"\nrepository = \"https://github.com/example/codeleveler\"\n\n[workspace.dependencies]\nleveler-core = {{ version = \"{version}\", path = \"crates/leveler-core\" }}\ntokio = {{ version = \"1.0.8\" }}\n"
        ),
    )
    .unwrap();
    std::fs::create_dir_all(repo.join("docs")).unwrap();
    std::fs::write(
        repo.join("docs/RELEASE.md"),
        format!("# CodeLeveler {version}\n\nNotes mention {version}.\n"),
    )
    .unwrap();
    std::fs::write(
        repo.join("docs/RELEASE.zh-CN.md"),
        format!("# CodeLeveler {version}\n"),
    )
    .unwrap();
}

fn write_lab(lab: &Path, baseline: &str) {
    std::fs::create_dir_all(lab.join("eval/scripts")).unwrap();
    std::fs::write(lab.join("eval/scripts/release_check.py"), b"# marker\n").unwrap();
    std::fs::write(lab.join("eval/scripts/rc_check.py"), b"# marker\n").unwrap();
    std::fs::create_dir_all(lab.join("repos/codeleveler")).unwrap();
    std::fs::create_dir_all(lab.join("eval/release")).unwrap();
    std::fs::write(lab.join("eval/release/BASELINE"), format!("{baseline}\n")).unwrap();
}

fn point_config(repo: &Path, lab: &Path, model: &str) {
    std::fs::create_dir_all(repo.join(".dev")).unwrap();
    std::fs::write(
        repo.join(".dev/config"),
        format!("dogfood_root={}\nmodel={model}\n", lab.display()),
    )
    .unwrap();
}

fn pass_layers(lab: &Path) {
    let dir = lab.join(RUN);
    std::fs::create_dir_all(&dir).unwrap();
    let l0 = r#"{"status":"PASS","checks":[{"id":"fmt","status":"PASS"}]}"#;
    std::fs::write(dir.join("L0.json"), l0).unwrap();
    std::fs::write(dir.join("L1.json"), r#"{"status":"PASS","checks":[]}"#).unwrap();
    std::fs::write(dir.join("L2.json"), r#"{"status":"PASS","checks":[]}"#).unwrap();
}

fn process(status: i32, stdout: &str) -> ProcessOut {
    ProcessOut {
        status,
        stdout: stdout.to_string(),
        stderr: String::new(),
    }
}

fn text(stdout: &str) -> ProcessOut {
    process(0, stdout)
}

fn assert_fail(outcome: &Outcome, code: &str) {
    assert!(
        outcome.stderr.contains(&format!("FAIL {code}")),
        "expected {code}, got code {} stderr {} stdout {}",
        outcome.code,
        outcome.stderr,
        outcome.stdout
    );
}

fn unformatted() -> &'static str {
    "fn demo() {\nlet _ = 1;\n}\n"
}

#[test]
fn fmt_formats_only_the_staged_file() {
    let tmp = Tmp::new();
    let repo = tmp.path.as_path();
    git(repo, &["init"]);
    std::fs::write(repo.join("rustfmt.toml"), "edition = \"2024\"\n").unwrap();
    std::fs::write(repo.join("owned.rs"), "fn demo() {\n    let _ = 1;\n}\n").unwrap();
    std::fs::write(repo.join("other.rs"), "fn demo() {\n    let _ = 1;\n}\n").unwrap();
    git(repo, &["add", "owned.rs", "other.rs", "rustfmt.toml"]);
    git_commit(repo);
    std::fs::write(repo.join("owned.rs"), unformatted()).unwrap();
    std::fs::write(repo.join("other.rs"), unformatted()).unwrap();
    git(repo, &["add", "owned.rs"]);
    let other_before = std::fs::read(repo.join("other.rs")).unwrap();
    let outcome = dispatch_with(
        repo,
        &args(&["fmt"]),
        &Env::default(),
        &SystemHost { echo: false },
    );
    assert_eq!(outcome.code, 0, "{}", outcome.stderr);
    let owned = std::fs::read_to_string(repo.join("owned.rs")).unwrap();
    assert!(owned.contains("    let _ = 1;"), "{owned}");
    assert_eq!(std::fs::read(repo.join("other.rs")).unwrap(), other_before);
}

#[test]
fn fmt_explicit_path_leaves_other_staged_file() {
    let tmp = Tmp::new();
    let repo = tmp.path.as_path();
    git(repo, &["init"]);
    std::fs::write(repo.join("rustfmt.toml"), "edition = \"2024\"\n").unwrap();
    std::fs::write(repo.join("owned.rs"), unformatted()).unwrap();
    std::fs::write(repo.join("other.rs"), unformatted()).unwrap();
    git(repo, &["add", "owned.rs", "other.rs"]);
    let other_before = std::fs::read(repo.join("other.rs")).unwrap();
    let outcome = dispatch_with(
        repo,
        &args(&["fmt", "owned.rs"]),
        &Env::default(),
        &SystemHost { echo: false },
    );
    assert_eq!(outcome.code, 0, "{}", outcome.stderr);
    assert!(
        std::fs::read_to_string(repo.join("owned.rs"))
            .unwrap()
            .contains("    let _ = 1;")
    );
    assert_eq!(std::fs::read(repo.join("other.rs")).unwrap(), other_before);
}

#[test]
fn fmt_reports_parse_failure() {
    let tmp = Tmp::new();
    let repo = tmp.path.as_path();
    git(repo, &["init"]);
    std::fs::write(repo.join("rustfmt.toml"), "edition = \"2024\"\n").unwrap();
    std::fs::write(repo.join("bad.rs"), "fn (\n").unwrap();
    let outcome = dispatch_with(
        repo,
        &args(&["fmt", "bad.rs"]),
        &Env::default(),
        &SystemHost { echo: false },
    );
    assert_fail(&outcome, "FMT_FAIL");
}

#[test]
fn fmt_verify_stops_before_cargo_when_rustfmt_fails() {
    let tmp = Tmp::new();
    let repo = tmp.path.as_path();
    write_versions(repo, "1.0.8");
    std::fs::write(repo.join("bad.rs"), "fn (\n").unwrap();
    let host = FakeHost::new(|spec| {
        if spec.program == "rustfmt" {
            return Ok(ProcessOut {
                status: 1,
                stdout: String::new(),
                stderr: "error: expected identifier\n".to_string(),
            });
        }
        if spec.program == "cargo" {
            panic!("verify must not cargo after rustfmt fails");
        }
        Ok(text(""))
    });
    let outcome = dispatch_with(repo, &args(&["verify", "bad.rs"]), &Env::default(), &host);
    assert_fail(&outcome, "FMT_FAIL");
}

#[test]
fn clean_default_keeps_target_and_unknown_dirs() {
    let tmp = Tmp::new();
    let repo = tmp.path.as_path();
    touch(&repo.join("target/debug/keep"));
    touch(&repo.join("mystery/keep"));
    touch(&repo.join("dist/gone"));
    touch(&repo.join("crates/leveler-execution/leveler-tool-cache/gone"));
    touch(&repo.join("crates/leveler-execution/leveler-ac-test/gone"));
    touch(&repo.join(".shots-1/gone"));
    let host = FakeHost::new(|_| panic!("default clean must not spawn a command"));
    let outcome = dispatch_with(repo, &args(&["clean"]), &Env::default(), &host);
    assert_eq!(outcome.code, 0, "{}", outcome.stderr);
    assert!(repo.join("target/debug/keep").is_file());
    assert!(repo.join("mystery/keep").is_file());
    assert!(!repo.join("dist").exists());
    assert!(
        !repo
            .join("crates/leveler-execution/leveler-tool-cache")
            .exists()
    );
    assert!(
        !repo
            .join("crates/leveler-execution/leveler-ac-test")
            .exists()
    );
    assert!(!repo.join(".shots-1").exists());
}

#[test]
fn clean_all_invokes_cargo_clean_and_keeps_unknown_dirs() {
    let tmp = Tmp::new();
    let repo = tmp.path.as_path();
    touch(&repo.join("target/debug/keep"));
    touch(&repo.join("mystery/keep"));
    touch(&repo.join("dist/gone"));
    touch(&repo.join("apps/leveler-mobile/build/gone"));
    touch(&repo.join("crates/leveler-web/web/dist/gone"));
    touch(&repo.join(".codeleveler-target/keep"));
    let host = FakeHost::new(|spec| {
        assert_eq!(spec.program, "cargo");
        assert_eq!(spec.args.first().map(String::as_str), Some("clean"));
        Ok(text(""))
    });
    let outcome = dispatch_with(repo, &args(&["clean", "--all"]), &Env::default(), &host);
    assert_eq!(outcome.code, 0, "{}", outcome.stderr);
    assert!(repo.join("target/debug/keep").is_file());
    assert!(repo.join("mystery/keep").is_file());
    assert!(!repo.join("dist").exists());
    assert!(!repo.join("apps/leveler-mobile/build").exists());
    assert!(!repo.join("crates/leveler-web/web/dist").exists());
    let log = host.log();
    assert!(log.iter().any(|spec| is_cmd(spec, "cargo", &["clean"])));
    assert!(log.iter().any(|spec| {
        is_cmd(
            spec,
            "cargo",
            &["clean", "--target-dir", ".codeleveler-target"],
        )
    }));
}

/// Unix only: creating a symlink on Windows needs a privilege the runner may
/// not have, and an optional OS capability must be detected, not assumed. The
/// guard under test reads link metadata, so unix exercises the same code.
#[cfg(unix)]
#[test]
fn clean_unlinks_registered_symlink_without_following_it() {
    let tmp = Tmp::new();
    let outside = Tmp::new();
    let repo = tmp.path.as_path();
    touch(&outside.path.join("secret"));
    std::os::unix::fs::symlink(outside.path.join("secret"), repo.join("dist")).unwrap();
    let host = FakeHost::new(|_| panic!("default clean must not spawn a command"));
    let outcome = dispatch_with(repo, &args(&["clean"]), &Env::default(), &host);
    assert_eq!(outcome.code, 0, "{}", outcome.stderr);
    assert!(outside.path.join("secret").is_file());
    assert!(std::fs::symlink_metadata(repo.join("dist")).is_err());
}

#[test]
fn clean_eval_is_not_a_local_target() {
    let tmp = Tmp::new();
    let host = FakeHost::new(|_| Ok(text("")));
    let outcome = dispatch_with(
        tmp.path.as_path(),
        &args(&["clean", "eval"]),
        &Env::default(),
        &host,
    );
    assert_fail(&outcome, "UNKNOWN_CLEAN_TARGET");
    assert!(outcome.stderr.contains("reclaim_builds.py"));
}

#[test]
fn publish_levels_malformed_mismatch_and_existing_tag() {
    let current = version::parse_stable("1.0.9").unwrap();
    assert_eq!(
        version::bump(current, Level::Patch).unwrap().to_string(),
        "1.0.10"
    );
    assert_eq!(
        version::bump(current, Level::Minor).unwrap().to_string(),
        "1.1.0"
    );
    assert_eq!(
        version::bump(current, Level::Major).unwrap().to_string(),
        "2.0.0"
    );

    let tmp = Tmp::new();
    let repo = tmp.path.as_path();
    std::fs::write(
        repo.join("Cargo.toml"),
        "[workspace.package]\nversion = \"1.2\"\nrepository = \"https://github.com/example/codeleveler\"\n",
    )
    .unwrap();
    let host = publish_host("", "v1.0.8\n", "v1.0.8\n");
    let outcome = dispatch_with(
        repo,
        &args(&["publish", "--dry-run"]),
        &Env::default(),
        &host,
    );
    assert_fail(&outcome, "MALFORMED_VERSION");

    let mismatch = Tmp::new();
    write_versions(mismatch.path.as_path(), "1.0.8");
    let host = publish_host("", "v1.0.7\n", "v1.0.8\n");
    let outcome = dispatch_with(
        mismatch.path.as_path(),
        &args(&["publish", "--dry-run"]),
        &Env::default(),
        &host,
    );
    assert_fail(&outcome, "VERSION_MISMATCH");

    let tagged = Tmp::new();
    write_versions(tagged.path.as_path(), "1.0.8");
    let host = publish_host("", "v1.0.8\nv1.0.9\n", "v1.0.8\n");
    let outcome = dispatch_with(
        tagged.path.as_path(),
        &args(&["publish", "--dry-run"]),
        &Env::default(),
        &host,
    );
    assert_fail(&outcome, "TAG_EXISTS");
}

#[test]
fn publish_dry_run_does_not_mutate_or_invoke_gates() {
    let tmp = Tmp::new();
    let repo = tmp.path.as_path();
    write_versions(repo, "1.0.8");
    let before = std::fs::read(repo.join("Cargo.toml")).unwrap();
    let host = publish_host("", "v1.0.8\n", "v1.0.8\n");
    let outcome = dispatch_with(
        repo,
        &args(&["publish", "--minor", "--dry-run"]),
        &Env::default(),
        &host,
    );
    assert_eq!(outcome.code, 0, "{}", outcome.stderr);
    assert!(outcome.stdout.contains("next: 1.1.0"), "{}", outcome.stdout);
    assert!(outcome.stdout.contains("WOULD_RUN: pre-release"));
    assert!(outcome.stdout.contains("WOULD_RUN: dogfood"));
    assert!(outcome.stdout.contains("READY_TO_PUBLISH: NO"));
    assert_eq!(std::fs::read(repo.join("Cargo.toml")).unwrap(), before);
    for spec in host.log() {
        assert_ne!(spec.program, "python3");
        assert_ne!(spec.program, "cargo");
        assert_ne!(spec.args.first().map(String::as_str), Some("commit"));
        assert_ne!(spec.args.first().map(String::as_str), Some("push"));
        assert!(!spec.args.iter().any(|arg| arg == "-a" || arg == "--force"));
    }
}

#[test]
fn publish_refuses_conflicting_levels_and_dirty_tree() {
    let tmp = Tmp::new();
    let host = FakeHost::new(|_| Ok(text("")));
    let outcome = dispatch_with(
        tmp.path.as_path(),
        &args(&["publish", "--patch", "--major"]),
        &Env::default(),
        &host,
    );
    assert_fail(&outcome, "PUBLISH_LEVEL_CONFLICT");

    write_versions(tmp.path.as_path(), "1.0.8");
    let host = publish_host(" M README.md\n", "v1.0.8\n", "v1.0.8\n");
    let outcome = dispatch_with(
        tmp.path.as_path(),
        &args(&["publish", "--dry-run"]),
        &Env::default(),
        &host,
    );
    assert_fail(&outcome, "WORKTREE_DIRTY");
    assert!(host.log().iter().all(|spec| spec.program != "python3"));
}

#[test]
fn publish_qualification_failure_does_not_tag_or_push() {
    let repo_tmp = Tmp::new();
    let lab_tmp = Tmp::new();
    let repo = repo_tmp.path.canonicalize().unwrap();
    let lab = lab_tmp.path.canonicalize().unwrap();
    write_versions(&repo, "1.0.8");
    write_lab(&lab, BASELINE);
    point_config(&repo, &lab, "example/model");
    let state = Mutex::new((false, false));
    let host = FakeHost::new(move |spec| {
        let mut state = state.lock().expect("state");
        if is_cmd(spec, "git", &["rev-parse", "HEAD"]) {
            return Ok(text(if state.1 { SHA_B } else { SHA_A }));
        }
        if is_cmd(spec, "git", &["status", "--porcelain"]) {
            if state.0 && !state.1 {
                return Ok(text(
                    " M Cargo.toml\n M docs/RELEASE.md\n M docs/RELEASE.zh-CN.md\n",
                ));
            }
            return Ok(text(""));
        }
        if is_cmd(spec, "git", &["rev-parse", "--abbrev-ref", "HEAD"]) {
            return Ok(text("main"));
        }
        if is_cmd(spec, "git", &["remote"]) {
            return Ok(text("origin"));
        }
        if is_cmd(spec, "git", &["tag", "--list"]) {
            return Ok(text("v1.0.8\n"));
        }
        if spec.program == "gh" {
            return Ok(text("v1.0.8\n"));
        }
        if spec.program == "git" && spec.args.first().map(String::as_str) == Some("ls-remote") {
            return Ok(text(""));
        }
        if spec.program == "cargo" {
            state.0 = true;
            return Ok(text(""));
        }
        if spec.program == "git" && spec.args.first().map(String::as_str) == Some("add") {
            return Ok(text(""));
        }
        if spec.program == "git" && spec.args.first().map(String::as_str) == Some("commit") {
            assert_eq!(
                spec.args.get(2).map(String::as_str),
                Some("release: v1.0.9")
            );
            state.1 = true;
            return Ok(text(""));
        }
        if spec.program == "git" && spec.args.first().map(String::as_str) == Some("cat-file") {
            return Ok(text(""));
        }
        if spec.program == "python3" {
            pass_layers(&lab);
            let l0 = lab.join(RUN).join("L0.json");
            std::fs::write(
                l0,
                r#"{"status":"FAIL","checks":[{"id":"fmt","status":"PASS"}]}"#,
            )
            .unwrap();
            return Ok(process(1, &format!("written: {RUN}/\n")));
        }
        if spec.program == "git" && spec.args.first().map(String::as_str) == Some("push") {
            panic!("qualification failure must not push: {:?}", spec.args);
        }
        if spec.program == "git"
            && spec.args.first().map(String::as_str) == Some("tag")
            && spec.args.iter().any(|arg| arg == "-a")
        {
            panic!(
                "qualification failure must not create a tag: {:?}",
                spec.args
            );
        }
        Ok(text(""))
    });
    let outcome = dispatch_with(&repo, &args(&["publish"]), &Env::default(), &host);
    assert_ne!(outcome.code, 0);
    let rendered = format!("{}{}", outcome.stdout, outcome.stderr);
    assert!(
        rendered.contains("did not tag, push, or reset"),
        "{rendered}"
    );
    assert!(
        std::fs::read_to_string(repo.join("Cargo.toml"))
            .unwrap()
            .contains("version = \"1.0.9\"")
    );
    assert!(host.log().iter().all(|spec| {
        spec.args.first().map(String::as_str) != Some("push")
            && !(spec.args.first().map(String::as_str) == Some("tag")
                && spec.args.iter().any(|arg| arg == "-a"))
    }));
}

#[test]
fn dogfood_reads_baseline_file_and_stops_when_sha_changes() {
    let repo_tmp = Tmp::new();
    let lab_tmp = Tmp::new();
    let repo = repo_tmp.path.canonicalize().unwrap();
    let lab = lab_tmp.path.canonicalize().unwrap();
    write_versions(&repo, "1.0.8");
    write_lab(&lab, BASELINE);
    point_config(&repo, &lab, "example/model");
    let heads = Mutex::new(0u32);
    let host = FakeHost::new(move |spec| {
        if is_cmd(spec, "git", &["rev-parse", "HEAD"]) {
            let mut seen = heads.lock().expect("heads");
            *seen += 1;
            return Ok(text(if *seen == 1 { SHA_A } else { SHA_B }));
        }
        if is_cmd(spec, "git", &["status", "--porcelain"]) {
            return Ok(text(""));
        }
        if spec.program == "git" && spec.args.first().map(String::as_str) == Some("cat-file") {
            return Ok(text(""));
        }
        if spec.program == "python3" {
            return Ok(process(0, &format!("written: {RUN}/\n")));
        }
        Ok(text(""))
    });
    let locked = Snapshot {
        head: SHA_A.to_string(),
        porcelain: String::new(),
    };
    let err =
        qualify::run_dogfood_locked(&host, &repo, &Env::default(), &GateOpts::default(), &locked)
            .expect_err("sha change");
    assert_eq!(err.code, "CANDIDATE_CHANGED");
    let release: Vec<_> = host
        .log()
        .into_iter()
        .filter(|spec| spec.args.iter().any(|arg| arg.contains("release_check.py")))
        .collect();
    assert_eq!(release.len(), 1);
    assert_eq!(
        arg_after(&release[0], "--baseline-sha").as_deref(),
        Some(BASELINE)
    );
    assert_eq!(
        arg_after(&release[0], "--model").as_deref(),
        Some("example/model")
    );
    assert_eq!(
        arg_after(&release[0], "--codeleveler-sha").as_deref(),
        Some(SHA_A)
    );
    assert!(!release[0].args.iter().any(|arg| arg == "--force"));
    assert!(
        host.log()
            .iter()
            .all(|spec| !spec.args.iter().any(|arg| arg.contains("rc_check.py")))
    );
}

#[test]
fn dogfood_does_not_start_when_the_locked_sha_already_changed() {
    let repo_tmp = Tmp::new();
    let lab_tmp = Tmp::new();
    let repo = repo_tmp.path.canonicalize().unwrap();
    write_versions(&repo, "1.0.8");
    write_lab(lab_tmp.path.as_path(), BASELINE);
    point_config(
        &repo,
        &lab_tmp.path.canonicalize().unwrap(),
        "example/model",
    );
    let host = FakeHost::new(|spec| {
        if is_cmd(spec, "git", &["rev-parse", "HEAD"]) {
            return Ok(text(SHA_B));
        }
        if is_cmd(spec, "git", &["status", "--porcelain"]) {
            return Ok(text(""));
        }
        if spec.program == "python3" {
            panic!("gate must not start");
        }
        Ok(text(""))
    });
    let err = qualify::run_dogfood_locked(
        &host,
        &repo,
        &Env::default(),
        &GateOpts::default(),
        &Snapshot {
            head: SHA_A.to_string(),
            porcelain: String::new(),
        },
    )
    .expect_err("changed");
    assert_eq!(err.code, "CANDIDATE_CHANGED");
}

#[test]
fn release_does_not_start_dogfood_after_the_candidate_changes() {
    let repo_tmp = Tmp::new();
    let lab_tmp = Tmp::new();
    let repo = repo_tmp.path.canonicalize().unwrap();
    let lab = lab_tmp.path.canonicalize().unwrap();
    write_versions(&repo, "1.0.8");
    write_lab(&lab, BASELINE);
    point_config(&repo, &lab, "example/model");
    let revs_after_layers = Mutex::new(0u32);
    let saw_layers = Mutex::new(false);
    let host = FakeHost::new(move |spec| {
        if spec.program == "python3" && spec.args.iter().any(|arg| arg == "--layers") {
            *saw_layers.lock().expect("flag") = true;
            pass_layers(&lab);
            return Ok(process(1, &format!("written: {RUN}/\n")));
        }
        if spec.program == "python3" {
            panic!("full dogfood gate must not start: {:?}", spec.args);
        }
        if is_cmd(spec, "git", &["rev-parse", "HEAD"]) {
            if *saw_layers.lock().expect("flag") {
                let mut count = revs_after_layers.lock().expect("count");
                *count += 1;
                if *count > 1 {
                    return Ok(text(SHA_B));
                }
            }
            return Ok(text(SHA_A));
        }
        if is_cmd(spec, "git", &["status", "--porcelain"])
            || spec.program == "git" && spec.args.first().map(String::as_str) == Some("cat-file")
        {
            return Ok(text(""));
        }
        Ok(text(""))
    });
    let err = qualify::run_release_locked(
        &host,
        &repo,
        &Env::default(),
        &GateOpts::default(),
        &Snapshot {
            head: SHA_A.to_string(),
            porcelain: String::new(),
        },
    )
    .expect_err("changed between stages");
    assert_eq!(err.code, "CANDIDATE_CHANGED");
    assert!(
        host.log()
            .iter()
            .any(|spec| spec.args.iter().any(|arg| arg == "--layers"))
    );
}

#[test]
fn dogfood_gate_exit_controls_whether_rc_starts() {
    assert_eq!(dogfood_exit(0, Some(0)), 0);
    assert_eq!(dogfood_exit(2, Some(0)), 2);
    assert_eq!(dogfood_exit(0, Some(2)), 2);
    assert_eq!(dogfood_exit(1, None), 1);
    assert_eq!(dogfood_exit(3, None), 3);
    assert_eq!(dogfood_exit(0, Some(4)), 4);

    let (code, rc_started) = scripted_dogfood(1);
    assert_eq!(code, 1);
    assert!(!rc_started);
    let (code, rc_started) = scripted_dogfood(3);
    assert_eq!(code, 3);
    assert!(!rc_started);
    let (code, rc_started) = scripted_dogfood(2);
    assert_eq!(code, 2);
    assert!(rc_started);
}

fn scripted_dogfood(release_status: i32) -> (i32, bool) {
    let repo_tmp = Tmp::new();
    let lab_tmp = Tmp::new();
    let repo = repo_tmp.path.canonicalize().unwrap();
    let lab = lab_tmp.path.canonicalize().unwrap();
    write_versions(&repo, "1.0.8");
    write_lab(&lab, BASELINE);
    point_config(&repo, &lab, "example/model");
    let host = FakeHost::new(move |spec| {
        if is_cmd(spec, "git", &["rev-parse", "HEAD"]) {
            return Ok(text(SHA_A));
        }
        if is_cmd(spec, "git", &["status", "--porcelain"])
            || spec.program == "git" && spec.args.first().map(String::as_str) == Some("cat-file")
        {
            return Ok(text(""));
        }
        if spec.program == "python3" && spec.args.iter().any(|arg| arg.contains("release_check.py"))
        {
            return Ok(process(release_status, &format!("written: {RUN}/\n")));
        }
        if spec.program == "python3" && spec.args.iter().any(|arg| arg.contains("rc_check.py")) {
            assert_eq!(arg_after(spec, "--release-run").as_deref(), Some(RUN));
            assert_eq!(arg_after(spec, "--candidate-sha").as_deref(), Some(SHA_A));
            return Ok(process(
                0,
                "written: eval/rc/20260101-000000-aaaaaaaaaaaa/\n",
            ));
        }
        Ok(text(""))
    });
    let qual = qualify::run_dogfood_locked(
        &host,
        &repo,
        &Env::default(),
        &GateOpts::default(),
        &Snapshot {
            head: SHA_A.to_string(),
            porcelain: String::new(),
        },
    )
    .unwrap_or_else(|err| panic!("{} {}", err.code, err.detail));
    let rc_started = host
        .log()
        .iter()
        .any(|spec| spec.args.iter().any(|arg| arg.contains("rc_check.py")));
    (qual.exit_code, rc_started)
}

#[test]
fn pre_release_accepts_layer_pass_when_overall_exit_is_partial() {
    let lab_tmp = Tmp::new();
    pass_layers(lab_tmp.path.as_path());
    let body = qualify::judge_pre_release(
        &lab_tmp.path.canonicalize().unwrap(),
        &format!("written: {RUN}/\n"),
        1,
    )
    .unwrap();
    assert!(body.contains("L0: PASS"), "{body}");
    assert!(body.contains("L2: PASS"), "{body}");

    let err = qualify::judge_pre_release(lab_tmp.path.as_path(), &format!("written: {RUN}/\n"), 3)
        .expect_err("not started");
    assert_eq!(err.code, "NOT_STARTED");
    assert_eq!(err.exit, 3);

    std::fs::write(
        lab_tmp.path.join(RUN).join("L0.json"),
        r#"{"status":"FAIL","checks":[{"id":"fmt","status":"FAIL"}]}"#,
    )
    .unwrap();
    let err = qualify::judge_pre_release(lab_tmp.path.as_path(), &format!("written: {RUN}/\n"), 1)
        .expect_err("fmt");
    assert_eq!(err.code, "FMT_CHECK_FAIL");
}

#[test]
fn lab_resolution_prefers_config_and_does_not_fall_through() {
    let parent = Tmp::new();
    let repo = parent.path.join("codeleveler");
    std::fs::create_dir_all(&repo).unwrap();
    write_lab(&parent.path.join("dogfood"), BASELINE);
    std::fs::create_dir_all(repo.join(".dev")).unwrap();
    std::fs::write(
        repo.join(".dev/config"),
        "dogfood_root=/this/lab/does/not/exist\n",
    )
    .unwrap();
    let err = root::resolve_lab(&repo, &GateOpts::default(), &Env::default()).unwrap_err();
    assert_eq!(err.code, "DOGFOOD_ROOT_INVALID");

    std::fs::remove_file(repo.join(".dev/config")).unwrap();
    let lab = root::resolve_lab(&repo, &GateOpts::default(), &Env::default()).unwrap();
    assert_eq!(
        lab.root,
        parent.path.join("dogfood").canonicalize().unwrap()
    );
    assert_eq!(lab.model, version::DEFAULT_MODEL);

    write_lab(&parent.path.join("dogfood-eval"), BASELINE);
    let err = root::resolve_lab(&repo, &GateOpts::default(), &Env::default()).unwrap_err();
    assert_eq!(err.code, "DOGFOOD_ROOT_AMBIGUOUS");
}

#[test]
fn check_maps_a_crate_file_and_a_lockfile() {
    let tmp = Tmp::new();
    let repo = tmp.path.as_path();
    std::fs::create_dir_all(repo.join("crates/demo/src")).unwrap();
    std::fs::write(
        repo.join("crates/demo/Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(repo.join("crates/demo/src/lib.rs"), "fn demo() {}\n").unwrap();
    std::fs::write(repo.join("Cargo.lock"), "# lock\n").unwrap();
    let host = FakeHost::new(|spec| {
        assert_eq!(spec.program, "cargo");
        assert!(
            !spec
                .args
                .iter()
                .any(|arg| arg == "--all-features" || arg == "--locked")
        );
        Ok(text(""))
    });
    let outcome = dispatch_with(
        repo,
        &args(&["check", "crates/demo/src/lib.rs"]),
        &Env::default(),
        &host,
    );
    assert_eq!(outcome.code, 0, "{}", outcome.stderr);
    assert!(outcome.stdout.contains("check: demo"), "{}", outcome.stdout);
    assert!(
        host.log()
            .iter()
            .any(|spec| is_cmd(spec, "cargo", &["check", "-p", "demo"]))
    );

    let host = FakeHost::new(|_| Ok(text("")));
    let outcome = dispatch_with(
        repo,
        &args(&["check", "Cargo.lock"]),
        &Env::default(),
        &host,
    );
    assert_eq!(outcome.code, 0, "{}", outcome.stderr);
    assert!(
        outcome.stdout.contains("Cargo.lock is in scope"),
        "{}",
        outcome.stdout
    );
    assert!(
        host.log()
            .iter()
            .any(|spec| is_cmd(spec, "cargo", &["check", "--workspace"]))
    );
}

#[test]
fn verify_without_owned_files_checks_version_and_skips_cargo() {
    let tmp = Tmp::new();
    let repo = tmp.path.as_path();
    write_versions(repo, "1.0.8");
    let host = FakeHost::new(|spec| {
        if spec.program == "git" {
            return Ok(text(""));
        }
        if spec.program == "cargo" {
            panic!("empty scope must not cargo");
        }
        Ok(text(""))
    });
    let outcome = dispatch_with(repo, &args(&["verify"]), &Env::default(), &host);
    assert_eq!(outcome.code, 0, "{}", outcome.stderr);
    assert!(outcome.stdout.contains("NO_OWNED_CHANGES"));
    assert!(outcome.stdout.contains("version: 1.0.8"));
}

#[test]
fn dirty_worktree_does_not_start_dogfood() {
    let tmp = Tmp::new();
    write_versions(tmp.path.as_path(), "1.0.8");
    let host = FakeHost::new(|spec| {
        if is_cmd(spec, "git", &["rev-parse", "HEAD"]) {
            return Ok(text(SHA_A));
        }
        if is_cmd(spec, "git", &["status", "--porcelain"]) {
            return Ok(text(" M src/lib.rs\n"));
        }
        if spec.program == "python3" {
            panic!("dirty tree must not start the gate");
        }
        Ok(text(""))
    });
    let outcome = dispatch_with(
        tmp.path.as_path(),
        &args(&["dogfood"]),
        &Env::default(),
        &host,
    );
    assert_fail(&outcome, "WORKTREE_DIRTY");
}

#[test]
fn help_lists_the_three_groups() {
    let tmp = Tmp::new();
    let host = FakeHost::new(|_| Ok(text("")));
    let outcome = dispatch_with(tmp.path.as_path(), &[], &Env::default(), &host);
    assert_eq!(outcome.code, 0);
    assert!(outcome.stdout.contains("Development"));
    assert!(outcome.stdout.contains("Maintenance"));
    assert!(outcome.stdout.contains("Release"));
    assert!(outcome.stdout.contains("dogfood"));
    let outcome = dispatch_with(tmp.path.as_path(), &args(&["nope"]), &Env::default(), &host);
    assert_eq!(outcome.code, 2);
    assert_fail(&outcome, "UNKNOWN_COMMAND");
}

#[test]
fn source_has_no_machine_path_or_hardcoded_baseline() {
    let home = ["/", "Use", "rs/"].concat();
    let lab = ["dengmeng", "mian/", "dog", "food"].concat();
    let baseline = ["5b8f31e5c360", "2a384e086462e6687b481dabb5ad"].concat();
    let retired = ["deepseek-v4", "-flash"].concat();
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    for entry in std::fs::read_dir(manifest_dir.join("src")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains(&home), "{}", path.display());
        assert!(!text.contains(&lab), "{}", path.display());
        assert!(!text.contains(&baseline), "{}", path.display());
        assert!(!text.contains(&retired), "{}", path.display());
    }
    let dev = std::fs::read_to_string(manifest_dir.join("../dev")).unwrap();
    assert!(!dev.contains(&home));
    assert!(!dev.contains(&lab));
}

fn publish_host(porcelain: &str, tags: &str, releases: &str) -> FakeHost {
    let porcelain = porcelain.to_string();
    let tags = tags.to_string();
    let releases = releases.to_string();
    FakeHost::new(move |spec| {
        if is_cmd(spec, "git", &["rev-parse", "HEAD"]) {
            return Ok(text(SHA_A));
        }
        if is_cmd(spec, "git", &["status", "--porcelain"]) {
            return Ok(text(&porcelain));
        }
        if is_cmd(spec, "git", &["rev-parse", "--abbrev-ref", "HEAD"]) {
            return Ok(text("main"));
        }
        if is_cmd(spec, "git", &["remote"]) {
            return Ok(text("origin\n"));
        }
        if is_cmd(spec, "git", &["tag", "--list"]) {
            return Ok(text(&tags));
        }
        if spec.program == "gh" {
            return Ok(text(&releases));
        }
        Ok(text(""))
    })
}
