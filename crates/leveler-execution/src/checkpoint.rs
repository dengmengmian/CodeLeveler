//! A lightweight checkpoint: the original content of every file a session
//! touches, captured just before the first write (spec §28 CheckpointHook).
//!
//! Restoring rewrites originals and removes files that did not previously exist,
//! so an interrupted or bad run can be rolled back exactly to its starting tree.

use std::collections::HashMap;
use std::fs::Permissions;
use std::io::{Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Originals larger than this spill to a temp file instead of staying
/// resident: a session that touches a build artifact or data file must not pin
/// megabytes of RAM per file for the whole run.
const MAX_IN_MEMORY_ORIGINAL: usize = 4 * 1024 * 1024;

/// The captured original state of one touched path.
enum Original {
    /// The file did not exist before the first write.
    Absent,
    InMemory {
        bytes: Vec<u8>,
        permissions: Permissions,
    },
    /// Large original spilled to a session temp file (removed on reset/drop).
    Spilled {
        file: tempfile::NamedTempFile,
        permissions: Permissions,
    },
}

/// Records the pre-modification state of touched files.
#[derive(Default)]
pub struct Checkpoint {
    originals: Mutex<HashMap<PathBuf, Original>>,
}

impl Checkpoint {
    pub fn new() -> Self {
        Self::default()
    }

    /// Capture the current state of `path` the first time it is touched. Reading
    /// happens synchronously; subsequent touches of the same path are ignored so
    /// the earliest (true original) state is preserved.
    pub fn record(&self, path: &Path) -> std::io::Result<()> {
        let mut map = self.originals.lock().unwrap();
        if map.contains_key(path) {
            return Ok(());
        }
        // Size-gate BEFORE reading: a large original is spilled via a streaming
        // copy so recording a multi-GB artifact never loads it into memory.
        let original = match std::fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Original::Absent,
            Err(error) => return Err(error),
            Ok(meta) if !meta.file_type().is_file() => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "checkpoint target is not a regular file: {}",
                        path.display()
                    ),
                ));
            }
            Ok(meta) if meta.len() as usize <= MAX_IN_MEMORY_ORIGINAL => Original::InMemory {
                bytes: std::fs::read(path)?,
                permissions: meta.permissions(),
            },
            Ok(meta) => match spill_copy(path) {
                Ok(file) => Original::Spilled {
                    file,
                    permissions: meta.permissions(),
                },
                // Rollback safety beats memory: fall back to reading it
                // resident rather than silently losing the ability to restore.
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "could not spill large checkpoint original; reading it into memory"
                    );
                    Original::InMemory {
                        bytes: std::fs::read(path)?,
                        permissions: meta.permissions(),
                    }
                }
            },
        };
        map.insert(path.to_path_buf(), original);
        Ok(())
    }

    /// Record bytes already read from a locked file descriptor. Commit paths
    /// use this after their compare step so a failed stale write cannot poison
    /// the checkpoint with unrelated on-disk state.
    pub fn record_captured(&self, path: &Path, bytes: Vec<u8>, permissions: Permissions) {
        self.originals
            .lock()
            .unwrap()
            .entry(path.to_path_buf())
            .or_insert(Original::InMemory { bytes, permissions });
    }

    /// Record that a create target was absent at its successful commit point.
    pub fn record_absent(&self, path: &Path) {
        self.originals
            .lock()
            .unwrap()
            .entry(path.to_path_buf())
            .or_insert(Original::Absent);
    }

    /// Clear all captured state, making the current tree the new restore point
    /// (an explicit `create_checkpoint`).
    pub fn reset(&self) {
        let mut map = self.originals.lock().unwrap();
        map.clear();
    }

    /// The number of distinct files captured.
    pub fn touched_count(&self) -> usize {
        self.originals.lock().unwrap().len()
    }

    /// Total original-content bytes currently held in memory (not spilled).
    pub fn in_memory_bytes(&self) -> usize {
        self.originals
            .lock()
            .unwrap()
            .values()
            .map(|v| match v {
                Original::InMemory { bytes, .. } => bytes.len(),
                Original::Absent | Original::Spilled { .. } => 0,
            })
            .sum()
    }

    /// Whether anything has been recorded.
    pub fn is_empty(&self) -> bool {
        self.originals.lock().unwrap().is_empty()
    }

    /// Restore every touched file to its captured state.
    pub fn restore(&self) -> std::io::Result<()> {
        let map = self.originals.lock().unwrap();
        for (path, original) in map.iter() {
            match original {
                Original::InMemory { bytes, permissions } => {
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    replace_atomically(path, permissions, |tmp| tmp.write_all(bytes))?;
                }
                Original::Spilled { file, permissions } => {
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    replace_atomically(path, permissions, |tmp| {
                        let mut source = file.reopen()?;
                        source.rewind()?;
                        std::io::copy(&mut source, tmp).map(|_| ())
                    })?;
                }
                Original::Absent => match std::fs::remove_file(path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                },
            }
        }
        Ok(())
    }
}

