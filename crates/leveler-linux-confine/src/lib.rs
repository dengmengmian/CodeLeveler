//! Linux write confinement primitives.
//!
//! `bwrap` confines writes by mounting everything read-only and re-binding the
//! authorized roots read-write. A protected path that does **not exist** has no
//! dentry to bind, so a confined shell could create it and persist policy text
//! (`AGENTS.md`, `.leveler/agents`, `.leveler/skills`, `.git`) that a later
//! session loads as authority. Linux has no path-regex deny: Landlock is an
//! allow-list, and a mount namespace cannot name a missing dentry.
//!
//! This crate is the audited `unsafe` boundary for that gap. The sandbox
//! inherits a `seccomp` user-notification filter; a supervisor thread resolves
//! each reported creation syscall, denies protected destinations, and performs
//! allowed ones itself (injecting descriptors with
//! `SECCOMP_IOCTL_NOTIF_ADDFD`). The destination parent is pinned with an
//! `O_PATH` descriptor and the final component is opened `O_NOFOLLOW`, so the
//! check and the operation cannot be separated by a symlink swap.
//!
//! `leveler-execution` stays `deny(unsafe_code)` and supplies the policy —
//! which paths are protected, which roots are writable, and the sandbox
//! launcher's identity — through [`FencePolicy`]. If the kernel, process
//! inspection, or descriptor injection is unavailable, installation or
//! activation fails and the caller refuses the command instead of running it
//! unfenced.
//!
//! Non-Linux builds compile to an empty crate: the workspace still builds and
//! tests on every host, and no caller can reach the API there.

#![cfg(target_os = "linux")]

