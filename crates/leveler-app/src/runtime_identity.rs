//! Durable local runtime identity.
//!
//! One `RuntimeId` per runtime state directory, persisted in
//! `<state_dir>/runtime-id`. A daemon restart reads the same id back; the id
//! changes only when the state directory itself is re-initialized. Never
//! derived from PID, hostname, or hardware.
//!
//! Concurrency: creation takes an exclusive `flock` on a sibling lock file,
//! so two daemons racing first startup cannot mint two identities — one
//! writes, the other reads the winner's id. The id file itself is written
//! via temp-file + rename, so a crashed writer can never leave a torn value.
//!
//! Corruption policy: an unreadable or malformed id file is a hard, named
//! error — never a silent regeneration, which would change the runtime's
//! identity behind the operator's back.

use std::io::Write;
use std::path::{Path, PathBuf};

use fs2::FileExt;

use leveler_core::RuntimeId;

/// File under the state dir holding the identity (one line, opaque token).
const RUNTIME_ID_FILE: &str = "runtime-id";
/// Sibling lock taken while creating the identity.
const RUNTIME_ID_LOCK: &str = "runtime-id.lock";
/// Upper bound accepted when reading back a persisted id. Generated ids are
/// UUIDs (36 bytes); the bound only rejects garbage, not future formats.
const MAX_ID_LEN: usize = 128;

/// A persisted runtime-id file that could not be used.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeIdentityError {
    #[error("cannot access runtime identity at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The file exists but its content is not a plausible identity. This is
    /// state corruption: refusing to guess (or silently re-mint) is the only
    /// way the identity contract — "stable until explicitly re-initialized" —
    /// stays trustworthy.
    #[error(
        "runtime identity file {path} is corrupt ({reason}); \
         remove the file to explicitly re-initialize this runtime's identity"
    )]
    Corrupt { path: PathBuf, reason: String },
}

/// Load the state directory's runtime identity, minting and persisting one on
/// first use. Restart-stable: subsequent calls (and subsequent processes)
/// return the same id.
pub fn load_or_create_runtime_id(state_dir: &Path) -> Result<RuntimeId, RuntimeIdentityError> {
    let id_path = state_dir.join(RUNTIME_ID_FILE);
    // Fast path: no lock needed once the id exists (rename is atomic, so a
    // readable file is always complete).
    match read_runtime_id(&id_path) {
        Ok(Some(id)) => return Ok(id),
        Ok(None) => {}
        Err(error) => return Err(error),
    }

    std::fs::create_dir_all(state_dir).map_err(|source| RuntimeIdentityError::Io {
        path: state_dir.to_path_buf(),
        source,
    })?;
    let lock_path = state_dir.join(RUNTIME_ID_LOCK);
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|source| RuntimeIdentityError::Io {
            path: lock_path.clone(),
            source,
        })?;
    lock.lock_exclusive()
        .map_err(|source| RuntimeIdentityError::Io {
            path: lock_path.clone(),
            source,
        })?;
    // Somebody else may have won the race while we waited for the lock.
    let result = match read_runtime_id(&id_path) {
        Ok(Some(id)) => Ok(id),
        Ok(None) => mint_runtime_id(state_dir, &id_path),
        Err(error) => Err(error),
    };
    let _ = fs2::FileExt::unlock(&lock);
    result
}

/// `Ok(None)` = no identity yet (first start). `Err` = present but unusable.
fn read_runtime_id(id_path: &Path) -> Result<Option<RuntimeId>, RuntimeIdentityError> {
    let raw = match std::fs::read_to_string(id_path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(RuntimeIdentityError::Io {
                path: id_path.to_path_buf(),
                source,
            });
        }
    };
    let id = raw.trim();
    if id.is_empty() {
        return Err(RuntimeIdentityError::Corrupt {
            path: id_path.to_path_buf(),
            reason: "empty".to_string(),
        });
    }
    if id.len() > MAX_ID_LEN {
        return Err(RuntimeIdentityError::Corrupt {
            path: id_path.to_path_buf(),
            reason: format!("{} bytes exceeds the {MAX_ID_LEN}-byte bound", id.len()),
        });
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(RuntimeIdentityError::Corrupt {
            path: id_path.to_path_buf(),
            reason: "contains characters outside [A-Za-z0-9_-]".to_string(),
        });
    }
    Ok(Some(RuntimeId::new(id)))
}

/// Write a fresh id via temp file + atomic rename (caller holds the lock).
fn mint_runtime_id(state_dir: &Path, id_path: &Path) -> Result<RuntimeId, RuntimeIdentityError> {
    let id = RuntimeId::generate();
    let tmp_path = state_dir.join(format!("{RUNTIME_ID_FILE}.tmp-{}", std::process::id()));
    let io = |source| RuntimeIdentityError::Io {
        path: id_path.to_path_buf(),
        source,
    };
    let mut tmp = std::fs::File::create(&tmp_path).map_err(io)?;
    tmp.write_all(id.as_str().as_bytes()).map_err(io)?;
    tmp.write_all(b"\n").map_err(io)?;
    tmp.sync_all().map_err(io)?;
    drop(tmp);
    std::fs::rename(&tmp_path, id_path).map_err(io)?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_start_mints_and_restart_reads_the_same_id() {
        let dir = tempfile::tempdir().unwrap();
        let first = load_or_create_runtime_id(dir.path()).unwrap();
        // "Restart": a fresh call with no shared in-memory state.
        let second = load_or_create_runtime_id(dir.path()).unwrap();
        assert_eq!(first, second);
        // The persisted form is the id itself, newline-terminated.
        let on_disk = std::fs::read_to_string(dir.path().join(RUNTIME_ID_FILE)).unwrap();
        assert_eq!(on_disk.trim(), first.as_str());
    }

    #[test]
    fn explicit_reinitialization_changes_the_id() {
        let dir = tempfile::tempdir().unwrap();
        let first = load_or_create_runtime_id(dir.path()).unwrap();
        std::fs::remove_file(dir.path().join(RUNTIME_ID_FILE)).unwrap();
        let second = load_or_create_runtime_id(dir.path()).unwrap();
        assert_ne!(first, second, "removing the file IS the reinitialization");
    }

    #[test]
    fn corrupt_id_file_is_a_named_error_not_a_silent_regeneration() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(RUNTIME_ID_FILE);

        std::fs::write(&path, "").unwrap();
        assert!(matches!(
            load_or_create_runtime_id(dir.path()),
            Err(RuntimeIdentityError::Corrupt { .. })
        ));

        std::fs::write(&path, "two\nlines").unwrap();
        assert!(matches!(
            load_or_create_runtime_id(dir.path()),
            Err(RuntimeIdentityError::Corrupt { .. })
        ));

        std::fs::write(&path, "x".repeat(MAX_ID_LEN + 1)).unwrap();
        assert!(matches!(
            load_or_create_runtime_id(dir.path()),
            Err(RuntimeIdentityError::Corrupt { .. })
        ));

        // The corrupt content was never replaced behind the operator's back.
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "x".repeat(MAX_ID_LEN + 1)
        );
    }

    #[test]
    fn concurrent_first_starts_agree_on_one_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || load_or_create_runtime_id(&path).unwrap())
            })
            .collect();
        let ids: Vec<RuntimeId> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert!(
            ids.windows(2).all(|w| w[0] == w[1]),
            "racing first starts must converge on one id: {ids:?}"
        );
    }
}
