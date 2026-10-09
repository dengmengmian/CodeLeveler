//! Process-identity witnesses and explicit process-control primitives retained
//! for diagnostics and their platform contracts.
//!
//! A PID/start/socket witness proves which process is addressed. It does not
//! prove that the runtime has closed admission or has no owned work. Runtime
//! reconciliation therefore never uses these signal primitives to replace an
//! endpoint that reports `RetireDecision::Unsupported`: the old owner must exit
//! normally before the new generation starts.
//!
//! Signals address a single verified PID, never a process group. Windows lacks
//! a verified process-identity witness and reports platform unsupported.

use std::path::Path;

use leveler_client_protocol::BuildIdentity;

/// Why a forced migration did not happen.
///
/// Every variant is a REFUSAL, never a partial action: the caller must be able
/// to state that nothing was signalled.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MigrationRefusal {
    /// The endpoint did not report a usable identity (no build, no pid, or the
    /// pid is this process). Nothing is known well enough to terminate.
    #[error("the previous runtime did not report a usable identity: {0}")]
    IdentityUnknown(String),
    /// The identity was usable but the process could not be witnessed, so a
    /// signal could land on a reused PID.
    #[error("the previous runtime's process could not be verified: {0}")]
    ProcessUnverifiable(String),
    /// Reaching this point requires a process-identity witness and a
    /// single-process signal, and this platform has not been verified to
    /// provide them.
    #[error("forced migration is not supported on this platform: {0}")]
    PlatformUnsupported(String),
    /// The process behind the captured PID is no longer the one that was
    /// captured — the PID was reused. Nothing further is signalled.
    #[error("pid {pid} was reused by a different process during migration")]
    TargetReused { pid: u32 },
    /// The runtime serving the socket is not the one this migration was
    /// planned against — it was replaced, or the socket was taken over.
    #[error("the runtime serving {socket} changed during migration")]
    TargetChanged { socket: String },
    /// The process was signalled and still serves after the grace period, with
    /// an identity that still matches. The migration stops here rather than
    /// escalating forever.
    #[error("the previous runtime (pid {pid}) did not exit within {waited_secs}s")]
    DidNotExit { pid: u32, waited_secs: u64 },
}

/// A PID plus the start time that makes it a *specific* process.
///
/// A bare PID is a snapshot that can be reused; `(pid, start)` is an identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessWitness {
    pub pid: u32,
    /// An opaque, comparable start marker. Equal markers for the same PID mean
    /// the same process; a different marker means the PID was reused.
    pub started: String,
}

/// What the socket path itself is, so "the same endpoint" is checkable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SocketObject {
    pub device: u64,
    pub inode: u64,
}

/// Everything this client can PROVE about the runtime it intends to replace.
#[derive(Debug, Clone)]
pub struct LegacyRuntimeTarget {
    pub pid: u32,
    pub runtime_id: String,
    pub build: BuildIdentity,
    pub socket: SocketObject,
    pub witness: ProcessWitness,
    pub ownership: OwnershipEvidence,
}