/// Stream a large original into a unique session temp file without loading it
/// into memory.
fn spill_copy(src: &Path) -> std::io::Result<tempfile::NamedTempFile> {
    let mut source = std::fs::File::open(src)?;
    let mut spill = tempfile::Builder::new()
        .prefix("leveler-checkpoint-")
        .suffix(".orig")
        .tempfile()?;
    std::io::copy(&mut source, spill.as_file_mut())?;
    spill.as_file_mut().flush()?;
    Ok(spill)
}

/// Replace `path` by staging into a same-directory temp file and renaming, so
/// an interrupted restore never leaves a truncated target.
fn replace_atomically(
    path: &Path,
    permissions: &Permissions,
    stage: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::Builder::new()
        .prefix(".leveler-restore-")
        .tempfile_in(parent)?;
    stage(tmp.as_file_mut())?;
    tmp.as_file().set_permissions(permissions.clone())?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map(|_| ()).map_err(|error| error.error)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "leveler-ckpt-{}",
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn restores_modified_file() {
        let dir = tmp();
        let file = dir.join("a.txt");
        std::fs::write(&file, "original").unwrap();
        let cp = Checkpoint::new();
        cp.record(&file).unwrap();
        std::fs::write(&file, "modified").unwrap();
        cp.restore().unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "original");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn restore_preserves_original_unix_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tmp();
        let file = dir.join("script.sh");
        std::fs::write(&file, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o751)).unwrap();
        let cp = Checkpoint::new();
        cp.record(&file).unwrap();

        std::fs::write(&file, "changed\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        cp.restore().unwrap();

        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o751
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn record_rejects_non_regular_files_instead_of_treating_them_as_absent() {
        let dir = tmp();
        let cp = Checkpoint::new();
        assert!(
            cp.record(&dir).is_err(),
            "a directory/read error must not become an Absent checkpoint"
        );
        assert_eq!(cp.touched_count(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn removes_newly_created_file_on_restore() {
        let dir = tmp();
        let file = dir.join("new.txt");
        let cp = Checkpoint::new();
        cp.record(&file).unwrap(); // does not exist yet
        std::fs::write(&file, "created").unwrap();
        cp.restore().unwrap();
        assert!(!file.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn large_originals_do_not_stay_resident_in_memory() {
        // A session touching a big artifact (build output, data file) must not
        // pin its whole original in RAM — spill it, but restore must still
        // reproduce it byte-for-byte.
        let dir = tmp();
        let file = dir.join("big.bin");
        let original: Vec<u8> = (0..6 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&file, &original).unwrap();

        let cp = Checkpoint::new();
        cp.record(&file).unwrap();
        assert!(
            cp.in_memory_bytes() < 5 * 1024 * 1024,
            "a 6MB original must not be held in memory ({} bytes resident)",
            cp.in_memory_bytes()
        );

        std::fs::write(&file, b"clobbered").unwrap();
        cp.restore().unwrap();
        assert_eq!(
            std::fs::read(&file).unwrap(),
            original,
            "spilled originals must still restore byte-for-byte"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn failed_restore_is_retryable_and_leaves_no_temp_residue() {
        // One unrestorable file must not poison the checkpoint: the captured
        // originals survive the error, a retry completes the rollback, and no
        // staging temp files are left behind.
        let dir = tmp();
        let plain = dir.join("plain.txt");
        let nested = dir.join("sub").join("nested.txt");
        std::fs::create_dir_all(nested.parent().unwrap()).unwrap();
        std::fs::write(&plain, "plain-original").unwrap();
        std::fs::write(&nested, "nested-original").unwrap();

        let cp = Checkpoint::new();
        cp.record(&plain).unwrap();
        cp.record(&nested).unwrap();
        std::fs::write(&plain, "plain-modified").unwrap();
        std::fs::write(&nested, "nested-modified").unwrap();

        // Block the nested restore: its parent dir becomes a plain file.
        std::fs::remove_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub"), "blocker").unwrap();
        assert!(
            cp.restore().is_err(),
            "blocked restore must surface the error"
        );

        // Unblock and retry: BOTH files must come back to their originals.
        std::fs::remove_file(dir.join("sub")).unwrap();
        cp.restore().unwrap();
        assert_eq!(std::fs::read_to_string(&plain).unwrap(), "plain-original");
        assert_eq!(std::fs::read_to_string(&nested).unwrap(), "nested-original");

        // No staging residue anywhere in the tree.
        for entry in walkdir(&dir) {
            assert!(
                !entry.to_string_lossy().contains("restore-tmp"),
                "staging temp must not survive: {entry:?}"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    fn walkdir(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Ok(read) = std::fs::read_dir(dir) {
            for e in read.flatten() {
                let p = e.path();
                if p.is_dir() {
                    out.extend(walkdir(&p));
                } else {
                    out.push(p);
                }
            }
        }
        out
    }

    #[test]
    fn first_record_wins() {
        let dir = tmp();
        let file = dir.join("a.txt");
        std::fs::write(&file, "v1").unwrap();
        let cp = Checkpoint::new();
        cp.record(&file).unwrap();
        std::fs::write(&file, "v2").unwrap();
        cp.record(&file).unwrap(); // ignored
        cp.restore().unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "v1");
        std::fs::remove_dir_all(&dir).ok();
    }
}
