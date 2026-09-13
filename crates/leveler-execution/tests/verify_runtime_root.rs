//! The verify sandbox gives a confined child its own CodeLeveler runtime root.
//!
//! A project's verification command may be CodeLeveler itself: a self-dogfood
//! run verifies this repository with this repository's own suite, which drives
//! the edit tools in-process and takes an advisory lock under its home. The
//! sandbox's writable roots are the workspace, the per-command scratch and the
//! tool caches, so a child that resolved the user's real home could not write
//! it — the target failed with
//! `lock ~/.leveler/run/locks/<hash>.lock: Operation not permitted` and the run
//! stayed `unavailable` (FOUNDATION_ACCEPTANCE_AND_FREEZE.md §6).
//!
//! These tests prove the confined child instead gets an isolated root inside
//! the scratch: that it is writable, that two runs never share one, that the
//! user's home gains nothing, and that the repository's own representative
//! target passes under the real verify request.
//!
//! Integration tests (not a lib test module) because the lib forbids unsafe and
//! this file needs none: the isolated root is injected through the runner's own
//! [`EnvSnapshot`], never by mutating the test process's environment, so these
//! tests stay parallel-safe.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use leveler_core::EnvSnapshot;
use leveler_execution::{
    CommandRunner, ProcessRequest, VerifyNetworkPolicy, process_request_for_verify_check,
};
use tokio_util::sync::CancellationToken;

/// A script that fails loudly unless it was handed a writable runtime root,
/// then writes the same kind of state the edit tools write (a lock file under
/// `<home>/run/locks`) and prints the root it used.
const WRITE_RUNTIME_STATE: &str = r#"
set -e
test -n "$LEVELER_HOME" || { echo "LEVELER_HOME unset" >&2; exit 9; }
mkdir -p "$LEVELER_HOME/run/locks"
: > "$LEVELER_HOME/run/locks/probe.lock"
printf '%s\n' "$LEVELER_HOME"
"#;

/// A runner whose environment resolves `LEVELER_HOME` to `home` — the parent's
/// (the daemon's) own runtime root, which is what the sandbox keys its scratch
/// on and what the child must NOT be handed.
fn runner_with_home(home: &Path, workspace: &Path) -> CommandRunner {
    let mut vars: Vec<(std::ffi::OsString, std::ffi::OsString)> = std::env::vars_os().collect();
    vars.retain(|(name, _)| name != "LEVELER_HOME");
    vars.push(("LEVELER_HOME".into(), home.as_os_str().to_os_string()));
    CommandRunner::with_environment(Arc::new(EnvSnapshot::new(
        vars,
        workspace.to_path_buf(),
        std::env::temp_dir(),
    )))
}

/// A verify request over `workspace` — the product's own request builder, so
/// what the test runs under is what a verification check runs under.
fn verify_request(script: &str, workspace: &Path) -> ProcessRequest {
    process_request_for_verify_check(
        "sh",
        vec!["-c".into(), script.to_string()],
        workspace.to_path_buf(),
        VerifyNetworkPolicy::InheritSession,
    )
}

struct Fixture {
    _base: tempfile::TempDir,
    home: PathBuf,
    workspace: PathBuf,
}

fn fixture() -> Fixture {
    let base = tempfile::Builder::new()
        .prefix("leveler-verify-root-")
        .tempdir()
        .expect("temp dir");
    let home = base.path().join("home");
    let workspace = base.path().join("workspace");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(&workspace).expect("workspace");
    // The sandbox canonicalizes the home it keys its scratch on, and on macOS
    // the temp tree is reached through a symlink (`/var` -> `/private/var`), so
    // an unresolved path would never be a prefix of the child's answer.
    let home = home.canonicalize().expect("canonical home");
    let workspace = workspace.canonicalize().expect("canonical workspace");
    Fixture {
        _base: base,
        home,
        workspace,
    }
}

/// Test 1 — the child gets an isolated, writable runtime root.
///
/// The script exits 9 when `LEVELER_HOME` is unset (the pre-repair world, where
/// the child resolved the user's real home) and exits non-zero when the write
/// is refused (the pre-repair failure mode), so a green here means the child
/// both received an isolated root and could write it.
#[tokio::test]
async fn a_confined_child_writes_its_runtime_state_under_its_own_root() {
    let fx = fixture();
    let runner = runner_with_home(&fx.home, &fx.workspace);
    let out = runner
        .run(
            verify_request(WRITE_RUNTIME_STATE, &fx.workspace),
            CancellationToken::new(),
        )
        .await
        .expect("run confined child");
    assert!(
        out.success(),
        "confined child could not write its runtime root (exit {:?})\nstdout: {}\nstderr: {}",
        out.exit_code,
        out.stdout,
        out.stderr
    );

    let child_home = PathBuf::from(out.stdout.trim());
    assert_ne!(
        child_home, fx.home,
        "the child must not be handed the parent's runtime root"
    );
    assert!(
        child_home.starts_with(&fx.home),
        "the isolated root lives inside the sandbox scratch under the parent home, got {}",
        child_home.display()
    );
    assert!(
        child_home.to_string_lossy().contains("leveler-home"),
        "isolated root should be the scratch's leveler-home, got {}",
        child_home.display()
    );
}