/// The socket path's file object identity, or `None` when it is not a socket.
pub fn socket_object(path: &Path) -> Option<SocketObject> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        let metadata = std::fs::symlink_metadata(path).ok()?;
        if !metadata.file_type().is_socket() {
            return None;
        }
        Some(SocketObject {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// The identity of the process behind `pid`, or `None` when it is gone — or
/// when this platform has no witness to offer.
///
/// Deliberately free of `unsafe`: the start marker is read from `/proc` where
/// the kernel publishes it, and from `ps` otherwise. A platform where neither
/// works reports `None`, which makes migration refuse rather than guess.
pub fn process_witness(pid: u32) -> Option<ProcessWitness> {
    if pid == 0 {
        return None;
    }
    #[cfg(target_os = "linux")]
    {
        // Field 22 of `/proc/<pid>/stat` is the start time in clock ticks since
        // boot. `comm` (field 2) may contain spaces and parentheses, so parsing
        // resumes after the LAST ')'.
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let rest = stat.rsplit_once(')')?.1;
        let start = rest.split_whitespace().nth(19)?;
        start.parse::<u64>().ok()?;
        Some(ProcessWitness {
            pid,
            started: start.to_string(),
        })
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        let output = std::process::Command::new("ps")
            .env("LC_ALL", "C")
            .arg("-o")
            .arg("lstart=")
            .arg("-p")
            .arg(pid.to_string())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let started = String::from_utf8(output.stdout).ok()?.trim().to_string();
        if started.is_empty() {
            return None;
        }
        Some(ProcessWitness { pid, started })
    }
    #[cfg(windows)]
    {
        let _ = pid;
        None
    }
}

/// The user that owns `pid`, as the kernel reports it.
pub fn process_uid(pid: u32) -> Option<u32> {
    #[cfg(target_os = "linux")]
    {
        // `/proc/<pid>/status` carries `Uid:\t<real>\t<effective>\t...`; the
        // real uid is the first field.
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        let line = status.lines().find(|line| line.starts_with("Uid:"))?;
        line.split_whitespace().nth(1)?.parse().ok()
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        let output = std::process::Command::new("ps")
            .env("LC_ALL", "C")
            .arg("-o")
            .arg("uid=")
            .arg("-p")
            .arg(pid.to_string())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        String::from_utf8(output.stdout).ok()?.trim().parse().ok()
    }
    #[cfg(windows)]
    {
        let _ = pid;
        None
    }
}

/// This process's real user id.
fn current_uid() -> Option<u32> {
    #[cfg(unix)]
    {
        Some(nix::unistd::Uid::current().as_raw())
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// How strongly the *endpoint* is tied to the process this migration is about
/// to signal, and that the process is a runtime for THIS repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnershipEvidence {
    /// The process holds the very socket object this endpoint is, proven from
    /// the kernel's own file-descriptor table.
    HoldsEndpointSocket,
    /// The process could not be tied to the socket object directly, but its
    /// command line identifies it as a `serve` runtime for this very workspace
    /// (or a workspace-free runtime, for an endpoint that has no workspace).
    /// This is the same identification the project's own tooling uses to find a
    /// repository's daemons.
    ServesThisWorkspace,
}

/// The full command line of `pid`, so a pid can be identified as a runtime for
/// a particular workspace.
///
/// `-ww` matters: without it `ps` truncates, and a truncated command line would
/// read as "not a runtime for this workspace", turning every migration into a
/// refusal.
pub fn process_command(pid: u32) -> Option<String> {
    #[cfg(unix)]
    {
        let output = std::process::Command::new("ps")
            .env("LC_ALL", "C")
            .arg("-ww")
            .arg("-o")
            .arg("command=")
            .arg("-p")
            .arg(pid.to_string())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let command = String::from_utf8(output.stdout).ok()?.trim().to_string();
        (!command.is_empty()).then_some(command)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        None
    }
}

/// Whether a command line is a runtime serving `workspace`.
///
/// Two facts, both required: the process runs the `serve` subcommand, and it was
/// pointed at this repository (or explicitly at no workspace, which is the one
/// legitimate case for an endpoint with no repository). Splitting on whitespace
/// deliberately: a substring test would accept `not-serve` and a path that
/// merely *contains* the workspace.
pub fn serves_workspace(command: &str, workspace: Option<&Path>) -> bool {
    let tokens: Vec<&str> = command.split_whitespace().collect();
    if !tokens.contains(&"serve") {
        return false;
    }
    match workspace {
        Some(workspace) => {
            // Compare RESOLVED paths: a layout may hold the canonical form while
            // the process was launched with the symlinked one (macOS `/var` vs
            // `/private/var`), and a textual comparison would then refuse a
            // runtime that is in fact serving this very repository.
            let wanted =
                std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
            tokens.iter().any(|token| {
                let token = Path::new(token.trim_matches('"'));
                token == wanted
                    || std::fs::canonicalize(token).is_ok_and(|resolved| resolved == wanted)
            })
        }
        None => tokens.contains(&"--no-workspace"),
    }
}

/// Prove the process holds the endpoint socket (Linux) or, where the kernel
/// publishes no file-descriptor table, that its command line is a runtime for
/// this workspace.
///
/// The second is weaker than the first and is reported as such: it ties the pid
/// to *a* runtime for this repository, not to the exact socket object.
/// It exists because the alternative is worse — a runtime that names an
/// unrelated live pid would otherwise be obeyed on every platform without
/// `/proc`.
pub fn ownership_evidence(
    pid: u32,
    socket: SocketObject,
    workspace: Option<&Path>,
) -> Option<OwnershipEvidence> {
    if holds_endpoint_socket(pid, socket) {
        return Some(OwnershipEvidence::HoldsEndpointSocket);
    }
    let command = process_command(pid)?;
    serves_workspace(&command, workspace).then_some(OwnershipEvidence::ServesThisWorkspace)
}

/// Whether the process's own file-descriptor table contains the endpoint
/// socket object, where the kernel publishes one.
///
/// Linux: scanning `/proc/<pid>/fd` for `socket:[<inode>]` matching the socket
/// path decides it outright. Other platforms have no equivalent without an
/// unverified platform shim, so they answer `false` and the caller falls back
/// to the command-line evidence — honestly weaker, never pretend-strong.
pub fn holds_endpoint_socket(pid: u32, socket: SocketObject) -> bool {
    #[cfg(target_os = "linux")]
    {
        let wanted = format!("socket:[{}]", socket.inode);
        let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
            return false;
        };
        fds.flatten().any(|fd| {
            std::fs::read_link(fd.path()).is_ok_and(|link| link.to_string_lossy() == wanted)
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (pid, socket);
        false
    }
}

/// Whether `pid` still exists.
///
/// `kill(pid, 0)` performs the existence and permission check without delivering
/// anything. EPERM still proves the process exists (it is owned by another
/// user); only ESRCH means it is gone.
pub fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        use nix::errno::Errno;
        use nix::sys::signal::kill;
        use nix::unistd::Pid;
        match kill(Pid::from_raw(pid as i32), None) {
            Ok(()) => true,
            Err(Errno::EPERM) => true,
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

/// The signals this migration may send. Named so the two-step escalation is a
/// value a test can assert on rather than a raw integer at the call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminationSignal {
    /// The graceful request. A runtime that can exit cleanly should.
    Term,
    /// The last resort, sent only after `Term` was ignored.
    Kill,
}

/// Send one signal to exactly one process — never to a group.
pub fn signal_process(pid: u32, signal: TerminationSignal) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use nix::sys::signal::{Signal, kill};
        use nix::unistd::Pid;
        let signal = match signal {
            TerminationSignal::Term => Signal::SIGTERM,
            TerminationSignal::Kill => Signal::SIGKILL,
        };
        kill(Pid::from_raw(pid as i32), Some(signal)).map_err(std::io::Error::from)
    }
    #[cfg(not(unix))]
    {
        let _ = (pid, signal);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "no single-process signalling on this platform",
        ))
    }
}

