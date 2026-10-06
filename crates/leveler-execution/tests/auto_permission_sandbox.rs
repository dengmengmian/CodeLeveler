//! The sandbox half of the Auto Permission Contract.
//!
//! `auto_permission_decision_table.rs` proves what the permission layer
//! decides. This file proves the thing the contract actually promises: after
//! the policy says ALLOW, no OS sandbox of CodeLeveler's silently answers
//! `Operation not permitted`.
//!
//! Every case here runs the real `CommandRunner` — the same one the
//! `run_command` / `shell_command` tools use — through a real `sandbox-exec`
//! with the production profile. Nothing is mocked and no profile string is
//! inspected; the observation is the child's exit status and output.

#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use leveler_execution::{CommandRunner, ProcessOutput, ProcessRequest, WriteScope};
use tokio_util::sync::CancellationToken;

/// A runner whose environment is the host's, with `LEVELER_HOME` pointed
/// outside the (temporary) workspace so the private sandbox scratch can be
/// created. This is the production shape: the home is never inside the
/// writable workspace.
fn runner(home: &Path) -> CommandRunner {
    let mut values: Vec<(std::ffi::OsString, std::ffi::OsString)> = std::env::vars_os().collect();
    values.push((
        std::ffi::OsString::from("LEVELER_HOME"),
        home.as_os_str().to_os_string(),
    ));
    CommandRunner::with_environment(Arc::new(leveler_core::EnvSnapshot::new(
        values,
        std::env::current_dir().unwrap_or_default(),
        std::env::temp_dir(),
    )))
}

struct Fixture {
    _workspace: tempfile::TempDir,
    _home: tempfile::TempDir,
    workspace: PathBuf,
    runner: CommandRunner,
}

impl Fixture {
    fn new() -> Self {
        let workspace = tempfile::tempdir().expect("workspace");
        let home = tempfile::tempdir().expect("home");
        let root = workspace
            .path()
            .canonicalize()
            .expect("canonical workspace");
        let runner = runner(home.path());
        Self {
            _workspace: workspace,
            _home: home,
            workspace: root,
            runner,
        }
    }

    /// Auto: the write boundary is the workspace.
    async fn auto(&self, program: &str, args: &[&str]) -> ProcessOutput {
        self.run(
            program,
            args,
            WriteScope::Workspace {
                root: self.workspace.clone(),
            },
            false,
        )
        .await
    }

    /// Full: no CodeLeveler sandbox at all.
    async fn full(&self, program: &str, args: &[&str]) -> ProcessOutput {
        self.run(program, args, WriteScope::Unrestricted, true)
            .await
    }

    async fn run(
        &self,
        program: &str,
        args: &[&str],
        write_scope: WriteScope,
        unrestricted: bool,
    ) -> ProcessOutput {
        let mut request = ProcessRequest::new(
            program,
            args.iter().map(|a| a.to_string()).collect(),
            self.workspace.clone(),
        );
        request.timeout = Duration::from_secs(60);
        request.write_scope = write_scope;
        request.unrestricted_execution = unrestricted;
        self.runner
            .run(request, CancellationToken::new())
            .await
            .unwrap_or_else(|error| panic!("{program} {args:?} failed to run: {error:?}"))
    }
}

fn detail(out: &ProcessOutput) -> String {
    format!(
        "exit={:?} stdout={:?} stderr={:?}",
        out.exit_code, out.stdout, out.stderr
    )
}