/// Test 1 (the other half) — the user's real runtime state is not touched.
///
/// The child wrote a lock file; it must be inside the scratch, so the parent's
/// home still has no `run/locks`. This is the "ISOLATE, not ALLOW REAL USER
/// STATE" half of the contract.
#[tokio::test]
async fn a_confined_child_leaves_the_parent_runtime_state_alone() {
    let fx = fixture();
    let runner = runner_with_home(&fx.home, &fx.workspace);
    let out = runner
        .run(
            verify_request(WRITE_RUNTIME_STATE, &fx.workspace),
            CancellationToken::new(),
        )
        .await
        .expect("run confined child");
    assert!(out.success(), "child failed: {}", out.stderr);

    assert!(
        !fx.home.join("run").join("locks").exists(),
        "the parent's own locks directory gained a file: the child was not isolated"
    );
    assert!(
        !fx.home.join("state").exists(),
        "the parent's own state root was written by a confined child"
    );
}

/// Test 2 — two runs get two independent roots.
///
/// A verification run must not observe, or contend for, another run's runtime
/// state: the scratch is per command, and the child's root is keyed on it.
#[tokio::test]
async fn two_confined_runs_never_share_a_runtime_root() {
    let fx = fixture();
    let runner = runner_with_home(&fx.home, &fx.workspace);
    let mut roots = Vec::new();
    for _ in 0..2 {
        let out = runner
            .run(
                verify_request(WRITE_RUNTIME_STATE, &fx.workspace),
                CancellationToken::new(),
            )
            .await
            .expect("run confined child");
        assert!(out.success(), "child failed: {}", out.stderr);
        roots.push(PathBuf::from(out.stdout.trim()));
    }
    assert_ne!(
        roots[0], roots[1],
        "two verification runs shared one runtime root"
    );
    // Neither run's scratch survives the command, so nothing accumulates.
    for root in &roots {
        assert!(
            !root.exists(),
            "the per-command runtime root outlived its command: {}",
            root.display()
        );
    }
}

/// The repository's own built test binary for a target, found beside this
/// test's own binary: `cargo test` builds every target into the same `deps/`
/// directory, so the representative target is already sitting next to us.
///
/// Running it directly rather than through `cargo` is deliberate. A second
/// `cargo` inside the sandbox would share this workspace's `target/` while
/// running under a private `CARGO_HOME`, so its units carry different metadata
/// hashes and its pruning would leave the outer build's fingerprints and
/// artifacts disagreeing — a self-inflicted source of flakiness, in CI too.
/// The binary itself needs no cargo and starts in milliseconds.
fn built_test_binary(stem: &str) -> Option<PathBuf> {
    let deps = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let prefix = format!("{stem}-");
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(&deps).ok()? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // `<stem>-<hash>` (plus `.exe` on Windows), never the `.d` sidecar.
        if !name.starts_with(&prefix) || name.ends_with(".d") {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(meta) if meta.is_file() => meta,
            _ => continue,
        };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        if best.as_ref().is_none_or(|(seen, _)| modified > *seen) {
            best = Some((modified, entry.path()));
        }
    }
    best.map(|(_, path)| path)
}

/// Test 3 — the representative self-repo target passes under verify confinement.
///
/// `leveler-tools --test edit_contract` is the target the Foundation record
/// reproduced the failure with: two of its tests wrote the real
/// `~/.leveler/run/locks` in-process and were refused by the write fence. The
/// control half (same binary, no confinement) keeps the claim honest — a
/// failure here must mean confinement, not a broken target.
#[tokio::test]
async fn the_repositorys_own_edit_contract_suite_passes_under_verify_confinement() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf();
    let Some(binary) = built_test_binary("edit_contract") else {
        // The target is built by `cargo test --workspace`; a run that filtered
        // it out cannot judge this, and quietly passing would claim otherwise.
        eprintln!("skipping: edit_contract test binary not built in this profile");
        return;
    };
    let program = binary.to_string_lossy().into_owned();

    // Control: the same binary, unconfined. A failure here is not about the
    // sandbox, so it must be reported as such rather than read as a pass.
    let control = CommandRunner::with_environment(Arc::new(EnvSnapshot::new(
        std::env::vars_os(),
        repo.clone(),
        std::env::temp_dir(),
    )));
    let control_out = control
        .run(
            ProcessRequest::new(program.clone(), Vec::new(), repo.clone()),
            CancellationToken::new(),
        )
        .await
        .expect("run control");
    assert!(
        control_out.success(),
        "control (unconfined) run failed, so this test cannot attribute anything \
         to confinement\nstdout: {}\nstderr: {}",
        control_out.stdout,
        control_out.stderr
    );
    assert!(
        control_out.stdout.contains("test result: ok"),
        "control run did not report a passing suite:\n{}",
        control_out.stdout
    );

    // Confined the way a verification check is confined, with the network
    // denied the way a model-supplied acceptance command gets it.
    let fx = fixture();
    let runner = runner_with_home(&fx.home, &fx.workspace);
    let mut request =
        process_request_for_verify_check(program, Vec::new(), repo, VerifyNetworkPolicy::ForceDeny);
    request.timeout = std::time::Duration::from_secs(300);
    let out = runner
        .run(request, CancellationToken::new())
        .await
        .expect("run confined");
    assert!(
        out.success(),
        "the repository's own edit_contract suite failed under verify confinement\n\
         stdout: {}\nstderr: {}",
        out.stdout,
        out.stderr
    );
    let combined = format!("{}{}", out.stdout, out.stderr);
    assert!(
        !combined.contains("Operation not permitted"),
        "confinement refused a write the check needed:\n{combined}"
    );
    // The parent's runtime state stayed out of it.
    assert!(
        !fx.home.join("run").join("locks").exists(),
        "the confined suite wrote the parent's locks directory"
    );
}
