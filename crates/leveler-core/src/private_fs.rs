//! Owner-only permissions for durable files that carry user data.
//!
//! SQLite creates a database with the process umask, which on a typical Unix
//! account leaves it readable by every local user. Session history contains
//! the user's conversation and tool arguments/results; the file and its WAL
//! sidecars must be readable only by their owner. Windows inherits the user
//! profile ACL, so these helpers are Unix-only in effect.

use std::io;
use std::path::Path;

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

/// Make a file that stores private user data owner-only (`0600`).
pub fn restrict_file(path: &Path) -> io::Result<()> {
    set_mode(path, 0o600)
}

/// Make a directory that contains private user data owner-only (`0700`).
pub fn restrict_dir(path: &Path) -> io::Result<()> {
    set_mode(path, 0o700)
}

/// Restrict a SQLite database and the `-wal` / `-shm` files beside it.
///
/// SQLite creates the sidecars with the database's mode, but an existing
/// database may predate this call, so every present sidecar is restricted
/// explicitly. A missing sidecar is not an error.
pub fn restrict_sqlite(path: &Path) -> io::Result<()> {
    restrict_file(path)?;
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(suffix);
        let sidecar = Path::new(&sidecar);
        if sidecar.exists() {
            restrict_file(sidecar)?;
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_restricted_file_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        std::fs::write(&path, b"db").unwrap();
        super::restrict_file(&path).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        super::restrict_dir(dir.path()).unwrap();
        assert_eq!(
            std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
}
