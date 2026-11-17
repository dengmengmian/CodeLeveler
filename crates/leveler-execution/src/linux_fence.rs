//! Policy for the Linux syscall fence over missing protected paths.
//!
//! `bwrap` can bind only an existing destination, so a protected path that
//! does not exist yet has no read-only mount. The kernel mechanism (a
//! `seccomp` user-notification supervisor that performs creation syscalls
//! itself) lives in the audited `unsafe` crate `leveler-linux-confine`; this
//! module decides *what* to protect and refuses the command when the fence
//! cannot be established.
//!
//! The protected set is [`protected_write_paths`]: the same list the macOS
//! profile binds read-only and the Windows integrity lease labels. The fence
//! additionally denies creating any entry named `AGENTS.md` below a writable
//! root, matching the macOS path rule for nested project instructions.

use std::ffi::OsString;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::WriteScope;
use crate::command::protected_write_paths;

/// The policy handed to the kernel supervisor, retained so a failed
/// activation keeps the reason.
pub(crate) struct FenceSpec {
    policy: leveler_linux_confine::FencePolicy,
}

pub(crate) type LinuxFence = leveler_linux_confine::Fence;

/// Decide whether this scope needs the syscall fence. `None` means every
/// protected path already exists and the `bwrap` read-only bind covers it.
pub(crate) fn prepare(
    scope: &WriteScope,
    writable_roots: &[PathBuf],
    environment_path: Option<OsString>,
) -> Result<Option<FenceSpec>, String> {
    if scope.root().is_none() {
        return Ok(None);
    }
    let mut missing = Vec::new();
    for path in protected_write_paths(scope) {
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) => {
                // An existing path is re-bound read-only. A symlink is not:
                // the bind would cover its target, not the name the sandbox
                // resolves, so keep it in the fence.
                if !metadata.file_type().is_symlink() {
                    continue;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "cannot inspect protected path {}: {error}",
                    path.display()
                ));
            }
        }
        missing.push(canonicalize_with_missing_tail(&path)?);
    }
    if missing.is_empty() {
        return Ok(None);
    }
    let roots = writable_roots
        .iter()
        .filter(|path| path.is_dir())
        .map(|path| path.canonicalize().unwrap_or_else(|_| path.clone()))
        .collect::<Vec<_>>();
    let launcher = resolve_launcher(environment_path)
        .ok_or_else(|| "the Linux sandbox launcher `bwrap` was not found on PATH".to_string())?;
    Ok(Some(FenceSpec {
        policy: leveler_linux_confine::FencePolicy {
            protected: missing,
            writable_roots: roots,
            launcher,
        },
    }))
}

/// Canonicalize the deepest existing ancestor and append the missing tail, so
/// the comparison uses the same resolved spelling the supervisor observes.
fn canonicalize_with_missing_tail(path: &Path) -> Result<PathBuf, String> {
    let mut tail: Vec<OsString> = Vec::new();
    let mut cursor = path.to_path_buf();
    loop {
        match cursor.canonicalize() {
            Ok(mut base) => {
                for name in tail.iter().rev() {
                    base.push(name);
                }
                return Ok(base);
            }
            Err(_) => {
                let Some(name) = cursor.file_name().map(|name| name.to_os_string()) else {
                    return Err(format!("cannot resolve protected path {}", path.display()));
                };
                tail.push(name);
                let Some(parent) = cursor.parent() else {
                    return Err(format!("cannot resolve protected path {}", path.display()));
                };
                if parent == cursor {
                    return Err(format!("cannot resolve protected path {}", path.display()));
                }
                cursor = parent.to_path_buf();
            }
        }
    }
}

fn resolve_launcher(environment_path: Option<OsString>) -> Option<(u64, u64)> {
    let path = environment_path.or_else(|| std::env::var_os("PATH"))?;
    for directory in std::env::split_paths(&path) {
        let candidate = if directory.as_os_str().is_empty() {
            PathBuf::from("bwrap")
        } else {
            directory.join("bwrap")
        };
        let Ok(metadata) = std::fs::metadata(&candidate) else {
            continue;
        };
        if metadata.is_file() {
            return Some((metadata.dev(), metadata.ino()));
        }
    }
    None
}

/// Attach the filter to the sandbox launcher. The returned launcher must be
/// activated after `spawn`; the child blocks in the kernel until then.
pub(crate) fn install(
    cmd: &mut std::process::Command,
    spec: FenceSpec,
) -> io::Result<leveler_linux_confine::FenceLauncher> {
    leveler_linux_confine::install(cmd, spec.policy)
}
