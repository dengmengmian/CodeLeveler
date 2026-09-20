//! Test-owned OS processes: spawn them in their own process group and reclaim
//! the whole tree when the owning scope goes away.
//!
//! libtest has no teardown hook, so a test that starts a daemon, a shell, a
//! helper or a browser expresses ownership as an RAII guard. [`TestProcessScope`]
//! tracks every child it spawned and kills each child's process **group** when
//! the scope is dropped — on success, assertion failure, panic, or when the
//! test's deadline fires and the body future is dropped.
//!
//! Killing the group, not just the direct child, is the point: `leveler serve`
//! spawns a helper, a background task spawns a shell, a shell spawns a
//! descendant. Killing only the direct child leaves the descendants holding the
//! pipes and living on after the test binary is gone.
//!
//! The tree-kill mirrors the product's `leveler-execution::ProcessIdentity`
//! (Unix process group; Windows `taskkill /T /F`). The product crate owns the
//! product-side equivalent; this is the test-owned mirror, kept here so a
//! fixture crate does not have to link the whole execution engine to clean up.

use std::ffi::OsStr;
use std::io;
use std::process::{Child, Command, ExitStatus};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
/// How long a guarded child gets to exit after `SIGTERM` before `SIGKILL`.
pub const CHILD_TERM_GRACE: Duration = Duration::from_millis(500);

/// A live child a test scope still owns, for timeout diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildRecord {
    pub scope: String,
    pub program: String,
    pub pid: u32,
}

fn registry() -> &'static Mutex<Vec<ChildRecord>> {
    static REGISTRY: OnceLock<Mutex<Vec<ChildRecord>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Vec::new()))
}

/// Every live test-owned child in this process.
///
/// Presented in a timeout report so a hang names the processes it still owned.
/// Tests inside one binary run in parallel threads, so this is the process's
/// set, not necessarily the timing-out test's alone — it is diagnostic, not
/// ownership.
pub fn live_children() -> Vec<ChildRecord> {
    registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

fn register(record: ChildRecord) {
    registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(record);
}

fn unregister(pid: u32) {
    let mut live = registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    live.retain(|record| record.pid != pid);
}

/// A child process in its own process group, owned by a [`TestProcessScope`].
///
/// Dropping this guard kills the whole owned tree. It is normally held by the
/// scope rather than by the test body, so the guard's lifetime is the scope's.
pub struct ManagedChild {
    child: Option<Child>,
    pid: u32,
    program: String,
}

impl std::fmt::Debug for ManagedChild {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagedChild")
            .field("pid", &self.pid)
            .field("program", &self.program)
            .finish_non_exhaustive()
    }
}

impl ManagedChild {
    /// Spawn `command` in its own process group and take ownership.
    ///
    /// For tests that want a single owned child rather than a scope. The guard
    /// kills the whole tree on drop, so a panic or a fired deadline still
    /// reclaims it.
    pub fn spawn(command: &mut Command) -> io::Result<Self> {
        spawn_managed(command, "")
    }

    pub fn id(&self) -> u32 {
        self.pid
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn program(&self) -> &str {
        &self.program
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        match &mut self.child {
            Some(child) => child.try_wait(),
            None => Ok(None),
        }
    }

    /// Signal the direct child without waiting (Windows: the whole job).
    pub fn kill(&mut self) -> io::Result<()> {
        match &mut self.child {
            Some(child) => child.kill(),
            None => Ok(()),
        }
    }

    /// Wait for the direct child to exit.
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        match &mut self.child {
            Some(child) => child.wait(),
            None => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "the child was already reaped",
            )),
        }
    }

    /// Wait up to `timeout` for the direct child to exit. Returns `true` if it
    /// is gone.
    pub fn wait_for_exit(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            match self.try_wait() {
                Ok(Some(_)) => return true,
                Ok(None) => {}
                Err(_) => return true,
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Terminate the direct child and every process in its group.
    ///
    /// Unix: `SIGTERM` the group, brief grace, then `SIGKILL`. Windows:
    /// `taskkill /T /F`. The group is signalled before the direct child is
    /// reaped, so its pid cannot have been recycled into another process group
    /// first. Always reaps the direct child so no zombie survives.
    pub fn kill_tree(&mut self) {
        if self.child.is_none() {
            return;
        }
        terminate_group(self.pid);
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.child = None;
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        self.kill_tree();
        unregister(self.pid);
    }
}

/// A test's collection of owned children.
///
/// One scope per test (or per harness). `spawn` puts the child in its own
/// process group and records it; drop kills every recorded tree. Explicit
/// [`Self::cleanup`] lets a test reclaim children before a long assertion tail,
/// but nothing depends on it — the Drop path is what covers panic and timeout.
#[derive(Debug)]
pub struct TestProcessScope {
    name: String,
    children: Vec<ManagedChild>,
}

impl TestProcessScope {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            children: Vec::new(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Spawn `command` in a fresh process group and take ownership.
    ///
    /// The command's own stdio/env are used as configured. Returns the child's
    /// pid; get the guard back with [`Self::child_mut`].
    pub fn spawn(&mut self, command: &mut Command) -> io::Result<u32> {
        let child = spawn_managed(command, &self.name)?;
        let pid = child.pid();
        self.children.push(child);
        Ok(pid)
    }

    /// Spawn a program with arguments, inheriting stdio as `Stdio::inherit`.
    pub fn spawn_program(&mut self, program: impl AsRef<OsStr>, args: &[&str]) -> io::Result<u32> {
        let mut command = Command::new(program);
        command.args(args);
        self.spawn(&mut command)
    }

    pub fn child_mut(&mut self, pid: u32) -> Option<&mut ManagedChild> {
        self.children.iter_mut().find(|child| child.pid == pid)
    }

    pub fn children(&self) -> &[ManagedChild] {
        &self.children
    }

    /// Kill every owned tree now. Drop calls this too; calling it early is
    /// only an optimization.
    pub fn cleanup(&mut self) {
        for child in &mut self.children {
            child.kill_tree();
        }
        self.children.clear();
    }
}

impl Drop for TestProcessScope {
    fn drop(&mut self) {
        self.cleanup();
    }
}

#[cfg(unix)]
fn configure_new_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(windows)]
fn configure_new_group(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP);
}