/// Whether this platform can prove process identity and signal one process
/// well enough to force a migration at all.
pub fn platform_supports_forced_migration() -> bool {
    process_witness(std::process::id()).is_some()
}

/// Classify the observed runtime, and refuse when it is not a target this
/// migration may act on.
///
/// Split out from the signal loop because this is where all the policy lives,
/// and it must be testable without terminating anything.
pub fn verify_target(
    socket_path: &Path,
    reported_pid: u32,
    reported_runtime_id: &str,
    reported_build: &BuildIdentity,
    expected_build: &BuildIdentity,
    workspace: Option<&Path>,
) -> Result<LegacyRuntimeTarget, MigrationRefusal> {
    if reported_pid == 0 || reported_runtime_id.is_empty() || !reported_build.is_known() {
        return Err(MigrationRefusal::IdentityUnknown(format!(
            "pid={reported_pid}, runtime_id={reported_runtime_id:?}, build_known={}",
            reported_build.is_known()
        )));
    }
    if reported_pid == std::process::id() {
        return Err(MigrationRefusal::IdentityUnknown(
            "the runtime reported this client's own pid".to_string(),
        ));
    }
    if !expected_build.is_known() {
        return Err(MigrationRefusal::IdentityUnknown(
            "this client cannot state which build it expected".to_string(),
        ));
    }
    if expected_build == reported_build {
        return Err(MigrationRefusal::IdentityUnknown(
            "the runtime already reports the current build".to_string(),
        ));
    }
    // Platform FIRST: on a platform with no verified process witness there is
    // nothing further to examine, and `PlatformUnsupported` says exactly that
    // instead of blaming the socket.
    if !platform_supports_forced_migration() {
        return Err(MigrationRefusal::PlatformUnsupported(
            std::env::consts::OS.to_string(),
        ));
    }
    let socket = socket_object(socket_path).ok_or_else(|| {
        MigrationRefusal::ProcessUnverifiable(format!(
            "{} is not a socket owned by a runtime",
            socket_path.display()
        ))
    })?;
    let witness = process_witness(reported_pid).ok_or_else(|| {
        MigrationRefusal::ProcessUnverifiable(format!(
            "no process identity is available for pid {reported_pid}"
        ))
    })?;
    if !process_alive(reported_pid) {
        return Err(MigrationRefusal::ProcessUnverifiable(format!(
            "pid {reported_pid} is already gone"
        )));
    }
    // Ownership: only a runtime owned by this user may be terminated. Without
    // this a broken runtime that names, say, pid 1 would be obeyed.
    match (process_uid(reported_pid), current_uid()) {
        (Some(owner), Some(current)) if owner == current => {}
        (Some(owner), Some(current)) => {
            return Err(MigrationRefusal::ProcessUnverifiable(format!(
                "pid {reported_pid} is owned by uid {owner}, not this user (uid {current})"
            )));
        }
        _ => {
            return Err(MigrationRefusal::ProcessUnverifiable(format!(
                "the owner of pid {reported_pid} could not be read"
            )));
        }
    }
    // Ownership: the pid must be tied to THIS endpoint. The strong proof is the
    // kernel's own file-descriptor table; where that does not exist, the process
    // must at least identify itself as a `serve` runtime for this very
    // workspace. Without one of the two, a runtime that names an unrelated live
    // pid would be obeyed, which is precisely the process this check exists to
    // refuse.
    let ownership = ownership_evidence(reported_pid, socket, workspace).ok_or_else(|| {
        MigrationRefusal::ProcessUnverifiable(format!(
            "pid {reported_pid} is not tied to {}: it neither holds the endpoint socket \
             nor identifies itself as a runtime for this workspace",
            socket_path.display()
        ))
    })?;
    Ok(LegacyRuntimeTarget {
        pid: reported_pid,
        runtime_id: reported_runtime_id.to_string(),
        build: reported_build.clone(),
        socket,
        witness,
        ownership,
    })
}

