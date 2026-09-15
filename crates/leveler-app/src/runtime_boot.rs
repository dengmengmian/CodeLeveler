//! Boot identity: which execution incarnation of a runtime is alive.
//!
//! [`leveler_core::RuntimeId`] names the durable runtime and survives restarts;
//! it cannot say whether the process that did something is still running. A
//! boot can: each one mints a fresh [`BootId`] and holds an exclusive OS lock on
//! `<state_dir>/boots/<boot_id>.lock` for as long as it lives. The OS releases
//! that lock when the holder exits by any means — clean exit, panic, SIGKILL —
//! so another runtime can take it exactly when the boot is gone. BootIds are
//! never reused, so "this boot has ended" can never become false again.
//!
//! The lock is per boot, not per state directory: any number of live boots may
//! share one database.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use fs2::FileExt;

use leveler_core::BootId;

const BOOTS_DIR: &str = "boots";

/// Whether a boot still has a live holder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootLiveness {
    Live,
    Ended,
}

/// A live boot: its id and the locked file that proves it. Dropping it (or the
/// process dying) ends the boot.
#[derive(Debug)]
pub struct RuntimeBootLease {
    id: BootId,
    path: PathBuf,
    _file: File,
}

impl RuntimeBootLease {
    /// Start a new boot under `state_dir`.
    pub fn acquire(state_dir: &Path) -> io::Result<Self> {
        let id = BootId::generate();
        let dir = state_dir.join(BOOTS_DIR);
        std::fs::create_dir_all(&dir)?;
        let path = lock_path(state_dir, &id)?;
        let file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)?;
        file.try_lock_exclusive()?;
        Ok(Self {
            id,
            path,
            _file: file,
        })
    }

    pub fn id(&self) -> &BootId {
        &self.id
    }
}

impl Drop for RuntimeBootLease {
    fn drop(&mut self) {
        // Housekeeping only: a missing lock file reads as an ended boot, the
        // same answer the released lock gives, so a failed removal (or one
        // skipped by a crash) changes nothing.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Whether `boot` still has a live holder. Taking its lock succeeds only when
/// no process holds it; the probe's own lock is released at once, which is
/// harmless because an ended boot never comes back.
pub fn boot_liveness(state_dir: &Path, boot: &BootId) -> io::Result<BootLiveness> {
    let path = lock_path(state_dir, boot)?;
    let file = match OpenOptions::new().read(true).write(true).open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BootLiveness::Ended),
        Err(error) => return Err(error),
    };
    match file.try_lock_exclusive() {
        Ok(()) => Ok(BootLiveness::Ended),
        Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {
            Ok(BootLiveness::Live)
        }
        Err(error) => Err(error),
    }
}

/// The id comes back from the database; it only ever names a file inside the
/// boots directory.
fn lock_path(state_dir: &Path, boot: &BootId) -> io::Result<PathBuf> {
    let id = boot.as_str();
    if id.is_empty() || id.len() > 64 || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("not a boot id: {id:?}"),
        ));
    }
    Ok(state_dir.join(BOOTS_DIR).join(format!("{id}.lock")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_held_lease_is_live_and_a_released_one_has_ended() {
        let dir = tempfile::tempdir().unwrap();
        let lease = RuntimeBootLease::acquire(dir.path()).unwrap();
        let id = lease.id().clone();
        assert_eq!(boot_liveness(dir.path(), &id).unwrap(), BootLiveness::Live);
        drop(lease);
        assert_eq!(boot_liveness(dir.path(), &id).unwrap(), BootLiveness::Ended);
    }

    /// A crashed boot leaves its file behind with nobody holding the lock.
    #[test]
    fn an_unheld_lock_file_is_an_ended_boot() {
        let dir = tempfile::tempdir().unwrap();
        let lease = RuntimeBootLease::acquire(dir.path()).unwrap();
        let id = lease.id().clone();
        let path = lease.path.clone();
        std::mem::forget(lease); // keep the file, as SIGKILL would
        // The forgotten handle still holds the lock in this process; emulate
        // the OS releasing it by replacing the file with an unlocked one.
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"").unwrap();
        assert_eq!(boot_liveness(dir.path(), &id).unwrap(), BootLiveness::Ended);
    }

    #[test]
    fn a_boot_that_left_no_file_has_ended() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            boot_liveness(dir.path(), &BootId::generate()).unwrap(),
            BootLiveness::Ended
        );
    }

    #[test]
    fn every_acquire_is_a_new_boot_and_two_can_live_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let a = RuntimeBootLease::acquire(dir.path()).unwrap();
        let b = RuntimeBootLease::acquire(dir.path()).unwrap();
        assert_ne!(a.id(), b.id());
        assert_eq!(
            boot_liveness(dir.path(), a.id()).unwrap(),
            BootLiveness::Live
        );
        assert_eq!(
            boot_liveness(dir.path(), b.id()).unwrap(),
            BootLiveness::Live
        );
    }

    #[test]
    fn a_stored_id_cannot_name_a_path_outside_the_boots_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert!(boot_liveness(dir.path(), &BootId::new("../runtime-id")).is_err());
    }
}