use std::ffi::{CString, OsStr, OsString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// `SECCOMP_RET_USER_NOTIF` is missing from the pinned libc.
const SECCOMP_RET_USER_NOTIF: u32 = 0x7fc0_0000;

const SECCOMP_SET_MODE_FILTER: libc::c_ulong = 1;

const SECCOMP_IOCTL_NOTIF_RECV: libc::c_ulong = ioc(3, 0x50, b'!', 0);
const SECCOMP_IOCTL_NOTIF_SEND: libc::c_ulong = ioc(3, 0x18, b'!', 1);
const SECCOMP_IOCTL_NOTIF_ADDFD: libc::c_ulong = ioc(1, 0x18, b'!', 3);

const SECCOMP_ADDFD_FLAG_SEND: u32 = 1 << 1;

/// Path arguments are copied out of the child once, bounded by `PATH_MAX`.
const CHILD_PATH_LIMIT: usize = 4096;

/// Upper bound on symlink expansion while resolving a final component.
const SYMLINK_LIMIT: usize = 40;

const fn ioc(dir: u32, size: u32, kind: u8, nr: u8) -> libc::c_ulong {
    ((dir as libc::c_ulong) << 30)
        | ((size as libc::c_ulong) << 16)
        | ((kind as libc::c_ulong) << 8)
        | (nr as libc::c_ulong)
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SeccompData {
    nr: i32,
    arch: u32,
    instruction_pointer: u64,
    args: [u64; 6],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SeccompNotif {
    id: u64,
    pid: u32,
    flags: u32,
    data: SeccompData,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SeccompNotifResp {
    id: u64,
    val: i64,
    error: i32,
    flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SeccompNotifAddfd {
    id: u64,
    flags: u32,
    srcfd: u32,
    newfd: u32,
    newfd_flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RuleKind {
    /// `openat`-shaped: dirfd, path, flags, mode.
    OpenAt {
        dirfd: u8,
        path: u8,
        flags: u8,
        mode: u8,
    },
    /// `open(path, flags, mode)` / `creat(path, mode)`. x86_64 only; aarch64
    /// has neither syscall.
    #[cfg_attr(target_arch = "aarch64", allow(dead_code))]
    OpenLegacy {
        path: u8,
        flags: Option<u8>,
        mode: u8,
        force_create: bool,
    },
    /// `openat2`: dirfd, path, pointer-to-`open_how`.
    OpenAt2 { dirfd: u8, path: u8, how: u8 },
    /// `mkdirat`-shaped: dirfd, path, mode.
    MkdirAt { dirfd: u8, path: u8, mode: u8 },
    /// `symlinkat`-shaped: target, dirfd, path.
    SymlinkAt { target: u8, dirfd: u8, path: u8 },
    /// `linkat`-shaped: olddirfd, oldpath, newdirfd, newpath, flags.
    LinkAt {
        old_dirfd: u8,
        old_path: u8,
        new_dirfd: u8,
        new_path: u8,
        flags: u8,
    },
    /// `renameat`-shaped: olddirfd, oldpath, newdirfd, newpath[, flags].
    RenameAt {
        old_dirfd: u8,
        old_path: u8,
        new_dirfd: u8,
        new_path: u8,
        flags: Option<u8>,
    },
    /// `mknodat`-shaped: dirfd, path, mode, dev.
    MknodAt {
        dirfd: u8,
        path: u8,
        mode: u8,
        dev: u8,
    },
}

#[derive(Clone, Copy)]
struct CreationRule {
    nr: i64,
    kind: RuleKind,
}

#[derive(Clone, Copy)]
struct ExecRule {
    nr: i64,
}

#[cfg(target_arch = "x86_64")]
fn creation_rules() -> Vec<CreationRule> {
    vec![
        CreationRule {
            nr: 2,
            kind: RuleKind::OpenLegacy {
                path: 0,
                flags: Some(1),
                mode: 2,
                force_create: false,
            },
        },
        CreationRule {
            nr: 85,
            kind: RuleKind::OpenLegacy {
                path: 0,
                flags: None,
                mode: 1,
                force_create: true,
            },
        },
        CreationRule {
            nr: 257,
            kind: RuleKind::OpenAt {
                dirfd: 0,
                path: 1,
                flags: 2,
                mode: 3,
            },
        },
        CreationRule {
            nr: 437,
            kind: RuleKind::OpenAt2 {
                dirfd: 0,
                path: 1,
                how: 2,
            },
        },
        CreationRule {
            nr: 83,
            kind: RuleKind::MkdirAt {
                dirfd: u8::MAX,
                path: 0,
                mode: 1,
            },
        },
        CreationRule {
            nr: 258,
            kind: RuleKind::MkdirAt {
                dirfd: 0,
                path: 1,
                mode: 2,
            },
        },
        CreationRule {
            nr: 133,
            kind: RuleKind::MknodAt {
                dirfd: u8::MAX,
                path: 0,
                mode: 1,
                dev: 2,
            },
        },
        CreationRule {
            nr: 259,
            kind: RuleKind::MknodAt {
                dirfd: 0,
                path: 1,
                mode: 2,
                dev: 3,
            },
        },
        CreationRule {
            nr: 88,
            kind: RuleKind::SymlinkAt {
                target: 0,
                dirfd: u8::MAX,
                path: 1,
            },
        },
        CreationRule {
            nr: 266,
            kind: RuleKind::SymlinkAt {
                target: 0,
                dirfd: 1,
                path: 2,
            },
        },
        CreationRule {
            nr: 86,
            kind: RuleKind::LinkAt {
                old_dirfd: u8::MAX,
                old_path: 0,
                new_dirfd: u8::MAX,
                new_path: 1,
                flags: 2,
            },
        },
        CreationRule {
            nr: 265,
            kind: RuleKind::LinkAt {
                old_dirfd: 0,
                old_path: 1,
                new_dirfd: 2,
                new_path: 3,
                flags: 4,
            },
        },
        CreationRule {
            nr: 82,
            kind: RuleKind::RenameAt {
                old_dirfd: u8::MAX,
                old_path: 0,
                new_dirfd: u8::MAX,
                new_path: 1,
                flags: None,
            },
        },
        CreationRule {
            nr: 264,
            kind: RuleKind::RenameAt {
                old_dirfd: 0,
                old_path: 1,
                new_dirfd: 2,
                new_path: 3,
                flags: None,
            },
        },
        CreationRule {
            nr: 316,
            kind: RuleKind::RenameAt {
                old_dirfd: 0,
                old_path: 1,
                new_dirfd: 2,
                new_path: 3,
                flags: Some(4),
            },
        },
    ]
}

#[cfg(target_arch = "aarch64")]
fn creation_rules() -> Vec<CreationRule> {
    vec![
        CreationRule {
            nr: 56,
            kind: RuleKind::OpenAt {
                dirfd: 0,
                path: 1,
                flags: 2,
                mode: 3,
            },
        },
        CreationRule {
            nr: 437,
            kind: RuleKind::OpenAt2 {
                dirfd: 0,
                path: 1,
                how: 2,
            },
        },
        CreationRule {
            nr: 34,
            kind: RuleKind::MkdirAt {
                dirfd: 0,
                path: 1,
                mode: 2,
            },
        },
        CreationRule {
            nr: 33,
            kind: RuleKind::MknodAt {
                dirfd: 0,
                path: 1,
                mode: 2,
                dev: 3,
            },
        },
        CreationRule {
            nr: 36,
            kind: RuleKind::SymlinkAt {
                target: 0,
                dirfd: 1,
                path: 2,
            },
        },
        CreationRule {
            nr: 37,
            kind: RuleKind::LinkAt {
                old_dirfd: 0,
                old_path: 1,
                new_dirfd: 2,
                new_path: 3,
                flags: 4,
            },
        },
        CreationRule {
            nr: 38,
            kind: RuleKind::RenameAt {
                old_dirfd: 0,
                old_path: 1,
                new_dirfd: 2,
                new_path: 3,
                flags: None,
            },
        },
        CreationRule {
            nr: 276,
            kind: RuleKind::RenameAt {
                old_dirfd: 0,
                old_path: 1,
                new_dirfd: 2,
                new_path: 3,
                flags: Some(4),
            },
        },
    ]
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn creation_rules() -> Vec<CreationRule> {
    Vec::new()
}

#[cfg(target_arch = "x86_64")]
fn exec_rules() -> Vec<ExecRule> {
    vec![ExecRule { nr: 59 }, ExecRule { nr: 322 }]
}

#[cfg(target_arch = "aarch64")]
fn exec_rules() -> Vec<ExecRule> {
    vec![ExecRule { nr: 221 }, ExecRule { nr: 281 }]
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn exec_rules() -> Vec<ExecRule> {
    Vec::new()
}

fn arg_offset(index: u8) -> u32 {
    (std::mem::size_of::<SeccompData>() - std::mem::size_of::<[u64; 6]>() + index as usize * 8)
        as u32
}

/// The BPF program the child installs before `exec`.
struct FilterProgram {
    instructions: Vec<libc::sock_filter>,
}

impl FilterProgram {
    fn build(creations: &[CreationRule], execs: &[ExecRule]) -> Self {
        // Layout: arch block, creation blocks, exec blocks, io_uring pairs,
        // final `RET ALLOW`. Rule blocks jump to the next block; the last one
        // jumps into the io_uring pairs, which fall through to ALLOW.
        let mut blocks: Vec<Vec<libc::sock_filter>> = Vec::new();
        for rule in creations {
            blocks.push(match rule.kind {
                RuleKind::OpenAt { flags, .. } => vec![
                    jeq(rule.nr, 0, 4),
                    load(arg_offset(flags)),
                    jset(libc::O_CREAT as u32, 0, 2),
                    jset(libc::O_TMPFILE as u32, 1, 0),
                    ret(SECCOMP_RET_USER_NOTIF),
                ],
                RuleKind::OpenLegacy { force_create, .. } if force_create => {
                    vec![jeq(rule.nr, 0, 1), ret(SECCOMP_RET_USER_NOTIF)]
                }
                _ => vec![jeq(rule.nr, 0, 1), ret(SECCOMP_RET_USER_NOTIF)],
            });
        }
        for rule in execs {
            blocks.push(vec![jeq(rule.nr, 0, 1), ret(SECCOMP_RET_USER_NOTIF)]);
        }
        let io_start: usize = 4 + blocks.iter().map(Vec::len).sum::<usize>();
        let mut instructions = vec![
            load(4),
            jeq_arch(1, 0),
            ret(libc::SECCOMP_RET_KILL_PROCESS),
            load(0),
        ];
        for block in &blocks {
            instructions.extend(block.iter().copied());
        }
        for nr in [425_i64, 426, 427] {
            instructions.push(jeq(nr, 0, 1));
            instructions.push(ret(libc::SECCOMP_RET_ERRNO | libc::EPERM as u32));
        }
        instructions.push(ret(libc::SECCOMP_RET_ALLOW));
        debug_assert_eq!(instructions.len(), io_start + 7);
        Self { instructions }
    }
}

fn load(offset: u32) -> libc::sock_filter {
    libc::sock_filter {
        code: (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16,
        jt: 0,
        jf: 0,
        k: offset,
    }
}

fn jeq(nr: i64, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter {
        code: (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
        jt,
        jf,
        k: nr as u32,
    }
}

fn jeq_arch(jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter {
        code: (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
        jt,
        jf,
        k: native_arch(),
    }
}

fn jset(mask: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter {
        code: (libc::BPF_JMP | libc::BPF_JSET | libc::BPF_K) as u16,
        jt,
        jf,
        k: mask,
    }
}

fn ret(value: u32) -> libc::sock_filter {
    libc::sock_filter {
        code: (libc::BPF_RET | libc::BPF_K) as u16,
        jt: 0,
        jf: 0,
        k: value,
    }
}

#[cfg(target_arch = "x86_64")]
const fn native_arch() -> u32 {
    // AUDIT_ARCH_X86_64; the pinned libc does not export it.
    0xC000_003E
}

#[cfg(target_arch = "aarch64")]
const fn native_arch() -> u32 {
    // AUDIT_ARCH_AARCH64; the pinned libc does not export it.
    0xC000_00B7
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
const fn native_arch() -> u32 {
    0
}

/// What the execution layer asks this crate to protect.
pub struct FencePolicy {
    /// Absolute paths (or path prefixes) the sandbox must not create or
    /// modify. A path here is protected together with everything below it.
    pub protected: Vec<PathBuf>,
    /// Canonical roots the command may write to. A creation whose parent is
    /// outside all of them is left to the sandbox's read-only bind.
    pub writable_roots: Vec<PathBuf>,
    /// Device and inode of the sandbox launcher (`bwrap`). Its own setup runs
    /// in a private mount namespace and is skipped until it execs the sandboxed
    /// program.
    pub launcher: (u64, u64),
}

struct ChildHandoff {
    socket: RawFd,
    program: Arc<FilterProgram>,
    len: u16,
}

unsafe impl Send for ChildHandoff {}
unsafe impl Sync for ChildHandoff {}

/// Installed before `exec`; hands the listener descriptor to the supervisor.
///
/// The supervisor thread starts here, before `spawn`. The child's `execve` is
/// itself a reported syscall, and `spawn` does not return until that `execve`
/// completes — so serving notifications only after `spawn` would deadlock.
pub struct FenceLauncher {
    /// `None` after a successful activation: the running fence has taken over
    /// ownership, and dropping the launcher must not stop it.
    control: Option<Arc<SupervisorControl>>,
    ready: Option<std::sync::mpsc::Receiver<Result<(), String>>>,
}

impl FenceLauncher {
    /// Wait until the child's listener is installed and the supervisor is
    /// serving. On error the caller must refuse the command.
    pub fn activate(mut self) -> io::Result<Fence> {
        let ready = self
            .ready
            .take()
            .ok_or_else(|| io::Error::other("fence already activated"))?;
        match ready.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok(Ok(())) => {
                let control = self
                    .control
                    .take()
                    .ok_or_else(|| io::Error::other("fence control already released"))?;
                Ok(Fence { control })
            }
            Ok(Err(reason)) => Err(io::Error::other(format!(
                "the protected-path supervisor could not start: {reason}"
            ))),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(io::Error::other(
                "the protected-path supervisor did not receive a listener",
            )),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(io::Error::other(
                "the protected-path supervisor stopped before activation",
            )),
        }
    }
}

impl Drop for FenceLauncher {
    fn drop(&mut self) {
        if let Some(control) = self.control.take() {
            control.shutdown();
        }
    }
}

pub struct Fence {
    control: Arc<SupervisorControl>,
}

impl Fence {
    pub fn shutdown(&self) {
        self.control.shutdown();
    }
}

impl Drop for Fence {
    fn drop(&mut self) {
        self.control.shutdown();
    }
}

struct SupervisorControl {
    shutdown: AtomicBool,
    wake_read: OwnedFd,
    wake_write: OwnedFd,
}

impl SupervisorControl {
    fn new() -> io::Result<Self> {
        let mut fds = [0i32; 2];
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            shutdown: AtomicBool::new(false),
            // SAFETY: pipe2 returned two fresh descriptors this value owns.
            wake_read: unsafe { OwnedFd::from_raw_fd(fds[0]) },
            wake_write: unsafe { OwnedFd::from_raw_fd(fds[1]) },
        })
    }

    fn shutdown(&self) {
        if self.shutdown.swap(true, Ordering::SeqCst) {
            return;
        }
        let byte = 1u8;
        // A full pipe already carries a wake-up.
        unsafe {
            libc::write(
                self.wake_write.as_raw_fd(),
                &byte as *const u8 as *const libc::c_void,
                1,
            );
        }
    }
}

/// Build the child-side filter and arrange for its listener to reach the
/// caller. Until [`FenceLauncher::activate`] runs, the child is blocked in the
/// kernel on its first reported syscall.
pub fn install(cmd: &mut std::process::Command, policy: FencePolicy) -> io::Result<FenceLauncher> {
    use std::os::unix::process::CommandExt;

    let program = Arc::new(FilterProgram::build(&creation_rules(), &exec_rules()));
    let len = u16::try_from(program.instructions.len())
        .map_err(|_| io::Error::other("seccomp filter is too long"))?;
    let mut fds = [0i32; 2];
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let parent = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let child = Arc::new(ChildHandoff {
        socket: fds[1],
        program,
        len,
    });
    let handoff = Arc::clone(&child);
    // The closure runs after fork and must not allocate: it only issues
    // prctl/seccomp/sendmsg/close.
    unsafe {
        cmd.pre_exec(move || child_install(&handoff));
    }
    let control = Arc::new(SupervisorControl::new()?);
    let (ready, ready_rx) = std::sync::mpsc::sync_channel(1);
    let policy = Arc::new(policy);
    let thread_control = Arc::clone(&control);
    std::thread::Builder::new()
        .name("leveler-linux-fence".to_string())
        .spawn(move || supervisor_entry(parent, policy, thread_control, ready))?;
    Ok(FenceLauncher {
        control: Some(control),
        ready: Some(ready_rx),
    })
}

fn child_install(state: &ChildHandoff) -> io::Result<()> {
    let rc = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    let program = libc::sock_fprog {
        len: state.len,
        filter: state.program.instructions.as_ptr() as *mut libc::sock_filter,
    };
    let listener = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            SECCOMP_SET_MODE_FILTER,
            libc::SECCOMP_FILTER_FLAG_NEW_LISTENER,
            &program as *const libc::sock_fprog,
        )
    };
    if listener < 0 {
        return Err(io::Error::last_os_error());
    }
    let listener = listener as RawFd;
    let mut byte = 0u8;
    let mut iov = libc::iovec {
        iov_base: &mut byte as *mut u8 as *mut libc::c_void,
        iov_len: 1,
    };
    // 8-byte aligned control buffer for one SCM_RIGHTS descriptor.
    let mut control = [0u64; 8];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr() as *mut libc::c_void;
    message.msg_controllen =
        unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let sent = unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        if header.is_null() {
            libc::close(listener);
            return Err(io::Error::other("no control buffer for the fence listener"));
        }
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as usize;
        std::ptr::copy_nonoverlapping(
            &listener as *const RawFd as *const u8,
            libc::CMSG_DATA(header),
            std::mem::size_of::<RawFd>(),
        );
        libc::sendmsg(state.socket, &message, 0)
    };
    unsafe {
        libc::close(listener);
    }
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn recv_listener(socket: &OwnedFd) -> io::Result<OwnedFd> {
    let mut byte = 0u8;
    let mut iov = libc::iovec {
        iov_base: &mut byte as *mut u8 as *mut libc::c_void,
        iov_len: 1,
    };
    let mut control = [0u64; 8];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr() as *mut libc::c_void;
    message.msg_controllen = control.len() * std::mem::size_of::<u64>();
    if unsafe { libc::recvmsg(socket.as_raw_fd(), &mut message, 0) } <= 0 {
        return Err(io::Error::last_os_error());
    }
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        if header.is_null()
            || (*header).cmsg_level != libc::SOL_SOCKET
            || (*header).cmsg_type != libc::SCM_RIGHTS
        {
            return Err(io::Error::other(
                "the protected-path listener was not received",
            ));
        }
        let mut fd: RawFd = -1;
        std::ptr::copy_nonoverlapping(
            libc::CMSG_DATA(header),
            &mut fd as *mut RawFd as *mut u8,
            std::mem::size_of::<RawFd>(),
        );
        if fd < 0 {
            return Err(io::Error::other("the protected-path listener is invalid"));
        }
        Ok(OwnedFd::from_raw_fd(fd))
    }
}

fn supervisor_entry(
    socket: OwnedFd,
    policy: Arc<FencePolicy>,
    control: Arc<SupervisorControl>,
    ready: std::sync::mpsc::SyncSender<Result<(), String>>,
) {
    let mut fds = [
        libc::pollfd {
            fd: socket.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: control.wake_read.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    let polled = unsafe { libc::poll(fds.as_mut_ptr(), 2, 10_000) };
    if polled < 0 {
        let _ = ready.send(Err(io::Error::last_os_error().to_string()));
        return;
    }
    if polled == 0 {
        let _ = ready.send(Err("timed out waiting for the listener".to_string()));
        return;
    }
    if fds[1].revents & libc::POLLIN != 0 {
        let _ = ready.send(Err("shut down during startup".to_string()));
        return;
    }
    let listener = match recv_listener(&socket) {
        Ok(listener) => listener,
        Err(error) => {
            let _ = ready.send(Err(error.to_string()));
            return;
        }
    };
    let _ = ready.send(Ok(()));
    supervise(listener, policy, control);
}

fn supervise(listener: OwnedFd, policy: Arc<FencePolicy>, control: Arc<SupervisorControl>) {
    install_view(&policy);
    let listener_fd = listener.as_raw_fd();
    let wake_fd = control.wake_read.as_raw_fd();
    let mut notif = vec![0u8; std::mem::size_of::<SeccompNotif>()];
    let mut resp = vec![0u8; std::mem::size_of::<SeccompNotifResp>()];
    let target_started = AtomicBool::new(false);
    loop {
        if control.shutdown.load(Ordering::SeqCst) {
            return;
        }
        let mut fds = [
            libc::pollfd {
                fd: listener_fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake_fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, 250) };
        if ready < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        if fds[1].revents & libc::POLLIN != 0 {
            return;
        }
        if fds[0].revents == 0 {
            continue;
        }
        loop {
            if control.shutdown.load(Ordering::SeqCst) {
                return;
            }
            // The kernel requires a zeroed request structure on every call;
            // a stale `flags` word from the previous notification is EINVAL.
            notif.fill(0);
            let request = notif.as_mut_ptr() as *mut SeccompNotif;
            if unsafe { libc::ioctl(listener_fd, SECCOMP_IOCTL_NOTIF_RECV, request) } != 0 {
                let error = io::Error::last_os_error();
                if matches!(
                    error.raw_os_error(),
                    Some(libc::EAGAIN) | Some(libc::ENOENT) | Some(libc::EINTR)
                ) {
                    break;
                }
                tracing::warn!(%error, "protected-path fence listener failed");
                return;
            }
            let request = unsafe { &*request };
            let action = decide(request, &policy, &target_started);
            match respond(listener_fd, request, action, &mut resp) {
                SendOutcome::Delivered | SendOutcome::Canceled => {}
                SendOutcome::Failed(error) => {
                    tracing::warn!(%error, "protected-path fence supervisor stopped");
                    return;
                }
            }
        }
    }
}

enum Action {
    Continue,
    Deny(i32),
    Allow { val: i64 },
    Inject { fd: RawFd, cloexec: bool },
}

enum SendOutcome {
    Delivered,
    /// The notifying thread died before the response; nothing to answer.
    Canceled,
    Failed(io::Error),
}

/// Answer one notification. A canceled notification is not an error; a
/// failed response closes the fence so later reported syscalls fail closed
/// (`ENOSYS`) instead of running unchecked.
fn respond(
    listener: RawFd,
    request: &SeccompNotif,
    action: Action,
    buffer: &mut [u8],
) -> SendOutcome {
    match action {
        Action::Inject { fd, cloexec } => {
            let add = SeccompNotifAddfd {
                id: request.id,
                flags: SECCOMP_ADDFD_FLAG_SEND,
                srcfd: fd as u32,
                newfd: 0,
                newfd_flags: if cloexec { libc::O_CLOEXEC as u32 } else { 0 },
            };
            let rc = unsafe { libc::ioctl(listener, SECCOMP_IOCTL_NOTIF_ADDFD, &add) };
            unsafe {
                libc::close(fd);
            }
            if rc == 0 {
                // With SEND the kernel completes the syscall and returns the
                // installed descriptor; no explicit response is sent.
                return SendOutcome::Delivered;
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENOENT) {
                return SendOutcome::Canceled;
            }
            // Never leave the child blocked on an unanswered notification.
            let response = SeccompNotifResp {
                id: request.id,
                val: 0,
                error: -libc::EPERM,
                flags: 0,
            };
            unsafe {
                buffer
                    .as_mut_ptr()
                    .cast::<SeccompNotifResp>()
                    .write(response)
            };
            match unsafe { libc::ioctl(listener, SECCOMP_IOCTL_NOTIF_SEND, buffer.as_ptr()) } {
                0 => SendOutcome::Delivered,
                _ => {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() == Some(libc::ENOENT) {
                        SendOutcome::Canceled
                    } else {
                        SendOutcome::Failed(error)
                    }
                }
            }
        }
        action => {
            let response = match action {
                Action::Continue => SeccompNotifResp {
                    id: request.id,
                    val: 0,
                    error: 0,
                    flags: libc::SECCOMP_USER_NOTIF_FLAG_CONTINUE as u32,
                },
                Action::Deny(errno) => SeccompNotifResp {
                    id: request.id,
                    val: 0,
                    error: -errno,
                    flags: 0,
                },
                Action::Allow { val } => SeccompNotifResp {
                    id: request.id,
                    val,
                    error: 0,
                    flags: 0,
                },
                Action::Inject { .. } => unreachable!(),
            };
            unsafe {
                buffer
                    .as_mut_ptr()
                    .cast::<SeccompNotifResp>()
                    .write(response)
            };
            match unsafe { libc::ioctl(listener, SECCOMP_IOCTL_NOTIF_SEND, buffer.as_ptr()) } {
                0 => SendOutcome::Delivered,
                _ => {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() == Some(libc::ENOENT) {
                        SendOutcome::Canceled
                    } else {
                        SendOutcome::Failed(error)
                    }
                }
            }
        }
    }
}

fn decide(request: &SeccompNotif, policy: &FencePolicy, target_started: &AtomicBool) -> Action {
    if exec_rules()
        .iter()
        .any(|rule| rule.nr == request.data.nr as i64)
    {
        // The launcher exec'ing anything means the sandbox is handing over to
        // the program under test. Until then its setup runs in a private
        // mount namespace this supervisor cannot resolve.
        if is_launcher(request.pid, policy) {
            target_started.store(true, Ordering::SeqCst);
        }
        return Action::Continue;
    }
    let creation_table = creation_rules();
    let Some(rule) = creation_table
        .iter()
        .find(|rule| rule.nr == request.data.nr as i64)
    else {
        return Action::Continue;
    };
    if is_launcher(request.pid, policy) && !target_started.load(Ordering::SeqCst) {
        return Action::Continue;
    }
    match rule.kind {
        RuleKind::OpenAt {
            dirfd,
            path,
            flags,
            mode,
        } => {
            let flags = request.data.args[flags as usize] as i32;
            if flags & libc::O_CREAT == 0 || flags & libc::O_TMPFILE == libc::O_TMPFILE {
                return Action::Continue;
            }
            emulate_open(
                request.pid,
                dirfd_value(request, dirfd),
                request.data.args[path as usize],
                flags,
                request.data.args[mode as usize] as libc::mode_t,
                request.pid,
            )
        }
        RuleKind::OpenLegacy {
            path,
            flags,
            mode,
            force_create,
        } => {
            let flags = if force_create {
                libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC
            } else {
                let Some(index) = flags else {
                    return Action::Continue;
                };
                let flags = request.data.args[index as usize] as i32;
                if flags & libc::O_CREAT == 0 || flags & libc::O_TMPFILE == libc::O_TMPFILE {
                    return Action::Continue;
                }
                flags
            };
            emulate_open(
                request.pid,
                libc::AT_FDCWD,
                request.data.args[path as usize],
                flags,
                request.data.args[mode as usize] as libc::mode_t,
                request.pid,
            )
        }
        RuleKind::OpenAt2 { dirfd, path, how } => {
            let Some(how) =
                read_child_struct::<OpenHow>(request.pid, request.data.args[how as usize])
            else {
                return Action::Deny(libc::EPERM);
            };
            if how.flags & libc::O_CREAT as u64 == 0
                || how.flags & libc::O_TMPFILE as u64 == libc::O_TMPFILE as u64
            {
                return Action::Continue;
            }
            emulate_openat2(
                request.pid,
                dirfd_value(request, dirfd),
                request.data.args[path as usize],
                how,
            )
        }
        RuleKind::MkdirAt { dirfd, path, mode } => {
            emulate_at(request, dirfd, path, |parent, name, tid| {
                let mode = request.data.args[mode as usize] as libc::mode_t;
                let mode = apply_child_umask(tid, mode);
                let name = CString::new(name.as_bytes()).map_err(|_| libc::EINVAL)?;
                let rc = unsafe { libc::mkdirat(parent, name.as_ptr(), mode) };
                if rc == 0 { Ok(0) } else { Err(errno()) }
            })
        }
        RuleKind::MknodAt {
            dirfd,
            path,
            mode,
            dev,
        } => emulate_at(request, dirfd, path, |parent, name, tid| {
            let mode = request.data.args[mode as usize] as libc::mode_t;
            let mode = apply_child_umask(tid, mode);
            let name = CString::new(name.as_bytes()).map_err(|_| libc::EINVAL)?;
            let rc = unsafe {
                libc::mknodat(
                    parent,
                    name.as_ptr(),
                    mode,
                    request.data.args[dev as usize] as libc::dev_t,
                )
            };
            if rc == 0 { Ok(0) } else { Err(errno()) }
        }),
        RuleKind::SymlinkAt {
            target,
            dirfd,
            path,
        } => {
            let Some(target) = read_child_string(request.pid, request.data.args[target as usize])
            else {
                return Action::Deny(libc::EPERM);
            };
            emulate_at(request, dirfd, path, |parent, name, _tid| {
                let name = CString::new(name.as_bytes()).map_err(|_| libc::EINVAL)?;
                let target = CString::new(target.as_bytes()).map_err(|_| libc::EINVAL)?;
                let rc = unsafe { libc::symlinkat(target.as_ptr(), parent, name.as_ptr()) };
                if rc == 0 { Ok(0) } else { Err(errno()) }
            })
        }
        RuleKind::LinkAt {
            old_dirfd,
            old_path,
            new_dirfd,
            new_path,
            flags,
        } => emulate_link(
            request,
            old_dirfd,
            old_path,
            new_dirfd,
            new_path,
            request.data.args[flags as usize] as i32,
        ),
        RuleKind::RenameAt {
            old_dirfd,
            old_path,
            new_dirfd,
            new_path,
            flags,
        } => emulate_rename(
            request,
            old_dirfd,
            old_path,
            new_dirfd,
            new_path,
            flags
                .map(|index| request.data.args[index as usize] as u32)
                .unwrap_or(0),
        ),
    }
}

fn dirfd_value(request: &SeccompNotif, index: u8) -> i32 {
    if index == u8::MAX {
        libc::AT_FDCWD
    } else {
        request.data.args[index as usize] as i32
    }
}

fn is_launcher(tid: u32, policy: &FencePolicy) -> bool {
    let Ok(metadata) = std::fs::metadata(format!("/proc/{tid}/exe")) else {
        return false;
    };
    (metadata.dev(), metadata.ino()) == policy.launcher
}

struct Destination {
    parent: OwnedFd,
    parent_path: PathBuf,
    name: OsString,
}

fn emulate_at<F>(request: &SeccompNotif, dirfd: u8, path: u8, operation: F) -> Action
where
    F: FnOnce(RawFd, &OsStr, u32) -> Result<i64, i32>,
{
    let Some(path) = read_child_string(request.pid, request.data.args[path as usize]) else {
        return Action::Deny(libc::EPERM);
    };
    let destination =
        match resolve_destination(request.pid, dirfd_value(request, dirfd), &path, false) {
            Ok(Some(destination)) => destination,
            Ok(None) => return Action::Continue,
            Err(errno) => return Action::Deny(errno),
        };
    if !inside_writable_root(&destination.parent_path) {
        return Action::Continue;
    }
    if is_protected(&destination.parent_path, &destination.name) {
        return Action::Deny(libc::EPERM);
    }
    match operation(
        destination.parent.as_raw_fd(),
        &destination.name,
        request.pid,
    ) {
        Ok(val) => Action::Allow { val },
        Err(errno) => Action::Deny(errno),
    }
}

fn emulate_open(
    tid: u32,
    dirfd: i32,
    address: u64,
    flags: i32,
    mode: libc::mode_t,
    umask_tid: u32,
) -> Action {
    let Some(path) = read_child_string(tid, address) else {
        return Action::Deny(libc::EPERM);
    };
    if is_protected_name(&path) && is_absolute(&path) {
        // Cheap refusal for the common case before resolving anything.
        return Action::Deny(libc::EPERM);
    }
    let follow = flags & libc::O_NOFOLLOW == 0;
    let destination = match resolve_destination(tid, dirfd, &path, follow) {
        Ok(Some(destination)) => destination,
        Ok(None) => return Action::Continue,
        Err(errno) => return Action::Deny(errno),
    };
    if !inside_writable_root(&destination.parent_path) {
        return Action::Continue;
    }
    if is_protected(&destination.parent_path, &destination.name) {
        return Action::Deny(libc::EPERM);
    }
    open_and_inject(&destination, flags, apply_child_umask(umask_tid, mode))
}

fn emulate_openat2(tid: u32, dirfd: i32, address: u64, how: OpenHow) -> Action {
    let Some(path) = read_child_string(tid, address) else {
        return Action::Deny(libc::EPERM);
    };
    let Some(path_c) = to_cstring(&path) else {
        return Action::Deny(libc::EINVAL);
    };
    let destination = match resolve_destination(
        tid,
        dirfd,
        &path,
        (how.flags as i32 & libc::O_NOFOLLOW) == 0,
    ) {
        Ok(Some(destination)) => destination,
        Ok(None) => return Action::Continue,
        Err(errno) => return Action::Deny(errno),
    };
    if !inside_writable_root(&destination.parent_path) {
        return Action::Continue;
    }
    if is_protected(&destination.parent_path, &destination.name) {
        return Action::Deny(libc::EPERM);
    }
    let base = match child_base(tid, dirfd) {
        Ok(base) => base,
        Err(errno) => return Action::Deny(errno),
    };
    let mut effective = how;
    effective.mode = apply_child_umask(tid, how.mode as libc::mode_t) as u64;
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            base.as_raw_fd(),
            path_c.as_ptr(),
            &effective as *const OpenHow,
            std::mem::size_of::<OpenHow>(),
        )
    };
    if fd < 0 {
        return Action::Deny(errno());
    }
    let fd = fd as RawFd;
    // The descriptor we opened is authoritative about where the operation
    // landed; a symlink swapped in after the check cannot hide the target.
    if let Some(real) = descriptor_path(fd)
        && is_protected_path(&real)
    {
        unsafe {
            libc::close(fd);
        }
        return Action::Deny(libc::EPERM);
    }
    Action::Inject {
        fd,
        cloexec: how.flags as i32 & libc::O_CLOEXEC != 0,
    }
}

fn emulate_link(
    request: &SeccompNotif,
    old_dirfd: u8,
    old_path: u8,
    new_dirfd: u8,
    new_path: u8,
    flags: i32,
) -> Action {
    let (Some(old_path), Some(new_path)) = (
        read_child_string(request.pid, request.data.args[old_path as usize]),
        read_child_string(request.pid, request.data.args[new_path as usize]),
    ) else {
        return Action::Deny(libc::EPERM);
    };
    let destination = match resolve_destination(
        request.pid,
        dirfd_value(request, new_dirfd),
        &new_path,
        false,
    ) {
        Ok(Some(destination)) => destination,
        Ok(None) => return Action::Continue,
        Err(errno) => return Action::Deny(errno),
    };
    if !inside_writable_root(&destination.parent_path) {
        return Action::Continue;
    }
    if is_protected(&destination.parent_path, &destination.name) {
        return Action::Deny(libc::EPERM);
    }
    if flags & libc::AT_EMPTY_PATH != 0 && old_path.is_empty() {
        // The source is an open descriptor, which cannot be re-created from
        // the supervisor. The destination has been checked.
        return Action::Continue;
    }
    let source = match resolve_destination(
        request.pid,
        dirfd_value(request, old_dirfd),
        &old_path,
        false,
    ) {
        Ok(Some(source)) => source,
        Ok(None) => return Action::Continue,
        Err(errno) => return Action::Deny(errno),
    };
    if is_protected(&source.parent_path, &source.name) {
        return Action::Deny(libc::EPERM);
    }
    let mut source_path = source.parent_path.clone();
    source_path.push(&source.name);
    if is_mount_anchor(&source_path) {
        return Action::Continue;
    }
    let (Ok(old_c), Ok(new_c)) = (
        CString::new(source.name.as_bytes()),
        CString::new(destination.name.as_bytes()),
    ) else {
        return Action::Deny(libc::EINVAL);
    };
    let rc = unsafe {
        libc::linkat(
            source.parent.as_raw_fd(),
            old_c.as_ptr(),
            destination.parent.as_raw_fd(),
            new_c.as_ptr(),
            flags,
        )
    };
    if rc == 0 {
        Action::Allow { val: 0 }
    } else {
        Action::Deny(errno())
    }
}

fn emulate_rename(
    request: &SeccompNotif,
    old_dirfd: u8,
    old_path: u8,
    new_dirfd: u8,
    new_path: u8,
    flags: u32,
) -> Action {
    let (Some(old_path), Some(new_path)) = (
        read_child_string(request.pid, request.data.args[old_path as usize]),
        read_child_string(request.pid, request.data.args[new_path as usize]),
    ) else {
        return Action::Deny(libc::EPERM);
    };
    let destination = match resolve_destination(
        request.pid,
        dirfd_value(request, new_dirfd),
        &new_path,
        false,
    ) {
        Ok(Some(destination)) => destination,
        Ok(None) => return Action::Continue,
        Err(errno) => return Action::Deny(errno),
    };
    if !inside_writable_root(&destination.parent_path) {
        return Action::Continue;
    }
    let source = match resolve_destination(
        request.pid,
        dirfd_value(request, old_dirfd),
        &old_path,
        false,
    ) {
        Ok(Some(source)) => source,
        Ok(None) => return Action::Continue,
        Err(errno) => return Action::Deny(errno),
    };
    if is_protected(&destination.parent_path, &destination.name)
        || is_protected(&source.parent_path, &source.name)
    {
        return Action::Deny(libc::EPERM);
    }
    let mut source_path = source.parent_path.clone();
    source_path.push(&source.name);
    let mut destination_path = destination.parent_path.clone();
    destination_path.push(&destination.name);
    if is_mount_anchor(&source_path) || is_mount_anchor(&destination_path) {
        return Action::Continue;
    }
    let (Ok(old_c), Ok(new_c)) = (
        CString::new(source.name.as_bytes()),
        CString::new(destination.name.as_bytes()),
    ) else {
        return Action::Deny(libc::EINVAL);
    };
    let rc = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            source.parent.as_raw_fd(),
            old_c.as_ptr(),
            destination.parent.as_raw_fd(),
            new_c.as_ptr(),
            flags,
        )
    };
    if rc == 0 {
        Action::Allow { val: 0 }
    } else {
        Action::Deny(errno())
    }
}

fn resolve_destination(
    tid: u32,
    dirfd: i32,
    path: &OsStr,
    follow_final: bool,
) -> Result<Option<Destination>, i32> {
    let bytes = path.as_bytes();
    if bytes.is_empty() {
        return Ok(None);
    }
    let Some((parent_part, mut name)) = split_parent(bytes) else {
        return Ok(None);
    };
    let mut parent = child_base(tid, dirfd)?;
    if !parent_part.is_empty() {
        let parent_c = CString::new(parent_part).map_err(|_| libc::EINVAL)?;
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                parent_c.as_ptr(),
                libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            let errno = errno();
            return match errno {
                // A parent that exists only inside the sandbox's private
                // mount view, or one the command is about to fail on, cannot
                // name a host path this fence protects.
                libc::ENOENT | libc::ENOTDIR => Ok(None),
                other => Err(other),
            };
        }
        parent = unsafe { OwnedFd::from_raw_fd(fd) };
    }
    if follow_final {
        let mut hops = 0;
        loop {
            match stat_at(parent.as_raw_fd(), &name) {
                Ok(status) if status.st_mode & libc::S_IFMT == libc::S_IFLNK => {}
                Ok(_) => break,
                Err(libc::ENOENT) => break,
                Err(errno) => return Err(errno),
            }
            hops += 1;
            if hops > SYMLINK_LIMIT {
                return Err(libc::ELOOP);
            }
            let Some(target) = readlink_at(parent.as_raw_fd(), &name) else {
                return Err(libc::EIO);
            };
            let Some((target_parent, target_name)) = split_parent(target.as_bytes()) else {
                return Err(libc::ELOOP);
            };
            let mut next = if target.as_bytes().first() == Some(&b'/') {
                open_root()?
            } else {
                dup_fd(parent.as_raw_fd())?
            };
            if !target_parent.is_empty() {
                let parent_c = CString::new(target_parent).map_err(|_| libc::EINVAL)?;
                let fd = unsafe {
                    libc::openat(
                        next.as_raw_fd(),
                        parent_c.as_ptr(),
                        libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
                    )
                };
                if fd < 0 {
                    return Err(errno());
                }
                next = unsafe { OwnedFd::from_raw_fd(fd) };
            }
            parent = next;
            name = target_name;
            if name == OsStr::new(".") || name == OsStr::new("..") {
                return Ok(None);
            }
        }
    }
    let parent_path = match descriptor_path(parent.as_raw_fd()) {
        Some(path) => path,
        // The directory was unlinked after the child's syscall began. Nothing
        // under it can be a live protected path; the kernel semantics (create
        // in the unlinked directory) are preserved by continuing.
        None => return Ok(None),
    };
    Ok(Some(Destination {
        parent,
        parent_path,
        name,
    }))
}

fn open_and_inject(destination: &Destination, flags: i32, mode: libc::mode_t) -> Action {
    let Ok(name) = CString::new(destination.name.as_bytes()) else {
        return Action::Deny(libc::EINVAL);
    };
    // `O_NOFOLLOW` keeps the final component check race-free: a symlink
    // swapped in after resolution fails with ELOOP instead of redirecting.
    let effective = flags | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let fd = unsafe {
        libc::openat(
            destination.parent.as_raw_fd(),
            name.as_ptr(),
            effective,
            mode as libc::c_uint,
        )
    };
    if fd < 0 {
        return Action::Deny(errno());
    }
    if let Some(real) = descriptor_path(fd)
        && is_protected_path(&real)
    {
        unsafe {
            libc::close(fd);
        }
        return Action::Deny(libc::EPERM);
    }
    Action::Inject {
        fd,
        cloexec: flags & libc::O_CLOEXEC != 0,
    }
}

/// Whether this path is a bind-mount anchor in the sandbox's view: a writable
/// root itself, or an ancestor of one. Renaming such a directory is `EBUSY`
/// in the child (a mountpoint cannot move) but would succeed in the
/// supervisor's plain view; those operations are left to the sandbox so its
/// mount structure keeps its meaning. The protected-path decision still runs
/// first.
fn is_mount_anchor(path: &Path) -> bool {
    ROOTS.with(|roots| {
        roots
            .borrow()
            .iter()
            .any(|root| root == path || root.starts_with(path))
    })
}

fn inside_writable_root(parent: &Path) -> bool {
    ROOTS.with(|roots| roots.borrow().iter().any(|root| parent.starts_with(root)))
}

thread_local! {
    static ROOTS: std::cell::RefCell<Vec<PathBuf>> = const { std::cell::RefCell::new(Vec::new()) };
    static PROTECTED: std::cell::RefCell<Vec<PathBuf>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn is_protected_name(path: &OsStr) -> bool {
    Path::new(path)
        .file_name()
        .is_some_and(|name| name == OsStr::new("AGENTS.md"))
}

fn is_absolute(path: &OsStr) -> bool {
    path.as_bytes().first() == Some(&b'/')
}

fn is_protected(parent: &Path, name: &OsStr) -> bool {
    let mut full = parent.to_path_buf();
    full.push(name);
    is_protected_path(&full)
}

fn is_protected_path(full: &Path) -> bool {
    if full.file_name() == Some(OsStr::new("AGENTS.md")) {
        return true;
    }
    PROTECTED.with(|protected| {
        let protected = protected.borrow();
        protected
            .iter()
            .any(|path| full == path || full.starts_with(path))
    })
}

fn child_base(tid: u32, dirfd: i32) -> Result<OwnedFd, i32> {
    if dirfd == libc::AT_FDCWD {
        open_path(
            &format!("/proc/{tid}/cwd"),
            libc::O_PATH | libc::O_DIRECTORY,
        )
    } else {
        open_path(
            &format!("/proc/{tid}/fd/{dirfd}"),
            libc::O_PATH | libc::O_DIRECTORY,
        )
    }
}

fn open_root() -> Result<OwnedFd, i32> {
    open_path("/", libc::O_PATH | libc::O_DIRECTORY)
}

fn open_path(path: &str, flags: i32) -> Result<OwnedFd, i32> {
    let Ok(path) = CString::new(path) else {
        return Err(libc::EINVAL);
    };
    let fd = unsafe { libc::open(path.as_ptr(), flags | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(errno());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn dup_fd(fd: RawFd) -> Result<OwnedFd, i32> {
    let duplicated = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if duplicated < 0 {
        return Err(errno());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(duplicated) })
}

fn descriptor_path(fd: RawFd) -> Option<PathBuf> {
    let link = CString::new(format!("/proc/self/fd/{fd}")).ok()?;
    let mut buffer = vec![0u8; CHILD_PATH_LIMIT];
    let read = unsafe {
        libc::readlink(
            link.as_ptr(),
            buffer.as_mut_ptr() as *mut libc::c_char,
            buffer.len(),
        )
    };
    if read <= 0 {
        return None;
    }
    let bytes = &buffer[..read as usize];
    let bytes = bytes.strip_suffix(b" (deleted)").unwrap_or(bytes);
    Some(PathBuf::from(OsStr::from_bytes(bytes)))
}

fn stat_at(parent: RawFd, name: &OsStr) -> Result<libc::stat, i32> {
    let Ok(name) = CString::new(name.as_bytes()) else {
        return Err(libc::EINVAL);
    };
    let mut status: libc::stat = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::fstatat(
            parent,
            name.as_ptr(),
            &mut status,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc != 0 {
        return Err(errno());
    }
    Ok(status)
}

fn readlink_at(parent: RawFd, name: &OsStr) -> Option<OsString> {
    let name = CString::new(name.as_bytes()).ok()?;
    let mut buffer = vec![0u8; CHILD_PATH_LIMIT];
    let read = unsafe {
        libc::readlinkat(
            parent,
            name.as_ptr(),
            buffer.as_mut_ptr() as *mut libc::c_char,
            buffer.len(),
        )
    };
    if read < 0 {
        return None;
    }
    Some(OsStr::from_bytes(&buffer[..read as usize]).to_os_string())
}

/// Split a path into parent bytes and final component. Trailing slashes are
/// stripped; a path with no new name (root, `.`, `..`) returns `None`.
fn split_parent(path: &[u8]) -> Option<(&[u8], OsString)> {
    let mut end = path.len();
    while end > 1 && path[end - 1] == b'/' {
        end -= 1;
    }
    let trimmed = &path[..end];
    if trimmed.is_empty() || trimmed == b"/" {
        return None;
    }
    let (parent, name) = match trimmed.iter().rposition(|byte| *byte == b'/') {
        Some(index) => (&trimmed[..index], &trimmed[index + 1..]),
        None => (&trimmed[..0], trimmed),
    };
    if name.is_empty() || name == b"." || name == b".." {
        return None;
    }
    Some((parent, OsStr::from_bytes(name).to_os_string()))
}

fn read_child_string(tid: u32, address: u64) -> Option<OsString> {
    if address == 0 {
        return None;
    }
    let mut buffer = vec![0u8; CHILD_PATH_LIMIT + 1];
    let local = libc::iovec {
        iov_base: buffer.as_mut_ptr() as *mut libc::c_void,
        iov_len: CHILD_PATH_LIMIT,
    };
    let remote = libc::iovec {
        iov_base: address as *mut libc::c_void,
        iov_len: CHILD_PATH_LIMIT,
    };
    let read = unsafe { libc::process_vm_readv(tid as libc::pid_t, &local, 1, &remote, 1, 0) };
    if read <= 0 {
        return None;
    }
    let bytes = &buffer[..read as usize];
    let length = bytes.iter().position(|byte| *byte == 0)?;
    if length == 0 {
        return None;
    }
    Some(OsStr::from_bytes(&bytes[..length]).to_os_string())
}

fn read_child_struct<T: Copy>(tid: u32, address: u64) -> Option<T> {
    if address == 0 {
        return None;
    }
    let mut value = std::mem::MaybeUninit::<T>::uninit();
    let local = libc::iovec {
        iov_base: value.as_mut_ptr() as *mut libc::c_void,
        iov_len: std::mem::size_of::<T>(),
    };
    let remote = libc::iovec {
        iov_base: address as *mut libc::c_void,
        iov_len: std::mem::size_of::<T>(),
    };
    let read = unsafe { libc::process_vm_readv(tid as libc::pid_t, &local, 1, &remote, 1, 0) };
    if read as usize != std::mem::size_of::<T>() {
        return None;
    }
    Some(unsafe { value.assume_init() })
}

/// The child's umask is a process attribute this supervisor cannot adopt
/// without changing its own process. Applying it to the requested mode before
/// the kernel applies the supervisor's umask can only make the result more
/// restrictive, never more permissive.
fn apply_child_umask(tid: u32, mode: libc::mode_t) -> libc::mode_t {
    let Some(umask) = child_umask(tid) else {
        return mode;
    };
    mode & !umask
}

fn child_umask(tid: u32) -> Option<libc::mode_t> {
    let status = std::fs::read_to_string(format!("/proc/{tid}/status")).ok()?;
    for line in status.lines() {
        if let Some(value) = line.strip_prefix("Umask:") {
            return libc::mode_t::from_str_radix(value.trim(), 8).ok();
        }
    }
    None
}

fn to_cstring(value: &OsStr) -> Option<CString> {
    CString::new(value.as_bytes()).ok()
}

fn errno() -> i32 {
    io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

/// Populate the supervisor's per-thread view. Called by `supervise` before it
/// serves notifications; the values live for the thread's lifetime.
fn install_view(policy: &FencePolicy) {
    ROOTS.with(|roots| *roots.borrow_mut() = policy.writable_roots.clone());
    PROTECTED.with(|protected| *protected.borrow_mut() = policy.protected.clone());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_parent_strips_separators_and_rejects_non_names() {
        assert_eq!(
            split_parent(b"/ws/AGENTS.md"),
            Some((&b"/ws"[..], OsString::from("AGENTS.md")))
        );
        assert_eq!(
            split_parent(b"AGENTS.md"),
            Some((&b""[..], OsString::from("AGENTS.md")))
        );
        assert_eq!(
            split_parent(b"/ws/a/b/"),
            Some((&b"/ws/a"[..], OsString::from("b")))
        );
        assert!(split_parent(b"/").is_none());
        assert!(split_parent(b"/ws/").is_none());
        assert!(split_parent(b"/ws/.").is_none());
        assert!(split_parent(b"/ws/..").is_none());
    }

    #[test]
    fn protected_name_is_case_sensitive_on_linux() {
        assert!(is_protected_name(OsStr::new("AGENTS.md")));
        assert!(!is_protected_name(OsStr::new("agents.md")));
        assert!(!is_protected_name(OsStr::new("AGENTS.md.bak")));
    }

    #[test]
    fn every_rule_reads_arguments_within_the_seccomp_data() {
        for rule in creation_rules() {
            let fields = match rule.kind {
                RuleKind::OpenAt {
                    dirfd,
                    path,
                    flags,
                    mode,
                } => vec![dirfd, path, flags, mode],
                RuleKind::OpenLegacy {
                    path, flags, mode, ..
                } => {
                    let mut fields = vec![path, mode];
                    fields.extend(flags);
                    fields
                }
                RuleKind::OpenAt2 { dirfd, path, how } => vec![dirfd, path, how],
                RuleKind::MkdirAt { dirfd, path, mode } => vec![dirfd, path, mode],
                RuleKind::MknodAt {
                    dirfd,
                    path,
                    mode,
                    dev,
                } => vec![dirfd, path, mode, dev],
                RuleKind::SymlinkAt {
                    target,
                    dirfd,
                    path,
                } => vec![target, dirfd, path],
                RuleKind::LinkAt {
                    old_dirfd,
                    old_path,
                    new_dirfd,
                    new_path,
                    flags,
                } => {
                    vec![old_dirfd, old_path, new_dirfd, new_path, flags]
                }
                RuleKind::RenameAt {
                    old_dirfd,
                    old_path,
                    new_dirfd,
                    new_path,
                    flags,
                } => {
                    let mut fields = vec![old_dirfd, old_path, new_dirfd, new_path];
                    fields.extend(flags);
                    fields
                }
            };
            for field in fields {
                assert!(field == u8::MAX || (field as usize) < 6);
            }
        }
    }

    #[test]
    fn filter_ends_in_allow_and_contains_no_io_uring_escape() {
        let program = FilterProgram::build(&creation_rules(), &exec_rules());
        let last = program.instructions.last().expect("non-empty program");
        assert_eq!(last.k, libc::SECCOMP_RET_ALLOW);
        let io_uring = program
            .instructions
            .windows(2)
            .filter(|pair| {
                pair[0].k == 425 && pair[1].k == (libc::SECCOMP_RET_ERRNO | libc::EPERM as u32)
            })
            .count();
        assert_eq!(io_uring, 1);
    }

    #[test]
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    fn native_arch_is_the_linux_audit_arch() {
        // linux/audit.h: AUDIT_ARCH_<arch> = EM_<arch> | 64BIT | LE.
        const AUDIT_64BIT_LE: u32 = 0xC000_0000;
        #[cfg(target_arch = "x86_64")]
        assert_eq!(native_arch(), AUDIT_64BIT_LE | libc::EM_X86_64 as u32);
        #[cfg(target_arch = "aarch64")]
        assert_eq!(native_arch(), AUDIT_64BIT_LE | libc::EM_AARCH64 as u32);
    }
}
