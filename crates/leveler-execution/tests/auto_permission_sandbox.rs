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

// ── The network half of the same contract ───────────────────────────────────
//
// The permission layer decides ALLOW for Auto's ordinary network use and the
// seatbelt profile must carry the capability that decision implies. The failure
// this covers is a double truth source: the policy grants the network and the
// sandbox answers a silent DNS/connect failure, so the user sees
// `Could not resolve host` and blames the network.
//
// The evidence is the same shape as above — the real `CommandRunner` through a
// real `sandbox-exec` against a real loopback server — plus a NEGATIVE control:
// when the permission layer denies the network, the same call must fail. A test
// that only proved the allow half would also pass with the sandbox switched
// off.

use std::io::{Read, Write};
use std::net::TcpListener;

/// A one-shot loopback HTTP server. Returns the bound port; it serves every
/// request with `200` and the body `ok` until the listener is dropped.
fn loopback_http_server() -> (u16, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let port = listener.local_addr().expect("addr").port();
    let handle = std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
            let _ = stream.flush();
        }
    });
    (port, handle)
}

/// One request through the real runner. `deny` is the permission layer's
/// verdict; the sandbox must agree with it.
async fn network_probe(
    fixture: &Fixture,
    url: &str,
    deny: bool,
    unrestricted: bool,
) -> ProcessOutput {
    let mut request = ProcessRequest::new(
        "curl",
        [
            "-sS",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "--max-time",
            "10",
            "--noproxy",
            "*",
            url,
        ]
        .iter()
        .map(|a| a.to_string())
        .collect(),
        fixture.workspace.clone(),
    );
    request.timeout = Duration::from_secs(30);
    request.write_scope = if unrestricted {
        WriteScope::Unrestricted
    } else {
        WriteScope::Workspace {
            root: fixture.workspace.clone(),
        }
    };
    request.unrestricted_execution = unrestricted;
    request.network_scope = if deny {
        leveler_execution::NetworkScope::None
    } else {
        leveler_execution::NetworkScope::Internet
    };
    request.deny_network = deny;
    fixture
        .runner
        .run(request, CancellationToken::new())
        .await
        .unwrap_or_else(|error| panic!("curl failed to run: {error:?}"))
}

/// P2/P3. Auto's ALLOW reaches a real TCP destination through the sandbox, by
/// literal address and by name — and the same call is genuinely denied when the
/// permission layer denies it, so neither half is a no-op.
#[tokio::test]
async fn auto_network_allow_and_deny_agree_with_the_sandbox() {
    let fixture = Fixture::new();
    let (port, _server) = loopback_http_server();

    for url in [
        format!("http://127.0.0.1:{port}/"),
        format!("http://localhost:{port}/"),
    ] {
        // ALLOW: the sandbox must let the connection through.
        let allowed = network_probe(&fixture, &url, false, false).await;
        assert!(
            allowed.success() && allowed.stdout.trim() == "200",
            "ALLOW must reach the granted destination {url}: {}",
            detail(&allowed)
        );

        // DENY: the same request must fail in the sandbox, not "succeed".
        let denied = network_probe(&fixture, &url, true, false).await;
        assert!(
            !denied.success(),
            "a permission DENY must not be silently allowed by the sandbox {url}: {}",
            detail(&denied)
        );
        assert!(
            !denied.stdout.contains("200"),
            "a denied request must not report a status code {url}: {}",
            detail(&denied)
        );

        // Full parity: no CodeLeveler sandbox is involved.
        let full = network_probe(&fixture, &url, false, true).await;
        assert!(
            full.success() && full.stdout.trim() == "200",
            "Full must reach {url} with no sandbox in the way: {}",
            detail(&full)
        );
    }
}

/// The external hop, when the machine has one. It is opt-in because a build
/// farm has no egress; when the host cannot reach the destination, the run
/// reports why instead of pretending a sandbox PASS.
///
/// Enable with `LEVELER_TEST_EXTERNAL_NETWORK=1`. This is the real DNS + TLS
/// path (no HTTP proxy), which the loopback case above cannot cover.
#[tokio::test]
async fn auto_external_https_when_the_host_has_egress() {
    if std::env::var("LEVELER_TEST_EXTERNAL_NETWORK").as_deref() != Ok("1") {
        eprintln!("skipping: set LEVELER_TEST_EXTERNAL_NETWORK=1 to probe real egress");
        return;
    }
    let fixture = Fixture::new();
    let url = "https://example.com/";

    // Host control: if the bare host cannot resolve/connect, the sandbox is not
    // the variable under test and the result would be UNPROVEN, not a pass.
    let bare = network_probe(&fixture, url, false, true).await;
    if !bare.success() || bare.stdout.trim() != "200" {
        eprintln!(
            "skipping: this host has no direct egress to {url}: {}",
            detail(&bare)
        );
        return;
    }

    let auto = network_probe(&fixture, url, false, false).await;
    assert!(
        auto.success() && auto.stdout.trim() == "200",
        "Auto ALLOW must perform real DNS + TLS, not degrade to a sandbox DNS failure: {}",
        detail(&auto)
    );
}