fn spawn_managed(command: &mut Command, scope: &str) -> io::Result<ManagedChild> {
    configure_new_group(command);
    let child = command.spawn()?;
    let pid = child.id();
    let program = command.get_program().to_string_lossy().into_owned();
    register(ChildRecord {
        scope: scope.to_string(),
        program: program.clone(),
        pid,
    });
    Ok(ManagedChild {
        child: Some(child),
        pid,
        program,
    })
}

#[cfg(unix)]
fn terminate_group(pid: u32) {
    use nix::sys::signal::{Signal, killpg};
    use nix::unistd::Pid;
    let group = Pid::from_raw(pid as i32);
    // The child was spawned with `process_group(0)`, so its pid is the group.
    let _ = killpg(group, Signal::SIGTERM);
    let deadline = Instant::now() + CHILD_TERM_GRACE;
    while Instant::now() < deadline {
        // A signal-0 probe: ESRCH means the whole group is gone.
        if killpg(group, None) == Err(nix::errno::Errno::ESRCH) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = killpg(group, Signal::SIGKILL);
}

#[cfg(windows)]
fn terminate_group(pid: u32) {
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sleep_program() -> (&'static str, Vec<String>) {
        if cfg!(windows) {
            (
                "powershell",
                vec![
                    "-NoProfile".to_string(),
                    "-NonInteractive".to_string(),
                    "-Command".to_string(),
                    "Start-Sleep -Seconds 300".to_string(),
                ],
            )
        } else {
            ("sleep", vec!["300".to_string()])
        }
    }

    fn alive(pid: u32) -> bool {
        #[cfg(unix)]
        {
            use nix::sys::signal::kill;
            use nix::unistd::Pid;
            kill(Pid::from_raw(pid as i32), None).is_ok()
        }
        #[cfg(windows)]
        {
            let out = Command::new("tasklist")
                .args(["/FI", &format!("PID eq {pid}"), "/NH"])
                .output();
            out.map(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
                .unwrap_or(false)
        }
    }

    #[test]
    fn a_spawned_child_is_reclaimed_when_the_scope_drops() {
        let (program, args) = sleep_program();
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let pid = {
            let mut scope = TestProcessScope::new("unit-drop");
            scope.spawn_program(program, &args).unwrap()
        };
        assert!(!alive(pid), "dropping the scope must terminate the child");
        assert!(live_children().iter().all(|record| record.pid != pid));
    }

    #[test]
    fn explicit_cleanup_leaves_the_scope_reusable() {
        let (program, args) = sleep_program();
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let mut scope = TestProcessScope::new("unit-explicit");
        let pid = scope.spawn_program(program, &args).unwrap();
        assert!(alive(pid));
        scope.cleanup();
        assert!(!alive(pid));
        assert!(scope.children().is_empty());
    }

    #[test]
    fn two_scopes_do_not_reclaim_each_others_children() {
        let (program, args) = sleep_program();
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let mut a = TestProcessScope::new("unit-a");
        let mut b = TestProcessScope::new("unit-b");
        let pa = a.spawn_program(program, &args).unwrap();
        let pb = b.spawn_program(program, &args).unwrap();
        a.cleanup();
        assert!(!alive(pa));
        assert!(alive(pb), "scope A must not touch scope B's child");
        b.cleanup();
    }
}