/// P1. Ordinary process and system inspection must not be refused by
/// CodeLeveler's own sandbox.
#[tokio::test]
async fn auto_runs_process_and_system_inspection() {
    let fixture = Fixture::new();

    let ps = fixture.auto("ps", &["ax", "-o", "pid,comm"]).await;
    assert!(
        ps.success() && ps.stdout.contains("PID"),
        "ps must observe the process table: {}",
        detail(&ps)
    );

    let ps_aux = fixture.auto("ps", &["aux"]).await;
    assert!(
        ps_aux.success() && ps_aux.stdout.lines().count() > 1,
        "ps aux must list processes: {}",
        detail(&ps_aux)
    );

    let top = fixture.auto("top", &["-l", "1", "-n", "1"]).await;
    assert!(
        top.success() && top.stdout.contains("Processes:"),
        "top must sample once: {}",
        detail(&top)
    );

    let sysctl = fixture.auto("sysctl", &["-n", "kern.argmax"]).await;
    assert!(
        sysctl.success(),
        "sysctl -n kern.argmax must work: {}",
        detail(&sysctl)
    );
    assert!(
        sysctl.stdout.trim().parse::<u64>().is_ok_and(|v| v > 0),
        "kern.argmax must be a positive integer: {}",
        detail(&sysctl)
    );

    // A control that already worked, so the fix above is not "the sandbox got
    // switched off": lsof reads the process table through a different API.
    let lsof = fixture
        .auto("lsof", &["-nP", "-iTCP", "-sTCP:LISTEN"])
        .await;
    assert!(lsof.success(), "lsof must keep working: {}", detail(&lsof));
}

/// P3. Ordinary temporary files must be writable, exactly where a normal
/// process writes them: the shared `/tmp`, the redirected `$TMPDIR`, and the
/// workspace.
#[tokio::test]
async fn auto_writes_temporary_files() {
    let fixture = Fixture::new();
    let pid = std::process::id();

    for (label, directory) in [
        ("shared /tmp", "/tmp".to_string()),
        ("$TMPDIR", "$TMPDIR".to_string()),
        ("workspace", ".".to_string()),
    ] {
        let target = format!("{directory}/codeleveler-auto-test-{pid}.txt");
        let script = format!(
            "set -e; echo first > {target}; echo second >> {target}; \
             test \"$(cat {target})\" = \"first\nsecond\"; rm -f {target}; \
             test ! -e {target}"
        );
        let out = fixture.auto("sh", &["-c", &script]).await;
        assert!(
            out.success(),
            "create/write/read/append/remove in {label} must work: {}",
            detail(&out)
        );
    }
}

/// P6, sandbox scoped: the setuid exec grant is exactly two binaries. Every
/// other setuid image still fails closed rather than gaining the escape.
#[tokio::test]
async fn the_unsandboxed_exec_grant_does_not_leak() {
    let fixture = Fixture::new();
    if !Path::new("/usr/bin/atq").exists() {
        eprintln!("skipping: /usr/bin/atq is not installed");
        return;
    }
    let atq = fixture.auto("/usr/bin/atq", &[]).await;
    assert!(
        !atq.success(),
        "a setuid binary outside the process-inspection grant must stay sandboxed: {}",
        detail(&atq)
    );
}

/// Full parity plus the write boundary itself: Full must run the whole matrix
/// with no CodeLeveler sandbox, proven by writing to a host path that Auto
/// deliberately refuses.
#[tokio::test]
async fn full_access_has_no_codeleveler_sandbox() {
    let fixture = Fixture::new();
    let outside = tempfile::tempdir().expect("outside");
    let outside_path = outside.path().canonicalize().expect("canonical");

    // Auto genuinely fences this path: the same write must fail.
    let fenced = format!("echo x > {}", outside_path.join("fenced.txt").display());
    let auto = fixture.auto("sh", &["-c", &fenced]).await;
    assert!(
        !auto.success(),
        "Auto must keep the write fence around a host path outside its roots: {}",
        detail(&auto)
    );

    for (program, args) in [
        ("ps", vec!["ax", "-o", "pid,comm"]),
        ("top", vec!["-l", "1", "-n", "1"]),
        ("sysctl", vec!["-n", "kern.argmax"]),
    ] {
        let out = fixture.full(program, &args).await;
        assert!(
            out.success() && !out.stderr.contains("Operation not permitted"),
            "Full must run {program} with no sandbox: {}",
            detail(&out)
        );
    }

    let script = format!(
        "set -e; echo hi > {outside}/full.txt; cat {outside}/full.txt; rm -f {outside}/full.txt",
        outside = outside_path.display()
    );
    let out = fixture.full("sh", &["-c", &script]).await;
    assert!(
        out.success(),
        "Full must write outside the workspace: {}",
        detail(&out)
    );
}