/// Re-verify a captured target immediately before signalling it.
///
/// Two independent facts, both required: the PID is still the same *process*
/// (start marker unchanged) and it is still alive. Either failing means the
/// signal would not reach the runtime this migration examined.
pub fn revalidate(target: &LegacyRuntimeTarget) -> Result<(), MigrationRefusal> {
    match process_witness(target.pid) {
        Some(witness) if witness == target.witness => {
            if process_alive(target.pid) {
                Ok(())
            } else {
                Err(MigrationRefusal::ProcessUnverifiable(format!(
                    "pid {} disappeared between the identity read and the signal",
                    target.pid
                )))
            }
        }
        Some(_) => Err(MigrationRefusal::TargetReused { pid: target.pid }),
        None => Err(MigrationRefusal::ProcessUnverifiable(format!(
            "pid {} is gone",
            target.pid
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(tag: &str) -> BuildIdentity {
        BuildIdentity {
            version: "1.0.12".into(),
            revision: "rev".into(),
            dirty: false,
            fingerprint: tag.into(),
        }
    }

    /// A bound Unix socket at a chosen path.
    ///
    /// Unix-only, and so is every test that needs one: on a platform without a
    /// verified process witness there is no termination target to examine —
    /// [`verify_target`] refuses before it looks at the socket at all.
    #[cfg(unix)]
    fn socket_at(
        dir: &tempfile::TempDir,
    ) -> (std::path::PathBuf, std::os::unix::net::UnixListener) {
        let socket = dir.path().join("x.sock");
        // Kept alive by the caller: the path stays a socket object for the
        // whole test, and the listener is closed when the test ends.
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        (socket, listener)
    }

    #[test]
    #[cfg(unix)]
    fn a_runtime_that_reports_the_current_build_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (socket, _listener) = socket_at(&dir);
        let current = build("same");
        let refusal = verify_target(&socket, 42, "rt-1", &current, &current, None).unwrap_err();
        assert!(matches!(refusal, MigrationRefusal::IdentityUnknown(_)));
    }

    #[test]
    #[cfg(unix)]
    fn a_non_socket_path_is_never_a_termination_target() {
        let dir = tempfile::tempdir().unwrap();
        let not_a_socket = dir.path().join("plain.txt");
        std::fs::write(&not_a_socket, b"hello").unwrap();
        let refusal = verify_target(
            &not_a_socket,
            42,
            "rt-1",
            &build("old"),
            &build("new"),
            None,
        )
        .unwrap_err();
        assert!(
            matches!(refusal, MigrationRefusal::ProcessUnverifiable(_)),
            "{refusal:?}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn an_unreported_identity_is_never_a_termination_target() {
        let dir = tempfile::tempdir().unwrap();
        let (socket, _listener) = socket_at(&dir);
        let candidates = [
            (0u32, "rt-1", build("old")),
            (42, "", build("old")),
            (42, "rt-1", BuildIdentity::default()),
        ];
        for (pid, runtime_id, reported) in candidates {
            let refusal = verify_target(&socket, pid, runtime_id, &reported, &build("new"), None)
                .unwrap_err();
            assert!(
                matches!(refusal, MigrationRefusal::IdentityUnknown(_)),
                "{refusal:?}"
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn this_client_is_never_its_own_termination_target() {
        let dir = tempfile::tempdir().unwrap();
        let (socket, _listener) = socket_at(&dir);
        let refusal = verify_target(
            &socket,
            std::process::id(),
            "rt-1",
            &build("old"),
            &build("new"),
            None,
        )
        .unwrap_err();
        assert!(matches!(refusal, MigrationRefusal::IdentityUnknown(_)));
    }

    #[test]
    #[cfg(unix)]
    fn a_dead_pid_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (socket, _listener) = socket_at(&dir);
        // A pid this large is not allocated on any supported platform.
        let pid = 0x7fff_fffe;
        if process_alive(pid) {
            return;
        }
        let refusal =
            verify_target(&socket, pid, "rt-1", &build("old"), &build("new"), None).unwrap_err();
        assert!(
            matches!(refusal, MigrationRefusal::ProcessUnverifiable(_)),
            "{refusal:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn this_process_is_witnessable_and_stable() {
        let pid = std::process::id();
        let witness = process_witness(pid).expect("own process must be witnessable");
        assert_eq!(witness.pid, pid);
        assert!(
            !witness.started.is_empty(),
            "a witness without a start marker proves nothing"
        );
        assert_eq!(process_witness(pid).unwrap(), witness);
        assert!(process_alive(pid));
    }

    #[cfg(unix)]
    #[test]
    fn a_reused_pid_is_detected_before_it_is_signalled() {
        // A witness for a DIFFERENT identity must not validate: this is the
        // check that stops a signal from landing on an unrelated process.
        let pid = std::process::id();
        let mut target = LegacyRuntimeTarget {
            pid,
            runtime_id: "rt-1".into(),
            build: build("old"),
            socket: SocketObject {
                device: 1,
                inode: 1,
            },
            witness: process_witness(pid).unwrap(),
            ownership: OwnershipEvidence::ServesThisWorkspace,
        };
        assert!(revalidate(&target).is_ok());
        target.witness.started.push_str("-stale");
        assert!(
            matches!(
                revalidate(&target),
                Err(MigrationRefusal::TargetReused { .. })
            ),
            "a changed start marker must read as a reused pid"
        );
    }

    #[test]
    fn a_command_line_identifies_a_runtime_for_this_workspace() {
        let workspace = std::path::Path::new("/tmp/example-repo");
        assert!(serves_workspace(
            "leveler --repo /tmp/example-repo serve",
            Some(workspace)
        ));
        assert!(serves_workspace(
            "/usr/local/bin/leveler serve --no-workspace",
            None
        ));
        // The refusals this rule exists for: a process that is not a serve
        // runtime, and one serving a DIFFERENT repository.
        for (command, expected) in [
            ("sleep 600", false),
            ("/bin/sh -c 'leveler --repo /tmp/other serve'", false),
            ("leveler --repo /tmp/example-repository serve", false),
            ("leveler --repo /tmp/example-repo", false),
        ] {
            assert_eq!(
                serves_workspace(command, Some(workspace)),
                expected,
                "{command:?}"
            );
        }
        assert!(!serves_workspace("sleep 600", None));
    }

    #[cfg(unix)]
    #[test]
    fn a_socket_path_is_identified_by_its_file_object() {
        let dir = tempfile::tempdir().unwrap();
        let (socket, _listener) = socket_at(&dir);
        let first = socket_object(&socket).expect("a bound socket has an object identity");
        let second = socket_object(&socket).unwrap();
        assert_eq!(first, second);
        let plain = dir.path().join("plain");
        std::fs::write(&plain, b"").unwrap();
        assert!(socket_object(&plain).is_none());
    }
}
